//! Persistent, opaque Docker API streams over one helper stdio session.
//! CREDIT bounds each stream independently; the dispatcher never waits for
//! a stream's consumer. EOF is directional, CLOSE terminates only that id.
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::{mpsc, oneshot, watch, Semaphore},
};

const OPEN: u8 = 1;
const DATA: u8 = 2;
const EOF: u8 = 3;
const CLOSE: u8 = 4;
const READY: u8 = 5;
const CREDIT: u8 = 6;
const MAX_PAYLOAD: usize = 256 * 1024;
const WINDOW: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

enum Inbound {
    Data(Vec<u8>),
    Eof,
    Close,
}
struct Entry {
    input: mpsc::Sender<Inbound>,
    credit: Arc<Semaphore>,
    opened: Option<oneshot::Sender<()>>,
}
struct State {
    next: u32,
    entries: HashMap<u32, Entry>,
}
struct Inner {
    state: Mutex<State>,
    output: mpsc::Sender<Vec<u8>>,
    closed: watch::Sender<bool>,
}
#[derive(Clone)]
pub(super) struct Session(Arc<Inner>);

pub(super) struct Stream {
    io: DuplexStream,
    cancel: Option<oneshot::Sender<()>>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.cancel.take();
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(9 + payload.len());
    result.push(kind);
    result.extend_from_slice(&id.to_be_bytes());
    result.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    result.extend_from_slice(payload);
    result
}
async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<(u8, u32, Vec<u8>)> {
    let mut header = [0; 9];
    reader.read_exact(&mut header).await?;
    let kind = header[0];
    let id = u32::from_be_bytes(header[1..5].try_into().unwrap());
    let len = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD
        || !matches!(kind, OPEN | DATA | EOF | CLOSE | READY | CREDIT)
        || (kind != DATA && len != 0)
        || ((kind == READY) != (id == 0))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Docker API frame",
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await?;
    Ok((kind, id, payload))
}
impl Session {
    pub(super) fn is_closed(&self) -> bool {
        *self.0.closed.borrow()
    }
    pub(super) fn close(&self) {
        self.0.closed.send_replace(true);
    }
    pub(super) async fn closed(&self) {
        let mut closed = self.0.closed.subscribe();
        if !*closed.borrow_and_update() {
            let _ = closed.changed().await;
        }
    }
    pub(super) async fn start(
        mut reader: impl AsyncRead + Unpin + Send + 'static,
        mut writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> io::Result<Self> {
        // Only handshake is timed/cancelled; the frame reader task below is
        // never cancelled between partial header/payload reads.
        let (kind, _, _) = tokio::time::timeout(CONNECT_TIMEOUT, read_frame(&mut reader)).await??;
        if kind != READY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "helper not ready",
            ));
        }
        let (output, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let (closed, _) = watch::channel(false);
        let session = Self(Arc::new(Inner {
            state: Mutex::new(State {
                next: 1,
                entries: HashMap::new(),
            }),
            output,
            closed,
        }));
        let task = session.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = task.closed() => {}
                _ = async { while let Some(frame) = rx.recv().await { writer.write_all(&frame).await?; } Ok::<_, io::Error>(()) } => {}
            }
            task.close();
        });
        let task = session.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = task.closed() => {}
                _ = async {
                    loop {
                        let (kind, id, payload) = read_frame(&mut reader).await?;
                        let mut state = task.0.state.lock().unwrap_or_else(|p| p.into_inner());
                        if kind == CLOSE {
                            if let Some(entry) = state.entries.remove(&id) { let _ = entry.input.try_send(Inbound::Close); }
                            continue;
                        }
                        let Some(entry) = state.entries.get_mut(&id) else { continue };
                        match kind {
                            OPEN => { if let Some(opened) = entry.opened.take() { let _ = opened.send(()); } }
                            CREDIT => {
                                if entry.credit.available_permits() >= WINDOW { return Err::<(), io::Error>(io::Error::new(io::ErrorKind::InvalidData, "excess credits")); }
                                entry.credit.add_permits(1);
                            }
                            DATA | EOF => {
                                let message = if kind == DATA { Inbound::Data(payload) } else { Inbound::Eof };
                                if entry.input.try_send(message).is_err() { return Err(io::Error::new(io::ErrorKind::InvalidData, "stream window exceeded")); }
                            }
                            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected frame")),
                        }
                    }
                } => {}
            }
            task.close();
            task.0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entries
                .clear();
        });
        Ok(session)
    }
    pub(super) async fn open(&self) -> io::Result<Stream> {
        let _span = crate::diagnostics::PerfSpan::new("docker_api.stream_connect");
        if self.is_closed() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let (input, rx) = mpsc::channel(WINDOW + 2);
        let credit = Arc::new(Semaphore::new(WINDOW));
        let (opened, ready) = oneshot::channel();
        let id = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            // Never reuse ids within a session, including after cancelled opens.
            let id = state.next;
            state.next = id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("stream ids exhausted"))?;
            state.entries.insert(
                id,
                Entry {
                    input,
                    credit: credit.clone(),
                    opened: Some(opened),
                },
            );
            id
        };
        let (io, relay) = tokio::io::duplex(MAX_PAYLOAD);
        let (cancel, cancelled) = oneshot::channel();
        let stream = Stream {
            io,
            cancel: Some(cancel),
        };
        let session = self.clone();
        tokio::spawn(async move {
            session.connection(id, relay, rx, credit, cancelled).await;
        });
        tokio::time::timeout(CONNECT_TIMEOUT, ready)
            .await?
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "Docker socket connect failed",
                )
            })?;
        Ok(stream)
    }
    async fn connection(
        &self,
        id: u32,
        relay: DuplexStream,
        mut input: mpsc::Receiver<Inbound>,
        credit: Arc<Semaphore>,
        mut cancelled: oneshot::Receiver<()>,
    ) {
        // The owner task emits OPEN before observing cancellation, so a
        // dropped open future can never send CLOSE followed by a late OPEN.
        tokio::select! {
            result = self.0.output.send(frame(OPEN, id, &[])) => { if result.is_err() { self.close(); return; } }
            _ = self.closed() => return,
        }
        let (mut reader, mut writer) = tokio::io::split(relay);
        let upstream = async {
            let mut buffer = vec![0; MAX_PAYLOAD];
            loop {
                let permit = credit
                    .acquire()
                    .await
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                let read = reader.read(&mut buffer).await?;
                let kind = if read == 0 { EOF } else { DATA };
                self.0
                    .output
                    .send(frame(kind, id, &buffer[..read]))
                    .await
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                if read == 0 {
                    return std::future::pending::<io::Result<()>>().await;
                }
                permit.forget();
            }
        };
        let downstream = async {
            let mut first = Some(crate::diagnostics::PerfSpan::new(
                "docker_api.first_response",
            ));
            let mut eof = false;
            while let Some(message) = input.recv().await {
                match message {
                    Inbound::Data(data) if !eof => {
                        first.take();
                        writer.write_all(&data).await?;
                        self.0
                            .output
                            .send(frame(CREDIT, id, &[]))
                            .await
                            .map_err(|_| io::ErrorKind::BrokenPipe)?;
                    }
                    Inbound::Eof if !eof => {
                        writer.shutdown().await?;
                        eof = true;
                    }
                    Inbound::Close => break,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "data after EOF")),
                }
            }
            Ok::<_, io::Error>(())
        };
        tokio::select! { _ = upstream => {} _ = downstream => {} _ = &mut cancelled => {} _ = self.closed() => {} }
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .remove(&id);
        if !self.is_closed() {
            tokio::select! { _ = self.0.output.send(frame(CLOSE, id, &[])) => {} _ = self.closed() => {} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fake() -> (
        Session,
        tokio::io::ReadHalf<DuplexStream>,
        tokio::io::WriteHalf<DuplexStream>,
    ) {
        let (host, mut helper) = tokio::io::duplex(MAX_PAYLOAD * 4);
        helper.write_all(&frame(READY, 0, &[])).await.unwrap();
        let (read, write) = tokio::io::split(host);
        let session = Session::start(read, write).await.unwrap();
        let (read, write) = tokio::io::split(helper);
        (session, read, write)
    }
    async fn open(
        session: &Session,
        read: &mut (impl AsyncRead + Unpin),
        write: &mut (impl AsyncWrite + Unpin),
    ) -> (u32, Stream) {
        let opening = session.open();
        let helper = async {
            let (kind, id, _) = read_frame(read).await.unwrap();
            assert_eq!(kind, OPEN);
            write.write_all(&frame(OPEN, id, &[])).await.unwrap();
            id
        };
        let (stream, id) = tokio::join!(opening, helper);
        (id, stream.unwrap())
    }
    #[tokio::test]
    async fn opaque_upgrade_and_directional_half_closes() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (session, mut read, mut write) = fake().await;
            let (id, mut stream) = open(&session, &mut read, &mut write).await;
            let bytes = b"HTTP/1.1 101 UPGRADED\r\n\r\n\0\xffexec";
            write.write_all(&frame(DATA, id, bytes)).await.unwrap();
            write.write_all(&frame(EOF, id, &[])).await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, bytes);
            assert_eq!(read_frame(&mut read).await.unwrap().0, CREDIT);
            // Daemon output EOF must not close the client's input direction.
            stream.write_all(b"stdin after daemon EOF").await.unwrap();
            stream.shutdown().await.unwrap();
            let (kind, got, data) = read_frame(&mut read).await.unwrap();
            assert_eq!(
                (kind, got, data.as_slice()),
                (DATA, id, b"stdin after daemon EOF".as_slice())
            );
            assert_eq!(read_frame(&mut read).await.unwrap().0, EOF);
            drop(stream);
            assert_eq!(read_frame(&mut read).await.unwrap().0, CLOSE);
            session.close();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn slow_stream_does_not_block_another_and_failure_closes_all() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (session, mut read, mut write) = fake().await;
            let (slow_id, _slow) = open(&session, &mut read, &mut write).await;
            let (fast_id, mut fast) = open(&session, &mut read, &mut write).await;
            // More than the local duplex capacity, but within the flow window.
            for _ in 0..3 {
                write
                    .write_all(&frame(DATA, slow_id, &vec![42; MAX_PAYLOAD]))
                    .await
                    .unwrap();
            }
            write.write_all(&frame(DATA, fast_id, b"OK")).await.unwrap();
            let mut reply = [0; 2];
            fast.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"OK");
            write.shutdown().await.unwrap();
            drop(write);
            session.closed().await;
            assert!(session.open().await.is_err());
            let mut last = [0];
            assert_eq!(fast.read(&mut last).await.unwrap(), 0);
            // A replacement session starts cleanly; no old stream is replayed.
            let (replacement, mut read, mut write) = fake().await;
            let (id, _stream) = open(&replacement, &mut read, &mut write).await;
            assert_eq!(id, 1);
            replacement.close();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn upload_larger_than_window_and_client_half_close_preserve_response() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (session, mut read, mut write) = fake().await;
            let (id, mut stream) = open(&session, &mut read, &mut write).await;
            let length = MAX_PAYLOAD * (WINDOW + 3);
            let client = async {
                stream.write_all(&vec![0x5a; length]).await.unwrap();
                stream.shutdown().await.unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, b"late response after stdin EOF");
            };
            let helper = async {
                let mut total = 0;
                loop {
                    let (kind, got, payload) = read_frame(&mut read).await.unwrap();
                    assert_eq!(got, id);
                    match kind {
                        DATA => {
                            assert!(payload.iter().all(|byte| *byte == 0x5a));
                            total += payload.len();
                            write.write_all(&frame(CREDIT, id, &[])).await.unwrap();
                        }
                        EOF => break,
                        _ => panic!("unexpected upload frame {kind}"),
                    }
                }
                assert_eq!(total, length);
                write
                    .write_all(&frame(DATA, id, b"late response after stdin EOF"))
                    .await
                    .unwrap();
                write.write_all(&frame(EOF, id, &[])).await.unwrap();
                write.write_all(&frame(CLOSE, id, &[])).await.unwrap();
            };
            tokio::join!(client, helper);
            session.close();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn cancelled_open_and_failed_socket_are_isolated() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (session, mut read, mut write) = fake().await;
            let copy = session.clone();
            let opening = tokio::spawn(async move { copy.open().await });
            let (kind, id, _) = read_frame(&mut read).await.unwrap();
            assert_eq!(kind, OPEN);
            opening.abort();
            let _ = opening.await;
            loop {
                let (kind, got, _) = read_frame(&mut read).await.unwrap();
                assert_eq!(got, id);
                if kind == CLOSE {
                    break;
                }
                // Dropping the local duplex also readies EOF; either branch
                // may win before the cancellation is observed. Both are
                // non-mutating and must be followed by this stream's CLOSE.
                assert_eq!(kind, EOF);
            }
            // A late acknowledgement of a cancelled OPEN is harmless.
            write.write_all(&frame(OPEN, id, &[])).await.unwrap();
            let copy = session.clone();
            let failed = tokio::spawn(async move { copy.open().await });
            let (_, failed_id, _) = read_frame(&mut read).await.unwrap();
            write
                .write_all(&frame(CLOSE, failed_id, &[]))
                .await
                .unwrap();
            assert!(failed.await.unwrap().is_err());
            // Drain the owner's close, then a different id can still open.
            assert_eq!(read_frame(&mut read).await.unwrap().0, CLOSE);
            let (next_id, _stream) = open(&session, &mut read, &mut write).await;
            assert!(next_id > failed_id);
            assert!(!session.is_closed());
            session.close();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn rejects_oversized_and_invalid_control_frames() {
        for encoded in [frame(99, 1, &[]), frame(EOF, 1, b"x"), frame(DATA, 0, b"x")] {
            assert!(read_frame(&mut encoded.as_slice()).await.is_err());
        }
        let mut oversized = frame(DATA, 1, &[]);
        oversized[5..9].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        assert!(read_frame(&mut oversized.as_slice()).await.is_err());
    }
}
