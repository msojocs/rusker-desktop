//! VM side of the Rusker Desktop Engine proxy relay (the counterpart of the
//! endpoint behind Docker Desktop's `http.docker.internal:3128`).
//!
//! Usage: `rusker-vm-proxy <port>`. Listens on `127.0.0.1:<port>` inside the
//! WSL2 VM and multiplexes every accepted connection over stdin/stdout, which
//! the Windows backend holds through a `wsl.exe` session. Frames are
//! `kind: u8, id: u32 BE, len: u32 BE, payload` (see
//! `src/runtime_host/engine_proxy.rs` in the main crate):
//!
//! - VM -> Windows: `READY` once listening, `OPEN` per accepted connection,
//!   `DATA`, `EOF` (the client half-closed), `CLOSE` (the socket failed).
//! - Windows -> VM: `DATA`, `EOF` (half-close towards the client), `CLOSE`.
//!
//! One thread per connection with blocking I/O; reads go straight into the
//! frame buffer. Exits when stdin closes, i.e. when the backend goes away.

mod docker_api;

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    mem::ManuallyDrop,
    net::{Shutdown, TcpListener, TcpStream},
    os::fd::FromRawFd,
    process,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};

const OPEN: u8 = 1;
const DATA: u8 = 2;
const EOF: u8 = 3;
const CLOSE: u8 = 4;
const READY: u8 = 5;
// Docker API mode only: one credit per consumed DATA frame, eight initially.
const CREDIT: u8 = 6;
const HEADER_LEN: usize = 9;
const MAX_PAYLOAD: usize = 256 * 1024;
const PIDFILE: &str = "/run/rusker-vm-proxy.pid";
/// In every command line of this program; identifies a stale instance.
const NAME: &str = "rusker-vm-proxy";

type Connections = Mutex<HashMap<u32, Arc<TcpStream>>>;

fn connections() -> &'static Connections {
    static CONNECTIONS: OnceLock<Connections> = OnceLock::new();
    CONNECTIONS.get_or_init(Connections::default)
}

/// Raw stdout: frames are written whole under the lock, unbuffered.
fn output() -> &'static Mutex<ManuallyDrop<File>> {
    static OUTPUT: OnceLock<Mutex<ManuallyDrop<File>>> = OnceLock::new();
    // SAFETY: fd 1 stays open for the life of the process and is only
    // written through this handle; ManuallyDrop never closes it.
    OUTPUT.get_or_init(|| Mutex::new(ManuallyDrop::new(unsafe { File::from_raw_fd(1) })))
}

/// Writes one frame whose payload is already in `frame[HEADER_LEN..]`.
/// A broken stdout means the backend is gone: exit.
fn send(frame: &mut [u8], kind: u8, id: u32) {
    let len = (frame.len() - HEADER_LEN) as u32;
    frame[0] = kind;
    frame[1..5].copy_from_slice(&id.to_be_bytes());
    frame[5..9].copy_from_slice(&len.to_be_bytes());
    let mut out = output().lock().unwrap_or_else(|p| p.into_inner());
    if out.write_all(frame).is_err() {
        process::exit(0);
    }
}

fn signal(kind: u8, id: u32) {
    send(&mut [0u8; HEADER_LEN], kind, id);
}

/// Forgets a connection and closes its socket. Returns whether it was
/// still known (so the caller tells Windows exactly once).
fn drop_connection(id: u32) -> bool {
    let removed = connections()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
    match removed {
        Some(stream) => {
            let _ = stream.shutdown(Shutdown::Both);
            true
        }
        None => false,
    }
}

fn known(id: u32) -> bool {
    connections()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&id)
}

/// Client -> Windows for one connection.
fn pump(id: u32, stream: Arc<TcpStream>) {
    let mut frame = vec![0u8; HEADER_LEN + MAX_PAYLOAD];
    loop {
        match (&*stream).read(&mut frame[HEADER_LEN..]) {
            Ok(0) => {
                if known(id) {
                    signal(EOF, id);
                }
                return;
            }
            Ok(read) => send(&mut frame[..HEADER_LEN + read], DATA, id),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                if drop_connection(id) {
                    signal(CLOSE, id);
                }
                return;
            }
        }
    }
}

fn accept(listener: TcpListener) {
    let mut next_id: u32 = 1;
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let id = next_id;
        next_id = next_id.checked_add(1).unwrap_or(1);
        let stream = Arc::new(stream);
        connections()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, stream.clone());
        // OPEN goes out before the pump can send DATA for this id.
        signal(OPEN, id);
        thread::spawn(move || pump(id, stream));
    }
}

/// Stops an instance left over from an earlier backend session, which
/// would still hold the port.
fn stop_stale() {
    let Ok(text) = fs::read_to_string(PIDFILE) else {
        return;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return;
    };
    if pid == process::id() {
        return;
    }
    let ours = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| {
            cmdline
                .windows(NAME.len())
                .any(|window| window == NAME.as_bytes())
        })
        .unwrap_or(false);
    if ours {
        let _ = process::Command::new("kill").arg(pid.to_string()).status();
        thread::sleep(Duration::from_millis(300));
    }
}

fn listen(port: u16) -> TcpListener {
    // std sets SO_REUSEADDR on Unix listeners.
    let mut attempt = 0;
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(error) if attempt >= 9 => {
                eprintln!("无法监听 127.0.0.1:{port}：{error}");
                process::exit(1);
            }
            Err(_) => {
                attempt += 1;
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn main() {
    // No listener/PID file in Docker API mode: HTTP proxy instances must not
    // terminate this independent reverse relay (or vice versa).
    if std::env::args().nth(1).as_deref() == Some("--docker-api") {
        let socket = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/var/run/docker.sock".into());
        if let Err(error) = docker_api::run(io::stdin().lock(), &socket) {
            if error.kind() != io::ErrorKind::UnexpectedEof {
                eprintln!("Docker API relay: {error}");
                process::exit(1);
            }
        }
        return;
    }
    let Some(port) = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u16>().ok())
    else {
        eprintln!("用法：{NAME} <端口>");
        process::exit(2);
    };
    stop_stale();
    let listener = listen(port);
    let _ = fs::write(PIDFILE, format!("{}\n", process::id()));
    thread::spawn(move || accept(listener));
    signal(READY, 0);

    // SAFETY: fd 0 is only read here and never closed.
    let stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    let mut input = BufReader::with_capacity(HEADER_LEN + MAX_PAYLOAD, &*stdin);
    let mut header = [0u8; HEADER_LEN];
    let mut payload = vec![0u8; MAX_PAYLOAD];
    loop {
        if input.read_exact(&mut header).is_err() {
            process::exit(0);
        }
        let kind = header[0];
        let id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
        if len > MAX_PAYLOAD {
            eprintln!("帧过大：{len}");
            process::exit(1);
        }
        if input.read_exact(&mut payload[..len]).is_err() {
            process::exit(0);
        }
        if kind == CLOSE {
            drop_connection(id);
            continue;
        }
        let stream = connections()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned();
        let Some(stream) = stream else {
            continue;
        };
        let result = match kind {
            DATA => (&*stream).write_all(&payload[..len]),
            EOF => stream.shutdown(Shutdown::Write),
            _ => Ok(()),
        };
        if result.is_err() && drop_connection(id) {
            signal(CLOSE, id);
        }
    }
}
