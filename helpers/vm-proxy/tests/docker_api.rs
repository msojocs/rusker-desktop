//! Linux-only helper integration tests; no WSL distribution, Docker daemon,
//! named pipe, or real engine socket is touched.
#![cfg(unix)]
use std::{
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const OPEN: u8 = 1;
const DATA: u8 = 2;
const EOF: u8 = 3;
const CLOSE: u8 = 4;
const READY: u8 = 5;
const CREDIT: u8 = 6;

struct Helper {
    child: Child,
    input: ChildStdin,
    frames: mpsc::Receiver<(u8, u32, Vec<u8>)>,
    directory: PathBuf,
}
impl Helper {
    fn start() -> (Self, UnixListener) {
        let directory = std::env::temp_dir().join(format!(
            "rusker-mux-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("docker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let executable = std::env::var_os("RUSKER_TEST_VM_PROXY_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_rusker-vm-proxy").into());
        let mut child = Command::new(executable)
            .arg("--docker-api")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || loop {
            let mut header = [0; 9];
            if stdout.read_exact(&mut header).is_err() {
                return;
            }
            let id = u32::from_be_bytes(header[1..5].try_into().unwrap());
            let length = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
            assert!(length <= 256 * 1024);
            let mut bytes = vec![0; length];
            stdout.read_exact(&mut bytes).unwrap();
            if tx.send((header[0], id, bytes)).is_err() {
                return;
            }
        });
        let helper = Self {
            child,
            input,
            frames,
            directory,
        };
        assert_eq!(helper.recv(), (READY, 0, Vec::new()));
        (helper, listener)
    }
    fn send(&mut self, kind: u8, id: u32, payload: &[u8]) {
        let mut frame = vec![kind];
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        self.input.write_all(&frame).unwrap();
    }
    fn recv(&self) -> (u8, u32, Vec<u8>) {
        self.frames.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn unix_socket_half_close_preserves_late_binary_response() {
    let (mut helper, listener) = Helper::start();
    helper.send(OPEN, 1, &[]);
    assert_eq!(helper.recv().0, OPEN);
    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    helper.send(DATA, 1, b"stdin\0\xff");
    helper.send(EOF, 1, &[]);
    let mut input = Vec::new();
    socket.read_to_end(&mut input).unwrap();
    assert_eq!(input, b"stdin\0\xff");
    assert_eq!(helper.recv().0, CREDIT);
    socket
        .write_all(b"HTTP/1.1 101 UPGRADED\r\n\r\n\xff\0output")
        .unwrap();
    socket.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    loop {
        let (kind, id, payload) = helper.recv();
        assert_eq!(id, 1);
        match kind {
            DATA => {
                response.extend_from_slice(&payload);
                helper.send(CREDIT, 1, &[]);
            }
            EOF => break,
            _ => panic!("unexpected {kind}"),
        }
    }
    assert_eq!(response, b"HTTP/1.1 101 UPGRADED\r\n\r\n\xff\0output");
    assert_eq!(helper.recv().0, CLOSE);
}

#[test]
fn blocked_upload_does_not_block_another_socket_and_close_cancels_it() {
    let (mut helper, listener) = Helper::start();
    helper.send(OPEN, 1, &[]);
    assert_eq!(helper.recv().0, OPEN);
    let (_slow, _) = listener.accept().unwrap();
    // More than the Unix socket send buffer, but within the eight-frame
    // credit window: the socket worker blocks, not the frame dispatcher.
    for _ in 0..8 {
        helper.send(DATA, 1, &vec![1; 256 * 1024]);
    }
    helper.send(OPEN, 2, &[]);
    loop {
        let (kind, id, _) = helper.recv();
        if kind == OPEN && id == 2 {
            break;
        }
    }
    let (mut fast, _) = listener.accept().unwrap();
    fast.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    helper.send(DATA, 2, b"ping");
    let mut ping = [0; 4];
    fast.read_exact(&mut ping).unwrap();
    assert_eq!(&ping, b"ping");
    fast.write_all(b"pong").unwrap();
    loop {
        let (kind, id, data) = helper.recv();
        if kind == DATA && id == 2 {
            assert_eq!(data, b"pong");
            break;
        }
    }
    helper.send(CLOSE, 1, &[]);
    loop {
        let (kind, id, _) = helper.recv();
        if kind == CLOSE && id == 1 {
            break;
        }
    }
    // The second connection remains independently usable.
    helper.send(DATA, 2, b"more");
    fast.read_exact(&mut ping).unwrap();
    assert_eq!(&ping, b"more");
}
