use std::{collections::HashMap, time::Duration};

use bollard::{
    models::{
        ContainerCreateBody, ContainerInspectResponse, EndpointSettings, NetworkConnectRequest,
        NetworkDisconnectRequest, NetworkingConfig,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, RenameContainerOptionsBuilder,
    },
    Docker,
};

use super::{docker::DockerEngine, with_timeout, EngineError};
use crate::{container::ImageTagReference, format::short_id, model::COMPOSE_PROJECT_LABEL};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

struct ReplacementPlan {
    name: String,
    temporary_name: String,
    was_running: bool,
    body: ContainerCreateBody,
    networks: Vec<(String, EndpointSettings)>,
    pull_image: Option<String>,
}

impl ReplacementPlan {
    fn new(
        id: &str,
        inspect: ContainerInspectResponse,
        image: Option<&str>,
    ) -> Result<Self, EngineError> {
        let action = t!("engine.action.prepare_container_replacement");
        let config = inspect.config.clone().ok_or_else(|| {
            EngineError::operation(
                action.clone(),
                t!("engine.error.container_replace_config_missing"),
            )
        })?;
        if config
            .labels
            .as_ref()
            .is_some_and(|labels| labels.contains_key(COMPOSE_PROJECT_LABEL))
        {
            return Err(EngineError::operation(
                action,
                t!("engine.error.compose_replace_unsupported"),
            ));
        }
        let name = inspect
            .name
            .as_deref()
            .map(|name| name.trim_start_matches('/'))
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                EngineError::operation(
                    t!("engine.action.prepare_container_replacement"),
                    t!("engine.error.container_replace_config_missing"),
                )
            })?;

        let mut networks: Vec<_> = inspect
            .network_settings
            .and_then(|settings| settings.networks)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, endpoint)| (name, endpoint_for_create(endpoint)))
            .collect();
        networks.sort_by(|left, right| left.0.cmp(&right.0));
        let networking_config = networks.first().map(|(name, endpoint)| NetworkingConfig {
            endpoints_config: Some(HashMap::from([(name.clone(), endpoint.clone())])),
        });
        let mut target_image = None;
        let mut pull_image = None;
        if let Some(image) = image {
            if image.is_empty() || image.chars().any(char::is_whitespace) {
                return Err(EngineError::operation(
                    t!("engine.action.prepare_container_replacement"),
                    t!("engine.error.invalid_replacement_image", image = image),
                ));
            }
            let original_image = config.image.as_deref().ok_or_else(|| {
                EngineError::operation(
                    t!("engine.action.prepare_container_replacement"),
                    t!("engine.error.container_replace_config_missing"),
                )
            })?;
            let original = ImageTagReference::parse(original_image);
            let target = ImageTagReference::parse(image);
            if !original.equivalent(&target) {
                pull_image = Some(image.to_string());
                target_image = Some(image.to_string());
            }
        }
        let mut body: ContainerCreateBody = serde_json::to_value(config)
            .and_then(serde_json::from_value)
            .map_err(|error| {
                EngineError::operation(t!("engine.action.prepare_container_replacement"), error)
            })?;
        body.host_config = inspect.host_config;
        body.networking_config = networking_config;
        if let Some(image) = target_image {
            body.image = Some(image);
        }

        Ok(Self {
            temporary_name: format!("{name}-old-{}", short_id(id)),
            name,
            was_running: inspect
                .state
                .and_then(|state| state.running)
                .unwrap_or(false),
            body,
            networks,
            pull_image,
        })
    }
}

/// Replaces a standalone container while retaining its create-time config,
/// mounts and networks. The old container remains available for rollback until
/// the replacement has started successfully.
pub(super) async fn replace_container(
    engine: &DockerEngine,
    id: &str,
    image: Option<&str>,
) -> Result<String, EngineError> {
    let client = &engine.client;
    let inspect = with_timeout(
        &t!("engine.action.read_container_detail"),
        REQUEST_TIMEOUT,
        client.inspect_container(id, None),
    )
    .await?;
    let plan = ReplacementPlan::new(id, inspect, image)?;

    // Pull only a genuinely different image that is not already available
    // locally, before touching the old container.
    if let Some(reference) = &plan.pull_image {
        let local_image = with_timeout(
            &t!("engine.action.read_image_detail"),
            REQUEST_TIMEOUT,
            client.inspect_image(reference),
        )
        .await;
        if local_image.is_err() {
            engine.pull_image(reference).await?;
        }
    }

    if plan.was_running {
        with_timeout(
            &t!("model.action.stop"),
            ACTION_TIMEOUT,
            client.stop_container(id, None),
        )
        .await?;
    }
    let rename = RenameContainerOptionsBuilder::default()
        .name(&plan.temporary_name)
        .build();
    with_timeout(
        &t!("engine.action.rename_container_for_replacement"),
        ACTION_TIMEOUT,
        client.rename_container(id, rename),
    )
    .await?;

    let mut disconnected = Vec::new();
    let mut new_container_id = None;
    let result: Result<String, EngineError> = async {
        for (network, _) in &plan.networks {
            // Include the attempted network in rollback even if the response
            // fails: the daemon may already have applied the disconnect.
            disconnected.push(network.clone());
            let request = NetworkDisconnectRequest {
                container: id.into(),
                force: Some(true),
            };
            with_timeout(
                &t!(
                    "engine.action.disconnect_replaced_network",
                    network = network
                ),
                ACTION_TIMEOUT,
                client.disconnect_network(network, request),
            )
            .await?;
        }

        let options = CreateContainerOptionsBuilder::default()
            .name(&plan.name)
            .build();
        let created = with_timeout(
            &t!("engine.action.create_container"),
            ACTION_TIMEOUT,
            client.create_container(Some(options), plan.body.clone()),
        )
        .await?;
        new_container_id = Some(created.id.clone());

        // Docker accepts only one endpoint during creation. Attach the
        // remaining networks before starting the replacement.
        for (network, endpoint) in plan.networks.iter().skip(1) {
            let request = NetworkConnectRequest {
                container: created.id.clone(),
                endpoint_config: Some(endpoint.clone()),
            };
            with_timeout(
                &t!(
                    "engine.action.connect_replacement_network",
                    network = network
                ),
                ACTION_TIMEOUT,
                client.connect_network(network, request),
            )
            .await?;
        }

        with_timeout(
            &t!("engine.action.start_new_container"),
            ACTION_TIMEOUT,
            client.start_container(&created.id, None),
        )
        .await?;
        Ok(created.id)
    }
    .await;

    let new_id = match result {
        Ok(new_id) => new_id,
        Err(error) => {
            let rollback_errors = rollback(
                client,
                id,
                &plan,
                new_container_id.as_deref(),
                &disconnected,
            )
            .await;
            let message = if rollback_errors.is_empty() {
                t!("engine.error.replace_restored", error = error).into_owned()
            } else {
                t!(
                    "engine.error.replace_rollback_failed",
                    error = error,
                    rollback = rollback_errors.join("; ")
                )
                .into_owned()
            };
            return Err(EngineError::operation(
                t!("engine.action.replace_container"),
                message,
            ));
        }
    };

    let remove = RemoveContainerOptionsBuilder::default()
        .force(false)
        .v(false)
        .build();
    with_timeout(
        &t!("engine.action.remove_replaced_container"),
        ACTION_TIMEOUT,
        client.remove_container(id, Some(remove)),
    )
    .await
    .map_err(|error| {
        EngineError::operation(
            t!("engine.action.replace_container"),
            t!(
                "engine.error.replacement_cleanup_failed",
                id = new_id,
                error = error
            ),
        )
    })?;
    Ok(new_id)
}

async fn rollback(
    client: &Docker,
    old_id: &str,
    plan: &ReplacementPlan,
    new_id: Option<&str>,
    disconnected: &[String],
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(new_id) = new_id {
        let remove = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(false)
            .build();
        if let Err(error) = with_timeout(
            &t!("engine.action.cleanup_new_container"),
            ACTION_TIMEOUT,
            client.remove_container(new_id, Some(remove)),
        )
        .await
        {
            errors.push(error.to_string());
        }
    }

    for network in disconnected {
        let Some((_, endpoint)) = plan.networks.iter().find(|(name, _)| name == network) else {
            continue;
        };
        let request = NetworkConnectRequest {
            container: old_id.into(),
            endpoint_config: Some(endpoint.clone()),
        };
        if let Err(error) = with_timeout(
            &t!(
                "engine.action.connect_replacement_network",
                network = network
            ),
            ACTION_TIMEOUT,
            client.connect_network(network, request),
        )
        .await
        {
            errors.push(error.to_string());
        }
    }

    let rename = RenameContainerOptionsBuilder::default()
        .name(&plan.name)
        .build();
    if let Err(error) = with_timeout(
        &t!("engine.action.restore_replaced_container"),
        ACTION_TIMEOUT,
        client.rename_container(old_id, rename),
    )
    .await
    {
        errors.push(error.to_string());
    }
    if plan.was_running {
        if let Err(error) = with_timeout(
            &t!("engine.action.restore_replaced_container"),
            ACTION_TIMEOUT,
            client.start_container(old_id, None),
        )
        .await
        {
            errors.push(error.to_string());
        }
    }
    errors
}

fn endpoint_for_create(endpoint: EndpointSettings) -> EndpointSettings {
    EndpointSettings {
        // These are assigned by the engine and must not be carried over to a
        // newly created endpoint.
        endpoint_id: None,
        dns_names: None,
        ..endpoint
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, ContainerState, EndpointSettings, HostConfig,
        NetworkSettings,
    };

    use super::ReplacementPlan;
    use crate::model::COMPOSE_PROJECT_LABEL;

    fn inspect() -> ContainerInspectResponse {
        ContainerInspectResponse {
            name: Some("/web".into()),
            state: Some(ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(ContainerConfig {
                image: Some("nginx:latest".into()),
                env: Some(vec!["TZ=UTC".into()]),
                ..Default::default()
            }),
            host_config: Some(HostConfig {
                privileged: Some(true),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::from([
                    (
                        "zeta".into(),
                        EndpointSettings {
                            aliases: Some(vec!["web".into()]),
                            ..Default::default()
                        },
                    ),
                    (
                        "alpha".into(),
                        EndpointSettings {
                            endpoint_id: Some("old-endpoint".into()),
                            dns_names: Some(vec!["old-name".into()]),
                            ..Default::default()
                        },
                    ),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn replacement_preserves_config_and_prepares_all_networks() {
        let plan = ReplacementPlan::new("abcdef1234567890", inspect(), None).expect("替换计划");

        assert_eq!(plan.name, "web");
        assert_eq!(plan.temporary_name, "web-old-abcdef123456");
        assert!(plan.was_running);
        assert_eq!(plan.body.image.as_deref(), Some("nginx:latest"));
        assert_eq!(plan.body.env.as_deref(), Some(&["TZ=UTC".into()][..]));
        assert_eq!(
            plan.body
                .host_config
                .as_ref()
                .and_then(|config| config.privileged),
            Some(true)
        );
        assert_eq!(plan.networks[0].0, "alpha");
        assert_eq!(plan.networks[0].1.endpoint_id, None);
        assert_eq!(plan.networks[0].1.dns_names, None);
        assert!(plan
            .body
            .networking_config
            .as_ref()
            .and_then(|config| config.endpoints_config.as_ref())
            .is_some_and(|endpoints| endpoints.contains_key("alpha")));
    }

    #[test]
    fn image_switch_changes_the_reference_and_requests_a_pull() {
        let unchanged =
            ReplacementPlan::new("container-id", inspect(), Some("nginx:latest")).expect("原镜像");
        assert_eq!(unchanged.body.image.as_deref(), Some("nginx:latest"));
        assert_eq!(unchanged.pull_image, None);

        let changed_tag =
            ReplacementPlan::new("container-id", inspect(), Some("nginx:1.29")).expect("新 Tag");
        assert_eq!(changed_tag.body.image.as_deref(), Some("nginx:1.29"));
        assert_eq!(changed_tag.pull_image.as_deref(), Some("nginx:1.29"));

        let changed_repository = ReplacementPlan::new(
            "container-id",
            inspect(),
            Some("docker.io/library/nginx:1.29"),
        )
        .expect("新镜像名称");
        assert_eq!(
            changed_repository.body.image.as_deref(),
            Some("docker.io/library/nginx:1.29")
        );
        assert_eq!(
            changed_repository.pull_image.as_deref(),
            Some("docker.io/library/nginx:1.29")
        );
    }

    #[test]
    fn compose_containers_are_rejected_before_mutation() {
        let mut inspect = inspect();
        inspect.config.as_mut().unwrap().labels = Some(HashMap::from([(
            COMPOSE_PROJECT_LABEL.into(),
            "shop".into(),
        )]));

        assert!(ReplacementPlan::new("container-id", inspect, None).is_err());
    }
}
