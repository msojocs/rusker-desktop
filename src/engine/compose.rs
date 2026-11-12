//! Project removal through Compose, including its non-external networks.

use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{process::Command, time::timeout};

use super::{docker::DockerEngine, EngineError};
use crate::model::{
    ContainerAction, ContainerSummary, COMPOSE_PROJECT_LABEL, COMPOSE_SERVICE_LABEL,
};
use bollard::{
    models::ContainerSummary as ApiContainerSummary, query_parameters::ListContainersOptionsBuilder,
};

pub(super) fn remaining_project_error(
    containers: &[ContainerSummary],
    project: &str,
) -> Option<String> {
    let names: Vec<_> = containers
        .iter()
        .filter(|container| container.compose_project.as_deref() == Some(project))
        .map(|container| container.name.as_str())
        .collect();
    (!names.is_empty()).then(|| {
        t!(
            "engine.error.compose_containers_remaining",
            project = project,
            containers = names.join(", ")
        )
        .into_owned()
    })
}

fn legacy_import(
    container: &ApiContainerSummary,
    project: &str,
    approved: &[ContainerSummary],
) -> bool {
    let Some(labels) = &container.labels else {
        return false;
    };
    labels
        .get(COMPOSE_PROJECT_LABEL)
        .is_some_and(|value| value == project)
        && labels.contains_key(COMPOSE_SERVICE_LABEL)
        && !labels.contains_key("com.docker.compose.config-hash")
        && container
            .id
            .as_ref()
            .is_some_and(|id| approved.iter().any(|container| &container.id == id))
}

#[derive(Debug, PartialEq, Eq)]
struct ComposeSource {
    working_dir: Option<String>,
    files: Vec<String>,
}

impl ComposeSource {
    fn from_containers(containers: &[ContainerSummary]) -> Result<Self, EngineError> {
        let container = containers
            .iter()
            .find(|container| {
                container
                    .compose_config_files
                    .as_deref()
                    .is_some_and(|files| !files.trim().is_empty())
            })
            .ok_or_else(|| {
                EngineError::operation(
                    "compose down",
                    t!("engine.error.compose_source_unavailable"),
                )
            })?;
        let working_dir = container
            .compose_working_dir
            .clone()
            .filter(|path| !path.trim().is_empty());
        let files = container
            .compose_config_files
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        // Never discover a configuration in the application's working directory.
        if files.is_empty()
            || working_dir
                .as_deref()
                .is_some_and(|path| !absolute_path(path))
            || files
                .iter()
                .any(|path| !absolute_path(path) && working_dir.is_none())
        {
            return Err(EngineError::operation(
                "compose down",
                t!("engine.error.compose_source_unavailable"),
            ));
        }
        Ok(Self { working_dir, files })
    }

    fn args(&self, project: &str, wsl: bool) -> Vec<String> {
        let path = |path: &str| {
            if wsl {
                crate::runtime_host::to_wsl_path(path).unwrap_or_else(|| path.to_string())
            } else {
                path.to_string()
            }
        };
        let mut args = vec!["compose".into(), "--project-name".into(), project.into()];
        if let Some(directory) = &self.working_dir {
            args.extend(["--project-directory".into(), path(directory)]);
        }
        for file in &self.files {
            args.extend(["--file".into(), path(file)]);
        }
        args.extend(["down".into(), "--remove-orphans".into()]);
        args
    }
}

fn absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with(r"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn docker_binary() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|parent| parent.join("resources/bin/docker.exe"))
        })
        .filter(|path| path.is_file())
    {
        return path;
    }
    PathBuf::from("docker")
}

impl DockerEngine {
    async fn remove_legacy_compose_imports(
        &self,
        project: &str,
        approved: &[ContainerSummary],
    ) -> Result<(), EngineError> {
        let options = ListContainersOptionsBuilder::default().all(true).build();
        let containers = super::with_timeout(
            "compose down",
            Duration::from_secs(15),
            self.client.list_containers(Some(options)),
        )
        .await?;
        // Older imports lack the config-hash marker used by Compose's discovery.
        // Only clean up containers included in the user's confirmed deletion.
        for container in containers
            .iter()
            .filter(|container| legacy_import(container, project, approved))
        {
            let id = container.id.as_deref().unwrap();
            let state = container.state.as_ref().map(ToString::to_string);
            if state.as_deref() == Some("paused") {
                self.container_action(id, ContainerAction::Unpause).await?;
            }
            if !matches!(state.as_deref(), Some("exited" | "created" | "dead")) {
                self.container_action(id, ContainerAction::Stop).await?;
            }
            self.container_action(id, ContainerAction::Remove).await?;
        }
        Ok(())
    }

    pub(super) async fn compose_down(
        &self,
        project: &str,
        containers: &[ContainerSummary],
    ) -> Result<(), EngineError> {
        // Retain the original source for retries after containers have been removed.
        let source = ComposeSource::from_containers(containers)?;
        let mut command = if self.managed_wsl {
            // The managed rootfs includes both the CLI and Compose plugin.
            let mut command = Command::new("wsl.exe");
            command.args([
                "--distribution",
                crate::runtime_host::MANAGED_DISTRO_NAME,
                "--user",
                "root",
                "--cd",
            ]);
            let directory = source.working_dir.as_deref().unwrap_or("/");
            command.arg(
                crate::runtime_host::to_wsl_path(directory)
                    .unwrap_or_else(|| directory.to_string()),
            );
            command.args([
                "--exec",
                "/usr/bin/docker",
                "--host",
                "unix:///var/run/docker.sock",
            ]);
            command
        } else {
            let mut command = Command::new(docker_binary());
            // Match Bollard's endpoint, not the user's current Docker context.
            let host = std::env::var_os("DOCKER_HOST").unwrap_or_else(|| {
                if cfg!(windows) {
                    "npipe:////./pipe/docker_engine".into()
                } else {
                    "unix:///var/run/docker.sock".into()
                }
            });
            command.arg("--host").arg(host);
            if let Some(directory) = &source.working_dir {
                command.current_dir(directory);
            }
            command
        };
        command
            .args(source.args(project, self.managed_wsl))
            .env_remove("DOCKER_CONTEXT")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let duration = Duration::from_secs(5 * 60);
        self.remove_legacy_compose_imports(project, containers)
            .await?;
        let output = timeout(duration, command.output())
            .await
            .map_err(|_| EngineError::Timeout {
                action: "compose down".into(),
                seconds: duration.as_secs(),
            })?
            .map_err(|error| EngineError::operation("compose down", error))?;
        if !output.status.success() {
            let message = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim()
            );
            return Err(EngineError::operation(
                "compose down",
                format!("{}: {}", output.status, message.trim()),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(directory: Option<&str>, files: &str) -> ContainerSummary {
        ContainerSummary {
            compose_working_dir: directory.map(String::from),
            compose_config_files: Some(files.into()),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn legacy_cleanup_only_selects_confirmed_members_missing_the_compose_marker() {
        let mut container = ApiContainerSummary {
            id: Some("legacy".into()),
            labels: Some(std::collections::HashMap::from([
                (COMPOSE_PROJECT_LABEL.into(), "shop".into()),
                (COMPOSE_SERVICE_LABEL.into(), "web".into()),
            ])),
            ..Default::default()
        };
        let approved = vec![ContainerSummary {
            id: "legacy".into(),
            ..Default::default()
        }];
        assert!(legacy_import(&container, "shop", &approved));
        assert!(!legacy_import(&container, "other", &approved));
        assert!(!legacy_import(&container, "shop", &[]));
        container
            .labels
            .as_mut()
            .unwrap()
            .insert("com.docker.compose.config-hash".into(), "hash".into());
        assert!(!legacy_import(&container, "shop", &approved));
    }

    #[test]
    fn command_success_is_not_enough_when_project_containers_remain() {
        let containers = vec![ContainerSummary {
            name: "shop-web-1".into(),
            compose_project: Some("shop".into()),
            ..Default::default()
        }];
        assert!(remaining_project_error(&containers, "shop")
            .unwrap()
            .contains("shop-web-1"));
        assert!(remaining_project_error(&containers, "other").is_none());
        assert!(remaining_project_error(&[], "shop").is_none());
    }

    #[tokio::test]
    async fn legacy_import_is_stopped_then_removed_without_deleting_volumes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let containers = serde_json::json!([
                {"Id":"legacy", "State":"running", "Labels":{
                    "com.docker.compose.project":"shop", "com.docker.compose.service":"web"
                }},
                {"Id":"unapproved", "State":"running", "Labels":{
                    "com.docker.compose.project":"shop", "com.docker.compose.service":"db"
                }}
            ])
            .to_string();
            let mut requests = Vec::new();
            for (status, body) in [
                (200, containers),
                (204, String::new()),
                (204, String::new()),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut buffer = [0; 4096];
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let response = format!("HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let engine = DockerEngine {
            client: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                15,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            managed_wsl: false,
        };
        let approved = vec![ContainerSummary {
            id: "legacy".into(),
            ..Default::default()
        }];
        timeout(
            Duration::from_secs(5),
            engine.remove_legacy_compose_imports("shop", &approved),
        )
        .await
        .unwrap()
        .unwrap();
        let requests = task.await.unwrap();
        assert!(requests[0].contains("/containers/json?all=true"));
        assert!(requests[1].starts_with("POST "));
        assert!(requests[1].contains("/containers/legacy/stop"));
        assert!(requests[2].starts_with("DELETE "));
        assert!(requests[2].contains("/containers/legacy?"));
        assert!(requests[2].contains("v=false"));
        assert!(requests[2].contains("force=false"));
    }

    #[test]
    fn down_uses_explicit_project_and_all_files_without_deleting_volumes() {
        let source = ComposeSource::from_containers(&[container(
            Some(r"C:\projects\shop"),
            r"C:\projects\shop\compose.yml, C:\projects\shop\override.yml",
        )])
        .unwrap();
        assert_eq!(
            source.args("shop", true),
            [
                "compose",
                "--project-name",
                "shop",
                "--project-directory",
                "/mnt/c/projects/shop",
                "--file",
                "/mnt/c/projects/shop/compose.yml",
                "--file",
                "/mnt/c/projects/shop/override.yml",
                "down",
                "--remove-orphans",
            ]
        );
        assert_eq!(source.args("shop", false)[4], r"C:\projects\shop");
    }

    #[test]
    fn missing_or_relative_source_is_not_resolved_against_the_app_directory() {
        assert!(ComposeSource::from_containers(&[]).is_err());
        assert!(ComposeSource::from_containers(&[container(None, "compose.yml")]).is_err());
        assert!(
            ComposeSource::from_containers(&[container(Some("relative"), "compose.yml")]).is_err()
        );
        assert!(ComposeSource::from_containers(&[container(Some("/srv/shop"), " , ")]).is_err());
    }

    #[test]
    fn relative_files_keep_the_original_project_directory_and_order() {
        let source = ComposeSource::from_containers(&[container(
            Some("/srv/shop"),
            "compose.yml,override.yml",
        )])
        .unwrap();
        assert_eq!(
            source.args("shop", false),
            [
                "compose",
                "--project-name",
                "shop",
                "--project-directory",
                "/srv/shop",
                "--file",
                "compose.yml",
                "--file",
                "override.yml",
                "down",
                "--remove-orphans",
            ]
        );
    }
}
