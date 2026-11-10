use std::{collections::BTreeMap, time::Duration};

use rusker_desktop::runtime_host::RuntimeHostService;

/// Opt-in destructive test for the dedicated `rusker` WSL distribution only.
/// It never enables integration in another distribution and leaves the managed
/// environment installed for subsequent manual validation.
#[tokio::test]
async fn installs_and_starts_dedicated_wsl_runtime() {
    if std::env::var("RUSKER_WSL_RUNTIME_INTEGRATION").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_RUNTIME_INTEGRATION=1 后安装专用 WSL2 环境");
        return;
    }

    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let snapshot = service
        .install(BTreeMap::new())
        .await
        .expect("Tokio install task")
        .expect("安装并启动专用 WSL2 环境");
    assert!(snapshot.installed);
    assert!(snapshot.running);
    assert!(snapshot.engine_ready);
    eprintln!(
        "受管环境已就绪；NVIDIA：{}",
        if snapshot.gpu.detail.is_empty() {
            "未返回状态"
        } else {
            &snapshot.gpu.detail
        }
    );
    tokio::task::spawn_blocking(move || {
        service.shutdown_blocking(Duration::from_secs(50));
        drop(service);
    })
    .await
    .expect("在 blocking 线程释放 runtime-host");
}

/// Opt-in first-launch test. Requires a missing managed distribution and a
/// bundled system image; never removes an existing environment.
#[cfg(windows)]
#[test]
fn startup_installs_missing_wsl_runtime() {
    if std::env::var("RUSKER_WSL_RUNTIME_FIRST_LAUNCH").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_RUNTIME_FIRST_LAUNCH=1 后验证首次启动自动安装");
        return;
    }

    let client_runtime = tokio::runtime::Runtime::new().expect("测试运行时");
    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let before = client_runtime
        .block_on(service.snapshot(BTreeMap::new()))
        .expect("Tokio snapshot task")
        .expect("读取初始状态");
    assert!(
        !before.installed,
        "需要尚未安装 rusker 发行版；测试不会移除已有环境"
    );

    let result = client_runtime.block_on(service.startup(BTreeMap::new()));
    service.shutdown_blocking(Duration::from_secs(50));
    let snapshot = result
        .expect("Tokio startup task")
        .expect("首次启动自动安装专用 WSL2 环境");
    assert!(snapshot.installed);
    assert!(snapshot.running);
    assert!(snapshot.engine_ready, "warnings: {:?}", snapshot.warnings);
}

/// Opt-in lifecycle test against an already installed `rusker` distribution:
/// launch-time auto start, Docker API through `\\.\pipe\rusker_engine`, and
/// graceful shutdown. Set `RUSKER_WSL_HOLD_SECS` to keep the engine up for
/// manual `docker -H npipe:////./pipe/rusker_engine ...` checks.
#[cfg(windows)]
#[test]
fn serves_managed_engine_over_named_pipe() {
    if std::env::var("RUSKER_WSL_RUNTIME_LIFECYCLE").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_RUNTIME_LIFECYCLE=1 后启动已安装的专用 WSL2 环境");
        return;
    }
    use rusker_desktop::runtime_host::MANAGED_PIPE;

    let client_runtime = tokio::runtime::Runtime::new().expect("测试运行时");
    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let snapshot = client_runtime
        .block_on(service.startup(BTreeMap::new()))
        .expect("Tokio startup task")
        .expect("启动专用 WSL2 环境");
    assert!(snapshot.installed, "需要先安装 rusker 发行版");
    assert!(snapshot.engine_ready, "warnings: {:?}", snapshot.warnings);

    client_runtime.block_on(async {
        let docker = bollard::Docker::connect_with_named_pipe(
            MANAGED_PIPE,
            30,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("named pipe 客户端");
        let version = docker.version().await.expect("Docker version");
        assert_eq!(version.os.as_deref(), Some("linux"));
        // Several requests in parallel exercise multiple pipe instances.
        let (info, containers, images) = tokio::join!(
            docker.info(),
            docker.list_containers(None::<bollard::query_parameters::ListContainersOptions>),
            docker.list_images(None::<bollard::query_parameters::ListImagesOptions>),
        );
        info.expect("info");
        containers.expect("containers");
        images.expect("images");
        eprintln!("通过 {MANAGED_PIPE} 连接 Engine {:?}", version.version);
    });

    if let Some(seconds) = std::env::var("RUSKER_WSL_HOLD_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        eprintln!("保持 Engine 运行 {seconds} 秒以便手动验证");
        std::thread::sleep(Duration::from_secs(seconds));
    }

    service.shutdown_blocking(Duration::from_secs(50));
    let snapshot = client_runtime
        .block_on(service.snapshot(BTreeMap::new()))
        .expect("Tokio snapshot task")
        .expect("读取状态");
    assert!(!snapshot.engine_ready);
    client_runtime.block_on(async {
        let docker =
            bollard::Docker::connect_with_named_pipe(MANAGED_PIPE, 5, bollard::API_DEFAULT_VERSION)
                .expect("named pipe 客户端");
        assert!(docker.ping().await.is_err(), "关闭后不应继续转发");
    });
    drop(service);
}

#[cfg(windows)]
fn wsl_output(distribution: &str, script: &str, stdin: Option<&[u8]>) -> (bool, String) {
    use std::{io::Write, process::Stdio};
    let mut child = std::process::Command::new("wsl.exe")
        .args([
            "--distribution",
            distribution,
            "--exec",
            "/bin/sh",
            "-c",
            script,
        ])
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 wsl.exe");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("写入 stdin");
    }
    let output = child.wait_with_output().expect("等待 wsl.exe");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// Opt-in: enables integration in `RUSKER_WSL_INTEGRATION_DISTRO`, checks the
/// CLI and Compose there, checks `docker run -i` stdin EOF through the Windows
/// pipe (needs `docker.exe` on PATH), then disables the integration again.
#[cfg(windows)]
#[test]
fn integrates_distribution_and_relays_stdin_eof() {
    let Ok(distribution) = std::env::var("RUSKER_WSL_INTEGRATION_DISTRO") else {
        eprintln!("跳过：设置 RUSKER_WSL_INTEGRATION_DISTRO=<发行版> 后验证集成");
        return;
    };
    use rusker_desktop::runtime_host::MANAGED_DOCKER_HOST;

    let client_runtime = tokio::runtime::Runtime::new().expect("测试运行时");
    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let enabled = BTreeMap::from([(distribution.clone(), true)]);
    let snapshot = client_runtime
        .block_on(service.startup(BTreeMap::new()))
        .expect("Tokio startup task")
        .expect("启动专用 WSL2 环境");
    assert!(snapshot.engine_ready, "warnings: {:?}", snapshot.warnings);

    for round in 0..2 {
        // The second round exercises the fingerprint fast path.
        let started = std::time::Instant::now();
        client_runtime
            .block_on(service.set_integration(distribution.clone(), true, enabled.clone()))
            .expect("Tokio integration task")
            .expect("启用集成");
        eprintln!("第 {} 次启用集成耗时 {:?}", round + 1, started.elapsed());
    }
    let (ok, text) = wsl_output(
        &distribution,
        "/usr/local/bin/docker version --format '{{.Server.Os}}' && /usr/local/bin/docker compose version",
        None,
    );
    assert!(ok, "{text}");
    assert!(text.contains("linux"), "{text}");
    eprintln!("集成发行版内：{}", text.trim());

    // Build a tiny local image from the managed distro's static busybox so
    // no registry access is needed, then pipe stdin through `run -i`.
    let (ok, text) = wsl_output(
        "rusker",
        "set -e; d=$(mktemp -d); mkdir -p $d/bin; cp /bin/busybox $d/bin/; \
         ln -s busybox $d/bin/cat; tar -C $d -c . | docker import - rusker-test/busybox:local >/dev/null; \
         rm -rf $d; echo imported",
        None,
    );
    assert!(ok, "{text}");
    if let Ok(mut docker) = std::process::Command::new("docker.exe")
        .args([
            "-H",
            MANAGED_DOCKER_HOST,
            "run",
            "--rm",
            "-i",
            "rusker-test/busybox:local",
            "/bin/cat",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        use std::io::Write;
        docker
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"hello-through-pipe\n")
            .expect("写入");
        let output = docker.wait_with_output().expect("docker run -i");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello-through-pipe"
        );
        eprintln!("docker run -i 通过 named pipe 正确收到 stdin EOF");
    } else {
        eprintln!("未找到 docker.exe，跳过 Windows CLI stdin 验证");
    }
    let _ = wsl_output("rusker", "docker rmi rusker-test/busybox:local", None);

    client_runtime
        .block_on(service.set_integration(distribution.clone(), false, BTreeMap::new()))
        .expect("Tokio integration task")
        .expect("停用集成");
    let (_, text) = wsl_output(
        &distribution,
        "ls /usr/local/bin/docker /usr/local/lib/rusker \
         /usr/local/lib/docker/cli-plugins/docker-compose 2>&1 || true",
        None,
    );
    assert!(!text.contains("rusker/"), "停用后残留：{text}");
    assert_eq!(
        text.matches("No such file").count() + text.matches("没有那个文件").count(),
        3,
        "{text}"
    );

    service.shutdown_blocking(Duration::from_secs(50));
    drop(service);
}

/// Opt-in: like Docker Desktop, dockerd always uses the in-VM relay and the
/// Windows side applies the proxy settings per request, so a proxy on the
/// Windows loopback works in NAT mode too and changing proxy settings never
/// restarts dockerd. Keeps the NVIDIA runtime config; with `RUSKER_TEST_PULL`
/// an image is pulled through it. Restores "no proxy" at the end.
#[cfg(windows)]
#[test]
fn applies_engine_proxy_directly() {
    if std::env::var("RUSKER_WSL_RUNTIME_LIFECYCLE").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_RUNTIME_LIFECYCLE=1 后验证代理直接生效");
        return;
    }
    use rusker_desktop::{
        runtime_host::{ProxyApplyOutcome, ENGINE_PROXY_URL, MANAGED_PIPE},
        settings::{DockerProxySettings, NetworkSettings, ProxyMode},
    };

    let proxy_url =
        std::env::var("RUSKER_TEST_PROXY").unwrap_or_else(|_| "http://127.0.0.1:7890".into());
    let client_runtime = tokio::runtime::Runtime::new().expect("测试运行时");
    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let snapshot = client_runtime
        .block_on(service.startup(BTreeMap::new()))
        .expect("Tokio startup task")
        .expect("启动专用 WSL2 环境");
    assert!(snapshot.engine_ready, "warnings: {:?}", snapshot.warnings);

    let engine_proxy = |runtime: &tokio::runtime::Runtime| {
        runtime.block_on(async {
            let docker = bollard::Docker::connect_with_named_pipe(
                MANAGED_PIPE,
                30,
                bollard::API_DEFAULT_VERSION,
            )
            .expect("named pipe 客户端");
            let info = docker.info().await.expect("docker info");
            (
                info.http_proxy.unwrap_or_default(),
                info.runtimes.unwrap_or_default().contains_key("nvidia"),
            )
        })
    };
    let apply = |settings: DockerProxySettings| {
        client_runtime
            .block_on(service.apply_engine_config(
                settings,
                NetworkSettings::default(),
                None,
                BTreeMap::new(),
            ))
            .expect("Tokio apply task")
            .expect("应用代理")
            .0
    };

    let enabled = DockerProxySettings {
        mode: ProxyMode::Manual,
        http_proxy: proxy_url.clone(),
        https_proxy: proxy_url.clone(),
        no_proxy: "localhost,127.0.0.1,::1".into(),
    };
    let first = apply(enabled.clone());
    let (http_proxy, nvidia) = engine_proxy(&client_runtime);
    eprintln!("首次应用：{first:?}；Engine HTTPProxy={http_proxy}；nvidia runtime={nvidia}");
    assert_eq!(http_proxy, ENGINE_PROXY_URL, "dockerd 应使用 VM 内转发地址");
    assert_eq!(apply(enabled.clone()), ProxyApplyOutcome::Unchanged);

    if let Ok(image) = std::env::var("RUSKER_TEST_PULL") {
        let (ok, text) = wsl_output(
            "rusker",
            &format!("docker rmi -f {image} >/dev/null 2>&1; docker pull -q {image}"),
            None,
        );
        eprintln!("经代理拉取 {image}：{ok} {}", text.trim());
        assert!(ok, "经 Engine 代理转发拉取 {image} 失败：{text}");

        // Only the Windows-side upstream changes: no restart, and the pull
        // now fails inside the relay, proving dockerd goes through it.
        let dead = DockerProxySettings {
            http_proxy: "http://127.0.0.1:1".into(),
            https_proxy: "http://127.0.0.1:1".into(),
            ..enabled.clone()
        };
        assert_eq!(apply(dead), ProxyApplyOutcome::Unchanged);
        let (ok, text) = wsl_output(
            "rusker",
            &format!("docker rmi -f {image} >/dev/null 2>&1; docker pull -q {image}"),
            None,
        );
        eprintln!("上游不可用时拉取：{ok} {}", text.trim());
        assert!(!ok && text.contains("Bad Gateway"), "{text}");
        assert_eq!(apply(enabled.clone()), ProxyApplyOutcome::Unchanged);
    }

    // Plain HTTP through the relay: the Windows side parses the request and
    // sends it to the HTTP upstream.
    let (ok, text) = wsl_output(
        "rusker",
        &format!(
            "curl -sS -o /dev/null -w '%{{http_code}}' --max-time 30 -x {ENGINE_PROXY_URL} http://example.com/"
        ),
        None,
    );
    eprintln!("经转发访问 http://example.com/：{ok} {}", text.trim());
    assert!(ok && text.trim() == "200", "{text}");

    // Like Docker Desktop, turning the proxy off only changes the Windows
    // side: dockerd keeps the relay and is not restarted.
    assert_eq!(
        apply(DockerProxySettings::default()),
        ProxyApplyOutcome::Unchanged
    );
    let (http_proxy, nvidia_after) = engine_proxy(&client_runtime);
    assert_eq!(http_proxy, ENGINE_PROXY_URL);
    assert_eq!(nvidia, nvidia_after, "合并 daemon.json 不应影响 runtimes");

    service.shutdown_blocking(Duration::from_secs(50));
    drop(service);
}

/// Opt-in benchmark of the Engine proxy relay. A Windows HTTP server sends
/// `RUSKER_BENCH_MIB` (default 1024) MiB from memory; `curl` in `rusker`
/// downloads it through the relay (plain HTTP, a CONNECT tunnel, three
/// tunnels in parallel like a layer pull) and, as baselines, over the raw
/// `wsl.exe` stdio pipe and directly (mirrored networking only).
#[cfg(windows)]
#[test]
fn benchmarks_engine_proxy_relay() {
    if std::env::var("RUSKER_WSL_PROXY_BENCH").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_PROXY_BENCH=1 后测量 Engine 代理转发吞吐");
        return;
    }
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };

    use rusker_desktop::runtime_host::ENGINE_PROXY_URL;

    let mib: u64 = std::env::var("RUSKER_BENCH_MIB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);
    let size = mib * 1024 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").expect("监听");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let length = if head.starts_with(b"GET /small") {
                    2
                } else {
                    size
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                let chunk = vec![0u8; 1024 * 1024];
                let mut left = length;
                while left > 0 {
                    let part = left.min(chunk.len() as u64) as usize;
                    if stream.write_all(&chunk[..part]).is_err() {
                        return;
                    }
                    left -= part as u64;
                }
            });
        }
    });

    let client_runtime = tokio::runtime::Runtime::new().expect("测试运行时");
    let service = RuntimeHostService::new().expect("创建 runtime-host");
    let snapshot = client_runtime
        .block_on(service.startup(BTreeMap::new()))
        .expect("Tokio startup task")
        .expect("启动专用 WSL2 环境");
    assert!(snapshot.engine_ready, "warnings: {:?}", snapshot.warnings);

    // The relay starts within a poll interval of the engine session.
    let probe = format!(
        "curl -sS -o /dev/null -w '%{{http_code}}' --max-time 5 -x {ENGINE_PROXY_URL} http://127.0.0.1:{port}/small"
    );
    let ready = (0..30).any(|_| {
        let (ok, text) = wsl_output("rusker", &probe, None);
        ok && text.trim() == "200" || {
            std::thread::sleep(Duration::from_secs(1));
            false
        }
    });
    assert!(ready, "Engine 代理转发未就绪");

    let report = |label: &str, bytes: u64, seconds: f64| {
        eprintln!(
            "{label:<28} {:>8.1} MiB/s（{:.2} 秒）",
            bytes as f64 / 1024.0 / 1024.0 / seconds,
            seconds
        );
    };
    let timed = |label: &str, script: &str, bytes: u64| {
        let started = Instant::now();
        let (ok, text) = wsl_output("rusker", script, None);
        let seconds = started.elapsed().as_secs_f64();
        if ok {
            report(label, bytes, seconds);
        } else {
            eprintln!("{label:<28} 失败：{}", text.trim());
        }
    };
    let url = format!("http://127.0.0.1:{port}/big");
    let curl = "curl -sS -o /dev/null --max-time 600";

    // Baseline: the wsl.exe stdio pipe the relay rides on.
    {
        let started = Instant::now();
        let mut child = std::process::Command::new("wsl.exe")
            .args(["--distribution", "rusker", "--exec", "head", "-c"])
            .arg(size.to_string())
            .arg("/dev/zero")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("wsl.exe");
        let mut stdout = child.stdout.take().unwrap();
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut total = 0u64;
        loop {
            let read = stdout.read(&mut buffer).expect("读取");
            if read == 0 {
                break;
            }
            total += read as u64;
        }
        let _ = child.wait();
        assert_eq!(total, size);
        report(
            "基线：wsl.exe stdio 管道",
            size,
            started.elapsed().as_secs_f64(),
        );
    }
    timed(
        "基线：直连（仅 mirrored）",
        &format!("{curl} --noproxy '*' {url}"),
        size,
    );
    timed(
        "转发：纯 HTTP",
        &format!("{curl} -x {ENGINE_PROXY_URL} {url}"),
        size,
    );
    timed(
        "转发：CONNECT 隧道",
        &format!("{curl} -p -x {ENGINE_PROXY_URL} {url}"),
        size,
    );
    timed(
        "转发：3 条隧道并发（合计）",
        &format!("for i in 1 2 3; do {curl} -p -x {ENGINE_PROXY_URL} {url} & done; wait"),
        size * 3,
    );
    let (_, cpu) = wsl_output(
        "rusker",
        "ps -o pid,etime,time,rss,args -C rusker-vm-proxy | cut -c1-100",
        None,
    );
    eprintln!("VM 内转发进程累计 CPU：\n{}", cpu.trim_end());

    service.shutdown_blocking(Duration::from_secs(50));
    drop(service);
}
