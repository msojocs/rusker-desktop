use std::{collections::HashMap, path::Path, time::Duration};

use bollard::{
    body_try_stream,
    container::LogOutput,
    errors::Error as BollardError,
    models::{
        ContainerCreateBody, ContainerSummary, DeviceRequest, HostConfig, ImageSummary, Mount,
        MountType, PortBinding, Volume, VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DataUsageOptionsBuilder,
        DownloadFromContainerOptionsBuilder, ListContainersOptionsBuilder,
        ListImagesOptionsBuilder, ListVolumesOptions, LogsOptionsBuilder,
        RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
        UploadToContainerOptionsBuilder,
    },
    Docker,
};
use bytes::Bytes;
use futures_util::{pin_mut, stream, StreamExt as _, TryStreamExt};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    time::timeout,
};

use crate::{
    engine::{with_timeout, EngineError},
    format::{bound_logs, short_id},
    model::{
        ContainerAction, ContainerSummary as ContainerDto, CreateContainerRequest, EngineInfo,
        EngineSnapshot, ImageSummary as ImageDto, PortMapping, VolumeSummary,
    },
    volume::{
        container_path, parse_listing, parse_timestamp, partial_export_path, validate_volume_name,
        DirectoryListing, CLONE_SCRIPT, EMPTY_SCRIPT, EXPORT_SOURCE, HELPER_IMAGE, HELPER_LABEL,
        LIST_SCRIPT, VOLUME_MOUNT,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Volume sizes require the engine to walk every volume.
const DISK_USAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Copying or importing volume data can take a while for large volumes.
const DATA_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_HELPER_OUTPUT: usize = 8 * 1024 * 1024;
const IMPORT_CHUNK: usize = 256 * 1024;

pub struct DockerEngine {
    client: Docker,
    managed_wsl: bool,
}

impl DockerEngine {
    pub async fn connect() -> Result<Self, EngineError> {
        #[cfg(windows)]
        let managed_wsl = std::env::var_os("DOCKER_HOST").is_none();
        #[cfg(not(windows))]
        let managed_wsl = false;

        #[cfg(windows)]
        let client = if managed_wsl {
            Docker::connect_with_named_pipe(
                crate::runtime_host::MANAGED_PIPE,
                CONNECT_TIMEOUT.as_secs(),
                bollard::API_DEFAULT_VERSION,
            )
            .map_err(|error| EngineError::Connect(error.to_string()))?
        } else {
            Docker::connect_with_local_defaults()
                .map_err(|error| EngineError::Connect(error.to_string()))?
        };
        #[cfg(not(windows))]
        let client = Docker::connect_with_local_defaults()
            .map_err(|error| EngineError::Connect(error.to_string()))?;

        let client = match timeout(CONNECT_TIMEOUT, client.negotiate_version()).await {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                let message = if managed_wsl {
                    format!(
                        "无法连接 Rusker WSL2 受管 Engine，请在设置中安装并启动运行环境：{error}"
                    )
                } else {
                    error.to_string()
                };
                return Err(EngineError::Connect(message));
            }
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: "连接容器引擎".into(),
                    seconds: CONNECT_TIMEOUT.as_secs(),
                });
            }
        };
        Ok(Self {
            client,
            managed_wsl,
        })
    }

    pub async fn snapshot(&self) -> Result<EngineSnapshot, EngineError> {
        let containers_options = ListContainersOptionsBuilder::default().all(true).build();
        // Top-level images only (like `docker images`), without
        // intermediate build layers.
        let images_options = ListImagesOptionsBuilder::default().all(false).build();
        let (version, info, containers, images, volumes) = tokio::try_join!(
            with_timeout("读取引擎版本", REQUEST_TIMEOUT, self.client.version()),
            with_timeout("读取引擎信息", REQUEST_TIMEOUT, self.client.info()),
            with_timeout(
                "读取容器列表",
                REQUEST_TIMEOUT,
                self.client.list_containers(Some(containers_options)),
            ),
            with_timeout(
                "读取镜像列表",
                REQUEST_TIMEOUT,
                self.client.list_images(Some(images_options)),
            ),
            with_timeout(
                "读取卷列表",
                REQUEST_TIMEOUT,
                self.client.list_volumes(None::<ListVolumesOptions>),
            ),
        )?;

        let mut containers: Vec<ContainerDto> = containers.into_iter().map(container_dto).collect();
        containers.sort_by_cached_key(|container| container.name.to_lowercase());
        let mut images: Vec<ImageDto> = images.into_iter().map(image_dto).collect();
        images.sort_by_cached_key(|image| image.tags.first().cloned().unwrap_or_default());
        let mut volumes: Vec<VolumeSummary> = volumes
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(volume_dto)
            .collect();
        volumes.sort_by_cached_key(|volume| volume.name.to_lowercase());

        Ok(EngineSnapshot {
            engine: EngineInfo {
                name: {
                    let name = info.name.unwrap_or_else(|| "本地引擎".into());
                    if self.managed_wsl {
                        format!("Rusker WSL2 / {name}")
                    } else {
                        name
                    }
                },
                version: version.version.unwrap_or_else(|| "未知".into()),
                api_version: version.api_version.unwrap_or_else(|| "未知".into()),
                operating_system: info
                    .operating_system
                    .or(version.os)
                    .unwrap_or_else(|| "未知".into()),
                architecture: info
                    .architecture
                    .or(version.arch)
                    .unwrap_or_else(|| "未知".into()),
                cpus: info
                    .ncpu
                    .and_then(|cpus| u32::try_from(cpus).ok())
                    .unwrap_or(0),
                memory_bytes: info
                    .mem_total
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .unwrap_or(0),
            },
            containers,
            images,
            volumes,
        })
    }

    pub async fn create_and_run(
        &self,
        request: CreateContainerRequest,
    ) -> Result<String, EngineError> {
        request
            .validate()
            .map_err(|error| EngineError::operation("校验容器配置", error))?;
        let mut options = CreateContainerOptionsBuilder::default();
        if let Some(name) = &request.name {
            options = options.name(name);
        }

        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &request.ports {
            port_bindings
                .entry(format!("{}/tcp", port.container_port))
                .or_default()
                .get_or_insert_with(Vec::new)
                .push(PortBinding {
                    host_ip: Some(port.host_ip.clone()),
                    host_port: Some(port.host_port.to_string()),
                });
        }
        let mut exposed_ports: Vec<String> = port_bindings.keys().cloned().collect();
        exposed_ports.sort();
        let device_requests = nvidia_device_requests(request.nvidia_gpu);
        let host_config =
            if port_bindings.is_empty() && device_requests.is_none() && request.binds.is_empty() {
                None
            } else {
                Some(HostConfig {
                    port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
                    device_requests,
                    binds: (!request.binds.is_empty()).then_some(request.binds),
                    ..Default::default()
                })
            };
        let body = ContainerCreateBody {
            image: Some(request.image),
            cmd: (!request.command.is_empty()).then_some(request.command),
            env: (!request.env.is_empty()).then_some(request.env),
            exposed_ports: (!exposed_ports.is_empty()).then_some(exposed_ports),
            host_config,
            ..Default::default()
        };
        let created = with_timeout(
            "创建容器",
            ACTION_TIMEOUT,
            self.client.create_container(Some(options.build()), body),
        )
        .await?;
        let id = created.id;

        if let Err(start_error) = with_timeout(
            "启动新容器",
            ACTION_TIMEOUT,
            self.client.start_container(&id, None),
        )
        .await
        {
            let cleanup_options = RemoveContainerOptionsBuilder::default()
                .force(true)
                .v(false)
                .build();
            let cleanup = with_timeout(
                "清理未启动的新容器",
                ACTION_TIMEOUT,
                self.client.remove_container(&id, Some(cleanup_options)),
            )
            .await;
            let cleanup_message = match cleanup {
                Ok(()) => "新建容器已清理".into(),
                Err(error) => format!("新建容器 {id} 清理失败：{error}"),
            };
            return Err(EngineError::operation(
                "创建并运行容器",
                format!("{start_error}；{cleanup_message}"),
            ));
        }
        Ok(id)
    }

    pub async fn container_action(
        &self,
        id: &str,
        action: ContainerAction,
    ) -> Result<(), EngineError> {
        match action {
            ContainerAction::Start => {
                with_timeout(
                    action.label(),
                    ACTION_TIMEOUT,
                    self.client.start_container(id, None),
                )
                .await
            }
            ContainerAction::Stop => {
                with_timeout(
                    action.label(),
                    ACTION_TIMEOUT,
                    self.client.stop_container(id, None),
                )
                .await
            }
            ContainerAction::Restart => {
                with_timeout(
                    action.label(),
                    ACTION_TIMEOUT,
                    self.client.restart_container(id, None),
                )
                .await
            }
            ContainerAction::Remove => {
                let options = RemoveContainerOptionsBuilder::default()
                    .force(false)
                    .v(false)
                    .build();
                with_timeout(
                    action.label(),
                    ACTION_TIMEOUT,
                    self.client.remove_container(id, Some(options)),
                )
                .await
            }
        }
    }

    /// Removes several containers one by one (never forced). Returns one
    /// message per container that could not be removed.
    pub async fn remove_containers(&self, ids: &[String]) -> Vec<String> {
        let mut failures = Vec::new();
        for id in ids {
            if let Err(error) = self.container_action(id, ContainerAction::Remove).await {
                failures.push(format!("{}: {error}", short_id(id)));
            }
        }
        failures
    }

    pub async fn container_logs(&self, id: &str) -> Result<String, EngineError> {
        let options = LogsOptionsBuilder::default()
            .follow(false)
            .stdout(true)
            .stderr(true)
            .timestamps(false)
            .tail("200")
            .build();
        let stream = self.client.logs(id, Some(options));
        pin_mut!(stream);

        let result = timeout(REQUEST_TIMEOUT, async {
            let mut logs = String::new();
            while let Some(output) = stream
                .try_next()
                .await
                .map_err(|error| EngineError::operation("读取容器日志", error))?
            {
                logs.push_str(&output.to_string());
                if logs.len() > 256 * 1024 {
                    logs = bound_logs(&logs);
                }
            }
            Ok(logs)
        })
        .await;

        match result {
            Ok(logs) => logs.map(|logs| bound_logs(&logs)),
            Err(_) => Err(EngineError::Timeout {
                action: "读取容器日志".into(),
                seconds: REQUEST_TIMEOUT.as_secs(),
            }),
        }
    }

    pub async fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        let options = RemoveImageOptionsBuilder::default()
            .force(false)
            .noprune(false)
            .build();
        with_timeout(
            "删除镜像",
            ACTION_TIMEOUT,
            self.client.remove_image(id, Some(options), None),
        )
        .await?;
        Ok(())
    }

    pub async fn pull_image(&self, reference: &str) -> Result<(), EngineError> {
        let mut options = CreateImageOptionsBuilder::default();
        options = options.from_image(reference);
        if needs_default_tag(reference) {
            options = options.tag("latest");
        }
        let stream = self.client.create_image(Some(options.build()), None, None);
        pin_mut!(stream);

        let result =
            timeout(PULL_TIMEOUT, async {
                while let Some(response) = stream.try_next().await.map_err(|error| {
                    EngineError::operation(format!("拉取镜像 {reference}"), error)
                })? {
                    if let Some(detail) = response.error_detail {
                        return Err(EngineError::operation(
                            format!("拉取镜像 {reference}"),
                            detail.message.unwrap_or_else(|| "引擎返回未知错误".into()),
                        ));
                    }
                }
                Ok(())
            })
            .await;

        match result {
            Ok(result) => result,
            Err(_) => Err(EngineError::Timeout {
                action: format!("拉取镜像 {reference}"),
                seconds: PULL_TIMEOUT.as_secs(),
            }),
        }
    }

    /// Per-volume disk usage. Only engines with API ≥ 1.52 report it in a
    /// shape bollard can read; others yield an empty map (sizes unknown).
    pub async fn volume_sizes(&self) -> Result<HashMap<String, u64>, EngineError> {
        let options = DataUsageOptionsBuilder::default().verbose(true).build();
        // bollard cannot URL-encode the `type` list, so add `type=volume`
        // to the request itself; this keeps the engine from also sizing
        // every image layer and container filesystem.
        let client = self.client.clone().with_request_modifier(|mut request| {
            let uri = request.uri().to_string();
            if uri.contains("/system/df") && !uri.contains("type=") {
                let separator = if uri.contains('?') { '&' } else { '?' };
                if let Ok(uri) = format!("{uri}{separator}type=volume").parse() {
                    *request.uri_mut() = uri;
                }
            }
            request
        });
        let usage =
            with_timeout("统计卷大小", DISK_USAGE_TIMEOUT, client.df(Some(options))).await?;
        let mut sizes = HashMap::new();
        for item in usage
            .volume_usage
            .and_then(|usage| usage.items)
            .unwrap_or_default()
        {
            let Ok(volume) = serde_json::from_value::<Volume>(item) else {
                continue;
            };
            if let Some(size) = volume
                .usage_data
                .and_then(|usage| u64::try_from(usage.size).ok())
            {
                sizes.insert(volume.name, size);
            }
        }
        Ok(sizes)
    }

    pub async fn create_volume(&self, name: Option<String>) -> Result<(), EngineError> {
        if let Some(name) = &name {
            validate_volume_name(name).map_err(|error| EngineError::operation("创建卷", error))?;
            self.ensure_volume_absent("创建卷", name).await?;
        }
        with_timeout(
            "创建卷",
            ACTION_TIMEOUT,
            self.client.create_volume(VolumeCreateRequest {
                name,
                ..Default::default()
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_volume(&self, name: &str) -> Result<(), EngineError> {
        let options = RemoveVolumeOptionsBuilder::default().force(false).build();
        with_timeout(
            format!("删除卷 {name}").as_str(),
            ACTION_TIMEOUT,
            self.client.remove_volume(name, Some(options)),
        )
        .await
    }

    /// Creates `target` and copies every file of `source` into it. The new
    /// volume is removed again when copying fails.
    pub async fn clone_volume(&self, source: &str, target: &str) -> Result<(), EngineError> {
        let action = format!("克隆卷 {source}");
        validate_volume_name(target).map_err(|error| EngineError::operation(&action, error))?;
        self.ensure_volume_absent(&action, target).await?;
        with_timeout(
            &action,
            ACTION_TIMEOUT,
            self.client.create_volume(VolumeCreateRequest {
                name: Some(target.to_string()),
                ..Default::default()
            }),
        )
        .await?;

        let result = self
            .run_helper(
                &action,
                vec![
                    volume_mount(source, "/from", true),
                    volume_mount(target, "/to", false),
                ],
                CLONE_SCRIPT,
                Vec::new(),
                DATA_TIMEOUT,
            )
            .await
            .and_then(|output| output.success(&action));
        if let Err(error) = result {
            let options = RemoveVolumeOptionsBuilder::default().force(true).build();
            let cleanup = match self.client.remove_volume(target, Some(options)).await {
                Ok(()) => format!("已删除未完成的卷 {target}"),
                Err(cleanup) => format!("未完成的卷 {target} 删除失败：{cleanup}"),
            };
            return Err(EngineError::operation(
                action,
                format!("{error}；{cleanup}"),
            ));
        }
        Ok(())
    }

    /// Deletes all files inside the volume, keeping the volume itself.
    pub async fn empty_volume(&self, name: &str) -> Result<(), EngineError> {
        let action = format!("清空卷 {name}");
        self.run_helper(
            &action,
            vec![volume_mount(name, VOLUME_MOUNT, false)],
            EMPTY_SCRIPT,
            Vec::new(),
            DATA_TIMEOUT,
        )
        .await?
        .success(&action)
    }

    /// Lists one directory of the volume (`relative` to its root), mounted
    /// read-only into a short-lived helper container.
    pub async fn list_volume_directory(
        &self,
        name: &str,
        relative: &str,
    ) -> Result<DirectoryListing, EngineError> {
        let action = "读取卷内容";
        let path =
            container_path(relative).map_err(|error| EngineError::operation(action, error))?;
        let output = self
            .run_helper(
                action,
                vec![volume_mount(name, VOLUME_MOUNT, true)],
                LIST_SCRIPT,
                vec![format!("RUSKER_PATH={path}")],
                ACTION_TIMEOUT,
            )
            .await?;
        if output.code == 3 {
            return Err(EngineError::operation(
                action,
                format!("目录 /{relative} 不存在或无法访问"),
            ));
        }
        output.success(action)?;
        Ok(parse_listing(&output.stdout))
    }

    /// Extracts a tar archive (optionally gzip/bzip2/xz compressed) from the
    /// local file system into the volume root. Existing files with the same
    /// path are overwritten; ownership in the archive is preserved.
    pub async fn import_volume(&self, name: &str, archive: &Path) -> Result<(), EngineError> {
        let action = format!("导入数据到卷 {name}");
        let file = tokio::fs::File::open(archive).await.map_err(|error| {
            EngineError::operation(&action, format!("无法读取 {}：{error}", archive.display()))
        })?;
        let id = self
            .create_helper(
                &action,
                vec![volume_mount(name, VOLUME_MOUNT, false)],
                vec!["true".into()],
                Vec::new(),
            )
            .await?;
        let options = UploadToContainerOptionsBuilder::default()
            .path(VOLUME_MOUNT)
            .build();
        let result = with_timeout(
            &action,
            DATA_TIMEOUT,
            self.client
                .upload_to_container(&id, Some(options), body_try_stream(file_chunks(file))),
        )
        .await;
        self.remove_helper(&id).await;
        result
    }

    /// Removes several volumes one by one (never forced). Returns one
    /// message per volume that could not be removed.
    /// Removes each image reference (`repo:tag` or ID) independently and
    /// returns one error message per reference that was not removed.
    pub async fn remove_images(&self, references: &[String]) -> Vec<String> {
        let mut failures = Vec::new();
        for reference in references {
            if let Err(error) = self.remove_image(reference).await {
                failures.push(error.to_string());
            }
        }
        failures
    }

    pub async fn remove_volumes(&self, names: &[String]) -> Vec<String> {
        let mut failures = Vec::new();
        for name in names {
            if let Err(error) = self.remove_volume(name).await {
                failures.push(error.to_string());
            }
        }
        failures
    }

    /// Writes the volume content as an uncompressed tar to `destination`
    /// and returns its size. Entries are relative to the volume root, so the
    /// file can be imported again with [`Self::import_volume`]. Data is
    /// streamed into a sibling temporary file that only replaces
    /// `destination` once complete.
    pub async fn export_volume(&self, name: &str, destination: &Path) -> Result<u64, EngineError> {
        let action = format!("导出卷 {name}");
        let partial = partial_export_path(destination);
        let mut file = tokio::fs::File::create(&partial).await.map_err(|error| {
            EngineError::operation(&action, format!("无法创建 {}：{error}", partial.display()))
        })?;
        let id = match self
            .create_helper(
                &action,
                vec![volume_mount(name, VOLUME_MOUNT, true)],
                vec!["true".into()],
                Vec::new(),
            )
            .await
        {
            Ok(id) => id,
            Err(error) => {
                drop(file);
                _ = tokio::fs::remove_file(&partial).await;
                return Err(error);
            }
        };

        let options = DownloadFromContainerOptionsBuilder::default()
            .path(EXPORT_SOURCE)
            .build();
        let archive = self.client.download_from_container(&id, Some(options));
        let copy = timeout(DATA_TIMEOUT, async {
            pin_mut!(archive);
            let mut written = 0u64;
            while let Some(chunk) = archive
                .try_next()
                .await
                .map_err(|error| EngineError::operation(&action, error))?
            {
                file.write_all(&chunk).await.map_err(|error| {
                    EngineError::operation(&action, format!("写入导出文件失败：{error}"))
                })?;
                written += chunk.len() as u64;
            }
            file.flush().await.map_err(|error| {
                EngineError::operation(&action, format!("写入导出文件失败：{error}"))
            })?;
            Ok(written)
        })
        .await;
        self.remove_helper(&id).await;
        drop(file);

        let result = match copy {
            Ok(result) => result,
            Err(_) => Err(EngineError::Timeout {
                action: action.clone(),
                seconds: DATA_TIMEOUT.as_secs(),
            }),
        };
        let written = match result {
            Ok(written) => written,
            Err(error) => {
                _ = tokio::fs::remove_file(&partial).await;
                return Err(error);
            }
        };
        // The save dialog already confirmed overwriting; Windows rename does
        // not replace an existing file.
        if tokio::fs::metadata(destination).await.is_ok() {
            _ = tokio::fs::remove_file(destination).await;
        }
        if let Err(error) = tokio::fs::rename(&partial, destination).await {
            _ = tokio::fs::remove_file(&partial).await;
            return Err(EngineError::operation(
                &action,
                format!("无法保存到 {}：{error}", destination.display()),
            ));
        }
        Ok(written)
    }

    async fn ensure_volume_absent(&self, action: &str, name: &str) -> Result<(), EngineError> {
        match timeout(REQUEST_TIMEOUT, self.client.inspect_volume(name)).await {
            Ok(Ok(_)) => Err(EngineError::operation(action, format!("卷 {name} 已存在"))),
            Ok(Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            })) => Ok(()),
            Ok(Err(error)) => Err(EngineError::operation(action, error)),
            Err(_) => Err(EngineError::Timeout {
                action: action.into(),
                seconds: REQUEST_TIMEOUT.as_secs(),
            }),
        }
    }

    async fn ensure_helper_image(&self) -> Result<(), EngineError> {
        if self.client.inspect_image(HELPER_IMAGE).await.is_ok() {
            return Ok(());
        }
        self.pull_image(HELPER_IMAGE).await.map_err(|error| {
            EngineError::operation(
                "准备卷辅助镜像",
                format!("卷操作需要 {HELPER_IMAGE} 镜像，自动拉取失败：{error}"),
            )
        })
    }

    async fn create_helper(
        &self,
        action: &str,
        mounts: Vec<Mount>,
        cmd: Vec<String>,
        env: Vec<String>,
    ) -> Result<String, EngineError> {
        self.ensure_helper_image().await?;
        let body = ContainerCreateBody {
            image: Some(HELPER_IMAGE.into()),
            cmd: Some(cmd),
            env: (!env.is_empty()).then_some(env),
            labels: Some(HashMap::from([(HELPER_LABEL.into(), "volume".into())])),
            host_config: Some(HostConfig {
                mounts: Some(mounts),
                network_mode: Some("none".into()),
                // Output is read back through the logs API, so do not depend
                // on the engine's default log driver.
                log_config: Some(bollard::models::HostConfigLogConfig {
                    typ: Some("json-file".into()),
                    config: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = with_timeout(
            action,
            ACTION_TIMEOUT,
            self.client.create_container(None, body),
        )
        .await?;
        Ok(created.id)
    }

    async fn remove_helper(&self, id: &str) {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(false)
            .build();
        _ = timeout(
            ACTION_TIMEOUT,
            self.client.remove_container(id, Some(options)),
        )
        .await;
    }

    /// Runs `script` with `sh -c` in a fresh helper container, waits for it
    /// and returns its exit code and output. The container is always removed.
    async fn run_helper(
        &self,
        action: &str,
        mounts: Vec<Mount>,
        script: &str,
        env: Vec<String>,
        duration: Duration,
    ) -> Result<HelperOutput, EngineError> {
        let id = self
            .create_helper(
                action,
                mounts,
                vec!["sh".into(), "-c".into(), script.into()],
                env,
            )
            .await?;
        let result = self.run_created_helper(action, &id, duration).await;
        self.remove_helper(&id).await;
        result
    }

    async fn run_created_helper(
        &self,
        action: &str,
        id: &str,
        duration: Duration,
    ) -> Result<HelperOutput, EngineError> {
        with_timeout(
            action,
            ACTION_TIMEOUT,
            self.client.start_container(id, None),
        )
        .await?;
        let wait = self.client.wait_container(id, None);
        pin_mut!(wait);
        let code = match timeout(duration, wait.next()).await {
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: action.into(),
                    seconds: duration.as_secs(),
                })
            }
            Ok(Some(Ok(response))) => response.status_code,
            Ok(Some(Err(BollardError::DockerContainerWaitError { code, .. }))) => code,
            Ok(Some(Err(error))) => return Err(EngineError::operation(action, error)),
            Ok(None) => {
                return Err(EngineError::operation(action, "辅助容器未返回退出状态"));
            }
        };

        let options = LogsOptionsBuilder::default()
            .follow(false)
            .stdout(true)
            .stderr(true)
            .timestamps(false)
            .build();
        let logs = self.client.logs(id, Some(options));
        pin_mut!(logs);
        let collected = timeout(REQUEST_TIMEOUT, async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            while let Some(output) = logs
                .try_next()
                .await
                .map_err(|error| EngineError::operation(action, error))?
            {
                let (target, message) = match output {
                    LogOutput::StdErr { message } => (&mut stderr, message),
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        (&mut stdout, message)
                    }
                    LogOutput::StdIn { .. } => continue,
                };
                if target.len() + message.len() > MAX_HELPER_OUTPUT {
                    return Err(EngineError::operation(action, "辅助容器输出过大"));
                }
                target.extend_from_slice(&message);
            }
            Ok((stdout, stderr))
        })
        .await;
        let (stdout, stderr) = match collected {
            Ok(result) => result?,
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: action.into(),
                    seconds: REQUEST_TIMEOUT.as_secs(),
                })
            }
        };
        Ok(HelperOutput {
            code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

struct HelperOutput {
    code: i64,
    stdout: String,
    stderr: String,
}

impl HelperOutput {
    fn success(&self, action: &str) -> Result<(), EngineError> {
        if self.code == 0 {
            return Ok(());
        }
        let detail = self.stderr.trim();
        let detail = if detail.is_empty() {
            format!("辅助容器退出码 {}", self.code)
        } else {
            format!("辅助容器退出码 {}：{}", self.code, bound_logs(detail))
        };
        Err(EngineError::operation(action, detail))
    }
}

fn volume_mount(name: &str, target: &str, read_only: bool) -> Mount {
    Mount {
        typ: Some(MountType::VOLUME),
        source: Some(name.to_string()),
        target: Some(target.to_string()),
        read_only: Some(read_only),
        // Never copy image content into the volume on first mount.
        volume_options: Some(bollard::models::MountVolumeOptions {
            no_copy: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn file_chunks(
    file: tokio::fs::File,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0; IMPORT_CHUNK];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some((Bytes::from(buffer), file)))
    })
}

fn volume_dto(volume: Volume) -> VolumeSummary {
    let mut labels: Vec<(String, String)> = volume.labels.into_iter().collect();
    labels.sort();
    VolumeSummary {
        name: volume.name,
        driver: volume.driver,
        mountpoint: volume.mountpoint,
        scope: volume
            .scope
            .map(|scope| scope.to_string())
            .unwrap_or_default(),
        created: volume
            .created_at
            .as_deref()
            .map(parse_timestamp)
            .unwrap_or(0),
        labels,
    }
}

fn container_dto(container: ContainerSummary) -> ContainerDto {
    let id = container.id.unwrap_or_default();
    let name = container
        .names
        .and_then(|names| names.into_iter().next())
        .map(|name| name.trim_start_matches('/').to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| short_id(&id));
    let ports = container
        .ports
        .unwrap_or_default()
        .into_iter()
        .map(|port| PortMapping {
            host_ip: port.ip,
            private_port: port.private_port,
            public_port: port.public_port,
            protocol: port.typ.map(|kind| kind.to_string()).unwrap_or_default(),
        })
        .collect();
    let volumes = container
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter(|mount| mount.typ.as_deref() == Some("volume"))
        .filter_map(|mount| mount.name)
        .collect();

    ContainerDto {
        id,
        name,
        image: container.image.unwrap_or_else(|| "未知镜像".into()),
        image_id: container.image_id.unwrap_or_default(),
        state: container
            .state
            .map(|state| state.to_string())
            .unwrap_or_else(|| "unknown".into()),
        status: container.status.unwrap_or_default(),
        created: container.created.unwrap_or_default(),
        ports,
        volumes,
    }
}

fn image_dto(image: ImageSummary) -> ImageDto {
    ImageDto {
        id: image.id,
        tags: image.repo_tags,
        digests: image.repo_digests,
        created: image.created,
        size: u64::try_from(image.size).unwrap_or(0),
    }
}

fn nvidia_device_requests(enabled: bool) -> Option<Vec<DeviceRequest>> {
    enabled.then(|| {
        vec![DeviceRequest {
            driver: Some("nvidia".into()),
            count: Some(-1),
            capabilities: Some(vec![vec!["gpu".into()]]),
            ..Default::default()
        }]
    })
}

fn needs_default_tag(reference: &str) -> bool {
    if reference.contains('@') {
        return false;
    }
    let last_segment = reference.rsplit('/').next().unwrap_or(reference);
    !last_segment.contains(':')
}

#[cfg(test)]
mod tests {
    use super::{needs_default_tag, nvidia_device_requests};

    #[test]
    fn maps_nvidia_gpu_switch_to_all_device_requests() {
        assert!(nvidia_device_requests(false).is_none());
        let requests = nvidia_device_requests(true).expect("GPU 请求");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].driver.as_deref(), Some("nvidia"));
        assert_eq!(requests[0].count, Some(-1));
        assert_eq!(requests[0].device_ids, None);
        assert_eq!(
            requests[0].capabilities,
            Some(vec![vec!["gpu".to_string()]])
        );
    }

    #[test]
    fn only_untagged_image_references_receive_latest() {
        assert!(needs_default_tag("nginx"));
        assert!(needs_default_tag("docker.io/library/nginx"));
        assert!(needs_default_tag("localhost:5000/team/api"));
        assert!(!needs_default_tag("nginx:1.29"));
        assert!(!needs_default_tag("localhost:5000/api:dev"));
        assert!(!needs_default_tag("nginx@sha256:abcd"));
    }
}
