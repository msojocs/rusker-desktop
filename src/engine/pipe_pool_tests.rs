use futures_util::StreamExt;
use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::ServerOptions,
    time::timeout,
};

/// Real Windows named pipes, but no real Docker or WSL resources.
#[tokio::test]
async fn normal_requests_reuse_one_named_pipe_and_idle_pool_closes() {
    let path = format!(r"\\.\pipe\rusker-test-pool-{}", std::process::id());
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&path)
        .unwrap();
    let docker = super::pooled_named_pipe(&path).unwrap();
    let server_task = tokio::spawn(async move {
        server.connect().await.unwrap();
        // If pooling were disabled, request 2 would try opening a different
        // pipe and this test would time out instead of receiving it here.
        for _ in 0..2 {
            let mut head = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                assert_eq!(server.read(&mut byte).await.unwrap(), 1);
                head.push(byte[0]);
                assert!(head.len() < 8192);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&head).contains("/_ping"));
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .await
                .unwrap();
        }
        // Holding `docker` must not leave idle pipe clients permanently counted
        // by Resource Saver. Hyper drops only the idle connection after 5 s.
        let mut byte = [0u8; 1];
        let result = timeout(Duration::from_secs(9), server.read(&mut byte))
            .await
            .expect("idle connection must be closed");
        match result {
            Ok(count) => assert_eq!(count, 0),
            Err(error) => assert!(matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            )),
        }
    });
    for _ in 0..2 {
        assert_eq!(
            timeout(Duration::from_secs(3), docker.ping())
                .await
                .expect("ping must reuse the existing pipe")
                .unwrap(),
            "OK"
        );
    }
    server_task.await.unwrap();
}

#[tokio::test]
async fn pooled_transport_preserves_exec_upgrade_and_half_close() {
    let path = format!(r"\\.\pipe\rusker-test-pool-exec-{}", std::process::id());
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&path)
        .unwrap();
    let docker = super::pooled_named_pipe(&path).unwrap();
    let server_task = tokio::spawn(async move {
        server.connect().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(server.read_u8().await.unwrap());
        }
        let text = String::from_utf8(head).unwrap();
        assert!(text.contains("/exec/test/start"));
        let length = text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        server.read_exact(&mut body).await.unwrap();
        server
            .write_all(b"HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
            .await
            .unwrap();
        let mut input = [0; 4];
        server.read_exact(&mut input).await.unwrap();
        assert_eq!(&input, b"ping");
        // tty=false Docker output has an 8-byte stream header.
        server.write_all(&[1, 0, 0, 0, 0, 0, 0, 4]).await.unwrap();
        server.write_all(b"pong").await.unwrap();
    });
    timeout(Duration::from_secs(5), async {
        let bollard::exec::StartExecResults::Attached {
            mut input,
            mut output,
        } = docker
            .start_exec("test", None::<bollard::exec::StartExecOptions>)
            .await
            .unwrap()
        else {
            panic!("expected upgraded stream");
        };
        input.write_all(b"ping").await.unwrap();
        input.shutdown().await.unwrap();
        let frame = output.next().await.unwrap().unwrap();
        assert_eq!(frame.into_bytes().as_ref(), b"pong");
        assert!(output.next().await.is_none());
        server_task.await.unwrap();
    })
    .await
    .expect("upgraded exec stream must finish");
}
