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

/// Opt-in: saving proxy settings takes effect in the managed dockerd directly
/// (daemon.json merge + restart), keeps the NVIDIA runtime config, and an
/// unchanged config does not restart the engine. Restores "no proxy" at the end.
#[cfg(windows)]
#[test]
fn applies_engine_proxy_directly() {
    if std::env::var("RUSKER_WSL_RUNTIME_LIFECYCLE").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_WSL_RUNTIME_LIFECYCLE=1 后验证代理直接生效");
        return;
    }
    use rusker_desktop::{
        runtime_host::{ProxyApplyOutcome, MANAGED_PIPE},
        settings::DockerProxySettings,
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
            .block_on(service.apply_proxy(settings, BTreeMap::new()))
            .expect("Tokio apply task")
            .expect("应用代理")
            .0
    };

    let enabled = DockerProxySettings {
        enabled: true,
        http_proxy: proxy_url.clone(),
        https_proxy: proxy_url.clone(),
        no_proxy: "localhost,127.0.0.1,::1".into(),
    };
    let first = apply(enabled.clone());
    let (http_proxy, nvidia) = engine_proxy(&client_runtime);
    eprintln!("首次应用：{first:?}；Engine HTTPProxy={http_proxy}；nvidia runtime={nvidia}");
    assert!(http_proxy.starts_with(proxy_url.trim_end_matches('/')));
    assert_eq!(apply(enabled), ProxyApplyOutcome::Unchanged);

    if let Ok(image) = std::env::var("RUSKER_TEST_PULL") {
        let (ok, text) = wsl_output("rusker", &format!("docker pull -q {image}"), None);
        eprintln!("经代理拉取 {image}：{ok} {}", text.trim());
    }

    assert_eq!(
        apply(DockerProxySettings::default()),
        ProxyApplyOutcome::Restarted
    );
    let (http_proxy, nvidia_after) = engine_proxy(&client_runtime);
    assert!(http_proxy.is_empty(), "关闭后仍有代理：{http_proxy}");
    assert_eq!(nvidia, nvidia_after, "合并 daemon.json 不应影响 runtimes");

    service.shutdown_blocking(Duration::from_secs(50));
    drop(service);
}
