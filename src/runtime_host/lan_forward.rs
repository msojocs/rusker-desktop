//! Windows-side port forwarding for WSL NAT networking, the counterpart of
//! Docker Desktop's `com.docker.backend` forwarder.
//!
//! With NAT, WSL's own relay (`wslrelay`) only forwards TCP ports to Windows
//! `localhost`: other devices on the network cannot reach published
//! containers, and UDP is not forwarded at all. Like Docker Desktop, while the
//! engine session is alive Rusker therefore listens on Windows for:
//!
//! - ports published on all interfaces (`-p 8080:80`, TCP and UDP): on `::`
//!   (dual-stack), so LAN devices reach them over IPv4 and IPv6;
//! - UDP ports published on `127.0.0.1`: on Windows `127.0.0.1`;
//! - with host networking enabled, the same for sockets that `--net=host`
//!   containers listen on.
//!
//! Connections are relayed to the VM's `eth0` address. Loopback TCP is left
//! to `wslrelay`. In mirrored mode the VM shares the Windows network, so
//! nothing is forwarded.

use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

/// Upper bound on forwarded ports, so a huge published range cannot exhaust
/// Windows sockets.
const MAX_FORWARDED_PORTS: usize = 1024;

/// Runs inside the managed distribution: prints `mode <wslinfo mode>`, then
/// a block whenever the VM address, the published ports or the containers'
/// listening sockets change, ending with `end`. Lines are tagged: `A` the
/// `eth0` line of `ip -o addr`, `P` a `docker ps` Ports column, `T`/`U` an
/// `ss` TCP/UDP listener owned by a container process (cgroup under
/// `docker`), so the VM's own services (chronyd, WSL's DNS, docker-proxy)
/// are never forwarded. Exits early unless WSL uses NAT.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) const FEED_SCRIPT: &str = r#"set -u
mode=$(wslinfo --networking-mode 2>/dev/null || true)
printf 'mode %s\n' "${mode:-unknown}"
case "${mode:-unknown}" in
    nat|unknown) ;;
    *) exit 0 ;;
esac
container_sockets() {
    ss -H "-l${1}np" 2>/dev/null | while IFS= read -r line; do
        pid=${line##*pid=}
        pid=${pid%%,*}
        case "$pid" in ''|*[!0-9]*) continue ;; esac
        if grep -Eqs '/docker[/-]' "/proc/$pid/cgroup"; then
            printf '%s %s\n' "$2" "$line"
        fi
    done | sort
}
last=
while :; do
    cur="$(ip -4 -o addr show dev eth0 2>/dev/null | head -n 1 | sed 's/^/A /')
$(/usr/bin/docker ps --format '{{.Ports}}' 2>/dev/null | sed 's/^/P /' | sort)
$(container_sockets t T)
$(container_sockets u U)"
    if [ "$cur" != "$last" ]; then
        printf '%s\nend\n' "$cur"
        last=$cur
    fi
    sleep 2
done
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
}

/// Which Windows addresses a forwarded port listens on. `All` wins when a
/// port is wanted in both scopes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Loopback,
    All,
}

type PortKey = (Protocol, u16);

/// What the forwarder is doing, for the settings page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanForwardState {
    /// WSL networking mode reported by `wslinfo` (`nat`, `mirrored`, …);
    /// None while no engine session is running.
    pub mode: Option<String>,
    /// Ports currently listened on by Windows.
    pub forwarded: Vec<(Protocol, u16)>,
    /// Why some ports are not forwarded.
    pub failures: Vec<String>,
}

#[derive(Default)]
pub struct LanForwardStatus {
    state: Mutex<LanForwardState>,
    host_networking: AtomicBool,
}

impl LanForwardStatus {
    pub fn get(&self) -> LanForwardState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Docker Desktop's "Enable host networking"; takes effect within a
    /// poll interval.
    pub fn set_host_networking(&self, enabled: bool) {
        self.host_networking.store(enabled, Ordering::Relaxed);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn host_networking(&self) -> bool {
        self.host_networking.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn update(&self, change: impl FnOnce(&mut LanForwardState)) {
        change(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()));
    }
}

/// One complete feed block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FeedState {
    vm_address: Option<Ipv4Addr>,
    /// From `docker ps`.
    published: BTreeMap<PortKey, Scope>,
    /// Listening sockets in the VM (host networking).
    listening: BTreeMap<PortKey, Scope>,
}

impl FeedState {
    /// The ports to listen on, and whether some were dropped because of
    /// [`MAX_FORWARDED_PORTS`].
    #[cfg_attr(not(windows), allow(dead_code))]
    fn wanted(&self, host_networking: bool) -> (BTreeMap<PortKey, Scope>, bool) {
        if self.vm_address.is_none() {
            // No VM address (e.g. mirrored mode on a WSL without
            // `wslinfo`): nothing can be forwarded.
            return (BTreeMap::new(), false);
        }
        let mut wanted = self.published.clone();
        if host_networking {
            for (&key, &scope) in &self.listening {
                merge(&mut wanted, key, scope);
            }
        }
        let truncated = wanted.len() > MAX_FORWARDED_PORTS;
        (
            wanted.into_iter().take(MAX_FORWARDED_PORTS).collect(),
            truncated,
        )
    }
}

fn merge(ports: &mut BTreeMap<PortKey, Scope>, key: PortKey, scope: Scope) {
    let entry = ports.entry(key).or_insert(scope);
    *entry = (*entry).max(scope);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_block(lines: &[String]) -> FeedState {
    let mut state = FeedState::default();
    for line in lines {
        let Some((tag, rest)) = line.split_once(' ') else {
            continue;
        };
        match tag {
            "A" => state.vm_address = parse_inet_line(rest).or(state.vm_address),
            "P" => {
                for (key, scope) in parse_ports_line(rest) {
                    merge(&mut state.published, key, scope);
                }
            }
            "T" | "U" => {
                let protocol = if tag == "T" {
                    Protocol::Tcp
                } else {
                    Protocol::Udp
                };
                if let Some((port, scope)) = parse_listener_line(protocol, rest) {
                    merge(&mut state.listening, (protocol, port), scope);
                }
            }
            _ => {}
        }
    }
    state
}

/// The IPv4 address in an `ip -4 -o addr` line
/// (`2: eth0    inet 172.20.197.46/20 brd … scope global eth0`).
fn parse_inet_line(line: &str) -> Option<Ipv4Addr> {
    let mut words = line.split_whitespace();
    words.find(|word| *word == "inet")?;
    words.next()?.split('/').next()?.parse().ok()
}

/// Which Windows scope a socket bound to `address` in the VM maps to.
/// Loopback TCP is already relayed by `wslrelay`.
fn scope_for(protocol: Protocol, address: &str) -> Option<Scope> {
    match address {
        "0.0.0.0" | "*" => Some(Scope::All),
        "127.0.0.1" if protocol == Protocol::Udp => Some(Scope::Loopback),
        // IPv6-only sockets cannot be reached through the VM's IPv4
        // address; specific addresses (e.g. WSL's DNS) stay internal.
        _ => None,
    }
}

/// Mappings from the Docker CLI's Ports column
/// (`0.0.0.0:8080->80/tcp, [::]:8080->80/tcp`, ranges as
/// `0.0.0.0:8000-8002->8000-8002/tcp`).
fn parse_ports_line(line: &str) -> Vec<(PortKey, Scope)> {
    let mut ports = Vec::new();
    for entry in line.split(',').map(str::trim) {
        let Some((host, container)) = entry.split_once("->") else {
            continue; // Exposed but not published.
        };
        let protocol = if container.ends_with("/tcp") {
            Protocol::Tcp
        } else if container.ends_with("/udp") {
            Protocol::Udp
        } else {
            continue;
        };
        let Some((address, range)) = host.rsplit_once(':') else {
            continue;
        };
        let Some(scope) = scope_for(protocol, address) else {
            continue;
        };
        let (start, end) = match range.split_once('-') {
            Some((start, end)) => (start.parse::<u16>(), end.parse::<u16>()),
            None => (range.parse::<u16>(), range.parse::<u16>()),
        };
        if let (Ok(start), Ok(end)) = (start, end) {
            if start != 0 && start <= end {
                ports.extend((start..=end).map(|port| ((protocol, port), scope)));
            }
        }
    }
    ports
}

/// A listening socket from `ss -H -ltnp` / `ss -H -lunp`
/// (`LISTEN 0 4096 0.0.0.0:8080 0.0.0.0:*`, `UNCONN 0 0 *:5353 *:*`).
fn parse_listener_line(protocol: Protocol, line: &str) -> Option<(u16, Scope)> {
    let local = line.split_whitespace().nth(3)?;
    let (address, port) = local.rsplit_once(':')?;
    if address.contains('%') {
        return None; // Bound to one interface.
    }
    let port: u16 = port.parse().ok().filter(|port| *port != 0)?;
    Some((port, scope_for(protocol, address)?))
}

/// Ports to open (new, or a different scope) and to close.
#[cfg_attr(not(windows), allow(dead_code))]
fn diff_ports<V>(
    current: &BTreeMap<PortKey, (Scope, V)>,
    wanted: &BTreeMap<PortKey, Scope>,
) -> (Vec<(PortKey, Scope)>, Vec<PortKey>) {
    let open = wanted
        .iter()
        .filter(|(key, scope)| current.get(key).map(|(open, _)| open) != Some(scope))
        .map(|(&key, &scope)| (key, scope))
        .collect();
    let close = current
        .iter()
        .filter(|(key, (scope, _))| wanted.get(key) != Some(scope))
        .map(|(&key, _)| key)
        .collect();
    (open, close)
}

#[cfg(windows)]
pub(super) use windows::serve;

#[cfg(windows)]
mod windows {
    use std::{
        collections::{BTreeMap, HashMap},
        io,
        net::{Ipv4Addr, Ipv6Addr, SocketAddr},
        process::Stdio,
        sync::{Arc, Mutex, RwLock},
        time::Duration,
    };

    use socket2::{Domain, Socket, Type};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        net::{TcpListener, TcpStream, UdpSocket},
        task::AbortHandle,
        time::{sleep, timeout},
    };

    use super::{
        diff_ports, parse_block, FeedState, LanForwardStatus, PortKey, Protocol, Scope,
        FEED_SCRIPT, MAX_FORWARDED_PORTS,
    };
    use crate::runtime_host::wsl::{managed_command, WslHost};

    const POLL: Duration = Duration::from_secs(2);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    /// A UDP client mapping is dropped after this long without replies.
    const UDP_IDLE: Duration = Duration::from_secs(60);

    type Target = Arc<RwLock<Option<Ipv4Addr>>>;

    /// How a feed session ended.
    enum FeedEnd {
        /// WSL does not use NAT: nothing to do until the next session.
        NotNeeded,
        /// The engine session ended or the feed failed; retry later.
        Retry,
    }

    /// Runs for the lifetime of the app, following engine sessions.
    pub(in crate::runtime_host) async fn serve(host: Arc<WslHost>) {
        let status = host.lan_forward_status();
        loop {
            sleep(POLL).await;
            if !host.session_running() {
                continue;
            }
            let end = run_feed(&host, &status).await;
            status.update(|state| {
                state.forwarded.clear();
                state.failures.clear();
            });
            if let FeedEnd::NotNeeded = end {
                // The networking mode only changes after `wsl --shutdown`,
                // which also ends the engine session.
                while host.session_running() {
                    sleep(POLL).await;
                }
            }
            status.update(|state| state.mode = None);
        }
    }

    async fn run_feed(host: &WslHost, status: &LanForwardStatus) -> FeedEnd {
        let mut command = managed_command("root");
        command
            .args(["/bin/sh", "-c", FEED_SCRIPT])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return FeedEnd::Retry;
        };
        let Some(stdout) = child.stdout.take() else {
            return FeedEnd::Retry;
        };
        let mut lines = BufReader::new(stdout).lines();
        let target = Target::default();
        let mut listeners = BTreeMap::<PortKey, (Scope, Listener)>::new();
        let mut block = Vec::new();
        let mut feed: Option<FeedState> = None;
        let mut applied_host_networking = status.host_networking();
        let mut nat = false;
        let end = loop {
            let line = tokio::select! {
                line = lines.next_line() => line,
                () = sleep(POLL) => {
                    if !host.session_running() {
                        break FeedEnd::Retry;
                    }
                    let host_networking = status.host_networking();
                    if host_networking != applied_host_networking {
                        applied_host_networking = host_networking;
                        if let Some(feed) = &feed {
                            apply(&mut listeners, feed, host_networking, &target, status);
                        }
                    }
                    continue;
                }
            };
            let Ok(Some(line)) = line else {
                break if nat {
                    FeedEnd::Retry
                } else {
                    FeedEnd::NotNeeded
                };
            };
            if let Some(mode) = line.strip_prefix("mode ") {
                let mode = mode.trim().to_string();
                nat = matches!(mode.as_str(), "nat" | "unknown");
                status.update(|state| state.mode = Some(mode));
                continue;
            }
            if line != "end" {
                block.push(line);
                continue;
            }
            let parsed = parse_block(&block);
            block.clear();
            *target.write().unwrap_or_else(|p| p.into_inner()) = parsed.vm_address;
            applied_host_networking = status.host_networking();
            apply(
                &mut listeners,
                &parsed,
                applied_host_networking,
                &target,
                status,
            );
            feed = Some(parsed);
        };
        drop(listeners);
        let _ = child.kill().await;
        end
    }

    /// Opens and closes Windows listeners to match the feed.
    fn apply(
        listeners: &mut BTreeMap<PortKey, (Scope, Listener)>,
        feed: &FeedState,
        host_networking: bool,
        target: &Target,
        status: &LanForwardStatus,
    ) {
        let (wanted, truncated) = feed.wanted(host_networking);
        let (open, close) = diff_ports(listeners, &wanted);
        for key in close {
            listeners.remove(&key);
        }
        let mut failures = Vec::new();
        for ((protocol, port), scope) in open {
            match listen(protocol, scope, port, target.clone()) {
                Ok(listener) => {
                    listeners.insert((protocol, port), (scope, listener));
                }
                Err(error) => failures.push(
                    t!(
                        "runtime_host.error.listen_failed",
                        protocol = protocol.label(),
                        port = port,
                        error = error
                    )
                    .into_owned(),
                ),
            }
        }
        if truncated {
            failures.push(
                t!(
                    "runtime_host.error.too_many_forwarded_ports",
                    max = MAX_FORWARDED_PORTS
                )
                .into_owned(),
            );
        }
        let forwarded = listeners.keys().copied().collect();
        status.update(|state| {
            state.forwarded = forwarded;
            state.failures = failures;
        });
    }

    fn listen(protocol: Protocol, scope: Scope, port: u16, target: Target) -> io::Result<Listener> {
        let socket = bind(protocol, scope, port)?;
        let task = match protocol {
            Protocol::Tcp => {
                let listener = TcpListener::from_std(socket.into())?;
                tokio::spawn(tcp_loop(listener, port, target))
            }
            Protocol::Udp => {
                let socket = UdpSocket::from_std(socket.into())?;
                tokio::spawn(udp_loop(Arc::new(socket), port, target))
            }
        };
        Ok(Listener(task.abort_handle()))
    }

    /// A non-blocking socket on `[::]` (dual-stack, like Docker Desktop) or
    /// `0.0.0.0` when IPv6 is unavailable; `127.0.0.1` for loopback.
    fn bind(protocol: Protocol, scope: Scope, port: u16) -> io::Result<Socket> {
        let (kind, proto) = match protocol {
            Protocol::Tcp => (Type::STREAM, socket2::Protocol::TCP),
            Protocol::Udp => (Type::DGRAM, socket2::Protocol::UDP),
        };
        let finish = |socket: Socket| -> io::Result<Socket> {
            if protocol == Protocol::Tcp {
                socket.listen(1024)?;
            }
            socket.set_nonblocking(true)?;
            Ok(socket)
        };
        if scope == Scope::All {
            let dual_stack = Socket::new(Domain::IPV6, kind, Some(proto)).and_then(|socket| {
                socket.set_only_v6(false)?;
                socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
                Ok(socket)
            });
            match dual_stack {
                Ok(socket) => return finish(socket),
                // The port is taken: an IPv4 bind would fail the same way.
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Err(error),
                Err(_) => {}
            }
        }
        let address = match scope {
            Scope::All => Ipv4Addr::UNSPECIFIED,
            Scope::Loopback => Ipv4Addr::LOCALHOST,
        };
        let socket = Socket::new(Domain::IPV4, kind, Some(proto))?;
        socket.bind(&SocketAddr::from((address, port)).into())?;
        finish(socket)
    }

    fn vm_address(target: &Target) -> Option<Ipv4Addr> {
        *target.read().unwrap_or_else(|p| p.into_inner())
    }

    async fn tcp_loop(listener: TcpListener, port: u16, target: Target) {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                sleep(Duration::from_millis(50)).await;
                continue;
            };
            let Some(address) = vm_address(&target) else {
                continue;
            };
            tokio::spawn(async move {
                let Ok(Ok(mut upstream)) =
                    timeout(CONNECT_TIMEOUT, TcpStream::connect((address, port))).await
                else {
                    return;
                };
                let _ = client.set_nodelay(true);
                let _ = upstream.set_nodelay(true);
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    }

    /// Client address -> its upstream socket and reply task.
    type Sessions = Arc<Mutex<HashMap<SocketAddr, UdpSession>>>;

    struct UdpSession {
        upstream: Arc<UdpSocket>,
        /// Holds a handle to the listening socket, so it must end with the
        /// listener for the Windows port to be released.
        _replies: Listener,
    }

    /// Relays datagrams: each client address gets its own upstream socket,
    /// so replies find their way back. When this task is aborted, dropping
    /// `sessions` aborts every reply task too.
    async fn udp_loop(socket: Arc<UdpSocket>, port: u16, target: Target) {
        let sessions = Sessions::default();
        let mut buffer = vec![0u8; 65_535];
        loop {
            // Windows reports ICMP "port unreachable" for an earlier reply
            // as a receive error; it only concerns that one client.
            let Ok((length, client)) = socket.recv_from(&mut buffer).await else {
                continue;
            };
            let Some(address) = vm_address(&target) else {
                continue;
            };
            let existing = sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&client)
                .map(|session| session.upstream.clone());
            let upstream = match existing {
                Some(upstream) => upstream,
                None => {
                    let Ok(upstream) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
                        continue;
                    };
                    if upstream.connect((address, port)).await.is_err() {
                        continue;
                    }
                    let upstream = Arc::new(upstream);
                    let replies = tokio::spawn(udp_replies(
                        upstream.clone(),
                        socket.clone(),
                        client,
                        Arc::downgrade(&sessions),
                    ));
                    sessions.lock().unwrap_or_else(|p| p.into_inner()).insert(
                        client,
                        UdpSession {
                            upstream: upstream.clone(),
                            _replies: Listener(replies.abort_handle()),
                        },
                    );
                    upstream
                }
            };
            let _ = upstream.send(&buffer[..length]).await;
        }
    }

    async fn udp_replies(
        upstream: Arc<UdpSocket>,
        downstream: Arc<UdpSocket>,
        client: SocketAddr,
        sessions: std::sync::Weak<Mutex<HashMap<SocketAddr, UdpSession>>>,
    ) {
        let mut buffer = vec![0u8; 65_535];
        while let Ok(Ok(length)) = timeout(UDP_IDLE, upstream.recv(&mut buffer)).await {
            if downstream.send_to(&buffer[..length], client).await.is_err() {
                break;
            }
        }
        drop(downstream);
        // Removing the entry drops this task's own abort guard, which is a
        // no-op for a task that is already finishing.
        if let Some(sessions) = sessions.upgrade() {
            let session = sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&client);
            drop(session);
        }
    }

    /// A listening port; closed when dropped.
    struct Listener(AbortHandle);

    impl Drop for Listener {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(ports: Vec<(PortKey, Scope)>) -> Vec<(Protocol, u16, Scope)> {
        ports
            .into_iter()
            .map(|((protocol, port), scope)| (protocol, port, scope))
            .collect()
    }

    #[test]
    fn parses_published_ports() {
        use Protocol::{Tcp, Udp};
        assert_eq!(
            keys(parse_ports_line(
                "0.0.0.0:18081->80/tcp, [::]:18081->80/tcp"
            )),
            vec![(Tcp, 18081, Scope::All)]
        );
        assert_eq!(
            keys(parse_ports_line(
                "0.0.0.0:8000-8002->8000-8002/tcp, [::]:8000-8002->8000-8002/tcp"
            )),
            vec![
                (Tcp, 8000, Scope::All),
                (Tcp, 8001, Scope::All),
                (Tcp, 8002, Scope::All)
            ]
        );
        assert_eq!(
            keys(parse_ports_line("0.0.0.0:5353->53/udp")),
            vec![(Udp, 5353, Scope::All)]
        );
        // Loopback UDP needs a Windows loopback listener; loopback TCP is
        // `wslrelay`'s job.
        assert_eq!(
            keys(parse_ports_line("127.0.0.1:5353->53/udp")),
            vec![(Udp, 5353, Scope::Loopback)]
        );
        assert!(parse_ports_line("127.0.0.1:8080->80/tcp").is_empty());
        assert!(parse_ports_line("[::]:8080->80/tcp").is_empty());
        assert!(parse_ports_line("80/tcp, 443/tcp").is_empty());
        assert!(parse_ports_line("").is_empty());
    }

    #[test]
    fn parses_listening_sockets() {
        use Protocol::{Tcp, Udp};
        assert_eq!(
            parse_listener_line(
                Tcp,
                "LISTEN 0 9 *:18080 *:* users:((\"httpd\",pid=952,fd=3))"
            ),
            Some((18080, Scope::All))
        );
        assert_eq!(
            parse_listener_line(Tcp, "LISTEN 0 4096 0.0.0.0:18081 0.0.0.0:*"),
            Some((18081, Scope::All))
        );
        assert_eq!(
            parse_listener_line(Udp, "UNCONN 0 0 127.0.0.1:9999 0.0.0.0:*"),
            Some((9999, Scope::Loopback))
        );
        assert_eq!(
            parse_listener_line(Tcp, "LISTEN 0 4096 127.0.0.1:631 0.0.0.0:*"),
            None
        );
        assert_eq!(
            parse_listener_line(Tcp, "LISTEN 0 4096 [::]:18081 [::]:*"),
            None
        );
        assert_eq!(
            parse_listener_line(Udp, "UNCONN 0 0 10.255.255.254:53 0.0.0.0:*"),
            None
        );
        assert_eq!(
            parse_listener_line(Udp, "UNCONN 0 0 0.0.0.0%eth0:68 0.0.0.0:*"),
            None
        );
        assert_eq!(parse_listener_line(Tcp, "garbage"), None);
    }

    #[test]
    fn host_networking_adds_listening_sockets() {
        let inet = "A 2: eth0    inet 172.20.197.46/20 brd 172.20.207.255 scope global eth0\\       valid_lft forever preferred_lft forever";
        assert_eq!(parse_inet_line(inet), Some(Ipv4Addr::new(172, 20, 197, 46)));
        let block: Vec<String> = [
            inet,
            "P 0.0.0.0:18081->80/tcp, [::]:18081->80/tcp",
            "P ",
            "P 127.0.0.1:9000->9000/tcp",
            "T LISTEN 0 4096 0.0.0.0:18081 0.0.0.0:*",
            "T LISTEN 0 9 *:18080 *:*",
            "U UNCONN 0 0 127.0.0.1:9999 0.0.0.0:*",
            "U UNCONN 0 0 0.0.0.0:9999 0.0.0.0:*",
        ]
        .map(String::from)
        .into();
        let feed = parse_block(&block);
        assert_eq!(feed.vm_address, Some(Ipv4Addr::new(172, 20, 197, 46)));

        let (published, truncated) = feed.wanted(false);
        assert_eq!(
            published,
            BTreeMap::from([((Protocol::Tcp, 18081), Scope::All)])
        );
        assert!(!truncated);
        let (all, _) = feed.wanted(true);
        assert_eq!(
            all,
            BTreeMap::from([
                ((Protocol::Tcp, 18080), Scope::All),
                ((Protocol::Tcp, 18081), Scope::All),
                // Bound on both loopback and all interfaces: all wins.
                ((Protocol::Udp, 9999), Scope::All),
            ])
        );

        // Without a VM address nothing is forwarded.
        let no_address = parse_block(&block[1..]);
        assert!(no_address.wanted(true).0.is_empty());

        let huge = parse_block(&[
            inet.to_string(),
            "P 0.0.0.0:1-60000->1-60000/tcp".to_string(),
        ]);
        let (wanted, truncated) = huge.wanted(false);
        assert_eq!(wanted.len(), MAX_FORWARDED_PORTS);
        assert!(truncated);
    }

    #[test]
    fn diffs_forwarded_ports() {
        let tcp = |port| (Protocol::Tcp, port);
        let current = BTreeMap::from([
            (tcp(80), (Scope::All, ())),
            (tcp(443), (Scope::All, ())),
            ((Protocol::Udp, 53), (Scope::Loopback, ())),
        ]);
        let wanted = BTreeMap::from([
            (tcp(443), Scope::All),
            (tcp(8080), Scope::All),
            ((Protocol::Udp, 53), Scope::All),
        ]);
        let (open, close) = diff_ports(&current, &wanted);
        assert_eq!(
            open,
            vec![(tcp(8080), Scope::All), ((Protocol::Udp, 53), Scope::All)]
        );
        assert_eq!(close, vec![tcp(80), (Protocol::Udp, 53)]);
    }

    #[test]
    fn feed_only_runs_for_nat() {
        assert!(FEED_SCRIPT.contains("wslinfo --networking-mode"));
        assert!(FEED_SCRIPT.contains("nat|unknown"));
        assert!(FEED_SCRIPT.contains("{{.Ports}}"));
        // Only container processes count for host networking.
        assert!(FEED_SCRIPT.contains("/proc/$pid/cgroup"));
        assert!(FEED_SCRIPT.contains("container_sockets u U"));
    }
}
