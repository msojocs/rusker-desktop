//! Explicit, non-destructive copies from desktop engines into the managed engine.

use super::*;
use bollard::models::ContainerInspectResponse;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DesktopImportSource {
    #[default]
    DockerDesktop,
    RancherDesktop,
}

impl DesktopImportSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::DockerDesktop => "Docker Desktop",
            Self::RancherDesktop => "Rancher Desktop",
        }
    }

    pub fn connection_hint(self) -> &'static str {
        match self {
            Self::DockerDesktop => "请先启动 Docker Desktop 并切换到 Linux 容器",
            Self::RancherDesktop => {
                "请先启动 Rancher Desktop 并选择 dockerd (moby)；不支持 containerd/nerdctl 后端"
            }
        }
    }

    #[cfg(any(windows, test))]
    fn pipe(self) -> &'static str {
        match self {
            Self::DockerDesktop => "npipe:////./pipe/dockerDesktopLinuxEngine",
            Self::RancherDesktop => "npipe:////./pipe/docker_engine",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DesktopContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub blocked_reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DesktopImportReport {
    pub imported: Vec<String>,
    pub failures: Vec<String>,
}

impl DockerEngine {
    async fn desktop_source(source: DesktopImportSource) -> Result<Self, EngineError> {
        #[cfg(windows)]
        {
            // Never consult DOCKER_HOST or the active Docker context.
            let client = pooled_named_pipe(source.pipe())?;
            let action = format!("连接 {}（{}）", source.label(), source.connection_hint());
            let client = with_timeout(&action, CONNECT_TIMEOUT, client.negotiate_version()).await?;
            if source == DesktopImportSource::RancherDesktop {
                // Rancher shares docker_engine with other products. Compare daemon IDs
                // against its own WSL socket before reading containers or copying data.
                let mut command = tokio::process::Command::new("wsl.exe");
                command.args([
                    "--distribution",
                    "rancher-desktop",
                    "--exec",
                    "docker",
                    "--host",
                    "unix:///var/run/docker.sock",
                    "info",
                    "--format",
                    "{{.ID}}",
                ]);
                command.kill_on_drop(true).creation_flags(0x08000000);
                let output = timeout(CONNECT_TIMEOUT, command.output())
                    .await
                    .map_err(|_| EngineError::Timeout {
                        action: "验证 Rancher Desktop 引擎身份".into(),
                        seconds: CONNECT_TIMEOUT.as_secs(),
                    })?
                    .map_err(|error| {
                        EngineError::operation("验证 Rancher Desktop 引擎身份", error)
                    })?;
                if !output.status.success() {
                    return Err(EngineError::operation(
                        "验证 Rancher Desktop 引擎身份",
                        format!(
                            "{}；{}",
                            source.connection_hint(),
                            String::from_utf8_lossy(&output.stderr).trim()
                        ),
                    ));
                }
                let info =
                    with_timeout("验证 Rancher Desktop 管道", REQUEST_TIMEOUT, client.info())
                        .await?;
                verify_rancher_identity(
                    info.id.as_deref(),
                    &String::from_utf8_lossy(&output.stdout),
                )?;
            }
            Ok(Self {
                client,
                managed_wsl: false,
            })
        }
        #[cfg(not(windows))]
        Err(EngineError::operation(
            format!("连接 {}", source.label()),
            "目前仅支持 Windows 的 Linux 容器后端",
        ))
    }

    pub(crate) async fn desktop_containers(
        source: DesktopImportSource,
    ) -> Result<Vec<DesktopContainer>, EngineError> {
        let source = Self::desktop_source(source).await?;
        let containers = with_timeout(
            "扫描源引擎容器",
            REQUEST_TIMEOUT,
            source.client.list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            )),
        )
        .await?;
        let mut entries = Vec::new();
        for container in containers {
            let dto = container_dto(container);
            let inspected = with_timeout(
                "读取源容器配置",
                REQUEST_TIMEOUT,
                source.client.inspect_container(&dto.id, None),
            )
            .await;
            let blocked_reason = match inspected {
                Ok(inspect) => validate_source(&inspect)
                    .err()
                    .map(|error| error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            entries.push(DesktopContainer {
                id: dto.id,
                name: dto.name,
                image: dto.image,
                state: dto.state,
                blocked_reason,
            });
        }
        entries.sort_by_cached_key(|entry| entry.name.to_lowercase());
        Ok(entries)
    }

    pub(crate) async fn import_desktop_containers(
        &self,
        source: DesktopImportSource,
        ids: Vec<String>,
        progress: watch::Sender<String>,
    ) -> Result<DesktopImportReport, EngineError> {
        if !self.managed_wsl {
            return Err(EngineError::operation(
                "导入桌面引擎容器",
                "目标必须是 Rusker 受管 Engine，请取消 DOCKER_HOST 覆盖",
            ));
        }
        let source = Self::desktop_source(source).await?;
        let (source_info, target_info) = tokio::try_join!(
            with_timeout("检查源引擎", REQUEST_TIMEOUT, source.client.info()),
            with_timeout("检查目标引擎", REQUEST_TIMEOUT, self.client.info()),
        )?;
        if source_info.id.is_none() || source_info.id == target_info.id {
            return Err(EngineError::operation(
                "导入桌面引擎容器",
                "源与目标引擎相同或无法确认源引擎身份",
            ));
        }
        // TempDir is private and removes archives (which can contain secrets) on every exit path.
        let scratch = tempfile::tempdir()
            .map_err(|error| EngineError::operation("准备导入临时目录", error))?;
        let batch = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut volumes = HashMap::new();
        let mut report = DesktopImportReport::default();
        let mut seen = std::collections::HashSet::new();
        for (index, id) in ids.iter().enumerate() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let _ = progress.send(format!(
                "正在导入 {}/{}：{}",
                index + 1,
                ids.len(),
                short_id(id)
            ));
            let mut created_volumes = Vec::new();
            let image = format!("rusker-import/{batch}-{index}:latest");
            let result = self
                .import_desktop_container(
                    &source,
                    id,
                    &image,
                    &batch,
                    scratch.path(),
                    &mut volumes,
                    &mut created_volumes,
                    &progress,
                )
                .await;
            match result {
                Ok(name) => report.imported.push(name),
                Err(error) => {
                    let mut detail = format!("{}：{error}", short_id(id));
                    // Only resources created by this attempt are eligible for cleanup.
                    for (original, target) in created_volumes {
                        volumes.remove(&original);
                        if let Err(error) = self.remove_volume(&target).await {
                            detail.push_str(&format!("；残留卷 {target} 清理失败：{error}"));
                        }
                    }
                    // An interrupted create may have succeeded remotely. Do not delete its image.
                    detail.push_str(&format!("；如传输中断，请检查目标容器及导入镜像 {image}"));
                    report.failures.push(detail);
                }
            }
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    async fn import_desktop_container(
        &self,
        source: &Self,
        id: &str,
        image: &str,
        batch: &str,
        scratch: &Path,
        volumes: &mut HashMap<String, String>,
        created_volumes: &mut Vec<(String, String)>,
        progress: &watch::Sender<String>,
    ) -> Result<String, EngineError> {
        let inspect = with_timeout(
            "读取源容器",
            REQUEST_TIMEOUT,
            source.client.inspect_container(id, None),
        )
        .await?;
        validate_source(&inspect)?;
        let name = inspect
            .name
            .as_deref()
            .unwrap_or("")
            .trim_start_matches('/');
        if name.is_empty() {
            return Err(EngineError::operation("导入容器", "源容器没有名称"));
        }
        match timeout(REQUEST_TIMEOUT, self.client.inspect_container(name, None)).await {
            Ok(Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            })) => {}
            Ok(Err(error)) => return Err(EngineError::operation("检查目标容器名称", error)),
            Ok(Ok(_)) => {
                return Err(EngineError::operation(
                    "导入容器",
                    format!("目标容器 {name} 已存在，不会覆盖"),
                ))
            }
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: "检查目标容器名称".into(),
                    seconds: REQUEST_TIMEOUT.as_secs(),
                })
            }
        }
        let mut body = create_body(&inspect, image)?;
        let mut mounts = body
            .host_config
            .as_mut()
            .unwrap()
            .mounts
            .take()
            .unwrap_or_default();
        for mount in inspect.mounts.as_deref().unwrap_or_default() {
            if mount.typ.as_deref() != Some("volume") {
                continue;
            }
            let original = mount.name.as_deref().unwrap();
            // Validate every time, even when another selected container shares the volume.
            source.ensure_volume_quiet(original).await?;
            let target = if let Some(target) = volumes.get(original) {
                target.clone()
            } else {
                let volume = with_timeout(
                    "检查源卷",
                    REQUEST_TIMEOUT,
                    source.client.inspect_volume(original),
                )
                .await?;
                if volume.driver != "local" || !volume.options.is_empty() {
                    return Err(EngineError::operation(
                        "导入卷",
                        format!("卷 {original} 不是普通本地卷，无法安全复制"),
                    ));
                }
                use sha2::{Digest as _, Sha256};
                let digest = format!("{:x}", Sha256::digest(original.as_bytes()));
                let target = format!("rusker-import-{batch}-{}", &digest[..16]);
                self.create_volume(Some(target.clone())).await?;
                created_volumes.push((original.to_string(), target.clone()));
                let archive = scratch.join("volume.tar");
                let _ = progress.send(format!("{name}：复制数据卷 {original}"));
                source.export_volume(original, &archive).await?;
                self.import_volume(&target, &archive).await?;
                tokio::fs::remove_file(&archive)
                    .await
                    .map_err(|error| EngineError::operation("清理临时卷归档", error))?;
                volumes.insert(original.to_string(), target.clone());
                target
            };
            mounts.push(volume_mount(
                &target,
                mount.destination.as_deref().unwrap(),
                !mount.rw.unwrap_or(true),
            ));
        }
        body.host_config.as_mut().unwrap().mounts = (!mounts.is_empty()).then_some(mounts);
        let _ = progress.send(format!("{name}：复制容器文件系统（不包含镜像历史）"));
        let archive = scratch.join("rootfs.tar");
        let mut file = tokio::fs::File::create(&archive)
            .await
            .map_err(|error| EngineError::operation("创建临时归档", error))?;
        let export = source.client.clone().with_timeout(DATA_TIMEOUT);
        let stream = export.export_container(id);
        pin_mut!(stream);
        let copy = async {
            while let Some(chunk) = stream
                .try_next()
                .await
                .map_err(|error| EngineError::operation("导出容器文件系统", error))?
            {
                file.write_all(&chunk)
                    .await
                    .map_err(|error| EngineError::operation("写入临时归档", error))?;
            }
            file.flush()
                .await
                .map_err(|error| EngineError::operation("写入临时归档", error))
        };
        timeout(DATA_TIMEOUT, copy)
            .await
            .map_err(|_| EngineError::Timeout {
                action: "导出容器文件系统".into(),
                seconds: DATA_TIMEOUT.as_secs(),
            })??;
        drop(file);
        let original_image = with_timeout(
            "读取源镜像平台",
            REQUEST_TIMEOUT,
            source
                .client
                .inspect_image(inspect.image.as_deref().unwrap_or_default()),
        )
        .await?;
        let platform = format!(
            "{}/{}",
            original_image.os.as_deref().unwrap_or("linux"),
            original_image.architecture.as_deref().unwrap_or("amd64")
        );
        let options = CreateImageOptionsBuilder::default()
            .from_src("-")
            .repo(image)
            .platform(&platform)
            .build();
        let file = tokio::fs::File::open(&archive)
            .await
            .map_err(|error| EngineError::operation("读取临时归档", error))?;
        let client = self.client.clone().with_timeout(DATA_TIMEOUT);
        let imported = client.create_image(
            Some(options),
            Some(body_try_stream(file_chunks(file))),
            None,
        );
        pin_mut!(imported);
        let load = async {
            while let Some(response) = imported
                .try_next()
                .await
                .map_err(|error| EngineError::operation("导入文件系统镜像", error))?
            {
                if let Some(error) = response.error_detail {
                    return Err(EngineError::operation(
                        "导入文件系统镜像",
                        error.message.unwrap_or_default(),
                    ));
                }
            }
            Ok(())
        };
        timeout(DATA_TIMEOUT, load)
            .await
            .map_err(|_| EngineError::Timeout {
                action: "导入文件系统镜像".into(),
                seconds: DATA_TIMEOUT.as_secs(),
            })??;
        tokio::fs::remove_file(&archive)
            .await
            .map_err(|error| EngineError::operation("清理临时容器归档", error))?;
        // Recheck after the long copy; a separately started source must not be reported as consistent.
        let current = with_timeout(
            "复核源容器状态",
            REQUEST_TIMEOUT,
            source.client.inspect_container(id, None),
        )
        .await?;
        validate_source(&current)?;
        for mount in inspect.mounts.as_deref().unwrap_or_default() {
            if let Some(volume) = mount
                .name
                .as_deref()
                .filter(|_| mount.typ.as_deref() == Some("volume"))
            {
                source.ensure_volume_quiet(volume).await?;
            }
        }
        let _ = progress.send(format!("{name}：创建目标容器（保持停止）"));
        let created = with_timeout(
            "创建导入容器",
            ACTION_TIMEOUT,
            self.client.create_container(
                Some(CreateContainerOptionsBuilder::default().name(name).build()),
                body,
            ),
        )
        .await?;
        Ok(format!("{name}（{}）", short_id(&created.id)))
    }

    async fn ensure_volume_quiet(&self, name: &str) -> Result<(), EngineError> {
        let filters = HashMap::from([("volume".to_string(), vec![name.to_string()])]);
        let users = with_timeout(
            "检查源卷使用状态",
            REQUEST_TIMEOUT,
            self.client.list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&filters)
                    .build(),
            )),
        )
        .await?;
        if users.iter().any(|container| {
            !matches!(
                container
                    .state
                    .as_ref()
                    .map(|state| state.to_string())
                    .as_deref(),
                Some("exited" | "created" | "dead")
            )
        }) {
            return Err(EngineError::operation(
                "导入卷",
                format!("卷 {name} 仍被运行中的源容器使用，请先停止所有使用者"),
            ));
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
fn verify_rancher_identity(pipe_id: Option<&str>, wsl_id: &str) -> Result<(), EngineError> {
    let wsl_id = wsl_id.trim();
    if wsl_id.is_empty() || pipe_id != Some(wsl_id) {
        return Err(EngineError::operation(
            "验证 Rancher Desktop 引擎身份",
            "docker_engine 管道不属于 Rancher Desktop 或无法确认身份；请关闭占用管道的其他桌面引擎，并确认 Rancher Desktop 使用 dockerd (moby)",
        ));
    }
    Ok(())
}

fn validate_source(inspect: &ContainerInspectResponse) -> Result<(), EngineError> {
    let reject = |message| Err(EngineError::operation("导入源容器", message));
    let state = inspect.state.as_ref();
    if state.is_none_or(|state| {
        state.running != Some(false)
            || state.paused.unwrap_or(false)
            || state.restarting.unwrap_or(false)
    }) {
        return reject("请先在源桌面引擎中停止容器，再重新扫描");
    }
    if inspect
        .platform
        .as_deref()
        .is_some_and(|platform| platform != "linux")
    {
        return reject("目前仅支持 Linux 容器");
    }
    if inspect.config.is_none() {
        return reject("缺少容器配置");
    }
    for mount in inspect.mounts.as_deref().unwrap_or_default() {
        match mount.typ.as_deref() {
            Some("volume")
                if mount.name.as_deref().is_some_and(|name| !name.is_empty())
                    && mount
                        .destination
                        .as_deref()
                        .is_some_and(|path| path.starts_with('/')) => {}
            Some("tmpfs") => {}
            _ => return reject("包含宿主机绑定目录或不支持的挂载，请手动迁移后重建"),
        }
    }
    if let Some(host) = &inspect.host_config {
        if host.mounts.as_ref().is_some_and(|mounts| {
            mounts
                .iter()
                .any(|mount| !matches!(mount.typ, Some(MountType::VOLUME | MountType::TMPFS)))
        }) {
            return reject("包含宿主机绑定目录或不支持的挂载，请手动迁移后重建");
        }
        // Created-but-never-started containers can have incomplete Mounts metadata.
        // Do not discard a bind unless inspect resolved it to a known local volume.
        if host.binds.as_ref().is_some_and(|binds| {
            binds.iter().any(|bind| {
                !inspect
                    .mounts
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|mount| {
                        mount.typ.as_deref() == Some("volume")
                            && mount
                                .name
                                .as_deref()
                                .is_some_and(|name| bind.starts_with(&format!("{name}:")))
                    })
            })
        }) {
            return reject("挂载尚未解析为本地卷，请先核对源容器挂载配置");
        }
        if host
            .network_mode
            .as_deref()
            .is_some_and(|mode| !matches!(mode, "" | "default" | "bridge" | "host" | "none"))
            || host.links.as_ref().is_some_and(|links| !links.is_empty())
            || host
                .volumes_from
                .as_ref()
                .is_some_and(|volumes| !volumes.is_empty())
            || [&host.ipc_mode, &host.pid_mode, &host.uts_mode]
                .iter()
                .any(|mode| {
                    mode.as_deref()
                        .is_some_and(|mode| mode.starts_with("container:"))
                })
        {
            return reject("包含自定义网络或跨容器依赖，请手动迁移后重建");
        }
        if host
            .devices
            .as_ref()
            .is_some_and(|devices| !devices.is_empty())
        {
            return reject("包含宿主机设备路径，请手动迁移后重建");
        }
        if host.mounts.as_ref().is_some_and(|mounts| {
            mounts.iter().any(|mount| {
                mount.volume_options.as_ref().is_some_and(|options| {
                    options
                        .subpath
                        .as_deref()
                        .is_some_and(|path| !path.is_empty())
                })
            })
        }) {
            return reject("包含卷子目录挂载，请手动迁移后重建");
        }
    }
    if inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.networks.as_ref())
        .is_some_and(|networks| {
            networks
                .keys()
                .any(|name| !matches!(name.as_str(), "bridge" | "host" | "none"))
        })
    {
        return reject("包含自定义网络，请手动迁移后重建");
    }
    Ok(())
}

fn create_body(
    inspect: &ContainerInspectResponse,
    image: &str,
) -> Result<ContainerCreateBody, EngineError> {
    validate_source(inspect)?;
    // Docker's Config and create request share wire field names, including healthcheck and stop signal.
    let config = serde_json::to_value(inspect.config.as_ref().unwrap())
        .map_err(|error| EngineError::operation("转换容器配置", error))?;
    let mut body: ContainerCreateBody = serde_json::from_value(config)
        .map_err(|error| EngineError::operation("转换容器配置", error))?;
    body.image = Some(image.into());
    let mut host = inspect.host_config.clone().unwrap_or_default();
    host.binds = None;
    host.mounts = host.mounts.take().map(|mounts| {
        mounts
            .into_iter()
            .filter(|mount| mount.typ == Some(MountType::TMPFS))
            .collect()
    });
    host.container_id_file = None;
    body.host_config = Some(host);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inspect(value: serde_json::Value) -> ContainerInspectResponse {
        serde_json::from_value(value).unwrap()
    }

    async fn mock_engine(
        responses: Vec<(&'static str, u16, String)>,
    ) -> (DockerEngine, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (path, status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                    let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    if headers.contains("transfer-encoding: chunked") {
                        if request.ends_with(b"0\r\n\r\n") {
                            break;
                        }
                    } else {
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map(|length| length.trim().parse().unwrap())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                assert!(request.lines().next().unwrap().contains(path), "{request}");
                requests.push(request);
                let response = format!("HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (
            DockerEngine {
                client: Docker::connect_with_http(
                    &format!("http://{address}"),
                    15,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
                managed_wsl: true,
            },
            task,
        )
    }

    #[tokio::test]
    async fn copies_rootfs_and_creates_stopped_container_without_mutating_source() {
        let config = json!({"Name":"/example","Image":"sha256:old","State":{"Running":false},"Config":{"Image":"old","Cmd":["sleep","30"],"Env":["SECRET=example"]},"HostConfig":{"NetworkMode":"bridge"}}).to_string();
        let (source, source_task) = mock_engine(vec![
            ("/containers/source/json", 200, config.clone()),
            ("/containers/source/export", 200, "rootfs-stream".into()),
            (
                "/images/sha256:old/json",
                200,
                json!({"Os":"linux","Architecture":"amd64"}).to_string(),
            ),
            ("/containers/source/json", 200, config),
        ])
        .await;
        let (target, target_task) = mock_engine(vec![
            (
                "/containers/example/json",
                404,
                json!({"message":"not found"}).to_string(),
            ),
            (
                "/images/create?",
                200,
                "{\"status\":\"sha256:new\"}\n".into(),
            ),
            (
                "/containers/create?name=example",
                201,
                json!({"Id":"new-container","Warnings":[]}).to_string(),
            ),
        ])
        .await;
        let scratch = tempfile::tempdir().unwrap();
        let (progress, _) = watch::channel(String::new());
        let result = timeout(
            Duration::from_secs(5),
            target.import_desktop_container(
                &source,
                "source",
                "rusker-import/test:latest",
                "test",
                scratch.path(),
                &mut HashMap::new(),
                &mut Vec::new(),
                &progress,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.contains("example"));
        let requests = target_task.await.unwrap();
        assert!(requests[1].contains("rootfs-stream"));
        assert!(requests[2].contains("SECRET=example"));
        assert!(requests[2].contains("rusker-import/test:latest"));
        assert!(source_task
            .await
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET ")));
        assert!(!scratch.path().join("rootfs.tar").exists());
    }

    #[tokio::test]
    async fn copies_volume_data_and_rewrites_mount_to_new_volume() {
        let config = json!({"Name":"/with-volume","Image":"sha256:old","State":{"Running":false},"Config":{"Image":"old","Cmd":["true"],"Volumes":{"/data":{}}},"Mounts":[{"Type":"volume","Name":"data","Destination":"/data","RW":false}]}).to_string();
        let source_volume = json!({"Name":"data","Driver":"local","Mountpoint":"/volumes/data","Labels":{},"Options":{},"Scope":"local"}).to_string();
        let (source, source_task) = mock_engine(vec![
            ("/containers/source/json", 200, config.clone()),
            ("/containers/json?", 200, "[]".into()),
            ("/volumes/data", 200, source_volume.clone()),
            ("/images/busybox:1.37/json", 200, "{}".into()),
            (
                "/containers/create",
                201,
                json!({"Id":"source-helper","Warnings":[]}).to_string(),
            ),
            (
                "/containers/source-helper/archive?",
                200,
                "volume-stream".into(),
            ),
            ("/containers/source-helper?", 204, String::new()),
            ("/containers/source/export", 200, "rootfs-stream".into()),
            (
                "/images/sha256:old/json",
                200,
                json!({"Os":"linux","Architecture":"amd64"}).to_string(),
            ),
            ("/containers/source/json", 200, config),
            ("/containers/json?", 200, "[]".into()),
        ])
        .await;
        let (target, target_task) = mock_engine(vec![
            (
                "/containers/with-volume/json",
                404,
                json!({"message":"not found"}).to_string(),
            ),
            (
                "/volumes/rusker-import-test-",
                404,
                json!({"message":"not found"}).to_string(),
            ),
            ("/volumes/create", 201, source_volume),
            ("/images/busybox:1.37/json", 200, "{}".into()),
            (
                "/containers/create",
                201,
                json!({"Id":"target-helper","Warnings":[]}).to_string(),
            ),
            ("/containers/target-helper/archive?", 200, String::new()),
            ("/containers/target-helper?", 204, String::new()),
            (
                "/images/create?",
                200,
                "{\"status\":\"sha256:new\"}\n".into(),
            ),
            (
                "/containers/create?name=with-volume",
                201,
                json!({"Id":"new-container","Warnings":[]}).to_string(),
            ),
        ])
        .await;
        let scratch = tempfile::tempdir().unwrap();
        let (progress, _) = watch::channel(String::new());
        let mut volumes = HashMap::new();
        let mut created = Vec::new();
        timeout(
            Duration::from_secs(5),
            target.import_desktop_container(
                &source,
                "source",
                "test:latest",
                "test",
                scratch.path(),
                &mut volumes,
                &mut created,
                &progress,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(volumes["data"], created[0].1);
        let requests = target_task.await.unwrap();
        assert!(requests[5].starts_with("PUT "));
        assert!(requests[5].contains("volume-stream"));
        let body = requests[8].split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["HostConfig"]["Mounts"][0]["Source"], volumes["data"]);
        assert_eq!(body["HostConfig"]["Mounts"][0]["ReadOnly"], true);
        let requests = source_task.await.unwrap();
        assert!(requests[4].contains("\"ReadOnly\":true"));
        assert!(!requests.iter().any(|request| request.contains("/start")));
    }

    #[tokio::test]
    async fn name_conflict_does_not_export_or_overwrite_anything() {
        let config = json!({"Name":"/existing","State":{"Running":false},"Config":{"Image":"old"}})
            .to_string();
        let (source, source_task) =
            mock_engine(vec![("/containers/source/json", 200, config)]).await;
        let (target, target_task) =
            mock_engine(vec![("/containers/existing/json", 200, "{}".into())]).await;
        let scratch = tempfile::tempdir().unwrap();
        let (progress, _) = watch::channel(String::new());
        let error = timeout(
            Duration::from_secs(5),
            target.import_desktop_container(
                &source,
                "source",
                "test:latest",
                "test",
                scratch.path(),
                &mut HashMap::new(),
                &mut Vec::new(),
                &progress,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("已存在"));
        assert_eq!(source_task.await.unwrap().len(), 1);
        assert_eq!(target_task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refuses_volume_shared_with_a_running_source_container() {
        let (source, task) = mock_engine(vec![(
            "/containers/json?",
            200,
            json!([{"Id":"other","State":"running"}]).to_string(),
        )])
        .await;
        let error = source.ensure_volume_quiet("shared-data").await.unwrap_err();
        assert!(error.to_string().contains("shared-data"));
        assert!(error.to_string().contains("停止所有使用者"));
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refuses_an_unmanaged_destination_before_connecting_to_desktop() {
        let (mut target, task) = mock_engine(Vec::new()).await;
        target.managed_wsl = false;
        let (progress, _) = watch::channel(String::new());
        let error = target
            .import_desktop_containers(
                DesktopImportSource::DockerDesktop,
                vec!["source".into()],
                progress,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("受管 Engine"));
        assert!(task.await.unwrap().is_empty());
    }

    #[test]
    fn desktop_sources_use_explicit_endpoints() {
        assert_eq!(
            DesktopImportSource::default(),
            DesktopImportSource::DockerDesktop
        );
        assert_eq!(
            DesktopImportSource::DockerDesktop.pipe(),
            "npipe:////./pipe/dockerDesktopLinuxEngine"
        );
        assert_eq!(
            DesktopImportSource::RancherDesktop.pipe(),
            "npipe:////./pipe/docker_engine"
        );
        assert!(DesktopImportSource::RancherDesktop
            .connection_hint()
            .contains("containerd"));
    }

    #[test]
    fn rancher_requires_matching_nonempty_daemon_ids() {
        assert!(verify_rancher_identity(Some("rancher-id"), "rancher-id\n").is_ok());
        for (pipe, wsl) in [
            (Some("docker-desktop-id"), "rancher-id"),
            (None, "rancher-id"),
            (Some(""), ""),
            (Some("rancher-id"), ""),
        ] {
            assert!(verify_rancher_identity(pipe, wsl).is_err());
        }
    }

    #[test]
    fn preserves_config_but_replaces_image_and_host_mounts() {
        let source = inspect(
            json!({"State":{"Running":false},"Platform":"linux","Config":{"Image":"old","Cmd":["sleep","30"],"Entrypoint":["sh"],"Env":["A=b"],"User":"1000","WorkingDir":"/data","ExposedPorts":{"80/tcp":{}},"Labels":{"app":"test"}},"HostConfig":{"Binds":["data:/data"],"PortBindings":{"80/tcp":[{"HostPort":"8080"}]},"Memory":1024},"Mounts":[{"Type":"volume","Name":"data","Destination":"/data"}]}),
        );
        let body = create_body(&source, "new:latest").unwrap();
        assert_eq!(body.image.as_deref(), Some("new:latest"));
        assert_eq!(body.cmd.unwrap(), ["sleep", "30"]);
        assert_eq!(body.env.unwrap(), ["A=b"]);
        assert_eq!(body.user.as_deref(), Some("1000"));
        assert_eq!(body.exposed_ports.unwrap(), ["80/tcp"]);
        let host = body.host_config.unwrap();
        assert!(host.binds.is_none());
        assert_eq!(host.memory, Some(1024));
        assert_eq!(
            host.port_bindings.unwrap()["80/tcp"].as_ref().unwrap()[0]
                .host_port
                .as_deref(),
            Some("8080")
        );
    }

    #[test]
    fn rejects_running_bind_and_cross_engine_dependencies() {
        for extra in [
            json!({"State":{"Running":true}}),
            json!({"Mounts":[{"Type":"bind","Destination":"/data"}]}),
            json!({"HostConfig":{"NetworkMode":"compose_default"}}),
            json!({"HostConfig":{"IpcMode":"container:other"}}),
            json!({"NetworkSettings":{"Networks":{"custom":{}}}}),
            json!({"Mounts":[{"Type":"volume","Name":"data"}]}),
            json!({"Platform":"windows"}),
        ] {
            let mut value = json!({"State":{"Running":false},"Config":{"Image":"alpine"}});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(validate_source(&inspect(value.clone())).is_err(), "{value}");
        }
    }
}
