use std::{collections::HashMap, time::Duration};

use bollard::{
    models::{ContainerCreateBody, ContainerSummary, HostConfig, ImageSummary, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
        ListImagesOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
        RemoveImageOptionsBuilder,
    },
    Docker,
};
use futures_util::{pin_mut, TryStreamExt};
use tokio::time::timeout;

use crate::{
    engine::{with_timeout, EngineError},
    format::{bound_logs, short_id},
    model::{
        ContainerAction, ContainerSummary as ContainerDto, CreateContainerRequest, EngineInfo,
        EngineSnapshot, ImageSummary as ImageDto, PortMapping,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub struct DockerEngine {
    client: Docker,
}

impl DockerEngine {
    pub async fn connect() -> Result<Self, EngineError> {
        let client = Docker::connect_with_local_defaults()
            .map_err(|error| EngineError::Connect(error.to_string()))?;
        let client = match timeout(CONNECT_TIMEOUT, client.negotiate_version()).await {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => return Err(EngineError::Connect(error.to_string())),
            Err(_) => {
                return Err(EngineError::Timeout {
                    action: "连接容器引擎".into(),
                    seconds: CONNECT_TIMEOUT.as_secs(),
                });
            }
        };
        Ok(Self { client })
    }

    pub async fn snapshot(&self) -> Result<EngineSnapshot, EngineError> {
        let containers_options = ListContainersOptionsBuilder::default().all(true).build();
        let images_options = ListImagesOptionsBuilder::default().all(true).build();
        let (version, info, containers, images) = tokio::try_join!(
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
        )?;

        let mut containers: Vec<ContainerDto> = containers.into_iter().map(container_dto).collect();
        containers.sort_by_cached_key(|container| container.name.to_lowercase());
        let mut images: Vec<ImageDto> = images.into_iter().map(image_dto).collect();
        images.sort_by_cached_key(|image| image.tags.first().cloned().unwrap_or_default());

        Ok(EngineSnapshot {
            engine: EngineInfo {
                name: info.name.unwrap_or_else(|| "本地引擎".into()),
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
            },
            containers,
            images,
        })
    }

    pub async fn create_and_run(&self, request: CreateContainerRequest) -> Result<(), EngineError> {
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
        let host_config = if port_bindings.is_empty() {
            None
        } else {
            Some(HostConfig {
                port_bindings: Some(port_bindings),
                ..Default::default()
            })
        };
        let body = ContainerCreateBody {
            image: Some(request.image),
            cmd: (!request.command.is_empty()).then_some(request.command),
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
        Ok(())
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

    ContainerDto {
        id,
        name,
        image: container.image.unwrap_or_else(|| "未知镜像".into()),
        state: container
            .state
            .map(|state| state.to_string())
            .unwrap_or_else(|| "unknown".into()),
        status: container.status.unwrap_or_default(),
        created: container.created.unwrap_or_default(),
        ports,
    }
}

fn image_dto(image: ImageSummary) -> ImageDto {
    ImageDto {
        id: image.id,
        tags: image.repo_tags,
        created: image.created,
        size: u64::try_from(image.size).unwrap_or(0),
    }
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
    use super::needs_default_tag;

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
