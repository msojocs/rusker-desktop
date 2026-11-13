use std::{collections::HashMap, path::Path, time::Duration};

use bollard::{
    body_try_stream,
    container::LogOutput,
    errors::Error as BollardError,
    models::{
        ContainerCreateBody, ContainerSummary, DeviceRequest, HostConfig, ImageHistoryResponseItem,
        ImageInspect, ImageSummary, Mount, MountType, PortBinding, Volume, VolumeCreateRequest,
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
        EngineSnapshot, ImageDetail, ImageLayer, ImageSummary as ImageDto, PortMapping,
        VolumeSummary, COMPOSE_CONFIG_FILES_LABEL, COMPOSE_PROJECT_LABEL, COMPOSE_REPLACE_LABEL,
        COMPOSE_SERVICE_LABEL, COMPOSE_WORKING_DIR_LABEL,
    },
    volume::{
        container_path, parse_listing, parse_timestamp, partial_export_path, validate_volume_name,
        DirectoryListing, CLONE_SCRIPT, EMPTY_SCRIPT, EXPORT_SOURCE, HELPER_IMAGE, HELPER_LABEL,
        LIST_SCRIPT, VOLUME_MOUNT,
    },
};

#[path = "desktop_import.rs"]
mod desktop_import;
pub use desktop_import::{
    DesktopContainer, DesktopImportProgress, DesktopImportReport, DesktopImportSource,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Includes the managed runtime's wake-up, not just a pipe handshake.
const MANAGED_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Volume sizes require the engine to walk every volume.
const DISK_USAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Copying or importing volume data can take a while for large volumes.
const DATA_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_HELPER_OUTPUT: usize = 8 * 1024 * 1024;
const IMPORT_CHUNK: usize = 256 * 1024;

/// Bollard's built-in transports explicitly disable idle pooling. A custom
/// transport preserves Hyper's upgrade support (exec/attach) while reusing normal
/// HTTP connections. Quiet *active* streams are never subject to this timeout;
/// only connections returned to Hyper's idle pool expire, allowing Resource
/// Saver to observe zero clients after a short idle period.
#[cfg(windows)]
fn pooled_named_pipe(path: &str) -> Result<Docker, EngineError> {
    use hyper_named_pipe::NamedPipeConnector;
    use hyper_util::{client::legacy::Client, rt::TokioExecutor, rt::TokioTimer};

    let mut builder = Client::builder(TokioExecutor::new());
    builder
        .http1_title_case_headers(true)
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(4)
        // Mutations must not be automatically replayed if a pooled connection
        // closes during a lifecycle transition.
        .retry_canceled_requests(false);
    let client = builder.build(NamedPipeConnector);
    let host = hex::encode(path.replacen("npipe://", "", 1).as_bytes());
    Docker::connect_with_custom_transport(
        move |request| {
            let client = client.clone();
            async move {
                client
                    .request(request)
                    .await
                    .map_err(bollard::errors::Error::from)
            }
        },
        Some(format!("net.pipe://{host}")),
        DATA_TIMEOUT.as_secs(),
        bollard::API_DEFAULT_VERSION,
    )
    .map_err(|error| EngineError::Connect(error.to_string()))
}

#[derive(Clone)]
pub struct DockerEngine {
    pub(super) client: Docker,
    pub(super) managed_wsl: bool,
}

impl DockerEngine {
    pub async fn connect() -> Result<Self, EngineError> {
        let _timing = crate::diagnostics::PerfSpan::new("engine.connect");
        #[cfg(windows)]
        let managed_wsl = std::env::var_os("DOCKER_HOST").is_none();
        #[cfg(not(windows))]
        let managed_wsl = false;

        #[cfg(windows)]
        let client = if managed_wsl {
            pooled_named_pipe(crate::runtime_host::MANAGED_PIPE)?
        } else if let Some(path) = std::env::var_os("DOCKER_HOST")
            .and_then(|host| host.into_string().ok())
            .filter(|host| host.starts_with("npipe://"))
        {
            pooled_named_pipe(&path)?
        } else {
            Docker::connect_with_local_defaults()
                .map_err(|error| EngineError::Connect(error.to_string()))?
        };
        #[cfg(not(windows))]
        let client = Docker::connect_with_local_defaults()
            .map_err(|error| EngineError::Connect(error.to_string()))?;

        let connect_timeout = if managed_wsl {
            MANAGED_CONNECT_TIMEOUT
        } else {
            CONNECT_TIMEOUT
        };
        let negotiation = crate::diagnostics::PerfSpan::new("engine.negotiate_version");
        let client = match timeout(connect_timeout, client.negotiate_version()).await {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                let message = if managed_wsl {
                    t!("engine.error.connect_managed", error = error).into_owned()
                } else {
                    error.to_string()
                };
                return Err(EngineError::Connect(message));
            }
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: t!("engine.action.connect_engine").into_owned(),
                    seconds: connect_timeout.as_secs(),
                });
            }
        };
        drop(negotiation);
        Ok(Self {
            client,
            managed_wsl,
        })
    }

    pub async fn sample_container_stats(&self) -> Result<super::ContainerStatsBatch, EngineError> {
        super::container_detail::sample_all(&self.client).await
    }

    pub async fn snapshot(&self) -> Result<EngineSnapshot, EngineError> {
        let containers_options = ListContainersOptionsBuilder::default().all(true).build();
        // Top-level images only (like `docker images`), without
        // intermediate build layers.
        let images_options = ListImagesOptionsBuilder::default().all(false).build();
        let version_action = t!("engine.action.read_engine_version");
        let info_action = t!("engine.action.read_engine_info");
        let containers_action = t!("engine.action.list_containers");
        let images_action = t!("engine.action.list_images");
        let volumes_action = t!("engine.action.list_volumes");
        let (version, info, containers, images, volumes) = tokio::try_join!(
            with_timeout(&version_action, REQUEST_TIMEOUT, self.client.version()),
            with_timeout(&info_action, REQUEST_TIMEOUT, self.client.info()),
            with_timeout(
                &containers_action,
                REQUEST_TIMEOUT,
                self.client.list_containers(Some(containers_options)),
            ),
            with_timeout(
                &images_action,
                REQUEST_TIMEOUT,
                self.client.list_images(Some(images_options)),
            ),
            with_timeout(
                &volumes_action,
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
                    let name = info
                        .name
                        .unwrap_or_else(|| t!("engine.info.local_engine").into_owned());
                    if self.managed_wsl {
                        format!("Rusker WSL2 / {name}")
                    } else {
                        name
                    }
                },
                version: version
                    .version
                    .unwrap_or_else(|| t!("engine.info.unknown").into_owned()),
                api_version: version
                    .api_version
                    .unwrap_or_else(|| t!("engine.info.unknown").into_owned()),
                operating_system: info
                    .operating_system
                    .or(version.os)
                    .unwrap_or_else(|| t!("engine.info.unknown").into_owned()),
                architecture: info
                    .architecture
                    .or(version.arch)
                    .unwrap_or_else(|| t!("engine.info.unknown").into_owned()),
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
        request.validate().map_err(|error| {
            EngineError::operation(t!("engine.action.validate_container_config"), error)
        })?;
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
            &t!("engine.action.create_container"),
            ACTION_TIMEOUT,
            self.client.create_container(Some(options.build()), body),
        )
        .await?;
        let id = created.id;

        if let Err(start_error) = with_timeout(
            &t!("engine.action.start_new_container"),
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
                &t!("engine.action.cleanup_new_container"),
                ACTION_TIMEOUT,
                self.client.remove_container(&id, Some(cleanup_options)),
            )
            .await;
            let cleanup_message = match cleanup {
                Ok(()) => t!("engine.error.new_container_cleaned").into_owned(),
                Err(error) => t!(
                    "engine.error.new_container_cleanup_failed",
                    id = id,
                    error = error
                )
                .into_owned(),
            };
            return Err(EngineError::operation(
                t!("engine.action.create_and_run_container"),
                t!(
                    "engine.error.with_cleanup",
                    error = start_error,
                    cleanup = cleanup_message
                ),
            ));
        }
        Ok(id)
    }

    pub async fn replace_container(
        &self,
        id: &str,
        image: Option<String>,
    ) -> Result<String, EngineError> {
        super::replace::replace_container(self, id, image.as_deref()).await
    }

    pub async fn container_action(
        &self,
        id: &str,
        action: ContainerAction,
    ) -> Result<(), EngineError> {
        match action {
            ContainerAction::Start => {
                with_timeout(
                    &action.label(),
                    ACTION_TIMEOUT,
                    self.client.start_container(id, None),
                )
                .await
            }
            ContainerAction::Stop => {
                with_timeout(
                    &action.label(),
                    ACTION_TIMEOUT,
                    self.client.stop_container(id, None),
                )
                .await
            }
            ContainerAction::Pause => {
                with_timeout(
                    &action.label(),
                    ACTION_TIMEOUT,
                    self.client.pause_container(id),
                )
                .await
            }
            ContainerAction::Unpause => {
                with_timeout(
                    &action.label(),
                    ACTION_TIMEOUT,
                    self.client.unpause_container(id),
                )
                .await
            }
            ContainerAction::Restart => {
                with_timeout(
                    &action.label(),
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
                    &action.label(),
                    ACTION_TIMEOUT,
                    self.client.remove_container(id, Some(options)),
                )
                .await
            }
        }
    }

    /// Applies an action to each container independently. Successful changes
    /// are kept when another container fails; one message is returned per failure.
    pub async fn container_actions(&self, ids: &[String], action: ContainerAction) -> Vec<String> {
        let mut failures = Vec::new();
        for id in ids {
            if let Err(error) = self.container_action(id, action).await {
                failures.push(format!("{}: {error}", short_id(id)));
            }
        }
        failures
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

    pub async fn container_inspect(&self, id: &str) -> Result<String, EngineError> {
        let inspect = with_timeout(
            &t!("engine.action.read_container_detail"),
            REQUEST_TIMEOUT,
            self.client.inspect_container(id, None),
        )
        .await?;
        serde_json::to_string_pretty(&inspect).map_err(|error| {
            EngineError::operation(t!("engine.action.format_container_detail"), error)
        })
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
            while let Some(output) = stream.try_next().await.map_err(|error| {
                EngineError::operation(t!("engine.action.read_container_logs"), error)
            })? {
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
                action: t!("engine.action.read_container_logs").into_owned(),
                seconds: REQUEST_TIMEOUT.as_secs(),
            }),
        }
    }

    pub async fn open_container_shell(
        &self,
        id: &str,
    ) -> Result<super::ContainerShell, EngineError> {
        super::container_detail::open_shell(&self.client, id).await
    }

    pub async fn list_container_directory(
        &self,
        id: &str,
        path: &str,
    ) -> Result<DirectoryListing, EngineError> {
        super::container_detail::list_directory(&self.client, id, path).await
    }

    pub async fn container_stats(&self, id: &str) -> Result<super::ContainerStats, EngineError> {
        super::container_detail::stats(&self.client, id).await
    }

    pub async fn image_detail(&self, id: &str) -> Result<ImageDetail, EngineError> {
        let detail_action = t!("engine.action.read_image_detail");
        let history_action = t!("engine.action.read_image_history");
        let (image, history) = tokio::try_join!(
            with_timeout(
                &detail_action,
                REQUEST_TIMEOUT,
                self.client.inspect_image(id)
            ),
            with_timeout(
                &history_action,
                REQUEST_TIMEOUT,
                self.client.image_history(id)
            ),
        )?;
        Ok(image_detail_dto(image, history))
    }

    pub async fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        let options = RemoveImageOptionsBuilder::default()
            .force(false)
            .noprune(false)
            .build();
        with_timeout(
            &t!("engine.action.remove_image"),
            ACTION_TIMEOUT,
            self.client.remove_image(id, Some(options), None),
        )
        .await?;
        Ok(())
    }

    pub async fn pull_image(&self, reference: &str) -> Result<(), EngineError> {
        self.pull_image_with_progress(reference, |_| {}).await
    }

    pub async fn pull_image_with_progress(
        &self,
        reference: &str,
        mut on_progress: impl FnMut(bollard::models::CreateImageInfo) + Send,
    ) -> Result<(), EngineError> {
        let mut options = CreateImageOptionsBuilder::default();
        options = options.from_image(reference);
        if needs_default_tag(reference) {
            options = options.tag("latest");
        }
        // Bollard's timeout applies to waiting for response headers, not just
        // connecting. Pulls may spend longer contacting the registry before
        // responding; keep this override local to the pull client.
        let client = self.client.clone().with_timeout(PULL_TIMEOUT);
        let stream = client.create_image(Some(options.build()), None, None);
        pin_mut!(stream);

        let result = timeout(PULL_TIMEOUT, async {
            while let Some(response) = stream.try_next().await.map_err(|error| match error {
                BollardError::RequestTimeoutError => EngineError::Timeout {
                    action: t!("engine.action.pull_image_wait", reference = reference).into_owned(),
                    seconds: client.timeout().as_secs(),
                },
                error => EngineError::operation(
                    t!("engine.action.pull_image", reference = reference),
                    error,
                ),
            })? {
                if let Some(detail) = &response.error_detail {
                    return Err(EngineError::operation(
                        t!("engine.action.pull_image", reference = reference),
                        detail
                            .message
                            .as_deref()
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                t!("engine.error.unknown_engine_error").into_owned()
                            }),
                    ));
                }
                on_progress(response);
            }
            Ok(())
        })
        .await;

        match result {
            Ok(result) => result,
            Err(_) => Err(EngineError::Timeout {
                action: t!("engine.action.pull_image", reference = reference).into_owned(),
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
        let usage = with_timeout(
            &t!("engine.action.volume_disk_usage"),
            DISK_USAGE_TIMEOUT,
            client.df(Some(options)),
        )
        .await?;
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
            validate_volume_name(name).map_err(|error| {
                EngineError::operation(t!("engine.action.create_volume"), error)
            })?;
            self.ensure_volume_absent(&t!("engine.action.create_volume"), name)
                .await?;
        }
        with_timeout(
            &t!("engine.action.create_volume"),
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
            t!("engine.action.remove_volume", name = name).as_ref(),
            ACTION_TIMEOUT,
            self.client.remove_volume(name, Some(options)),
        )
        .await
    }

    /// Creates `target` and copies every file of `source` into it. The new
    /// volume is removed again when copying fails.
    pub async fn clone_volume(&self, source: &str, target: &str) -> Result<(), EngineError> {
        let action = t!("engine.action.clone_volume", source = source).into_owned();
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
                Ok(()) => {
                    t!("engine.error.removed_incomplete_volume", target = target).into_owned()
                }
                Err(cleanup) => t!(
                    "engine.error.incomplete_volume_remove_failed",
                    target = target,
                    error = cleanup
                )
                .into_owned(),
            };
            return Err(EngineError::operation(
                action,
                t!(
                    "engine.error.with_cleanup",
                    error = error,
                    cleanup = cleanup
                ),
            ));
        }
        Ok(())
    }

    /// Deletes all files inside the volume, keeping the volume itself.
    pub async fn empty_volume(&self, name: &str) -> Result<(), EngineError> {
        let action = t!("engine.action.empty_volume", name = name).into_owned();
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
        let action = t!("engine.action.read_volume_content").into_owned();
        let path =
            container_path(relative).map_err(|error| EngineError::operation(&action, error))?;
        let output = self
            .run_helper(
                &action,
                vec![volume_mount(name, VOLUME_MOUNT, true)],
                LIST_SCRIPT,
                vec![format!("RUSKER_PATH={path}")],
                ACTION_TIMEOUT,
            )
            .await?;
        if output.code == 3 {
            return Err(EngineError::operation(
                &action,
                t!(
                    "engine.error.volume_directory_unavailable",
                    relative = relative
                ),
            ));
        }
        output.success(&action)?;
        Ok(parse_listing(&output.stdout))
    }

    /// Extracts a tar archive (optionally gzip/bzip2/xz compressed) from the
    /// local file system into the volume root. Existing files with the same
    /// path are overwritten; ownership in the archive is preserved.
    pub async fn import_volume(&self, name: &str, archive: &Path) -> Result<(), EngineError> {
        let action = t!("engine.action.import_volume", name = name).into_owned();
        let file = tokio::fs::File::open(archive).await.map_err(|error| {
            EngineError::operation(
                &action,
                t!(
                    "engine.error.cannot_read_file",
                    path = archive.display(),
                    error = error
                ),
            )
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
        let action = t!("engine.action.export_volume", name = name).into_owned();
        let partial = partial_export_path(destination);
        let mut file = tokio::fs::File::create(&partial).await.map_err(|error| {
            EngineError::operation(
                &action,
                t!(
                    "engine.error.cannot_create_file",
                    path = partial.display(),
                    error = error
                ),
            )
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
                    EngineError::operation(
                        &action,
                        t!("engine.error.export_write_failed", error = error),
                    )
                })?;
                written += chunk.len() as u64;
            }
            file.flush().await.map_err(|error| {
                EngineError::operation(
                    &action,
                    t!("engine.error.export_write_failed", error = error),
                )
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
                t!(
                    "engine.error.cannot_save_file",
                    path = destination.display(),
                    error = error
                ),
            ));
        }
        Ok(written)
    }

    async fn ensure_volume_absent(&self, action: &str, name: &str) -> Result<(), EngineError> {
        match timeout(REQUEST_TIMEOUT, self.client.inspect_volume(name)).await {
            Ok(Ok(_)) => Err(EngineError::operation(
                action,
                t!("engine.error.volume_exists", name = name),
            )),
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
                t!("engine.action.prepare_helper_image"),
                t!(
                    "engine.error.helper_image_pull_failed",
                    image = HELPER_IMAGE,
                    error = error
                ),
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
                return Err(EngineError::operation(
                    action,
                    t!("engine.error.helper_no_exit_status"),
                ));
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
                    return Err(EngineError::operation(
                        action,
                        t!("engine.error.helper_output_too_large"),
                    ));
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
            t!("engine.error.helper_exit_code", code = self.code).into_owned()
        } else {
            t!(
                "engine.error.helper_exit_code_logs",
                code = self.code,
                logs = bound_logs(detail)
            )
            .into_owned()
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

pub(super) fn container_dto(container: ContainerSummary) -> ContainerDto {
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

    let labels = container.labels.unwrap_or_default();

    ContainerDto {
        id,
        name,
        image: container
            .image
            .unwrap_or_else(|| t!("engine.info.unknown_image").into_owned()),
        image_id: container.image_id.unwrap_or_default(),
        state: container
            .state
            .map(|state| state.to_string())
            .unwrap_or_else(|| "unknown".into()),
        status: container.status.unwrap_or_default(),
        created: container.created.unwrap_or_default(),
        ports,
        volumes,
        compose_project: labels.get(COMPOSE_PROJECT_LABEL).cloned(),
        compose_service: labels.get(COMPOSE_SERVICE_LABEL).cloned(),
        compose_replace: labels.get(COMPOSE_REPLACE_LABEL).cloned(),
        compose_working_dir: labels.get(COMPOSE_WORKING_DIR_LABEL).cloned(),
        compose_config_files: labels.get(COMPOSE_CONFIG_FILES_LABEL).cloned(),
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

fn image_detail_dto(image: ImageInspect, history: Vec<ImageHistoryResponseItem>) -> ImageDetail {
    ImageDetail {
        id: image.id.unwrap_or_default(),
        architecture: image.architecture.unwrap_or_default(),
        os: image.os.unwrap_or_default(),
        // Keep every history entry in the engine's newest-first order.
        layers: history
            .into_iter()
            .map(|layer| ImageLayer {
                id: layer.id,
                created: layer.created,
                created_by: layer.created_by,
                size: u64::try_from(layer.size).unwrap_or(0),
                comment: layer.comment,
            })
            .collect(),
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

#[cfg(all(test, windows))]
#[path = "pipe_pool_tests.rs"]
mod pipe_pool_tests;

#[cfg(test)]
mod tests {
    use super::{
        container_dto, image_detail_dto, needs_default_tag, nvidia_device_requests, DockerEngine,
    };
    use bollard::models::{
        ContainerSummary as BollardContainerSummary, ImageHistoryResponseItem, ImageInspect,
    };

    use crate::model::{
        ImageDetail, ImageLayer, COMPOSE_CONFIG_FILES_LABEL, COMPOSE_PROJECT_LABEL,
        COMPOSE_REPLACE_LABEL, COMPOSE_SERVICE_LABEL, COMPOSE_WORKING_DIR_LABEL,
    };

    #[tokio::test]
    async fn pause_and_resume_use_lifecycle_endpoints_and_report_individual_failures() {
        use crate::model::ContainerAction;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (id, operation, status, body) in [
                ("web", "pause", "204 No Content", ""),
                (
                    "worker",
                    "pause",
                    "409 Conflict",
                    r#"{"message":"container is not running"}"#,
                ),
                ("web", "unpause", "204 No Content", ""),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let length = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(length, 0);
                    request.extend_from_slice(&buffer[..length]);
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with("POST "));
                assert!(request.contains(&format!("/containers/{id}/{operation} ")));
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let engine = DockerEngine {
            client: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                5,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            managed_wsl: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let failures = engine
                .container_actions(&["web".into(), "worker".into()], ContainerAction::Pause)
                .await;
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("worker"));
            assert!(failures[0].contains("container is not running"));
            engine
                .container_action("web", ContainerAction::Unpause)
                .await
                .unwrap();
            server.await.unwrap();
        })
        .await
        .expect("mock lifecycle requests should finish promptly");
    }

    #[tokio::test]
    async fn pull_uses_its_own_timeout_without_changing_other_requests() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = socket.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0);
                request.extend_from_slice(&buffer[..length]);
            }
            assert!(String::from_utf8_lossy(&request).contains("/images/create?"));
            // Exceed the original client's zero-second request timeout.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let body = "{\"status\":\"Pull complete\"}\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let engine = DockerEngine {
            client: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                0,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            managed_wsl: false,
        };
        let mut statuses = Vec::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.pull_image_with_progress("alpine:latest", |response| {
                statuses.push(response.status.unwrap_or_default());
            }),
        )
        .await
        .expect("mock pull should finish promptly");
        result.unwrap();
        assert_eq!(statuses, ["Pull complete"]);
        assert_eq!(engine.client.timeout(), std::time::Duration::ZERO);
        server.await.unwrap();
    }

    #[test]
    fn container_summary_identifies_compose_members_by_project_label() {
        let compose = container_dto(BollardContainerSummary {
            labels: Some(std::collections::HashMap::from([
                (COMPOSE_PROJECT_LABEL.into(), "shop".into()),
                (COMPOSE_SERVICE_LABEL.into(), "web".into()),
                (COMPOSE_REPLACE_LABEL.into(), "frontend".into()),
                (COMPOSE_WORKING_DIR_LABEL.into(), "/srv/shop".into()),
                (
                    COMPOSE_CONFIG_FILES_LABEL.into(),
                    "/srv/shop/compose.yml".into(),
                ),
            ])),
            ..Default::default()
        });
        let standalone = container_dto(BollardContainerSummary::default());

        assert_eq!(compose.compose_project.as_deref(), Some("shop"));
        assert_eq!(compose.compose_service.as_deref(), Some("web"));
        assert_eq!(compose.compose_replace.as_deref(), Some("frontend"));
        assert_eq!(compose.compose_working_dir.as_deref(), Some("/srv/shop"));
        assert_eq!(
            compose.compose_config_files.as_deref(),
            Some("/srv/shop/compose.yml")
        );
        assert!(compose.is_compose());
        assert!(!standalone.is_compose());
    }

    #[test]
    fn maps_image_details_and_preserves_all_history_in_engine_order() {
        let image = ImageInspect {
            id: Some("sha256:image".into()),
            architecture: Some("arm64".into()),
            os: Some("linux".into()),
            ..Default::default()
        };
        let history = vec![
            ImageHistoryResponseItem {
                id: "sha256:newest".into(),
                created: 1_700_000_002,
                created_by: "CMD [\"sh\"]".into(),
                size: 0,
                comment: "metadata only".into(),
                ..Default::default()
            },
            ImageHistoryResponseItem {
                id: "<missing>".into(),
                created: 1_700_000_001,
                created_by: "ADD rootfs /".into(),
                size: 4096,
                comment: "base layer".into(),
                ..Default::default()
            },
        ];

        assert_eq!(
            image_detail_dto(image, history),
            ImageDetail {
                id: "sha256:image".into(),
                architecture: "arm64".into(),
                os: "linux".into(),
                layers: vec![
                    ImageLayer {
                        id: "sha256:newest".into(),
                        created: 1_700_000_002,
                        created_by: "CMD [\"sh\"]".into(),
                        size: 0,
                        comment: "metadata only".into(),
                    },
                    ImageLayer {
                        id: "<missing>".into(),
                        created: 1_700_000_001,
                        created_by: "ADD rootfs /".into(),
                        size: 4096,
                        comment: "base layer".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn image_details_default_missing_inspect_fields_and_empty_history() {
        let image = serde_json::from_str::<ImageInspect>("{}").expect("缺失的可选字段");
        assert_eq!(image_detail_dto(image, Vec::new()), ImageDetail::default());
    }

    #[test]
    fn image_history_sizes_are_converted_without_wrapping() {
        let history = [i64::MIN, -1, 0, i64::MAX]
            .into_iter()
            .map(|size| ImageHistoryResponseItem {
                size,
                ..Default::default()
            })
            .collect();
        let detail = image_detail_dto(ImageInspect::default(), history);

        assert_eq!(detail.layers.len(), 4);
        assert_eq!(detail.layers[0], ImageLayer::default());
        assert_eq!(detail.layers[1], ImageLayer::default());
        assert_eq!(detail.layers[2], ImageLayer::default());
        assert_eq!(detail.layers[3].size, i64::MAX as u64);
    }

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
