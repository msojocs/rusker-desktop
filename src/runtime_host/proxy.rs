//! Windows named pipe -> WSL2 relay, the counterpart of Docker Desktop's
//! `\\.\pipe\docker_engine` proxy.
//!
//! Each pipe client gets its own `docker system dial-stdio` process inside the
//! managed distribution, so no TCP port is opened and the connection does not
//! depend on the WSL networking mode (NAT or mirrored).

use std::{io, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncWriteExt, ReadHalf},
    net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions},
    process::ChildStdin,
    sync::Mutex,
};

use super::{
    path_translation::forward_requests,
    wsl::{dial_stdio_command, WslHost, MANAGED_PIPE},
};

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
        tokio::spawn(async move {
            let _ = relay(client, &host, &lock).await;
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

async fn relay(pipe: NamedPipeServer, host: &WslHost, lock: &Mutex<()>) -> io::Result<()> {
    let saver = host.resource_saver();
    // Counted for the whole connection so Resource Saver never stops an
    // engine that a client (pull, logs -f, attach…) is still talking to.
    let _connection = saver.track_connection();
    // Never boot the distribution from a stray client: only relay while the
    // supervisor session that owns dockerd is alive, except to leave
    // Resource Saver mode.
    if !host.session_running() {
        if !saver.is_active() {
            return Ok(());
        }
        wake(host, lock).await;
        if !host.session_running() {
            return Ok(());
        }
    }
    let mut child = dial_stdio_command().spawn()?;
    let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(io::Error::other("dial-stdio 缺少标准输入输出"));
    };
    let (pipe_read, mut pipe_write) = tokio::io::split(pipe);

    let downstream = tokio::io::copy(&mut stdout, &mut pipe_write);
    let upstream = forward_until_eof(pipe_read, stdin);
    // The connection ends when the daemon side ends (or a write to the pipe
    // fails because the client went away). Client EOF only half-closes.
    tokio::select! {
        result = downstream => { result?; }
        _ = upstream => unreachable!("upstream never completes"),
    }
    let _ = pipe_write.flush().await;
    Ok(())
}

/// Leaves Resource Saver mode. Concurrent clients queue on the operation
/// lock; the first one starts the engine and the rest find it running.
async fn wake(host: &WslHost, lock: &Mutex<()>) {
    let _guard = lock.lock().await;
    if host.session_running() || !host.resource_saver().is_active() {
        return;
    }
    if let Err(error) = host.start().await {
        host.proxy_status()
            .set_error(format!("退出资源节省模式失败：{error}"));
    }
}

async fn forward_until_eof(pipe_read: ReadHalf<NamedPipeServer>, mut stdin: ChildStdin) {
    // Ok(0) is either a CloseWrite (zero-length message) or a disconnected
    // client; both mean "no more request bytes". dial-stdio turns stdin EOF
    // into a socket half-close. Windows bind mount paths are translated on
    // the way (`C:\src` -> `/mnt/c/src`).
    let _ = forward_requests(pipe_read, &mut stdin).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    std::future::pending::<()>().await
}
