//! Engine HTTP proxy, the counterpart of Docker Desktop's
//! `http.docker.internal:3128`.
//!
//! dockerd runs inside the WSL2 VM, so a proxy such as `127.0.0.1:7890`
//! written straight into its `daemon.json` points at the VM's own loopback
//! in NAT mode, not at the proxy running on Windows. Docker Desktop never
//! hands dockerd the user's addresses: `docker info` always shows
//! `http.docker.internal:3128`, an in-VM endpoint whose connections reach
//! `com.docker.backend` on Windows, and the backend's HTTP proxy decides per
//! request whether to use the upstream proxy or connect directly. Rusker
//! does the same:
//!
//! - dockerd always uses [`ENGINE_PROXY_URL`] for HTTP and HTTPS, whatever
//!   the proxy mode, so changing proxy settings never touches `daemon.json`
//!   or restarts dockerd. `NO_PROXY` is applied on Windows, not by dockerd.
//! - Every connection to that endpoint is served by the Windows-side
//!   forward proxy in [`super::http_proxy`], which resolves the settings for
//!   each request (re-reading the system proxy) and supports `http://` and
//!   `https://` upstreams.
//!
//! Transport (instead of Docker Desktop's Hyper-V socket): one long-lived
//! `wsl.exe` session runs `rusker-vm-proxy` (`helpers/vm-proxy`, a static
//! Linux binary built by `build.rs` and embedded in the app, like Docker
//! Desktop's own VM binaries) in the managed distribution. It listens on
//! `127.0.0.1:23128` there and multiplexes every accepted connection over its
//! stdin/stdout as frames (`kind: u8, id: u32, len: u32, payload`). Like the
//! named pipe proxy this needs no Windows listener, firewall rule,
//! administrator rights or socket service registration, and does not depend
//! on the WSL networking mode.

use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::settings::{DockerProxySettings, ResolvedProxy};

/// In-VM port of the relay. Below Linux's ephemeral range (32768+), so
/// outgoing connections never hold it.
pub const ENGINE_PROXY_PORT: u16 = 23128;
/// Proxy address written into the managed dockerd's `daemon.json`.
pub const ENGINE_PROXY_URL: &str = "http://127.0.0.1:23128";

// Frame protocol shared with `helpers/vm-proxy/src/main.rs`. Only the
// Windows relay speaks it; elsewhere the tests still check the constants.
#[cfg_attr(not(windows), allow(dead_code))]
const OPEN: u8 = 1;
#[cfg_attr(not(windows), allow(dead_code))]
const DATA: u8 = 2;
#[cfg_attr(not(windows), allow(dead_code))]
const EOF: u8 = 3;
#[cfg_attr(not(windows), allow(dead_code))]
const CLOSE: u8 = 4;
#[cfg_attr(not(windows), allow(dead_code))]
const READY: u8 = 5;
const HEADER_LEN: usize = 9;
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_PAYLOAD: usize = 256 * 1024;

/// The VM side, built for the VM's architecture by `build.rs`.
#[cfg(windows)]
const VM_PROXY: &[u8] = include_bytes!(env!("RUSKER_VM_PROXY_BIN"));

#[cfg(all(test, windows))]
pub(super) fn embedded_helper_for_test() -> &'static [u8] {
    VM_PROXY
}
/// Where versions of the VM side are installed, one directory per content
/// hash, so an app upgrade installs its own copy and removes older ones.
#[cfg_attr(not(windows), allow(dead_code))]
const VM_PROXY_ROOT: &str = "/usr/local/libexec/rusker";

/// Installs `$2` (stdin) as `$1`'s binary and removes other versions.
#[cfg_attr(not(windows), allow(dead_code))]
const INSTALL_VM_PROXY_SCRIPT: &str = r#"set -eu
root=/usr/local/libexec/rusker
dir="$root/$1"
mkdir -p "$dir"
cat > "$dir/rusker-vm-proxy.new"
chmod 0755 "$dir/rusker-vm-proxy.new"
mv -f "$dir/rusker-vm-proxy.new" "$dir/rusker-vm-proxy"
for old in "$root"/*; do
    [ "$old" = "$dir" ] || rm -rf -- "$old"
done
"#;

/// The `proxies` object of the managed dockerd's `daemon.json`: always the
/// relay, like Docker Desktop's `http.docker.internal:3128`.
pub(crate) fn daemon_proxies() -> Map<String, Value> {
    ResolvedProxy {
        http_proxy: ENGINE_PROXY_URL.into(),
        https_proxy: ENGINE_PROXY_URL.into(),
        no_proxy: String::new(),
    }
    .engine_proxies()
}

/// Engine proxy settings as the relay sees them, plus its last error.
#[derive(Default)]
pub(crate) struct EngineProxyState {
    settings: Mutex<DockerProxySettings>,
    error: Mutex<Option<String>>,
}

impl EngineProxyState {
    /// Takes effect for the next request; nothing restarts.
    pub(crate) fn configure(&self, settings: &DockerProxySettings) {
        *self.settings.lock().unwrap_or_else(|p| p.into_inner()) = settings.clone();
    }

    /// Upstream addresses for one request. The system proxy is read again
    /// each time, like Docker Desktop's system proxy mode.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn upstream(&self) -> ResolvedProxy {
        self.settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resolve(None)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn set_error(&self, message: Option<String>) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = message;
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_header(header: &[u8; HEADER_LEN]) -> (u8, u32, usize) {
    let id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]);
    (header[0], id, len as usize)
}

#[cfg(windows)]
pub(super) use relay::{install_vm_proxy, serve};

#[cfg(windows)]
mod relay {
    use std::{collections::HashMap, io, process::Stdio, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
        process::Child,
        sync::mpsc,
        time::{sleep, timeout},
    };

    use super::*;
    use crate::runtime_host::{
        http_proxy,
        wsl::{execute, execute_with_input, managed_command, WslHost, COMMAND_TIMEOUT},
    };

    const POLL: Duration = Duration::from_secs(1);
    const RETRY: Duration = Duration::from_secs(3);

    enum Inbound {
        Data(Vec<u8>),
        Eof,
    }

    /// `<root>/<content hash>/rusker-vm-proxy` for the embedded build.
    fn vm_proxy_path() -> &'static (String, String) {
        static PATH: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(VM_PROXY);
            let version: String = digest[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let path = format!("{VM_PROXY_ROOT}/{version}/rusker-vm-proxy");
            (version, path)
        })
    }

    /// Copies the embedded VM side into the distribution unless this build
    /// is already there.
    pub(in crate::runtime_host) async fn install_vm_proxy() -> Result<&'static str, String> {
        // HTTP and Docker API relays share the same embedded binary/install.
        static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = INSTALL.lock().await;
        let (version, path) = vm_proxy_path();
        let mut check = managed_command("root");
        check.args(["/usr/bin/test", "-x", path]);
        if execute(check, "检查 Engine 代理转发程序", COMMAND_TIMEOUT)
            .await
            .is_ok()
        {
            return Ok(path);
        }
        let mut install = managed_command("root");
        install
            .args(["/bin/sh", "-c", INSTALL_VM_PROXY_SCRIPT, "rusker-install"])
            .arg(version);
        execute_with_input(
            install,
            VM_PROXY,
            "安装 Engine 代理转发程序",
            COMMAND_TIMEOUT,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(path)
    }

    /// Runs the relay while an engine session is alive; restarts it if it
    /// exits. dockerd always points at it, so it is needed even without an
    /// upstream proxy (requests then go out directly from Windows).
    pub(in crate::runtime_host) async fn serve(host: Arc<WslHost>) {
        let state = host.engine_proxy_state();
        loop {
            sleep(POLL).await;
            if !host.session_running() {
                state.set_error(None);
                continue;
            }
            if let Some(error) = run(&host, &state).await {
                state.set_error(Some(error));
                sleep(RETRY).await;
            }
        }
    }

    /// One relay session. Returns an error to show when it ended
    /// unexpectedly; None when the engine session ended.
    async fn run(host: &WslHost, state: &Arc<EngineProxyState>) -> Option<String> {
        let path = match install_vm_proxy().await {
            Ok(path) => path,
            Err(error) => return Some(error),
        };
        let mut command = managed_command("root");
        command
            .arg(path)
            .arg(ENGINE_PROXY_PORT.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Some(format!("无法启动 Engine 代理转发：{error}")),
        };
        let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Some("无法连接 Engine 代理转发的标准输入输出".into());
        };
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(frame) = out_rx.recv().await {
                if stdin.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u32>();
        let mut conns = HashMap::<u32, mpsc::Sender<Inbound>>::new();
        let mut check = tokio::time::interval(POLL);
        let unexpected = loop {
            tokio::select! {
                frame = read_frame(&mut stdout) => {
                    let Ok((kind, id, payload)) = frame else { break true };
                    match kind {
                        READY => state.set_error(None),
                        OPEN => {
                            let (tx, rx) = mpsc::channel(64);
                            conns.insert(id, tx);
                            tokio::spawn(connection(
                                id,
                                rx,
                                out_tx.clone(),
                                done_tx.clone(),
                                state.clone(),
                            ));
                        }
                        DATA | EOF => {
                            let message = if kind == DATA {
                                Inbound::Data(payload)
                            } else {
                                Inbound::Eof
                            };
                            if let Some(tx) = conns.get(&id) {
                                if tx.send(message).await.is_err() {
                                    conns.remove(&id);
                                }
                            }
                        }
                        CLOSE => {
                            conns.remove(&id);
                        }
                        _ => {}
                    }
                }
                Some(id) = done_rx.recv() => {
                    conns.remove(&id);
                }
                _ = check.tick() => {
                    if !host.session_running() {
                        break false;
                    }
                }
            }
        };
        drop(conns);
        drop(out_tx);
        writer.abort();
        let stderr = finish(child).await;
        unexpected.then(|| {
            if stderr.is_empty() {
                "Engine 代理转发进程意外退出".into()
            } else {
                format!("Engine 代理转发进程退出：{stderr}")
            }
        })
    }

    async fn finish(mut child: Child) -> String {
        let _ = child.start_kill();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = timeout(Duration::from_secs(2), pipe.read_to_string(&mut stderr)).await;
        }
        let _ = child.wait().await;
        stderr.trim().chars().take(1024).collect()
    }

    async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<(u8, u32, Vec<u8>)> {
        let mut header = [0u8; HEADER_LEN];
        reader.read_exact(&mut header).await?;
        let (kind, id, len) = parse_header(&header);
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "帧过大"));
        }
        let mut payload = vec![0; len];
        reader.read_exact(&mut payload).await?;
        Ok((kind, id, payload))
    }

    /// Bridges one relayed connection into the Windows-side HTTP proxy
    /// through an in-memory pipe, keeping half-closes in both directions.
    async fn connection(
        id: u32,
        mut inbound: mpsc::Receiver<Inbound>,
        out: mpsc::Sender<Vec<u8>>,
        done: mpsc::UnboundedSender<u32>,
        state: Arc<EngineProxyState>,
    ) {
        let (proxy_side, relay_side) = tokio::io::duplex(MAX_PAYLOAD);
        let (mut from_proxy, mut to_proxy) = tokio::io::split(relay_side);
        let proxy = async move {
            let _ = http_proxy::serve(proxy_side, move || state.upstream()).await;
        };
        let drain = async {
            let mut buffer = vec![0; MAX_PAYLOAD];
            loop {
                let read = match from_proxy.read(&mut buffer).await {
                    Ok(read) => read,
                    Err(_) => return,
                };
                let message = if read == 0 {
                    frame(EOF, id, &[])
                } else {
                    frame(DATA, id, &buffer[..read])
                };
                if out.send(message).await.is_err() || read == 0 {
                    return;
                }
            }
        };
        // Ends only when the VM closes the connection (or the relay stops):
        // then everything is aborted.
        let feed = async {
            let mut open = true;
            while let Some(message) = inbound.recv().await {
                match message {
                    Inbound::Data(data) if open => {
                        open = to_proxy.write_all(&data).await.is_ok();
                    }
                    Inbound::Eof if open => {
                        let _ = to_proxy.shutdown().await;
                        open = false;
                    }
                    _ => {}
                }
            }
        };
        tokio::select! {
            _ = async { tokio::join!(proxy, drain) } => {}
            _ = feed => {}
        }
        let _ = out.send(frame(CLOSE, id, &[])).await;
        let _ = done.send(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ProxyMode;

    #[test]
    fn dockerd_always_uses_the_relay() {
        let proxies = daemon_proxies();
        assert_eq!(proxies["http-proxy"], ENGINE_PROXY_URL);
        assert_eq!(proxies["https-proxy"], ENGINE_PROXY_URL);
        // NO_PROXY is applied on Windows, like Docker Desktop.
        assert!(proxies.get("no-proxy").is_none());
    }

    #[test]
    fn upstream_follows_the_settings() {
        let state = EngineProxyState::default();
        assert!(state.upstream().is_empty());
        state.configure(&DockerProxySettings {
            mode: ProxyMode::Manual,
            http_proxy: "http://127.0.0.1:7890".into(),
            https_proxy: "https://proxy.example.com:8443".into(),
            no_proxy: "localhost".into(),
        });
        let upstream = state.upstream();
        assert_eq!(upstream.http_proxy, "http://127.0.0.1:7890");
        assert_eq!(upstream.https_proxy, "https://proxy.example.com:8443");
        assert_eq!(upstream.no_proxy, "localhost");
    }

    #[test]
    fn frames_round_trip() {
        let encoded = frame(DATA, 0x0102_0304, b"abc");
        let header: [u8; HEADER_LEN] = encoded[..HEADER_LEN].try_into().unwrap();
        assert_eq!(parse_header(&header), (DATA, 0x0102_0304, 3));
        assert_eq!(&encoded[HEADER_LEN..], b"abc");
    }

    #[test]
    fn vm_proxy_matches_protocol_constants() {
        let source = include_str!("../../helpers/vm-proxy/src/main.rs");
        for (name, value) in [
            ("OPEN", OPEN),
            ("DATA", DATA),
            ("EOF", EOF),
            ("CLOSE", CLOSE),
            ("READY", READY),
        ] {
            assert!(
                source.contains(&format!("const {name}: u8 = {value};")),
                "{name}"
            );
        }
        assert!(source.contains(&format!("const HEADER_LEN: usize = {HEADER_LEN};")));
        assert!(source.contains("const MAX_PAYLOAD: usize = 256 * 1024;"));
        assert_eq!(MAX_PAYLOAD, 256 * 1024);
        // Listens on the VM loopback only, never on all interfaces.
        assert!(source.contains("TcpListener::bind((\"127.0.0.1\", port))"));
        assert!(INSTALL_VM_PROXY_SCRIPT.contains(VM_PROXY_ROOT));
    }
}
