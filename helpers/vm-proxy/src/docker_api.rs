//! Reverse relay for the Docker Unix socket. A bounded, credit-based window
//! in each direction isolates slow streams without dropping large uploads or
//! blocking the shared frame dispatcher. HTTP mode keeps its original protocol.
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

use super::{send, signal, CLOSE, CREDIT, DATA, EOF, HEADER_LEN, MAX_PAYLOAD, OPEN, READY};

const WINDOW: usize = 8;
type Registry = Arc<Mutex<HashMap<u32, Arc<Connection>>>>;

enum Input {
    Data(Vec<u8>),
    Eof,
}

struct State {
    socket: Option<UnixStream>,
    closed: bool,
    read_eof: bool,
    write_eof: bool,
    credits: usize,
}

struct Connection {
    state: Mutex<State>,
    credit: Condvar,
    input: mpsc::SyncSender<Input>,
}

fn close(registry: &Registry, id: u32) {
    let connection = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
    if let Some(connection) = connection {
        let mut state = connection.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        if let Some(socket) = &state.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        connection.credit.notify_all();
        signal(CLOSE, id);
    }
}

fn half_closed(registry: &Registry, id: u32, connection: &Connection, read: bool) {
    let both = {
        let mut state = connection.state.lock().unwrap_or_else(|p| p.into_inner());
        if read {
            state.read_eof = true;
        } else {
            state.write_eof = true;
        }
        state.read_eof && state.write_eof
    };
    if both {
        close(registry, id);
    }
}

fn open(
    registry: Registry,
    id: u32,
    path: String,
    connection: Arc<Connection>,
    rx: mpsc::Receiver<Input>,
) {
    let result = (|| -> io::Result<()> {
        let socket = UnixStream::connect(path)?;
        let reader = socket.try_clone()?;
        let mut writer = socket.try_clone()?;
        {
            let mut state = connection.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.closed {
                return Ok(());
            }
            state.socket = Some(socket);
            // OPEN goes out before the reader can send DATA. Holding state
            // also prevents a cancelled open from acknowledging after CLOSE.
            signal(OPEN, id);
        }
        let reader_registry = registry.clone();
        let reader_connection = connection.clone();
        thread::spawn(move || {
            let mut frame = vec![0; HEADER_LEN + MAX_PAYLOAD];
            let mut reader = reader;
            loop {
                {
                    let mut state = reader_connection
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    while state.credits == 0 && !state.closed {
                        state = reader_connection
                            .credit
                            .wait(state)
                            .unwrap_or_else(|p| p.into_inner());
                    }
                    if state.closed {
                        return;
                    }
                    state.credits -= 1;
                }
                match reader.read(&mut frame[HEADER_LEN..]) {
                    Ok(0) => {
                        signal(EOF, id);
                        half_closed(&reader_registry, id, &reader_connection, true);
                        return;
                    }
                    Ok(read) => send(&mut frame[..HEADER_LEN + read], DATA, id),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        let mut state = reader_connection
                            .state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        state.credits += 1;
                    }
                    Err(_) => {
                        close(&reader_registry, id);
                        return;
                    }
                }
            }
        });
        loop {
            if connection
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .closed
            {
                return Ok(());
            }
            let message = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Ok(()),
            };
            match message {
                Input::Data(data) => {
                    writer.write_all(&data)?;
                    signal(CREDIT, id);
                }
                Input::Eof => {
                    writer.shutdown(Shutdown::Write)?;
                    half_closed(&registry, id, &connection, false);
                    // Keep the reader alive until its independent EOF/CLOSE.
                    return Ok(());
                }
            }
        }
    })();
    if let Err(error) = result {
        eprintln!("Docker API stream {id}: {error}");
        close(&registry, id);
    }
}

pub(super) fn run(mut input: impl Read, path: &str) -> io::Result<()> {
    let registry = Registry::default();
    signal(READY, 0);
    let result = (|| {
        loop {
            let mut header = [0; HEADER_LEN];
            input.read_exact(&mut header)?;
            let kind = header[0];
            let id = u32::from_be_bytes(header[1..5].try_into().unwrap());
            let len = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
            if len > MAX_PAYLOAD
                || id == 0
                || !matches!(kind, OPEN | DATA | EOF | CLOSE | CREDIT)
                || (kind != DATA && len != 0)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Docker API frame",
                ));
            }
            let mut payload = vec![0; len];
            input.read_exact(&mut payload)?;
            if kind == OPEN {
                let (tx, rx) = mpsc::sync_channel(WINDOW + 1); // DATA window plus EOF
                let connection = Arc::new(Connection {
                    state: Mutex::new(State {
                        socket: None,
                        closed: false,
                        read_eof: false,
                        write_eof: false,
                        credits: WINDOW,
                    }),
                    credit: Condvar::new(),
                    input: tx,
                });
                {
                    let mut entries = registry.lock().unwrap_or_else(|p| p.into_inner());
                    if entries.contains_key(&id) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate stream id",
                        ));
                    }
                    // Reserve before connect, so CLOSE can cancel an in-flight open.
                    entries.insert(id, connection.clone());
                }
                let registry = registry.clone();
                let path = path.to_owned();
                thread::spawn(move || open(registry, id, path, connection, rx));
            } else if kind == CLOSE {
                close(&registry, id);
            } else {
                let connection = registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&id)
                    .cloned();
                if let Some(connection) = connection {
                    if kind == CREDIT {
                        let mut state = connection.state.lock().unwrap_or_else(|p| p.into_inner());
                        if state.credits >= WINDOW {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "excess credits",
                            ));
                        }
                        state.credits += 1;
                        connection.credit.notify_one();
                    } else {
                        let message = if kind == DATA {
                            Input::Data(payload)
                        } else {
                            Input::Eof
                        };
                        if connection.input.try_send(message).is_err() {
                            close(&registry, id);
                        }
                    }
                }
            }
        }
    })();
    let ids: Vec<_> = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .copied()
        .collect();
    for id in ids {
        close(&registry, id);
    }
    result
}
