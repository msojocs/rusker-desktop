//! Opt-in full Windows pipe -> HTTP path translator -> multiplexed WSL helper
//! -> isolated Unix socket probe. Does not touch the managed engine or public
//! pipe. Needs Python 3 in the explicitly selected test distribution.
use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::windows::named_pipe::ClientOptions,
};

const MOCK_SERVER: &str = r#"
import os, socket, tempfile
with tempfile.TemporaryDirectory(prefix='rusker-transport-test-') as directory:
    path = os.path.join(directory, 'docker.sock')
    server = socket.socket(socket.AF_UNIX)
    server.bind(path)
    server.listen(16)
    print(path, flush=True)
    for _ in range(3):
        client, _ = server.accept()
        try:
            request = b''
            while b'\r\n\r\n' not in request:
                block = client.recv(4096)
                if not block: break
                request += block
            if request:
                client.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK')
        finally:
            client.close()
"#;

fn command(distro: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("wsl.exe");
    command
        .args(["--distribution", distro, "--user", "root", "--exec"])
        .kill_on_drop(true);
    command
}

#[tokio::test]
#[ignore = "requires explicit RUSKER_WSL_TRANSPORT_TEST_DISTRO with Python 3"]
async fn isolated_wsl_named_pipe_round_trips() {
    let distro = std::env::var("RUSKER_WSL_TRANSPORT_TEST_DISTRO").expect("test distro");
    // The mock exits after 3 accepts; TemporaryDirectory removes its socket.
    // The final kill below is only for the isolated helper.
    let mut mock = command(&distro)
        .args(["/usr/bin/python3", "-I", "-u", "-c", MOCK_SERVER])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(mock.stdout.take().unwrap()).lines();
    let socket = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(socket.starts_with("/tmp/rusker-transport-test-"));

    // Stream the embedded executable into memfd: no installation or persistent
    // helper files are left in the test distro (or the managed distro).
    let launcher = "import os,sys; fd=os.memfd_create('rusker-test-helper',0); data=sys.stdin.buffer.read(int(sys.argv[1])); os.write(fd,data); os.fchmod(fd,0o700); os.execv('/proc/self/fd/'+str(fd),['rusker-vm-proxy','--docker-api',sys.argv[2]])";
    let mut child = command(&distro)
        .args(["/usr/bin/python3", "-I", "-u", "-c", launcher])
        .arg(
            super::super::engine_proxy::embedded_helper_for_test()
                .len()
                .to_string(),
        )
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(super::super::engine_proxy::embedded_helper_for_test())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let cold = std::time::Instant::now();
    let session = Session::start(child.stdout.take().unwrap(), stdin)
        .await
        .unwrap();
    eprintln!("isolated WSL helper ready: {:?}", cold.elapsed());

    for index in 0..3 {
        let path = format!(r"\\.\pipe\rusker-test-wsl-{}-{index}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .unwrap();
        let relay_session = session.clone();
        let relay = tokio::spawn(async move {
            server.connect().await.unwrap();
            let stream = relay_session.open().await.unwrap();
            let (mut output, mut input) = tokio::io::split(stream);
            let (pipe_read, mut pipe_write) = tokio::io::split(server);
            tokio::select! {
                result = tokio::io::copy(&mut output, &mut pipe_write) => { result.unwrap(); }
                result = super::super::path_translation::forward_requests(pipe_read, &mut input) => { result.unwrap(); }
            }
        });
        let start = std::time::Instant::now();
        let mut pipe = ClientOptions::new().open(&path).unwrap();
        pipe.write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), pipe.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        assert!(response.ends_with(b"OK"));
        eprintln!(
            "isolated named pipe + WSL multiplexed ping #{index}: {:?}",
            start.elapsed()
        );
        relay.await.unwrap();
    }
    session.close();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), mock.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}
