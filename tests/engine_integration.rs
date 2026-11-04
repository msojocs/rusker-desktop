use rusker_desktop::engine::EngineService;

/// Opt-in smoke test for a real local engine. It is read-only and never starts,
/// stops, removes, pulls, or otherwise changes user resources.
#[tokio::test]
async fn connects_and_reads_real_engine_snapshot() {
    if std::env::var("RUSKER_DOCKER_INTEGRATION").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_DOCKER_INTEGRATION=1 后运行真实 Engine 只读测试");
        return;
    }

    let service = EngineService::new().expect("创建 Tokio runtime");
    let result = service.snapshot().await.expect("Tokio snapshot task");
    tokio::task::spawn_blocking(move || drop(service))
        .await
        .expect("在 blocking 线程释放 Engine runtime");
    let snapshot = result.expect("连接并读取本地 Engine");

    assert!(!snapshot.engine.version.is_empty());
    assert!(!snapshot.engine.api_version.is_empty());
    assert!(snapshot.running_containers() <= snapshot.containers.len());
}
