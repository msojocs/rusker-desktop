//! Explicit, non-destructive copies from desktop engines into the managed engine.

use super::*;
#[path = "desktop_import/mounts.rs"]
mod mounts;
#[path = "desktop_import/networks.rs"]
mod networks;
use bollard::models::{ContainerInspectResponse, NetworkConnectRequest};
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
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
    pub compose_working_dir: Option<String>,
    pub compose_config_files: Option<String>,
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
            let blocked_reason = match &inspected {
                Ok(inspect) => {
                    match validate_source(inspect).and_then(|()| mounts::bind_mounts(inspect)) {
                        Ok(bind_mounts) => match mounts::check_bind_paths(&bind_mounts).await {
                            Err(error) => Some(error.to_string()),
                            Ok(()) => networks::inspect_networks(&source, inspect)
                                .await
                                .err()
                                .map(|error| error.to_string()),
                        },
                        Err(error) => Some(error.to_string()),
                    }
                }
                Err(error) => Some(error.to_string()),
            };
            let labels = inspected
                .as_ref()
                .ok()
                .and_then(|inspect| inspect.config.as_ref())
                .and_then(|config| config.labels.as_ref());
            entries.push(DesktopContainer {
                id: dto.id,
                name: dto.name,
                image: dto.image,
                state: dto.state,
                compose_project: labels
                    .and_then(|labels| labels.get(crate::model::COMPOSE_PROJECT_LABEL))
                    .cloned(),
                compose_service: labels
                    .and_then(|labels| labels.get(crate::model::COMPOSE_SERVICE_LABEL))
                    .cloned(),
                compose_working_dir: labels
                    .and_then(|labels| labels.get(crate::model::COMPOSE_WORKING_DIR_LABEL))
                    .cloned(),
                compose_config_files: labels
                    .and_then(|labels| labels.get(crate::model::COMPOSE_CONFIG_FILES_LABEL))
                    .cloned(),
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
        self.validate_compose_import_selection(&source, &ids)
            .await?;
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
        let mut imported_networks = HashMap::new();
        let mut created_networks = HashMap::new();
        let mut network_users: HashMap<String, usize> = HashMap::new();
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
            let mut image = None;
            let result = self
                .import_desktop_container(
                    &source,
                    id,
                    &mut image,
                    &batch,
                    scratch.path(),
                    &mut volumes,
                    &mut created_volumes,
                    &mut imported_networks,
                    &mut created_networks,
                    &mut network_users,
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
                        match self.remove_volume(&target).await {
                            Ok(()) => {
                                volumes.remove(&original);
                            }
                            Err(error) => detail.push_str(&t!(
                                "engine.desktop.leftover_volume_cleanup_failed",
                                target = target,
                                error = error
                            )),
                        }
                    }
                    let unused: Vec<_> = created_networks
                        .iter()
                        .filter(|(source_id, _)| {
                            network_users.get(*source_id).copied().unwrap_or(0) == 0
                        })
                        .map(|(source_id, target_id)| (source_id.clone(), target_id.clone()))
                        .collect();
                    for (source_id, target_id) in unused {
                        let removed = with_timeout(
                            &t!("engine.desktop.network_action", name = target_id),
                            ACTION_TIMEOUT,
                            self.client.remove_network(&target_id),
                        )
                        .await;
                        match removed {
                            Ok(()) => {
                                created_networks.remove(&source_id);
                                imported_networks.remove(&source_id);
                            }
                            Err(error) => detail.push_str(&t!(
                                "engine.desktop.leftover_network_cleanup_failed",
                                target = target_id,
                                error = error
                            )),
                        }
                    }
                    // An interrupted create may have succeeded remotely. Do not delete its image.
                    if let Some(image) = image {
                        detail.push_str(&t!("engine.desktop.interrupted_hint", image = image));
                    }
                    report.failures.push(detail);
                }
            }
            progress.send_modify(|progress| progress.completed = index + 1);
        }
        Ok(report)
    }

    async fn validate_compose_import_selection(
        &self,
        source: &Self,
        ids: &[String],
    ) -> Result<(), EngineError> {
        let source_containers = with_timeout(
            &t!("engine.desktop.scan_source_containers"),
            REQUEST_TIMEOUT,
            source.client.list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            )),
        )
        .await?;
        let selected: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        let projects: std::collections::HashSet<String> = source_containers
            .iter()
            .filter(|container| {
                container.id.as_deref().is_some_and(|id| {
                    selected
                        .iter()
                        .any(|selected| id == *selected || id.starts_with(selected))
                })
            })
            .filter_map(|container| {
                container
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(crate::model::COMPOSE_PROJECT_LABEL))
                    .cloned()
            })
            .collect();
        if !projects.is_empty() {
            for project in &projects {
                let members: Vec<&str> = source_containers
                    .iter()
                    .filter(|container| {
                        container
                            .labels
                            .as_ref()
                            .and_then(|labels| labels.get(crate::model::COMPOSE_PROJECT_LABEL))
                            == Some(project)
                    })
                    .filter_map(|container| container.id.as_deref())
                    .collect();
                if members.iter().any(|id| {
                    !selected
                        .iter()
                        .any(|selected| id == selected || id.starts_with(selected))
                }) {
                    return Err(EngineError::operation(
                        t!("engine.desktop.import_desktop_containers"),
                        t!("engine.desktop.compose_group_incomplete", project = project),
                    ));
                }
            }
            let target_containers = with_timeout(
                &t!("engine.desktop.check_target_engine"),
                REQUEST_TIMEOUT,
                self.client.list_containers(Some(
                    ListContainersOptionsBuilder::default().all(true).build(),
                )),
            )
            .await?;
            for project in projects {
                if target_containers.iter().any(|container| {
                    container
                        .labels
                        .as_ref()
                        .and_then(|labels| labels.get(crate::model::COMPOSE_PROJECT_LABEL))
                        == Some(&project)
                }) {
                    return Err(EngineError::operation(
                        t!("engine.desktop.import_desktop_containers"),
                        t!("engine.desktop.compose_project_exists", project = project),
                    ));
                }
            }
        }
        for id in ids {
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
                .unwrap_or_default()
                .trim_start_matches('/');
            match timeout(REQUEST_TIMEOUT, self.client.inspect_container(name, None)).await {
                Ok(Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                })) => {}
                Ok(Ok(_)) => {
                    return Err(EngineError::operation(
                        t!("engine.desktop.import_container"),
                        t!("engine.desktop.target_container_exists", name = name),
                    ));
                }
                Ok(Err(error)) => {
                    return Err(EngineError::operation(
                        t!("engine.desktop.check_target_container_name"),
                        error,
                    ));
                }
                Err(_) => {
                    return Err(EngineError::Timeout {
                        action: t!("engine.desktop.check_target_container_name").into_owned(),
                        seconds: REQUEST_TIMEOUT.as_secs(),
                    });
                }
            }
            let binds = mounts::bind_mounts(&inspect)?;
            mounts::check_bind_paths(&binds).await?;
            for mount in inspect.mounts.as_deref().unwrap_or_default() {
                if mount.typ.as_deref() == Some("volume") {
                    let name = mount.name.as_deref().unwrap_or_default();
                    source.ensure_volume_quiet(name).await?;
                    let volume = with_timeout(
                        &t!("engine.desktop.check_source_volume"),
                        REQUEST_TIMEOUT,
                        source.client.inspect_volume(name),
                    )
                    .await?;
                    if volume.driver != "local" || !volume.options.is_empty() {
                        return Err(EngineError::operation(
                            t!("engine.desktop.import_volume"),
                            t!("engine.desktop.volume_not_plain_local", name = name),
                        ));
                    }
                    self.ensure_volume_absent(&t!("engine.desktop.check_target_volume"), name)
                        .await?;
                }
            }
            for plan in networks::inspect_networks(source, &inspect).await? {
                networks::check_network_absent(self, &plan.name).await?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn import_desktop_container(
        &self,
        source: &Self,
        id: &str,
        imported_image: &mut Option<String>,
        batch: &str,
        scratch: &Path,
        volumes: &mut HashMap<String, String>,
        created_volumes: &mut Vec<(String, String)>,
        imported_networks: &mut HashMap<String, String>,
        created_networks: &mut HashMap<String, String>,
        network_users: &mut HashMap<String, usize>,
        progress: &watch::Sender<DesktopImportProgress>,
    ) -> Result<String, EngineError> {
        let inspect = with_timeout(
            &t!("engine.desktop.read_source_container"),
            REQUEST_TIMEOUT,
            source.client.inspect_container(id, None),
        )
        .await?;
        validate_source(&inspect)?;
        let image = imported_image_reference(&inspect)?;
        *imported_image = Some(image.clone());
        let planned_bind_mounts = mounts::bind_mounts(&inspect)?;
        mounts::check_bind_paths(&planned_bind_mounts).await?;
        let network_plans = networks::inspect_networks(source, &inspect).await?;
        for plan in &network_plans {
            if !imported_networks.contains_key(&plan.source_id) {
                networks::check_network_absent(self, &plan.name).await?;
            }
        }
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
        let mut body = create_body(&inspect, &image)?;
        let mut mounts = body
            .host_config
            .as_mut()
            .unwrap()
            .mounts
            .take()
            .unwrap_or_default();
        mounts.extend(planned_bind_mounts.iter().cloned());
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
                let target = original.to_string();
                self.ensure_volume_absent(&t!("engine.desktop.check_target_volume"), &target)
                    .await?;
                let mut labels = volume.labels.clone();
                labels.insert("com.rusker.desktop.import.batch".into(), batch.into());
                with_timeout(
                    &t!("engine.desktop.create_target_volume"),
                    ACTION_TIMEOUT,
                    self.client.create_volume(VolumeCreateRequest {
                        name: Some(target.clone()),
                        labels: (!labels.is_empty()).then_some(labels),
                        ..Default::default()
                    }),
                )
                .await?;
                let created_volume = with_timeout(
                    &t!("engine.desktop.check_target_volume"),
                    REQUEST_TIMEOUT,
                    self.client.inspect_volume(&target),
                )
                .await?;
                if created_volume
                    .labels
                    .get("com.rusker.desktop.import.batch")
                    .map(String::as_str)
                    != Some(batch)
                {
                    return Err(EngineError::operation(
                        t!("engine.desktop.create_target_volume"),
                        t!("engine.desktop.resource_ownership", name = target),
                    ));
                }
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
            .repo(&image)
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
        let mut attached_networks = Vec::with_capacity(network_plans.len());
        for plan in network_plans {
            let target_id = if let Some(target_id) = imported_networks.get(&plan.source_id) {
                target_id.clone()
            } else {
                let target_id = networks::ensure_network(self, &plan, batch).await?;
                imported_networks.insert(plan.source_id.clone(), target_id.clone());
                created_networks.insert(plan.source_id.clone(), target_id.clone());
                target_id
            };
            attached_networks.push((
                plan.source_id,
                plan.name,
                target_id,
                plan.endpoint,
                plan.primary,
            ));
        }
        if let Some((_, primary_name, _, primary_endpoint, _)) =
            attached_networks.iter().find(|network| network.4)
        {
            let mut value = serde_json::to_value(&body).map_err(|error| {
                EngineError::operation(t!("engine.desktop.convert_container_config"), error)
            })?;
            value["HostConfig"]["NetworkMode"] = primary_name.clone().into();
            let endpoint = serde_json::to_value(primary_endpoint).map_err(|error| {
                EngineError::operation(
                    t!("engine.desktop.network_action", name = primary_name),
                    error,
                )
            })?;
            let endpoints = serde_json::Map::from_iter([(primary_name.clone(), endpoint)]);
            value["NetworkingConfig"] = serde_json::json!({
                "EndpointsConfig": endpoints
            });
            body = serde_json::from_value(value).map_err(|error| {
                EngineError::operation(t!("engine.desktop.convert_container_config"), error)
            })?;
        }
        mounts::check_bind_paths(&planned_bind_mounts).await?;
        let created = with_timeout(
            &t!("engine.desktop.create_imported_container"),
            ACTION_TIMEOUT,
            self.client.create_container(
                Some(CreateContainerOptionsBuilder::default().name(name).build()),
                body,
            ),
        )
        .await?;
        for (_, _, network_id, endpoint, _) in attached_networks.iter().filter(|network| !network.4)
        {
            let request = NetworkConnectRequest {
                container: created.id.clone(),
                endpoint_config: Some(endpoint.clone()),
            };
            if let Err(error) = with_timeout(
                &t!("engine.desktop.network_action", name = network_id),
                ACTION_TIMEOUT,
                self.client.connect_network(network_id, request),
            )
            .await
            {
                let options = RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .v(false)
                    .build();
                if let Err(cleanup) = with_timeout(
                    &t!("engine.desktop.cleanup_imported_container"),
                    ACTION_TIMEOUT,
                    self.client.remove_container(&created.id, Some(options)),
                )
                .await
                {
                    created_volumes.clear();
                    for (source_id, _, _, _, _) in &attached_networks {
                        *network_users.entry(source_id.clone()).or_default() += 1;
                    }
                    return Err(EngineError::operation(
                        t!("engine.desktop.network_action", name = network_id),
                        format!(
                            "{error}; {}",
                            t!(
                                "engine.desktop.leftover_container_cleanup_failed",
                                id = created.id,
                                error = cleanup
                            )
                        ),
                    ));
                }
                return Err(EngineError::operation(
                    t!("engine.desktop.network_action", name = network_id),
                    error,
                ));
            }
        }
        for (source_id, _, _, _, _) in &attached_networks {
            *network_users.entry(source_id.clone()).or_default() += 1;
        }
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

fn imported_image_reference(inspect: &ContainerInspectResponse) -> Result<String, EngineError> {
    let reference = inspect
        .config
        .as_ref()
        .and_then(|config| config.image.as_deref())
        .unwrap_or_default();
    let image = crate::container::ImageTagReference::parse(reference);
    let tag = image
        .default_tag()
        .filter(|_| !image.repository.is_empty())
        .ok_or_else(|| {
            EngineError::operation(
                t!("engine.desktop.import_fs_image"),
                t!("engine.desktop.image_tag_unavailable", image = reference),
            )
        })?;
    Ok(format!("{}:{tag}", image.repository))
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
            Some("tmpfs" | "bind") => {}
            _ => return reject(t!("engine.desktop.unsupported_mounts")),
        }
    }
    if let Some(host) = &inspect.host_config {
        if host.mounts.as_ref().is_some_and(|mounts| {
            mounts.iter().any(|mount| {
                !matches!(
                    mount.typ,
                    Some(MountType::VOLUME | MountType::TMPFS | MountType::BIND)
                )
            })
        }) {
            return reject(t!("engine.desktop.unsupported_mounts"));
        }
        // Bind mounts are planned separately from Docker's resolved mount list;
        // volume declarations are copied through the shared-volume path below.
        if host
            .network_mode
            .as_deref()
            .is_some_and(|mode| mode.starts_with("container:"))
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
    if let Some(labels) = body.labels.as_mut() {
        // Preserve Compose's ownership marker, but force a later `compose up`
        // to reconcile the imported runtime configuration instead of reusing it.
        if labels.contains_key("com.docker.compose.config-hash") {
            labels.insert(
                "com.docker.compose.config-hash".into(),
                "rusker-import".into(),
            );
        }
        labels.remove("com.docker.compose.image");
    }
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

    #[test]
    fn imported_images_preserve_the_source_repository_and_tag() {
        for (reference, expected) in [
            ("nginx:1.27", "nginx:1.27"),
            ("nginx", "nginx:latest"),
            ("registry:5000/team/app:v1", "registry:5000/team/app:v1"),
            ("registry:5000/team/app", "registry:5000/team/app:latest"),
        ] {
            let inspect = serde_json::from_value(json!({"Config":{"Image":reference}})).unwrap();
            assert_eq!(imported_image_reference(&inspect).unwrap(), expected);
        }
    }

    #[test]
    fn imported_images_do_not_invent_tags_for_digests_or_image_ids() {
        for reference in [
            String::new(),
            format!("sha256:{}", "a".repeat(64)),
            format!("nginx@sha256:{}", "a".repeat(64)),
        ] {
            let inspect = serde_json::from_value(json!({"Config":{"Image":reference}})).unwrap();
            assert!(imported_image_reference(&inspect).is_err());
        }
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
                &mut None,
                "test",
                scratch.path(),
                &mut HashMap::new(),
                &mut Vec::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
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
        assert!(requests[1].contains("repo=old%3Alatest"));
        assert!(requests[2].contains("old:latest"));
        assert!(source_task
            .await
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET ")));
        assert!(!scratch.path().join("rootfs.tar").exists());
    }

    #[tokio::test]
    async fn copies_volume_data_and_preserves_compose_volume_name() {
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
                "/volumes/data",
                404,
                json!({"message":"not found"}).to_string(),
            ),
            (
                "/volumes/create",
                201,
                json!({"Name":"data","Driver":"local","Mountpoint":"/volumes/data","Labels":{"com.rusker.desktop.import.batch":"test"},"Options":{},"Scope":"local"}).to_string(),
            ),
            (
                "/volumes/data",
                200,
                json!({"Name":"data","Driver":"local","Mountpoint":"/volumes/data","Labels":{"com.rusker.desktop.import.batch":"test"},"Options":{},"Scope":"local"}).to_string(),
            ),
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
        let mut imported_networks = HashMap::new();
        let mut created_networks = HashMap::new();
        let mut network_users = HashMap::new();
        timeout(
            Duration::from_secs(5),
            target.import_desktop_container(
                &source,
                "source",
                &mut None,
                "test",
                scratch.path(),
                &mut volumes,
                &mut created,
                &mut imported_networks,
                &mut created_networks,
                &mut network_users,
                &progress,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(volumes["data"], "data");
        let requests = target_task.await.unwrap();
        assert!(requests[6].starts_with("PUT "));
        assert!(requests[6].contains("volume-stream"));
        let body = requests[9].split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["HostConfig"]["Mounts"][0]["Source"], "data");
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
                &mut None,
                "test",
                scratch.path(),
                &mut HashMap::new(),
                &mut Vec::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
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
            json!({"State":{"Running":false},"Platform":"linux","Config":{"Image":"old","Cmd":["sleep","30"],"Entrypoint":["sh"],"Env":["A=b"],"User":"1000","WorkingDir":"/data","ExposedPorts":{"80/tcp":{}},"Labels":{"app":"test","com.docker.compose.project":"shop","com.docker.compose.service":"web","com.docker.compose.image":"source-image","com.docker.compose.config-hash":"stale-hash"}},"HostConfig":{"Binds":["data:/data"],"PortBindings":{"80/tcp":[{"HostPort":"8080"}]},"Memory":1024},"Mounts":[{"Type":"volume","Name":"data","Destination":"/data"}]}),
        );
        let body = create_body(&source, "new:latest").unwrap();
        assert_eq!(body.image.as_deref(), Some("new:latest"));
        assert_eq!(body.cmd.unwrap(), ["sleep", "30"]);
        assert_eq!(body.env.unwrap(), ["A=b"]);
        assert_eq!(body.user.as_deref(), Some("1000"));
        assert_eq!(body.exposed_ports.unwrap(), ["80/tcp"]);
        let labels = body.labels.unwrap();
        assert_eq!(
            labels
                .get(crate::model::COMPOSE_PROJECT_LABEL)
                .map(String::as_str),
            Some("shop")
        );
        assert_eq!(
            labels
                .get(crate::model::COMPOSE_SERVICE_LABEL)
                .map(String::as_str),
            Some("web")
        );
        assert!(!labels.contains_key("com.docker.compose.image"));
        assert_eq!(
            labels
                .get("com.docker.compose.config-hash")
                .map(String::as_str),
            Some("rusker-import")
        );
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
    fn rejects_running_container_namespace_and_unsupported_mounts() {
        for extra in [
            json!({"State":{"Running":true}}),
            json!({"HostConfig":{"NetworkMode":"container:other"}}),
            json!({"HostConfig":{"IpcMode":"container:other"}}),
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
