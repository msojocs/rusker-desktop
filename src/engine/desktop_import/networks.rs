use super::*;
use bollard::models::{ContainerInspectResponse, EndpointSettings, NetworkCreateRequest};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

const NETWORK_ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_LABEL: &str = "com.rusker.desktop.import.batch";

#[derive(Clone, Debug)]
pub(super) struct NetworkPlan {
    pub(super) source_id: String,
    pub(super) name: String,
    pub(super) endpoint: EndpointSettings,
    pub(super) request: NetworkCreateRequest,
    pub(super) primary: bool,
}

/// Resolve custom source networks from container inspect data, in deterministic attach order.
pub(super) async fn inspect_networks(
    source: &DockerEngine,
    inspect: &ContainerInspectResponse,
) -> Result<Vec<NetworkPlan>, EngineError> {
    let endpoints = inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.networks.as_ref());
    let mode = inspect
        .host_config
        .as_ref()
        .and_then(|host| host.network_mode.as_deref())
        .unwrap_or_default();
    if mode.starts_with("container:") {
        return Err(network_unsupported(mode));
    }
    let Some(endpoints) = endpoints else {
        if !matches!(mode, "" | "default" | "bridge" | "host" | "none") {
            return Err(network_unsupported(mode));
        }
        return Ok(Vec::new());
    };
    let primary = if mode.is_empty()
        || mode == "default"
        || mode == "bridge"
        || mode == "host"
        || mode == "none"
    {
        None
    } else {
        Some(mode)
    };
    let mut entries = Vec::new();
    for (name, endpoint) in endpoints {
        if matches!(name.as_str(), "bridge" | "host" | "none") {
            continue;
        }
        let id = endpoint
            .network_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| network_unsupported(name))?;
        entries.push((name.clone(), id.to_string(), endpoint.clone()));
    }
    entries.sort_by(|a, b| network_order(&a.0, &b.0, primary));
    if primary.is_some_and(|primary| !entries.iter().any(|(name, _, _)| name == primary)) {
        return Err(network_unsupported(primary.unwrap_or_default()));
    }
    if matches!(mode, "host" | "none") && !entries.is_empty() {
        return Err(network_unsupported(mode));
    }

    let mut plans = Vec::with_capacity(entries.len());
    for (name, source_id, endpoint) in entries {
        let action = t!("engine.desktop.network_action", name = name);
        let source_network = with_timeout(
            &action,
            REQUEST_TIMEOUT,
            source.client.inspect_network(&source_id, None),
        )
        .await?;
        let network = serde_json::to_value(source_network)
            .map_err(|error| EngineError::operation(action.as_ref(), error))?;
        validate_source_network(&name, &network)?;
        let driver = network
            .get("Driver")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let internal = network
            .get("Internal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let labels = network
            .get("Labels")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let labels: HashMap<String, String> = labels
            .into_iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_string())))
            .collect();
        let request_value = json!({
            "Name": name,
            "CheckDuplicate": true,
            "Driver": driver,
            "Internal": internal,
            "EnableIPv6": false,
            "Options": {},
            "Labels": labels,
        });
        let request = serde_json::from_value(request_value)
            .map_err(|error| EngineError::operation(action.as_ref(), error))?;
        let mut endpoint = clean_endpoint(endpoint, inspect.id.as_deref().unwrap_or_default())?;
        if let Some(service) = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(crate::model::COMPOSE_SERVICE_LABEL))
        {
            let aliases = endpoint.aliases.get_or_insert_with(Vec::new);
            if !aliases.contains(service) {
                aliases.push(service.clone());
            }
        }
        let is_primary = primary == Some(name.as_str());
        plans.push(NetworkPlan {
            source_id,
            name,
            endpoint,
            request,
            primary: is_primary,
        });
    }
    Ok(plans)
}

fn network_order(a: &str, b: &str, primary: Option<&str>) -> std::cmp::Ordering {
    let a_primary = primary == Some(a);
    let b_primary = primary == Some(b);
    b_primary.cmp(&a_primary).then_with(|| a.cmp(b))
}

fn validate_source_network(name: &str, network: &Value) -> Result<(), EngineError> {
    let reject = || network_unsupported(name);
    if network
        .get("Name")
        .and_then(Value::as_str)
        .is_none_or(|n| n != name)
        || network.get("Driver").and_then(Value::as_str) != Some("bridge")
        || network.get("Scope").and_then(Value::as_str) != Some("local")
        || network
            .get("EnableIPv6")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || network
            .get("Attachable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || network
            .get("Ingress")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || network
            .get("Options")
            .and_then(Value::as_object)
            .is_some_and(|v| !v.is_empty())
        || network
            .get("ConfigFrom")
            .and_then(|config| config.get("Network"))
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
        || network
            .get("Peers")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    {
        return Err(reject());
    }
    // Engine-assigned subnet and gateway values are normal for local networks. Reject
    // custom IPAM behavior, but intentionally do not reject those assigned values.
    if let Some(ipam) = network.get("IPAM") {
        if ipam
            .get("Driver")
            .and_then(Value::as_str)
            .is_some_and(|driver| driver != "default")
            || ipam
                .get("Options")
                .and_then(Value::as_object)
                .is_some_and(|v| !v.is_empty())
            || ipam
                .get("Config")
                .and_then(Value::as_array)
                .is_some_and(|configs| {
                    configs.iter().any(|config| {
                        config.as_object().is_some_and(|object| {
                            object
                                .keys()
                                .any(|key| !matches!(key.as_str(), "Subnet" | "Gateway"))
                        })
                    })
                })
        {
            return Err(reject());
        }
    }
    Ok(())
}

fn clean_endpoint(
    endpoint: EndpointSettings,
    source_container_id: &str,
) -> Result<EndpointSettings, EngineError> {
    let mut value = serde_json::to_value(endpoint)
        .map_err(|error| EngineError::operation(t!("engine.desktop.network_action"), error))?;
    let Some(object) = value.as_object_mut() else {
        return Err(network_unsupported("endpoint"));
    };
    // Whitelist portable endpoint data. Engine identity/address/DNS data belongs to
    // the source daemon and must be assigned afresh by the managed daemon.
    let allowed = ["Aliases", "GwPriority"];
    object.retain(|key, _| allowed.contains(&key.as_str()));
    if let Some(aliases) = object.get_mut("Aliases").and_then(Value::as_array_mut) {
        let short_id = source_container_id.get(..12).unwrap_or(source_container_id);
        aliases.retain(|alias| {
            alias.as_str().is_some_and(|alias| {
                !alias.is_empty() && alias != source_container_id && alias != short_id
            })
        });
    }
    serde_json::from_value(value)
        .map_err(|error| EngineError::operation(t!("engine.desktop.network_action"), error))
}

pub(super) async fn check_network_absent(
    target: &DockerEngine,
    name: &str,
) -> Result<(), EngineError> {
    let action = t!("engine.desktop.network_action", name = name);
    match timeout(REQUEST_TIMEOUT, target.client.inspect_network(name, None)).await {
        Ok(Ok(_)) => Err(network_conflict(name)),
        Ok(Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        })) => Ok(()),
        Ok(Err(error)) => Err(EngineError::operation(action, error)),
        Err(_) => Err(EngineError::Timeout {
            action: action.into_owned(),
            seconds: REQUEST_TIMEOUT.as_secs(),
        }),
    }
}

/// Create only the exact resource represented by this plan; never adopt an existing network.
pub(super) async fn ensure_network(
    target: &DockerEngine,
    plan: &NetworkPlan,
    batch: &str,
) -> Result<String, EngineError> {
    check_network_absent(target, &plan.name).await?;
    let action = t!("engine.desktop.network_action", name = plan.name);
    let mut request = plan.request.clone();
    let mut labels = request.labels.take().unwrap_or_default();
    labels.insert(BATCH_LABEL.to_string(), batch.to_string());
    request.labels = Some(labels);
    let created = with_timeout(
        &action,
        NETWORK_ACTION_TIMEOUT,
        target.client.create_network(request),
    )
    .await
    .map_err(|error| EngineError::operation(action.clone(), error))?;
    if created.id.is_empty() {
        return Err(network_changed(&plan.name));
    }
    let inspected = with_timeout(
        &action,
        REQUEST_TIMEOUT,
        target.client.inspect_network(&created.id, None),
    )
    .await
    .map_err(|error| EngineError::operation(action, error))?;
    let value = serde_json::to_value(inspected).map_err(|error| {
        EngineError::operation(t!("engine.desktop.network_action", name = plan.name), error)
    })?;
    let id = value.get("Id").and_then(Value::as_str).unwrap_or_default();
    let labels = value.get("Labels").and_then(Value::as_object);
    if id != created.id
        || value.get("Name").and_then(Value::as_str) != Some(plan.name.as_str())
        || labels
            .and_then(|labels| labels.get(BATCH_LABEL))
            .and_then(Value::as_str)
            != Some(batch)
    {
        return Err(resource_ownership(&plan.name));
    }
    Ok(created.id)
}

fn network_unsupported(name: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.network_action", name = name),
        t!("engine.desktop.network_unsupported", name = name),
    )
}

fn network_conflict(name: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.network_action", name = name),
        t!("engine.desktop.network_conflict", name = name),
    )
}

fn resource_ownership(name: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.network_action", name = name),
        t!("engine.desktop.resource_ownership", name = name),
    )
}

fn network_changed(name: &str) -> EngineError {
    EngineError::operation(
        t!("engine.desktop.network_action", name = name),
        t!("engine.desktop.network_changed", name = name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_keeps_alias_and_gateway_priority_but_drops_source_identity() {
        let endpoint: EndpointSettings = serde_json::from_value(json!({
            "NetworkID":"source-net", "EndpointID":"source-endpoint", "IPAddress":"10.1.0.4",
            "MacAddress":"02:42:0a:01:00:04", "DNSNames":["container", "id"],
            "Aliases":["service", "source-id"], "GwPriority":9, "DriverOpts":{"x":"y"}
        }))
        .unwrap();
        let cleaned = clean_endpoint(endpoint, "source-container-id").unwrap();
        let value = serde_json::to_value(cleaned).unwrap();
        assert_eq!(value["Aliases"], json!(["service", "source-id"]));
        assert_eq!(value["GwPriority"], 9);
        for key in [
            "NetworkID",
            "EndpointID",
            "IPAddress",
            "MacAddress",
            "DNSNames",
            "DriverOpts",
        ] {
            assert!(value.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn network_order_places_host_mode_network_first_then_sorts_stably() {
        let mut names = vec!["zeta", "primary", "alpha"];
        names.sort_by(|a, b| network_order(a, b, Some("primary")));
        assert_eq!(names, ["primary", "alpha", "zeta"]);
        names.sort_by(|a, b| network_order(a, b, None));
        assert_eq!(names, ["alpha", "primary", "zeta"]);
    }

    #[test]
    fn network_validation_accepts_engine_assigned_subnet_and_gateway() {
        let network = json!({"Name":"project_default", "Driver":"bridge", "Scope":"local",
            "Internal":true, "EnableIPv6":false, "IPAM":{"Driver":"default", "Config":[{"Subnet":"172.20.0.0/16","Gateway":"172.20.0.1"}],"Options":{}}, "Options":{}});
        assert!(validate_source_network("project_default", &network).is_ok());
    }

    #[test]
    fn network_validation_accepts_compose_default_with_empty_config_from() {
        let network = json!({
            "Name": "nginx_default",
            "Id": "0d37c683c18a6e25e5ab0a9c1e7f4c0ac95d4bd805cf51a7844c934f477a7cb0",
            "Scope": "local",
            "Driver": "bridge",
            "EnableIPv4": true,
            "EnableIPv6": false,
            "IPAM": {
                "Driver": "default",
                "Options": null,
                "Config": [{"Subnet": "172.21.0.0/16", "Gateway": "172.21.0.1"}]
            },
            "Internal": false,
            "Attachable": false,
            "Ingress": false,
            "ConfigFrom": {"Network": ""},
            "ConfigOnly": false,
            "Options": {},
            "Labels": {
                "com.docker.compose.network": "default",
                "com.docker.compose.project": "nginx",
                "com.docker.compose.version": "5.3.1"
            },
            "Containers": {}
        });
        // Exercise the same Bollard deserialization/serialization as inspect_networks.
        let inspected: bollard::models::NetworkInspect = serde_json::from_value(network).unwrap();
        let network = serde_json::to_value(inspected).unwrap();
        assert!(validate_source_network("nginx_default", &network).is_ok());
    }

    #[test]
    fn network_validation_accepts_unset_config_from() {
        for config_from in [Value::Null, json!({}), json!({"Network": ""})] {
            let network = json!({
                "Name": "n", "Driver": "bridge", "Scope": "local",
                "ConfigFrom": config_from
            });
            assert!(validate_source_network("n", &network).is_ok(), "{network}");
        }
    }

    #[test]
    fn network_validation_rejects_unsupported_boundaries() {
        for extra in [
            json!({"Driver":"overlay"}),
            json!({"EnableIPv6":true}),
            json!({"Options":{"com.docker.network.bridge.name":"br0"}}),
            json!({"IPAM":{"Config":[{"Subnet":"10.0.0.0/24","IPRange":"10.0.0.0/25"}]}}),
            json!({"ConfigFrom":{"Network":"other"}}),
        ] {
            let mut network =
                json!({"Name":"n", "Driver":"bridge", "Scope":"local", "EnableIPv6":false});
            network
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(validate_source_network("n", &network).is_err(), "{network}");
        }
    }

    #[tokio::test]
    async fn network_name_conflict_does_not_create_a_network() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let body = "{\"Id\":\"occupied\",\"Name\":\"n\"}";
            socket.write_all(format!("HTTP/1.1 200 OK\\r\\nContent-Type: application/json\\r\\nContent-Length: {}\\r\\nConnection: close\\r\\n\\r\\n{}", body.len(), body).as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let target = DockerEngine {
            client: Docker::connect_with_http(
                &format!("http://{address}"),
                15,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            managed_wsl: true,
        };
        let error = check_network_absent(&target, "n").await.unwrap_err();
        assert!(error.to_string().contains("n"));
        let request = task.await.unwrap();
        assert!(request.starts_with("GET "));
    }
}
