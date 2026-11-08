use std::time::Instant;

use rusker_desktop::engine::EngineService;

/// Explicitly opt-in, read-only latency probe. The named pipe may belong to an
/// already running app: this does not stop, restart, or take over that app.
#[test]
fn measures_repeated_engine_and_directory_requests() {
    if std::env::var("RUSKER_LATENCY_PROBE").as_deref() != Ok("1") {
        eprintln!("跳过：设置 RUSKER_LATENCY_PROBE=1 后运行只读延迟测量");
        return;
    }
    let service = EngineService::new().expect("创建 Engine runtime");
    let waiter = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for index in 1..=3 {
        let start = Instant::now();
        let snapshot = waiter
            .block_on(service.snapshot())
            .expect("snapshot task")
            .expect("snapshot request");
        eprintln!("snapshot #{index}: {:?}", start.elapsed());
        assert!(!snapshot.engine.version.is_empty());
    }
    if let Ok(id) = std::env::var("RUSKER_LATENCY_CONTAINER") {
        let path = std::env::var("RUSKER_LATENCY_DIRECTORY").unwrap_or_else(|_| "/".into());
        for index in 1..=3 {
            let start = Instant::now();
            let listing = waiter
                .block_on(service.list_container_directory(id.clone(), path.clone()))
                .expect("directory task")
                .expect("directory request");
            eprintln!(
                "directory #{index}: {:?}, {} entries",
                start.elapsed(),
                listing.entries.len()
            );
        }
    }
    drop(service);
}
