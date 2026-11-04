use std::{
    net::TcpListener,
    time::{SystemTime, UNIX_EPOCH},
};

use bollard::{
    query_parameters::{RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder},
    Docker,
};
use rusker_desktop::{container::ContainerDraft, engine::EngineService, model::ContainerAction};

const TEST_IMAGE: &str = "rusker-test-engine:ci";

/// Opt-in mutating smoke test. Prepare a dedicated tag first:
/// `docker tag alpine:3.23 rusker-test-engine:ci`.
/// The test removes that tag and its own rusker-test-* container in cleanup.
#[tokio::test]
async fn operates_only_on_dedicated_test_resources() {
    if std::env::var("RUSKER_DOCKER_OPERATIONS").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_DOCKER_OPERATIONS=1 后运行专用资源变更测试");
        return;
    }

    let docker = Docker::connect_with_local_defaults()
        .expect("创建 Docker client")
        .negotiate_version()
        .await
        .expect("连接 Docker");
    docker
        .inspect_image(TEST_IMAGE)
        .await
        .expect("先用 docker tag alpine:3.23 rusker-test-engine:ci 创建专用测试标签");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间")
        .as_millis();
    let name = format!("rusker-test-engine-{}-{nonce}", std::process::id());
    assert!(name.starts_with("rusker-test-"));
    let host_port = TcpListener::bind("127.0.0.1:0")
        .expect("分配测试宿主机端口")
        .local_addr()
        .expect("读取测试宿主机端口")
        .port();
    let service = EngineService::new().expect("创建 Engine runtime");

    let result = async {
        let request = ContainerDraft {
            image: TEST_IMAGE.into(),
            name: name.clone(),
            command: "sh -c \"echo rusker-engine-ok; sleep 120\"".into(),
            ports: format!("{host_port}:80"),
        }
        .parse()
        .map_err(|error| error.to_string())?;
        let snapshot = service
            .create_container(request)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let created = snapshot
            .containers
            .iter()
            .find(|container| container.name == name)
            .expect("创建后快照应包含专用容器");
        assert!(created.is_running());
        assert!(created.ports.iter().any(|port| {
            port.host_ip.as_deref() == Some("127.0.0.1")
                && port.public_port == Some(host_port)
                && port.private_port == 80
                && port.protocol == "tcp"
        }));
        let id = created.id.clone();

        let logs = service
            .container_logs(id.clone())
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        assert!(logs.contains("rusker-engine-ok"), "日志内容：{logs}");

        service
            .container_action(id.clone(), ContainerAction::Restart)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let snapshot = service
            .container_action(id.clone(), ContainerAction::Stop)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        assert!(snapshot
            .containers
            .iter()
            .any(|container| { container.id == id && !container.is_running() }));

        let snapshot = service
            .container_action(id.clone(), ContainerAction::Start)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        assert!(snapshot
            .containers
            .iter()
            .any(|container| { container.id == id && container.is_running() }));
        service
            .container_action(id.clone(), ContainerAction::Stop)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;

        let snapshot = service
            .container_action(id.clone(), ContainerAction::Remove)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        assert!(!snapshot
            .containers
            .iter()
            .any(|container| container.id == id));

        let snapshot = service
            .remove_image(TEST_IMAGE.into())
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        assert!(snapshot
            .images
            .iter()
            .all(|image| { image.tags.iter().all(|tag| tag != TEST_IMAGE) }));
        Ok::<(), String>(())
    }
    .await;

    let remove_container = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(false)
        .build();
    _ = docker.remove_container(&name, Some(remove_container)).await;
    let remove_image = RemoveImageOptionsBuilder::default()
        .force(false)
        .noprune(true)
        .build();
    _ = docker
        .remove_image(TEST_IMAGE, Some(remove_image), None)
        .await;
    tokio::task::spawn_blocking(move || drop(service))
        .await
        .expect("在 blocking 线程释放 Engine runtime");

    result.expect("专用资源上的创建、端口和容器生命周期操作");
}
