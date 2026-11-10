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

    pub fn connection_hint(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::DockerDesktop => t!("engine.desktop.hint_docker_desktop"),
            Self::RancherDesktop => t!("engine.desktop.hint_rancher_desktop"),
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

#[derive(Clone, Debug, Default)]
pub struct DesktopImportProgress {
    /// One-based active container; zero while the batch is being prepared.
    pub current: usize,
    pub completed: usize,
    pub total: usize,
    pub message: String,
}

impl DockerEngine {
    async fn desktop_source(source: DesktopImportSource) -> Result<Self, EngineError> {
        #[cfg(windows)]
        {
            // Never consult DOCKER_HOST or the active Docker context.
            let client = pooled_named_pipe(source.pipe())?;
            let action = t!(
                "engine.desktop.connect_source",
                source = source.label(),
                hint = source.connection_hint()
            )
            .into_owned();
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
                        action: t!("engine.desktop.verify_rancher_identity").into_owned(),
                        seconds: CONNECT_TIMEOUT.as_secs(),
                    })?
                    .map_err(|error| {
                        EngineError::operation(t!("engine.desktop.verify_rancher_identity"), error)
                    })?;
                if !output.status.success() {
                    return Err(EngineError::operation(
                        t!("engine.desktop.verify_rancher_identity"),
                        t!(
                            "engine.desktop.hint_with_stderr",
                            hint = source.connection_hint(),
                            stderr = String::from_utf8_lossy(&output.stderr).trim()
                        ),
                    ));
                }
                let info = with_timeout(
                    &t!("engine.desktop.verify_rancher_pipe"),
                    REQUEST_TIMEOUT,
                    client.info(),
                )
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
            t!(
                "engine.desktop.connect_source_plain",
                source = source.label()
            ),
            t!("engine.desktop.windows_only"),
        ))
    }

    pub(crate) async fn desktop_containers(
        source: DesktopImportSource,
    ) -> Result<Vec<DesktopContainer>, EngineError> {
        let source = Self::desktop_source(source).await?;
        let containers = with_timeout(
            &t!("engine.desktop.scan_source_containers"),
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
                &t!("engine.desktop.read_source_container_config"),
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
        progress: watch::Sender<DesktopImportProgress>,
    ) -> Result<DesktopImportReport, EngineError> {
        if !self.managed_wsl {
            return Err(EngineError::operation(
                t!("engine.desktop.import_desktop_containers"),
                t!("engine.desktop.target_must_be_managed"),
            ));
        }
        let source = Self::desktop_source(source).await?;
        let source_action = t!("engine.desktop.check_source_engine");
        let target_action = t!("engine.desktop.check_target_engine");
        let (source_info, target_info) = tokio::try_join!(
            with_timeout(&source_action, REQUEST_TIMEOUT, source.client.info()),
            with_timeout(&target_action, REQUEST_TIMEOUT, self.client.info()),
        )?;
        if source_info.id.is_none() || source_info.id == target_info.id {
            return Err(EngineError::operation(
                t!("engine.desktop.import_desktop_containers"),
                t!("engine.desktop.same_engine"),
            ));
        }
        // TempDir is private and removes archives (which can contain secrets) on every exit path.
        let scratch = tempfile::tempdir()
            .map_err(|error| EngineError::operation(t!("engine.desktop.prepare_scratch"), error))?;
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
            let _ = progress.send(DesktopImportProgress {
                current: index + 1,
                completed: index,
                total: ids.len(),
                message: t!(
                    "engine.desktop.importing_progress",
                    index = index + 1,
                    total = ids.len(),
                    id = short_id(id)
                )
                .into_owned(),
            });
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
                    let mut detail =
                        t!("engine.desktop.id_detail", id = short_id(id), error = error)
                            .into_owned();
                    // Only resources created by this attempt are eligible for cleanup.
                    for (original, target) in created_volumes {
                        volumes.remove(&original);
                        if let Err(error) = self.remove_volume(&target).await {
                            detail.push_str(&t!(
                                "engine.desktop.leftover_volume_cleanup_failed",
                                target = target,
                                error = error
                            ));
                        }
                    }
                    // An interrupted create may have succeeded remotely. Do not delete its image.
                    detail.push_str(&t!("engine.desktop.interrupted_hint", image = image));
                    report.failures.push(detail);
                }
            }
            progress.send_modify(|progress| progress.completed = index + 1);
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
        progress: &watch::Sender<DesktopImportProgress>,
    ) -> Result<String, EngineError> {
        let inspect = with_timeout(
            &t!("engine.desktop.read_source_container"),
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
            return Err(EngineError::operation(
                t!("engine.desktop.import_container"),
                t!("engine.desktop.source_container_unnamed"),
            ));
        }
        match timeout(REQUEST_TIMEOUT, self.client.inspect_container(name, None)).await {
            Ok(Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            })) => {}
            Ok(Err(error)) => {
                return Err(EngineError::operation(
                    t!("engine.desktop.check_target_container_name"),
                    error,
                ))
            }
            Ok(Ok(_)) => {
                return Err(EngineError::operation(
                    t!("engine.desktop.import_container"),
                    t!("engine.desktop.target_container_exists", name = name),
                ))
            }
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: t!("engine.desktop.check_target_container_name").into_owned(),
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
                    &t!("engine.desktop.check_source_volume"),
                    REQUEST_TIMEOUT,
                    source.client.inspect_volume(original),
                )
                .await?;
                if volume.driver != "local" || !volume.options.is_empty() {
                    return Err(EngineError::operation(
                        t!("engine.desktop.import_volume"),
                        t!("engine.desktop.volume_not_plain_local", name = original),
                    ));
                }
                use sha2::{Digest as _, Sha256};
                let digest = format!("{:x}", Sha256::digest(original.as_bytes()));
                let target = format!("rusker-import-{batch}-{}", &digest[..16]);
                self.create_volume(Some(target.clone())).await?;
                created_volumes.push((original.to_string(), target.clone()));
                let archive = scratch.join("volume.tar");
                progress.send_modify(|progress| {
                    progress.message = t!(
                        "engine.desktop.copy_volume_progress",
                        name = name,
                        volume = original
                    )
                    .into_owned();
                });
                source.export_volume(original, &archive).await?;
                self.import_volume(&target, &archive).await?;
                tokio::fs::remove_file(&archive).await.map_err(|error| {
                    EngineError::operation(t!("engine.desktop.cleanup_temp_volume_archive"), error)
                })?;
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
        progress.send_modify(|progress| {
            progress.message = t!("engine.desktop.copy_rootfs_progress", name = name).into_owned();
        });
        let archive = scratch.join("rootfs.tar");
        let mut file = tokio::fs::File::create(&archive).await.map_err(|error| {
            EngineError::operation(t!("engine.desktop.create_temp_archive"), error)
        })?;
        let export = source.client.clone().with_timeout(DATA_TIMEOUT);
        let stream = export.export_container(id);
        pin_mut!(stream);
        let copy = async {
            while let Some(chunk) = stream.try_next().await.map_err(|error| {
                EngineError::operation(t!("engine.desktop.export_container_fs"), error)
            })? {
                file.write_all(&chunk).await.map_err(|error| {
                    EngineError::operation(t!("engine.desktop.write_temp_archive"), error)
                })?;
            }
            file.flush().await.map_err(|error| {
                EngineError::operation(t!("engine.desktop.write_temp_archive"), error)
            })
        };
        timeout(DATA_TIMEOUT, copy)
            .await
            .map_err(|_| EngineError::Timeout {
                action: t!("engine.desktop.export_container_fs").into_owned(),
                seconds: DATA_TIMEOUT.as_secs(),
            })??;
        drop(file);
        let original_image = with_timeout(
            &t!("engine.desktop.read_source_image_platform"),
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
        let file = tokio::fs::File::open(&archive).await.map_err(|error| {
            EngineError::operation(t!("engine.desktop.read_temp_archive"), error)
        })?;
        let client = self.client.clone().with_timeout(DATA_TIMEOUT);
        let imported = client.create_image(
            Some(options),
            Some(body_try_stream(file_chunks(file))),
            None,
        );
        pin_mut!(imported);
        let load = async {
            while let Some(response) = imported.try_next().await.map_err(|error| {
                EngineError::operation(t!("engine.desktop.import_fs_image"), error)
            })? {
                if let Some(error) = response.error_detail {
                    return Err(EngineError::operation(
                        t!("engine.desktop.import_fs_image"),
                        error.message.unwrap_or_default(),
                    ));
                }
            }
            Ok(())
        };
        timeout(DATA_TIMEOUT, load)
            .await
            .map_err(|_| EngineError::Timeout {
                action: t!("engine.desktop.import_fs_image").into_owned(),
                seconds: DATA_TIMEOUT.as_secs(),
            })??;
        tokio::fs::remove_file(&archive).await.map_err(|error| {
            EngineError::operation(t!("engine.desktop.cleanup_temp_container_archive"), error)
        })?;
        // Recheck after the long copy; a separately started source must not be reported as consistent.
        let current = with_timeout(
            &t!("engine.desktop.recheck_source_state"),
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
        progress.send_modify(|progress| {
            progress.message =
                t!("engine.desktop.create_target_progress", name = name).into_owned();
        });
        let created = with_timeout(
            &t!("engine.desktop.create_imported_container"),
            ACTION_TIMEOUT,
            self.client.create_container(
                Some(CreateContainerOptionsBuilder::default().name(name).build()),
                body,
            ),
        )
        .await?;
        Ok(t!(
            "engine.desktop.imported_name",
            name = name,
            id = short_id(&created.id)
        )
        .into_owned())
    }

    async fn ensure_volume_quiet(&self, name: &str) -> Result<(), EngineError> {
        let filters = HashMap::from([("volume".to_string(), vec![name.to_string()])]);
        let users = with_timeout(
            &t!("engine.desktop.check_source_volume_usage"),
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
                t!("engine.desktop.import_volume"),
                t!("engine.desktop.volume_in_use", name = name),
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
            t!("engine.desktop.verify_rancher_identity"),
            t!("engine.desktop.rancher_identity_mismatch"),
        ));
    }
    Ok(())
}

fn validate_source(inspect: &ContainerInspectResponse) -> Result<(), EngineError> {
    let reject = |message| {
        Err(EngineError::operation(
            t!("engine.desktop.import_source_container"),
            message,
        ))
    };
    let state = inspect.state.as_ref();
    if state.is_none_or(|state| {
        state.running != Some(false)
            || state.paused.unwrap_or(false)
            || state.restarting.unwrap_or(false)
    }) {
        return reject(t!("engine.desktop.stop_source_first"));
    }
    if inspect
        .platform
        .as_deref()
        .is_some_and(|platform| platform != "linux")
    {
        return reject(t!("engine.desktop.linux_only"));
    }
    if inspect.config.is_none() {
        return reject(t!("engine.desktop.missing_container_config"));
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
            _ => return reject(t!("engine.desktop.unsupported_mounts")),
        }
    }
    if let Some(host) = &inspect.host_config {
        if host.mounts.as_ref().is_some_and(|mounts| {
            mounts
                .iter()
                .any(|mount| !matches!(mount.typ, Some(MountType::VOLUME | MountType::TMPFS)))
        }) {
            return reject(t!("engine.desktop.unsupported_mounts"));
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
            return reject(t!("engine.desktop.unresolved_mounts"));
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
            return reject(t!("engine.desktop.custom_network_deps"));
        }
        if host
            .devices
            .as_ref()
            .is_some_and(|devices| !devices.is_empty())
        {
            return reject(t!("engine.desktop.host_devices"));
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
            return reject(t!("engine.desktop.volume_subpath"));
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
        return reject(t!("engine.desktop.custom_network"));
    }
    Ok(())
}

fn create_body(
    inspect: &ContainerInspectResponse,
    image: &str,
) -> Result<ContainerCreateBody, EngineError> {
    validate_source(inspect)?;
    // Docker's Config and create request share wire field names, including healthcheck and stop signal.
    let config = serde_json::to_value(inspect.config.as_ref().unwrap()).map_err(|error| {
        EngineError::operation(t!("engine.desktop.convert_container_config"), error)
    })?;
    let mut body: ContainerCreateBody = serde_json::from_value(config).map_err(|error| {
        EngineError::operation(t!("engine.desktop.convert_container_config"), error)
    })?;
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
        let (progress, _) = watch::channel(DesktopImportProgress::default());
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
        let (progress, _) = watch::channel(DesktopImportProgress::default());
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
        let (progress, _) = watch::channel(DesktopImportProgress::default());
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
        assert!(error.to_string().contains("already exists"));
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
        assert!(error.to_string().contains("stop all of them first"));
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refuses_an_unmanaged_destination_before_connecting_to_desktop() {
        let (mut target, task) = mock_engine(Vec::new()).await;
        target.managed_wsl = false;
        let (progress, _) = watch::channel(DesktopImportProgress::default());
        let error = target
            .import_desktop_containers(
                DesktopImportSource::DockerDesktop,
                vec!["source".into()],
                progress,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("managed engine"));
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
