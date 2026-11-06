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
};

use super::wsl::{dial_stdio_command, EngineSession, ProxyStatus, MANAGED_PIPE};

/// Accepts pipe clients until the runtime shuts down. Failure to create the
/// first instance (for example another Rusker instance already owns the pipe)
/// is reported through `status` instead of silently retrying forever.
pub(super) async fn serve(session: Arc<EngineSession>, status: Arc<ProxyStatus>) {
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
        let session = session.clone();
        tokio::spawn(async move {
            let _ = relay(client, &session).await;
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

async fn relay(pipe: NamedPipeServer, session: &EngineSession) -> io::Result<()> {
    // Never boot the distribution from a stray client: only relay while the
    // supervisor session that owns dockerd is alive.
    if !session.is_running() {
        return Ok(());
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

async fn forward_until_eof(mut pipe_read: ReadHalf<NamedPipeServer>, mut stdin: ChildStdin) {
    // Ok(0) is either a CloseWrite (zero-length message) or a disconnected
    // client; both mean "no more request bytes". dial-stdio turns stdin EOF
    // into a socket half-close.
    let _ = tokio::io::copy(&mut pipe_read, &mut stdin).await;
    let _ = stdin.shutdown().await;
    drop(stdin);
    std::future::pending::<()>().await
}
