//! Windows bind mount paths for the managed engine, like Docker Desktop's
//! WSL2 backend: every drive is already visible in the `rusker`
//! distribution through WSL's automounts (`/mnt/c`, …), so the named pipe
//! proxy only rewrites `containers/create` requests from `docker.exe`,
//! turning `C:\src` into `/mnt/c/src`. Nothing is restricted.

use std::io;

use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite,
    AsyncWriteExt as _, BufReader,
};

/// Converts a drive-letter path (`C:\a\b`, `c:/a/b`) to its WSL automount
/// path (`/mnt/c/a/b`). Other sources (named volumes, Linux paths, UNC
/// paths) return `None` and are left to the engine.
fn to_wsl_path(source: &str) -> Option<String> {
    let mut chars = source.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    if chars.next() != Some(':') {
        return None;
    }
    let rest = chars.as_str();
    if !rest.is_empty() && !rest.starts_with(['\\', '/']) {
        return None;
    }
    let mut path = format!("/mnt/{}", drive.to_ascii_lowercase());
    for component in rest.split(['\\', '/']).filter(|part| !part.is_empty()) {
        path.push('/');
        path.push_str(component);
    }
    Some(path)
}

/// Splits `C:\src:/dst:ro` into the source and the remainder (`:/dst:ro`).
fn split_bind(bind: &str) -> (&str, &str) {
    let bytes = bind.as_bytes();
    let start = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        2
    } else {
        0
    };
    match bind[start..].find(':') {
        Some(index) => bind.split_at(start + index),
        None => (bind, ""),
    }
}

/// Rewrites Windows sources in `HostConfig.Binds` and bind-type
/// `HostConfig.Mounts` of a `containers/create` body. `None` when nothing
/// changed (including bodies that are not JSON — the engine reports those).
fn rewrite_create_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut document = serde_json::from_slice::<Value>(body).ok()?;
    let host_config = document.get_mut("HostConfig")?.as_object_mut()?;
    let mut changed = false;
    if let Some(binds) = host_config.get_mut("Binds").and_then(Value::as_array_mut) {
        for bind in binds {
            let Some(text) = bind.as_str() else { continue };
            let (source, rest) = split_bind(text);
            if let Some(translated) = to_wsl_path(source) {
                *bind = Value::String(format!("{translated}{rest}"));
                changed = true;
            }
        }
    }
    if let Some(mounts) = host_config.get_mut("Mounts").and_then(Value::as_array_mut) {
        for mount in mounts.iter_mut().filter_map(Value::as_object_mut) {
            if mount.get("Type").and_then(Value::as_str) != Some("bind") {
                continue;
            }
            if let Some(translated) = mount
                .get("Source")
                .and_then(Value::as_str)
                .and_then(to_wsl_path)
            {
                mount.insert("Source".into(), Value::String(translated));
                changed = true;
            }
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&document).ok()
}

const MAX_HEAD: usize = 64 * 1024;
const MAX_CREATE_BODY: u64 = 16 * 1024 * 1024;

enum Body {
    None,
    Length(u64),
    Chunked,
}

struct RequestHead {
    container_create: bool,
    body: Body,
    upgrade: bool,
}

impl RequestHead {
    fn parse(head: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(head).ok()?;
        if !text.ends_with("\r\n\r\n") && !text.ends_with("\n\n") {
            return None;
        }
        let mut lines = text.lines();
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?;
        let target = request_line.next()?;
        request_line.next()?.strip_prefix("HTTP/")?;
        let path = target.split('?').next().unwrap_or(target);
        let container_create = method == "POST"
            && path
                .strip_suffix("/containers/create")
                .is_some_and(|prefix| {
                    prefix.is_empty()
                        || prefix.strip_prefix("/v").is_some_and(|version| {
                            !version.is_empty()
                                && version.chars().all(|c| c.is_ascii_digit() || c == '.')
                        })
                });
        let mut length = None;
        let mut chunked = false;
        let mut upgrade = false;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = Some(value.parse::<u64>().ok()?),
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                "upgrade" => upgrade = true,
                "connection" => upgrade |= value.to_ascii_lowercase().contains("upgrade"),
                _ => {}
            }
        }
        let body = match (chunked, length) {
            (true, _) => Body::Chunked,
            (false, Some(length)) => Body::Length(length),
            (false, None) => Body::None,
        };
        Some(Self {
            container_create,
            body,
            upgrade,
        })
    }
}

/// Reads one request head (through the blank line). `None` at a clean EOF;
/// an incomplete or oversized head is returned as-is and then fails to parse.
async fn read_head<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    loop {
        let start = head.len();
        if reader.read_until(b'\n', &mut head).await? == 0 {
            return Ok((!head.is_empty()).then_some(head));
        }
        let line = &head[start..];
        if line == b"\r\n" || line == b"\n" || head.len() > MAX_HEAD {
            return Ok(Some(head));
        }
    }
}

fn with_content_length(head: &[u8], length: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut rebuilt = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match line.split_once(':') {
            Some((name, _)) if name.trim().eq_ignore_ascii_case("content-length") => {
                rebuilt.push_str(&format!("{}: {length}\r\n", name.trim()));
            }
            _ => rebuilt.push_str(line),
        }
    }
    rebuilt.into_bytes()
}

async fn copy_chunked<R, W>(reader: &mut R, writer: &mut W) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        writer.write_all(&line).await?;
        let size = std::str::from_utf8(&line)
            .ok()
            .and_then(|text| text.split(';').next())
            .and_then(|size| u64::from_str_radix(size.trim(), 16).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    t!("runtime_host.error.invalid_chunk_length").into_owned(),
                )
            })?;
        if size == 0 {
            // Trailers end with a blank line.
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line).await? == 0 {
                    return Ok(());
                }
                writer.write_all(&line).await?;
                if line == b"\r\n" || line == b"\n" {
                    return Ok(());
                }
            }
        }
        // Chunk data plus its trailing CRLF.
        let copied = tokio::io::copy(&mut (&mut *reader).take(size + 2), writer).await?;
        if copied < size + 2 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
}

/// Forwards client requests to the engine until the client stops sending,
/// rewriting Windows bind mount sources of `containers/create`. After an
/// upgrade (attach, exec, BuildKit session) or anything it cannot parse, the
/// rest of the stream is copied verbatim.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) async fn forward_requests<R, W>(reader: R, writer: &mut W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    loop {
        let Some(head) = read_head(&mut reader).await? else {
            return Ok(());
        };
        let Some(request) = RequestHead::parse(&head) else {
            writer.write_all(&head).await?;
            tokio::io::copy(&mut reader, writer).await?;
            return Ok(());
        };
        match request.body {
            Body::Length(length) if request.container_create && length <= MAX_CREATE_BODY => {
                let mut body = vec![0; length as usize];
                reader.read_exact(&mut body).await?;
                match rewrite_create_body(&body) {
                    Some(rewritten) => {
                        writer
                            .write_all(&with_content_length(&head, rewritten.len()))
                            .await?;
                        writer.write_all(&rewritten).await?;
                    }
                    None => {
                        writer.write_all(&head).await?;
                        writer.write_all(&body).await?;
                    }
                }
            }
            Body::Length(length) => {
                writer.write_all(&head).await?;
                let copied = tokio::io::copy(&mut (&mut reader).take(length), writer).await?;
                if copied < length {
                    return Ok(());
                }
            }
            Body::Chunked => {
                writer.write_all(&head).await?;
                copy_chunked(&mut reader, writer).await?;
            }
            Body::None => writer.write_all(&head).await?,
        }
        writer.flush().await?;
        if request.upgrade {
            tokio::io::copy(&mut reader, writer).await?;
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_drive_paths_only() {
        assert_eq!(
            to_wsl_path(r"C:\Users\Me\src").as_deref(),
            Some("/mnt/c/Users/Me/src")
        );
        assert_eq!(to_wsl_path("d:/data/").as_deref(), Some("/mnt/d/data"));
        assert_eq!(to_wsl_path("E:").as_deref(), Some("/mnt/e"));
        for unchanged in [
            "my-volume",
            "/mnt/c/src",
            "/var/run/docker.sock",
            r"\\server\share",
            "C:rel",
        ] {
            assert_eq!(to_wsl_path(unchanged), None, "{unchanged}");
        }
    }

    #[test]
    fn rewrites_binds_and_bind_mounts() {
        let body = serde_json::json!({
            "Image": "alpine",
            "HostConfig": {
                "Binds": [r"C:\Users\Me\src:/src:ro", "cache:/cache", "/mnt/d/x:/x"],
                "Mounts": [
                    {"Type": "bind", "Source": "D:/data", "Target": "/data"},
                    {"Type": "volume", "Source": "named", "Target": "/v"}
                ]
            }
        });
        let rewritten = rewrite_create_body(body.to_string().as_bytes()).expect("应改写");
        let value: Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(
            value["HostConfig"]["Binds"][0],
            "/mnt/c/Users/Me/src:/src:ro"
        );
        assert_eq!(value["HostConfig"]["Binds"][1], "cache:/cache");
        assert_eq!(value["HostConfig"]["Binds"][2], "/mnt/d/x:/x");
        assert_eq!(value["HostConfig"]["Mounts"][0]["Source"], "/mnt/d/data");
        assert_eq!(value["HostConfig"]["Mounts"][1]["Source"], "named");

        let untouched = serde_json::json!({"HostConfig": {"Binds": ["cache:/cache"]}});
        assert_eq!(rewrite_create_body(untouched.to_string().as_bytes()), None);
        assert_eq!(rewrite_create_body(b"{broken"), None);
    }

    #[test]
    fn rewrites_create_requests_and_passes_others_through() {
        let body = serde_json::json!({"HostConfig": {"Binds": [r"C:\a:/a"]}}).to_string();
        let mut input = Vec::new();
        input.extend_from_slice(b"GET /_ping HTTP/1.1\r\nHost: api.moby.localhost\r\n\r\n");
        input.extend_from_slice(
            format!(
                "POST /v1.47/containers/create?name=x HTTP/1.1\r\nHost: docker\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        input.extend_from_slice(
            b"POST /v1.47/build HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        );
        input.extend_from_slice(
            b"POST /v1.47/containers/x/attach HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\nraw C:\\stdin",
        );
        let mut output = Vec::new();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(forward_requests(&input[..], &mut output))
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        let expected_body = r#"{"HostConfig":{"Binds":["/mnt/c/a:/a"]}}"#;
        assert!(output.starts_with("GET /_ping HTTP/1.1\r\n"));
        assert!(output.contains(&format!(
            "Content-Length: {}\r\n\r\n{expected_body}POST /v1.47/build",
            expected_body.len()
        )));
        assert!(output.contains("3\r\nabc\r\n0\r\n\r\nPOST /v1.47/containers/x/attach"));
        assert!(output.ends_with("Upgrade: tcp\r\n\r\nraw C:\\stdin"));
    }
}
