//! Windows side of the Engine proxy: a forward HTTP proxy like the one in
//! Docker Desktop's backend behind `http.docker.internal:3128`.
//!
//! Every request is handled on its own. The current proxy settings are
//! resolved for it (the system proxy is read again each time), `NO_PROXY`
//! is applied here rather than in dockerd, and the request then goes either
//! to the upstream proxy (`http://` or `https://`) or straight to its target:
//!
//! - `CONNECT host:port`: the HTTPS upstream gets the `CONNECT` itself;
//!   without one the tunnel is opened directly and answered with `200`.
//! - Absolute-form requests (`GET http://host/path`): the HTTP upstream gets
//!   them unchanged; without one they are rewritten to origin form and sent
//!   to the target. Client connections are kept alive across requests, even
//!   to different hosts (Go's transport shares one proxy connection between
//!   plain-HTTP targets), and the outgoing connection is reused while
//!   requests keep going to the same place.

// Only the Windows relay (`engine_proxy::relay`) serves this proxy; on other
// platforms the module is still compiled so its unit tests run everywhere.
#![cfg_attr(not(windows), allow(dead_code))]

use std::{
    io,
    net::IpAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::TcpStream,
    time::timeout,
};
use url::{Host, Url};

use crate::settings::ResolvedProxy;

const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 128;
const BUFFER: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";
/// Hop-by-hop headers, never forwarded (`Transfer-Encoding` is kept: bodies
/// are relayed with their original framing).
const HOP_BY_HOP: [&str; 5] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "upgrade",
    "te",
];

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Serves one client connection until it closes.
pub(crate) async fn serve<C, F>(client: C, resolve: F) -> io::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin + Send,
    F: Fn() -> ResolvedProxy,
{
    let (read, mut write) = tokio::io::split(client);
    let mut reader = BufReader::with_capacity(BUFFER, read);
    let mut server: Option<Server> = None;
    loop {
        let Some(head) = read_head(&mut reader).await? else {
            break;
        };
        let request = match parse_request(&head) {
            Ok(request) => request,
            Err(message) => {
                respond_error(&mut write, 400, "Bad Request", &message).await?;
                break;
            }
        };
        let proxy = resolve();
        if request.method.eq_ignore_ascii_case("CONNECT") {
            return tunnel(&request, &head, reader, write, &proxy).await;
        }
        match forward(&request, &mut reader, &mut write, &proxy, &mut server).await? {
            Next::KeepAlive => {}
            Next::Close => break,
        }
    }
    let _ = write.shutdown().await;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Upstream {
    host: String,
    port: u16,
    tls: bool,
}

impl Upstream {
    fn parse(address: &str) -> Option<Self> {
        let url = Url::parse(address.trim()).ok()?;
        let tls = match url.scheme() {
            "http" => false,
            "https" => true,
            _ => return None,
        };
        Some(Self {
            host: host_string(&url)?,
            port: url.port_or_known_default()?,
            tls,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Direct,
    Via(Upstream),
}

fn route(address: &str, no_proxy: &str, host: &str, port: u16) -> Route {
    if bypass(no_proxy, host, port) {
        return Route::Direct;
    }
    Upstream::parse(address).map_or(Route::Direct, Route::Via)
}

/// `NO_PROXY` matching with Go's `httpproxy` rules: `*`; IPs and CIDRs;
/// `example.com` matches the domain and its subdomains, `.example.com` and
/// `*.example.com` only subdomains; an entry may carry `:port`.
fn bypass(no_proxy: &str, host: &str, port: u16) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let ip = host.parse::<IpAddr>().ok();
    no_proxy
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Some((network, bits)) = entry.split_once('/') {
                return match (ip, network.parse::<IpAddr>(), bits.parse::<u8>()) {
                    (Some(ip), Ok(network), Ok(bits)) => in_network(ip, network, bits),
                    _ => false,
                };
            }
            let (name, entry_port) = split_port(&entry);
            if entry_port.is_some_and(|entry_port| entry_port != port) {
                return false;
            }
            if let Ok(entry_ip) = name.parse::<IpAddr>() {
                return ip == Some(entry_ip);
            }
            let name = name.strip_prefix('*').unwrap_or(name);
            match name.strip_prefix('.') {
                Some(suffix) => host.ends_with(&format!(".{suffix}")),
                None => host == name || host.ends_with(&format!(".{name}")),
            }
        })
}

/// Splits `host:port`, `[v6]:port`; bare IPv6 addresses have no port.
fn split_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(rest) = entry.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (rest, None),
        };
    }
    match entry.split_once(':') {
        Some((host, port)) if !port.contains(':') => match port.parse() {
            Ok(port) => (host, Some(port)),
            Err(_) => (entry, None),
        },
        _ => (entry, None),
    }
}

fn in_network(ip: IpAddr, network: IpAddr, bits: u8) -> bool {
    fn masked(bytes: &[u8], bits: u8) -> Vec<u8> {
        bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                let keep = (bits as usize).saturating_sub(index * 8).min(8);
                if keep == 0 {
                    0
                } else {
                    byte & (0xFFu8 << (8 - keep))
                }
            })
            .collect()
    }
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) if bits <= 32 => {
            masked(&ip.octets(), bits) == masked(&network.octets(), bits)
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) if bits <= 128 => {
            masked(&ip.octets(), bits) == masked(&network.octets(), bits)
        }
        _ => false,
    }
}

fn host_string(url: &Url) -> Option<String> {
    Some(match url.host()? {
        Host::Domain(domain) => domain.to_string(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    })
}

async fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                t!("runtime_host.error.connect_timeout").into_owned(),
            )
        })??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Connects to the upstream proxy, or to `host:port` for a direct route.
async fn open(route: &Route, host: &str, port: u16) -> io::Result<Box<dyn Io>> {
    match route {
        Route::Direct => Ok(Box::new(dial(host, port).await?)),
        Route::Via(upstream) => {
            let stream = dial(&upstream.host, upstream.port).await?;
            if !upstream.tls {
                return Ok(Box::new(stream));
            }
            let name = rustls::pki_types::ServerName::try_from(upstream.host.clone())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let tls = tokio_rustls::TlsConnector::from(tls_config()?)
                .connect(name, stream)
                .await?;
            Ok(Box::new(tls))
        }
    }
}

/// TLS to `https://` upstream proxies, verified against the Windows
/// certificate store (so enterprise root CAs work, as in Docker Desktop).
fn tls_config() -> io::Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let (added, _) =
                roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            if added == 0 {
                return Err(t!("runtime_host.error.system_root_certs_unreadable").into_owned());
            }
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map(|builder| Arc::new(builder.with_root_certificates(roots).with_no_client_auth()))
            .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(io::Error::other)
}

async fn tunnel<R, W>(
    request: &Request,
    head: &[u8],
    mut reader: BufReader<R>,
    mut write: W,
    proxy: &ResolvedProxy,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some((host, port)) = connect_authority(&request.target) else {
        return respond_error(
            &mut write,
            400,
            "Bad Request",
            &t!("runtime_host.error.invalid_connect_target"),
        )
        .await;
    };
    let route = route(&proxy.https_proxy, &proxy.no_proxy, &host, port);
    let mut stream = match open(&route, &host, port).await {
        Ok(stream) => stream,
        Err(error) => {
            let message = connect_error(&route, &host, port, &error);
            return respond_error(&mut write, 502, "Bad Gateway", &message).await;
        }
    };
    match route {
        Route::Direct => write.write_all(ESTABLISHED).await?,
        // The upstream answers the CONNECT itself; its reply flows back.
        Route::Via(_) => stream.write_all(head).await?,
    }
    let (mut from_server, mut to_server) = tokio::io::split(stream);
    let upload = async {
        tokio::io::copy(&mut reader, &mut to_server).await?;
        to_server.shutdown().await
    };
    let download = async {
        tokio::io::copy(&mut from_server, &mut write).await?;
        write.shutdown().await
    };
    tokio::try_join!(upload, download).map(|_| ())
}

/// Target of `CONNECT host:port` (`[v6]:port`); the port defaults to 443.
fn connect_authority(target: &str) -> Option<(String, u16)> {
    let (host, port) = match target.strip_prefix('[') {
        Some(rest) => {
            let (host, tail) = rest.split_once(']')?;
            (host, tail.strip_prefix(':'))
        }
        None => match target.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (target, None),
        },
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => 443,
    };
    Some((host.to_string(), port))
}

fn connect_error(route: &Route, host: &str, port: u16, error: &io::Error) -> String {
    match route {
        Route::Direct => t!(
            "runtime_host.error.connect_failed",
            host = host,
            port = port,
            error = error
        )
        .into_owned(),
        Route::Via(upstream) => t!(
            "runtime_host.error.upstream_connect_failed",
            host = upstream.host,
            port = upstream.port,
            error = error
        )
        .into_owned(),
    }
}

enum Next {
    KeepAlive,
    Close,
}

/// Where an outgoing connection goes; it is reused while this stays equal.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ServerKey {
    Direct(String, u16),
    Via(Upstream),
}

struct Server {
    key: ServerKey,
    stream: BufReader<Box<dyn Io>>,
}

async fn forward<R, W>(
    request: &Request,
    reader: &mut BufReader<R>,
    write: &mut W,
    proxy: &ResolvedProxy,
    server: &mut Option<Server>,
) -> io::Result<Next>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let target = match absolute_target(&request.target) {
        Some(target) => target,
        None => {
            respond_error(
                write,
                400,
                "Bad Request",
                &t!("runtime_host.error.absolute_url_only"),
            )
            .await?;
            return Ok(Next::Close);
        }
    };
    let body = match request_body(&request.headers) {
        Ok(body) => body,
        Err(message) => {
            respond_error(write, 400, "Bad Request", &message).await?;
            return Ok(Next::Close);
        }
    };
    let route = route(
        &proxy.http_proxy,
        &proxy.no_proxy,
        &target.host,
        target.port,
    );
    let (key, outgoing) = match &route {
        Route::Direct => (
            ServerKey::Direct(target.host.clone(), target.port),
            request_head(request, &target.origin_form, Some(&target.authority)),
        ),
        Route::Via(upstream) => (
            ServerKey::Via(upstream.clone()),
            request_head(request, &request.target, None),
        ),
    };
    let client_keep = keeps_alive(request.version, &request.headers);

    // A reused connection may have been closed by the server in the
    // meantime; retry a body-less request once on a fresh one.
    let mut attempt = 0;
    let response = loop {
        attempt += 1;
        let reused = server.as_ref().is_some_and(|server| server.key == key);
        if !reused {
            *server = None;
            match open(&route, &target.host, target.port).await {
                Ok(stream) => {
                    *server = Some(Server {
                        key: key.clone(),
                        stream: BufReader::with_capacity(BUFFER, stream),
                    })
                }
                Err(error) => {
                    let message = connect_error(&route, &target.host, target.port, &error);
                    respond_error(write, 502, "Bad Gateway", &message).await?;
                    return Ok(Next::Close);
                }
            }
        }
        let connection = server.as_mut().expect("server connection just set");
        let exchanged = exchange(connection, &outgoing, body, reader, write).await;
        match exchanged {
            Ok(Some(response)) => break response,
            Ok(None) | Err(_) if reused && attempt == 1 && body == Body::Empty => {
                *server = None;
            }
            Ok(None) => {
                *server = None;
                respond_error(
                    write,
                    502,
                    "Bad Gateway",
                    &t!("runtime_host.error.server_closed_without_response"),
                )
                .await?;
                return Ok(Next::Close);
            }
            Err(error) => {
                *server = None;
                let _ = respond_error(write, 502, "Bad Gateway", &error.to_string()).await;
                return Ok(Next::Close);
            }
        }
    };

    let connection = server.as_mut().expect("server connection kept");
    let response_body = response_body(&request.method, response.code, &response.headers);
    let until_close = response_body == Body::UntilClose;
    let server_keep = !until_close && keeps_alive(response.version, &response.headers);
    let client_keep = client_keep && !until_close;
    write
        .write_all(&response_head(&response, !client_keep))
        .await?;
    copy_body(response_body, &mut connection.stream, write).await?;
    write.flush().await?;
    if !server_keep {
        *server = None;
    }
    Ok(if client_keep {
        Next::KeepAlive
    } else {
        Next::Close
    })
}

/// Sends the request (head and body) and reads the final response head,
/// relaying interim `1xx` responses. None: the server closed first.
async fn exchange<R, W>(
    server: &mut Server,
    outgoing: &[u8],
    body: Body,
    reader: &mut BufReader<R>,
    write: &mut W,
) -> io::Result<Option<Response>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let stream = server.stream.get_mut();
    stream.write_all(outgoing).await?;
    copy_body(body, reader, stream).await?;
    stream.flush().await?;
    loop {
        let Some(head) = read_head(&mut server.stream).await? else {
            return Ok(None);
        };
        let response = parse_response(&head)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
        if (100..200).contains(&response.code) && response.code != 101 {
            write.write_all(&head).await?;
            continue;
        }
        if response.code == 101 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                t!("runtime_host.error.protocol_upgrade_unsupported").into_owned(),
            ));
        }
        return Ok(Some(response));
    }
}

struct Target {
    host: String,
    port: u16,
    /// `host[:port]` as written, for a missing `Host` header.
    authority: String,
    /// Path and query as written, `/` when empty.
    origin_form: String,
}

fn absolute_target(target: &str) -> Option<Target> {
    let url = Url::parse(target).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let rest = &target[target.find("://")? + 3..];
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest[..end].rsplit('@').next().unwrap_or_default();
    let origin_form = match &rest[end..] {
        "" => "/".to_string(),
        tail if tail.starts_with('?') => format!("/{tail}"),
        tail => tail.split('#').next().unwrap_or(tail).to_string(),
    };
    Some(Target {
        host: host_string(&url)?,
        port: url.port_or_known_default()?,
        authority: authority.to_string(),
        origin_form,
    })
}

type Headers = Vec<(String, Vec<u8>)>;

struct Request {
    method: String,
    target: String,
    version: u8,
    headers: Headers,
}

struct Response {
    code: u16,
    reason: String,
    version: u8,
    headers: Headers,
}

fn parse_request(head: &[u8]) -> Result<Request, String> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(t!("runtime_host.error.invalid_request_headers").into_owned()),
    }
    Ok(Request {
        method: request.method.unwrap_or_default().to_string(),
        target: request.path.unwrap_or_default().to_string(),
        version: request.version.unwrap_or(1),
        headers: collect_headers(request.headers),
    })
}

fn parse_response(head: &[u8]) -> Result<Response, String> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(t!("runtime_host.error.invalid_response_headers").into_owned()),
    }
    Ok(Response {
        code: response.code.unwrap_or_default(),
        reason: response.reason.unwrap_or_default().to_string(),
        version: response.version.unwrap_or(1),
        headers: collect_headers(response.headers),
    })
}

fn collect_headers(headers: &[httparse::Header<'_>]) -> Headers {
    headers
        .iter()
        .map(|header| (header.name.to_string(), header.value.to_vec()))
        .collect()
}

fn header_values<'a>(headers: &'a Headers, name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
    headers
        .iter()
        .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_slice())
}

fn tokens<'a>(headers: &'a Headers, name: &'a str) -> impl Iterator<Item = String> + 'a {
    header_values(headers, name).flat_map(|value| {
        String::from_utf8_lossy(value)
            .split(',')
            .map(|token| token.trim().to_ascii_lowercase())
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>()
    })
}

fn keeps_alive(version: u8, headers: &Headers) -> bool {
    let mut connection = tokens(headers, "connection").chain(tokens(headers, "proxy-connection"));
    if version >= 1 {
        !connection.any(|token| token == "close")
    } else {
        connection.any(|token| token == "keep-alive")
    }
}

/// Head with hop-by-hop headers removed; `host` adds a missing `Host`
/// (direct requests, which also drop `Proxy-Authorization`).
fn request_head(request: &Request, target: &str, host: Option<&str>) -> Vec<u8> {
    let mut head =
        format!("{} {target} HTTP/1.{}\r\n", request.method, request.version).into_bytes();
    let has_host = header_values(&request.headers, "host").next().is_some();
    write_headers(&mut head, &request.headers, |name| {
        host.is_none() || !name.eq_ignore_ascii_case("proxy-authorization")
    });
    if let Some(host) = host.filter(|_| !has_host) {
        head.extend_from_slice(format!("Host: {host}\r\n").as_bytes());
    }
    head.extend_from_slice(b"\r\n");
    head
}

fn response_head(response: &Response, close: bool) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.{} {} {}\r\n",
        response.version, response.code, response.reason
    )
    .into_bytes();
    write_headers(&mut head, &response.headers, |_| true);
    if close {
        head.extend_from_slice(b"Connection: close\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

fn write_headers(head: &mut Vec<u8>, headers: &Headers, keep: impl Fn(&str) -> bool) {
    let listed: Vec<String> = tokens(headers, "connection").collect();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || listed.contains(&lower) || !keep(name) {
            continue;
        }
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Body {
    Empty,
    Length(u64),
    Chunked,
    UntilClose,
}

fn is_chunked(headers: &Headers) -> bool {
    tokens(headers, "transfer-encoding").last().as_deref() == Some("chunked")
}

fn content_length(headers: &Headers) -> Result<Option<u64>, String> {
    let mut length = None;
    for value in header_values(headers, "content-length") {
        let parsed = std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .ok_or_else(|| t!("runtime_host.error.invalid_content_length").into_owned())?;
        if length.is_some_and(|length| length != parsed) {
            return Err(t!("runtime_host.error.content_length_mismatch").into_owned());
        }
        length = Some(parsed);
    }
    Ok(length)
}

fn request_body(headers: &Headers) -> Result<Body, String> {
    if header_values(headers, "transfer-encoding").next().is_some() {
        return if is_chunked(headers) {
            Ok(Body::Chunked)
        } else {
            Err(t!("runtime_host.error.unsupported_transfer_encoding").into_owned())
        };
    }
    Ok(match content_length(headers)? {
        Some(0) | None => Body::Empty,
        Some(length) => Body::Length(length),
    })
}

fn response_body(method: &str, code: u16, headers: &Headers) -> Body {
    if method.eq_ignore_ascii_case("HEAD")
        || (100..200).contains(&code)
        || code == 204
        || code == 304
    {
        return Body::Empty;
    }
    if is_chunked(headers) {
        return Body::Chunked;
    }
    match content_length(headers) {
        Ok(Some(0)) => Body::Empty,
        Ok(Some(length)) => Body::Length(length),
        _ => Body::UntilClose,
    }
}

async fn copy_body<R, W>(body: Body, reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match body {
        Body::Empty => Ok(()),
        Body::Length(length) => copy_exact(reader, writer, length).await,
        Body::UntilClose => tokio::io::copy(reader, writer).await.map(|_| ()),
        Body::Chunked => loop {
            let line = read_line(reader).await?;
            writer.write_all(&line).await?;
            let size = std::str::from_utf8(&line)
                .ok()
                .and_then(|line| line.split(';').next())
                .and_then(|size| u64::from_str_radix(size.trim(), 16).ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        t!("runtime_host.error.invalid_chunk_length").into_owned(),
                    )
                })?;
            if size == 0 {
                // Trailers, then the final blank line.
                loop {
                    let line = read_line(reader).await?;
                    writer.write_all(&line).await?;
                    if line == b"\r\n" || line == b"\n" {
                        return Ok(());
                    }
                }
            }
            copy_exact(reader, writer, size).await?;
            let end = read_line(reader).await?;
            writer.write_all(&end).await?;
        },
    }
}

async fn copy_exact<R, W>(reader: &mut R, writer: &mut W, length: u64) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let copied = tokio::io::copy(&mut reader.take(length), writer).await?;
    if copied < length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    (&mut *reader)
        .take(MAX_HEAD as u64)
        .read_until(b'\n', &mut line)
        .await?;
    if !line.ends_with(b"\n") {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line)
}

/// A message head including the blank line; None on EOF before any byte.
async fn read_head<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    loop {
        let start = head.len();
        let limit = (MAX_HEAD + 1).saturating_sub(start) as u64;
        let read = (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut head)
            .await?;
        if read == 0 {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                t!("runtime_host.error.head_too_large").into_owned(),
            ));
        }
        if matches!(&head[start..], b"\r\n" | b"\n") {
            if start == 0 {
                head.clear();
                continue;
            }
            return Ok(Some(head));
        }
    }
}

async fn respond_error<W: AsyncWrite + Unpin>(
    write: &mut W,
    code: u16,
    reason: &str,
    message: &str,
) -> io::Result<()> {
    let body = t!("runtime_host.error.proxy_response_body", message = message).into_owned();
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    write.write_all(response.as_bytes()).await?;
    write.flush().await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::{io::DuplexStream, net::TcpListener, sync::mpsc};

    use super::*;

    #[test]
    fn matches_no_proxy_like_go() {
        let list = "*.internal, example.com, .corp.net, 10.0.0.0/8, 192.168.1.5, registry.local:5000, [::1]";
        assert!(bypass(list, "a.internal", 443));
        assert!(!bypass(list, "internal", 443));
        assert!(bypass(list, "example.com", 80));
        assert!(bypass(list, "hub.example.com", 80));
        assert!(!bypass(list, "notexample.com", 80));
        assert!(bypass(list, "x.corp.net", 80));
        assert!(!bypass(list, "corp.net", 80));
        assert!(bypass(list, "10.20.30.40", 80));
        assert!(!bypass(list, "11.0.0.1", 80));
        assert!(bypass(list, "192.168.1.5", 80));
        assert!(bypass(list, "registry.local", 5000));
        assert!(!bypass(list, "registry.local", 443));
        assert!(bypass(list, "::1", 80));
        assert!(bypass("*", "anything", 1));
        assert!(!bypass("", "example.com", 80));
        assert!(bypass("fd00::/8", "fd12::1", 80));
    }

    #[test]
    fn routes_by_scheme_and_no_proxy() {
        assert_eq!(
            route("http://127.0.0.1:7890", "", "registry-1.docker.io", 443),
            Route::Via(Upstream {
                host: "127.0.0.1".into(),
                port: 7890,
                tls: false
            })
        );
        assert_eq!(
            route("https://proxy.corp:8443", "", "a", 80),
            Route::Via(Upstream {
                host: "proxy.corp".into(),
                port: 8443,
                tls: true
            })
        );
        assert_eq!(
            route("http://127.0.0.1:7890", "a.io", "a.io", 443),
            Route::Direct
        );
        assert_eq!(route("", "", "a.io", 443), Route::Direct);
    }

    #[test]
    fn parses_targets() {
        let target = absolute_target("http://user@example.com:8080?x=1").unwrap();
        assert_eq!((target.host.as_str(), target.port), ("example.com", 8080));
        assert_eq!(target.authority, "example.com:8080");
        assert_eq!(target.origin_form, "/?x=1");
        let target = absolute_target("http://example.com/v2/a%20b?q").unwrap();
        assert_eq!(target.origin_form, "/v2/a%20b?q");
        assert_eq!(target.port, 80);
        assert!(absolute_target("/relative").is_none());
        assert!(absolute_target("https://example.com/").is_none());
        assert_eq!(
            connect_authority("registry-1.docker.io:443"),
            Some(("registry-1.docker.io".into(), 443))
        );
        assert_eq!(
            connect_authority("[2001:db8::1]:8443"),
            Some(("2001:db8::1".into(), 8443))
        );
        assert_eq!(connect_authority(":443"), None);
    }

    #[test]
    fn strips_hop_by_hop_headers() {
        let request = parse_request(
            b"GET http://a/x HTTP/1.1\r\nHost: a\r\nProxy-Connection: keep-alive\r\nConnection: X-Secret\r\nX-Secret: 1\r\nProxy-Authorization: Basic x\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        let direct = String::from_utf8(request_head(&request, "/x", Some("a"))).unwrap();
        assert_eq!(direct, "GET /x HTTP/1.1\r\nHost: a\r\nAccept: */*\r\n\r\n");
        let via = String::from_utf8(request_head(&request, &request.target, None)).unwrap();
        assert!(via.starts_with("GET http://a/x HTTP/1.1\r\n"));
        assert!(via.contains("Proxy-Authorization: Basic x\r\n"));
        assert!(!via.contains("X-Secret"));
    }

    /// A tiny HTTP/1.1 origin: answers each request on a connection with
    /// the request line it saw, then `extra` as raw response framing.
    async fn origin(chunked: bool) -> (u16, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read);
                    while let Ok(Some(head)) = read_head(&mut reader).await {
                        let request = parse_request(&head).unwrap();
                        if let Ok(Body::Length(length)) = request_body(&request.headers) {
                            let mut body = vec![0; length as usize];
                            reader.read_exact(&mut body).await.unwrap();
                        }
                        let text = String::from_utf8_lossy(&head).to_string();
                        let _ = seen.send(text);
                        let body = format!("{} {}", request.method, request.target);
                        let response = if chunked {
                            format!(
                                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                                body.len()
                            )
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        if write.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, rx)
    }

    /// Runs `serve` on an in-memory client with fixed proxy settings.
    fn spawn_client(proxy: ResolvedProxy) -> BufReader<DuplexStream> {
        let (ours, theirs) = tokio::io::duplex(BUFFER);
        tokio::spawn(async move {
            let _ = serve(theirs, move || proxy.clone()).await;
        });
        BufReader::new(ours)
    }

    async fn read_response(client: &mut BufReader<DuplexStream>) -> (u16, Headers, Vec<u8>) {
        let head = read_head(client).await.unwrap().expect("response");
        let response = parse_response(&head).unwrap();
        let mut body = Vec::new();
        copy_body(
            response_body("GET", response.code, &response.headers),
            client,
            &mut body,
        )
        .await
        .unwrap();
        (response.code, response.headers, body)
    }

    #[tokio::test]
    async fn forwards_plain_http_directly_with_keep_alive_across_hosts() {
        let (first, mut first_seen) = origin(false).await;
        let (second, mut second_seen) = origin(true).await;
        let mut client = spawn_client(ResolvedProxy::default());
        let request = format!(
            "GET http://127.0.0.1:{first}/a?b HTTP/1.1\r\nHost: 127.0.0.1:{first}\r\nProxy-Connection: keep-alive\r\n\r\n"
        );
        client
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .unwrap();
        let (code, _, body) = read_response(&mut client).await;
        assert_eq!((code, body.as_slice()), (200, b"GET /a?b".as_slice()));
        let seen = first_seen.recv().await.unwrap();
        assert!(seen.starts_with("GET /a?b HTTP/1.1\r\n"), "{seen}");
        assert!(!seen.contains("Proxy-Connection"));

        // Same client connection, another host, a request body, chunked reply.
        let request =
            format!("POST http://localhost:{second}/c HTTP/1.1\r\nContent-Length: 3\r\n\r\nxyz");
        client
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .unwrap();
        let (code, _, body) = read_response(&mut client).await;
        assert_eq!(code, 200);
        assert_eq!(body, b"7\r\nPOST /c\r\n0\r\n\r\n");
        let seen = second_seen.recv().await.unwrap();
        assert!(
            seen.contains(&format!("Host: localhost:{second}\r\n")),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn sends_plain_http_to_the_upstream_unless_bypassed() {
        let (upstream, mut upstream_seen) = origin(false).await;
        let (direct, mut direct_seen) = origin(false).await;
        let mut client = spawn_client(ResolvedProxy {
            http_proxy: format!("http://127.0.0.1:{upstream}"),
            https_proxy: String::new(),
            no_proxy: "localhost".into(),
        });
        client
            .get_mut()
            .write_all(b"GET http://example.invalid/x HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
            .await
            .unwrap();
        let (code, _, body) = read_response(&mut client).await;
        assert_eq!(
            (code, body.as_slice()),
            (200, b"GET http://example.invalid/x".as_slice())
        );
        assert!(upstream_seen
            .recv()
            .await
            .unwrap()
            .starts_with("GET http://example.invalid/x "));

        let request = format!("GET http://localhost:{direct}/y HTTP/1.1\r\n\r\n");
        client
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .unwrap();
        let (_, _, body) = read_response(&mut client).await;
        assert_eq!(body, b"GET /y");
        assert!(direct_seen.recv().await.unwrap().starts_with("GET /y "));
    }

    /// Echo server: proves the tunnel carries bytes both ways.
    async fn echo() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn opens_connect_tunnels_directly_or_through_the_upstream() {
        let target = echo().await;
        let mut client = spawn_client(ResolvedProxy::default());
        let request = format!("CONNECT 127.0.0.1:{target} HTTP/1.1\r\nHost: x\r\n\r\nping");
        client
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .unwrap();
        let head = read_head(&mut client).await.unwrap().unwrap();
        assert_eq!(head, ESTABLISHED);
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");

        // Through an upstream: it receives the CONNECT verbatim.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap().port();
        let recorded = seen.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0; 1024];
            let read = stream.read(&mut buffer).await.unwrap();
            recorded.lock().unwrap().extend_from_slice(&buffer[..read]);
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        });
        let mut client = spawn_client(ResolvedProxy {
            http_proxy: String::new(),
            https_proxy: format!("http://127.0.0.1:{upstream}"),
            no_proxy: String::new(),
        });
        client
            .get_mut()
            .write_all(b"CONNECT registry-1.docker.io:443 HTTP/1.1\r\nHost: registry-1.docker.io:443\r\n\r\n")
            .await
            .unwrap();
        let head = read_head(&mut client).await.unwrap().unwrap();
        assert_eq!(head, b"HTTP/1.1 200 OK\r\n\r\n");
        let seen = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
        assert!(
            seen.starts_with("CONNECT registry-1.docker.io:443 HTTP/1.1\r\n"),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn answers_bad_gateway_when_the_upstream_is_down() {
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let mut client = spawn_client(ResolvedProxy {
            http_proxy: format!("http://127.0.0.1:{port}"),
            https_proxy: format!("http://127.0.0.1:{port}"),
            no_proxy: String::new(),
        });
        client
            .get_mut()
            .write_all(b"CONNECT a.io:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let (code, headers, body) = read_response(&mut client).await;
        assert_eq!(code, 502);
        assert!(tokens(&headers, "connection").any(|token| token == "close"));
        assert!(String::from_utf8_lossy(&body).contains(&format!("127.0.0.1:{port}")));
    }
}
