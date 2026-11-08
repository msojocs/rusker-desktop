//! Windows named pipe -> WSL2 relay, the counterpart of Docker Desktop's
//! `\\.\pipe\docker_engine` proxy.
//!
//! One persistent multiplexed helper session serves all pipe clients. No TCP
//! listener, per-client WSL command, or dependency on WSL networking mode.

use std::{io, process::Stdio, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncWriteExt, ReadHalf},
    net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions},
    sync::Mutex,
};

use super::{
    docker_api_transport::{Session, Stream},
    engine_proxy::install_vm_proxy,
    path_translation::forward_requests,
    wsl::{managed_command, WslHost, MANAGED_PIPE},
};

#[derive(Default)]
struct Transport(Mutex<Option<Session>>);

impl Transport {
    async fn connect(&self, host: &Arc<WslHost>) -> io::Result<Stream> {
        let session = {
            let _span = crate::diagnostics::PerfSpan::new("docker_api.session_connect");
            let mut current = self.0.lock().await;
            if current.as_ref().is_some_and(Session::is_closed) {
                *current = None;
            }
            if current.is_none() {
                let path = install_vm_proxy().await.map_err(io::Error::other)?;
                let mut command = managed_command("root");
                command
                    .arg(path)
                    .arg("--docker-api")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                let mut child = command.spawn()?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("helper stdin missing"))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| io::Error::other("helper stdout missing"))?;
                let session = Session::start(stdout, stdin).await?;
                let monitor = session.clone();
                let host = host.clone();
                tokio::spawn(async move {
                    let mut check = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            _ = child.wait() => break,
                            _ = monitor.closed() => break,
                            _ = check.tick() => { if !host.session_running() { break; } }
                        }
                    }
                    monitor.close();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                });
                *current = Some(session);
            }
            current.as_ref().unwrap().clone()
        };
        // Failure invalidates this session via its dispatcher. Never replay an
        // OPEN/request on a new session: the next pipe client reconnects.
        session.open().await
    }
}

/// Accepts pipe clients until the runtime shuts down. Failure to create the
/// first instance (for example another Rusker instance already owns the pipe)
/// is reported through the host's proxy status instead of silently retrying
/// forever.
pub(super) async fn serve(host: Arc<WslHost>, lock: Arc<Mutex<()>>) {
    let status = host.proxy_status();
    let mut server = match create(true) {
        Ok(server) => server,
        Err(error) => {
            status.set_error(format!(
                "无法创建 {MANAGED_PIPE}（可能已有另一个 Rusker 实例在运行）：{error}"
            ));
            return;
        }
    };
    status.clear();
    let transport = Arc::new(Transport::default());
    loop {
        if server.connect().await.is_err() {
            // A client that connected and vanished before we accepted it; drop
            // this instance and keep listening.
            tokio::time::sleep(Duration::from_millis(50)).await;
            match create(false) {
                Ok(next) => server = next,
                Err(error) => {
                    status.set_error(format!("重建 {MANAGED_PIPE} 失败：{error}"));
                    return;
                }
            }
            continue;
        }
        // Create the next instance before serving, so clients never see
        // ERROR_FILE_NOT_FOUND between connections.
        let next = match create(false) {
            Ok(next) => next,
            Err(error) => {
                status.set_error(format!("重建 {MANAGED_PIPE} 失败：{error}"));
                return;
            }
        };
        let client = std::mem::replace(&mut server, next);
        let host = host.clone();
        let lock = lock.clone();
        let transport = transport.clone();
        tokio::spawn(async move {
            if let Err(error) = relay(client, &host, &lock, &transport).await {
                host.proxy_status()
                    .set_error(format!("Docker API 转发失败：{error}"));
            }
        });
    }
}

fn create(first: bool) -> io::Result<NamedPipeServer> {
    // Message mode matches dockerd's own Windows listener: go-winio clients
    // (the Docker CLI) then support CloseWrite via a zero-length message,
    // which `docker exec -i` / `attach` rely on to signal stdin EOF.
    ServerOptions::new()
        .first_pipe_instance(first)
        .pipe_mode(PipeMode::Message)
        .reject_remote_clients(true)
        .create(MANAGED_PIPE)
}

async fn relay(
    pipe: NamedPipeServer,
    host: &Arc<WslHost>,
    lock: &Mutex<()>,
    transport: &Transport,
) -> io::Result<()> {
    let _span = crate::diagnostics::PerfSpan::new("docker_api.pipe_relay");
    let saver = host.resource_saver();
    // Counted for the whole connection so Resource Saver never stops an
    // engine that a client (pull, logs -f, attach…) is still talking to.
    let _connection = saver.track_connection();
    // Always join the lifecycle lock, including when a supervisor exists:
    // its daemon may still be starting or frozen by Resource Saver.
    wake(host, lock).await?;
    let stream = transport.connect(host).await?;
    let (mut stdout, stdin) = tokio::io::split(stream);
    let (pipe_read, mut pipe_write) = tokio::io::split(pipe);

    let downstream = tokio::io::copy(&mut stdout, &mut pipe_write);
    let upstream = forward_until_eof(pipe_read, stdin);
    // The connection ends when the daemon side ends (or a write to the pipe
    // fails because the client went away). Client EOF only half-closes.
    tokio::select! {
        result = downstream => { result?; }
        result = upstream => { result?; }
    }
    let _ = pipe_write.flush().await;
    Ok(())
}

/// Hold the original connection until the engine is ready. Do not retry or
/// replay mutations: no request bytes have been forwarded yet.
async fn wake(host: &WslHost, lock: &Mutex<()>) -> io::Result<()> {
    let guard = {
        let _span = crate::diagnostics::PerfSpan::new("docker_api.wake_lock");
        lock.lock().await
    };
    let _guard = guard;
    let _span = crate::diagnostics::PerfSpan::new("docker_api.wake");
    let result = if host.resource_saver().is_active() {
        host.resume_resource_saver().await
    } else if host.engine_lost() {
        host.recover(1).await
    } else if host.session_running() {
        Ok(())
    } else {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Engine 已手动停止",
        ));
    };
    result.map_err(|error| {
        host.proxy_status().set_error(error.to_string());
        io::Error::other(error.to_string())
    })
}

async fn forward_until_eof(
    pipe_read: ReadHalf<NamedPipeServer>,
    mut stdin: tokio::io::WriteHalf<Stream>,
) -> io::Result<()> {
    // Ok(0) is CloseWrite or disconnect. EOF half-closes only the helper's
    // Unix socket write side; output from exec/attach must still drain.
    forward_requests(pipe_read, &mut stdin).await?;
    stdin.shutdown().await?;
    std::future::pending::<io::Result<()>>().await
}

#[cfg(test)]
#[path = "proxy_transport_tests.rs"]
mod transport_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Explicit opt-in only. Does not bind the shared Windows pipe, change
    /// containers, start/stop dockerd, or exercise wake/install lifecycle.
    /// It does install this build's embedded helper when missing.
    #[tokio::test]
    #[ignore = "requires RUSKER_DOCKER_INTEGRATION=1 and an already running managed engine"]
    async fn docker_api_live_ping() {
        assert_eq!(
            std::env::var("RUSKER_DOCKER_INTEGRATION").as_deref(),
            Ok("1")
        );
        let path = install_vm_proxy().await.unwrap();
        let mut child = managed_command("root")
            .arg(path)
            .arg("--docker-api")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let session = Session::start(child.stdout.take().unwrap(), child.stdin.take().unwrap())
            .await
            .unwrap();
        async fn ping(session: Session, half_close: bool) {
            let start = std::time::Instant::now();
            let mut stream = session.open().await.unwrap();
            stream
                .write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            if half_close {
                stream.shutdown().await.unwrap();
            }
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            assert!(
                response.starts_with(b"HTTP/1.1 200"),
                "{}",
                String::from_utf8_lossy(&response)
            );
            assert!(response.ends_with(b"OK"));
            eprintln!(
                "multiplexed /_ping (half_close={half_close}): {:?}",
                start.elapsed()
            );
        }
        ping(session.clone(), false).await;
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..8 {
            tasks.spawn(ping(session.clone(), index % 2 == 0));
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        session.close();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
}
