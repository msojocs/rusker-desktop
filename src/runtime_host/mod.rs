#[cfg(windows)]
mod proxy;
mod wsl;

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio::{runtime::Runtime, sync::Mutex, task::JoinHandle};

pub use wsl::{
    same_path, NvidiaGpuStatus, ProxyApplyOutcome, RuntimeHostSnapshot, WslDistribution,
    MANAGED_DISTRO_NAME, MANAGED_DOCKER_HOST, MANAGED_PIPE, WSL_DOCKER_HOST,
};

use crate::settings::{DockerProxySettings, ResourceSaverSettings};

#[derive(Debug, Error)]
pub enum RuntimeHostError {
    #[error("当前平台不支持 Rusker WSL2 运行环境")]
    Unsupported,
    #[error("{0}")]
    Conflict(String),
    #[error("{action}失败：{message}")]
    Command { action: String, message: String },
    #[error("{action}超时（{seconds} 秒）")]
    Timeout { action: String, seconds: u64 },
    #[error("{action}失败：{source}")]
    Io {
        action: &'static str,
        source: std::io::Error,
    },
    #[error("运行环境状态文件无效：{0}")]
    State(String),
    #[error("WSL2 后台任务异常结束：{0}")]
    Task(String),
}

#[derive(Clone)]
pub struct RuntimeHostService {
    runtime: Arc<Runtime>,
    host: Arc<wsl::WslHost>,
    operation_lock: Arc<Mutex<()>>,
}

impl RuntimeHostService {
    pub fn new() -> Result<Self, RuntimeHostError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rusker-runtime-host")
            .build()
            .map_err(|source| RuntimeHostError::Io {
                action: "创建 WSL2 后台运行时",
                source,
            })?;
        let host = Arc::new(wsl::WslHost::discover()?);
        let operation_lock = Arc::new(Mutex::new(()));
        #[cfg(windows)]
        {
            runtime.spawn(proxy::serve(host.clone(), operation_lock.clone()));
            runtime.spawn(resource_saver_monitor(host.clone(), operation_lock.clone()));
        }
        Ok(Self {
            runtime: Arc::new(runtime),
            host,
            operation_lock,
        })
    }

    /// Updates the Resource Saver configuration; takes effect immediately.
    pub fn set_resource_saver(&self, settings: &ResourceSaverSettings) {
        self.host.resource_saver().configure(settings);
    }

    /// Whether the engine is currently stopped by Resource Saver.
    pub fn resource_saver_active(&self) -> bool {
        self.host.resource_saver().is_active() && !self.host.session_running()
    }

    /// Whether this process holds a live engine session.
    pub fn engine_session_running(&self) -> bool {
        self.host.session_running()
    }

    pub fn default_disk_location(&self) -> PathBuf {
        self.host.default_disk_location()
    }

    /// Install target for the virtual disk (None = default location).
    pub fn set_install_location(&self, location: Option<PathBuf>) {
        self.host.set_install_location(location);
    }

    /// Moves the installed distribution's virtual disk to `target`.
    pub fn move_disk(
        &self,
        target: PathBuf,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.move_disk(Path::new(&target)).await?;
            host.snapshot(&integrations).await
        })
    }

    fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    /// Sets the Engine proxy written to `daemon.json` on the next engine
    /// start, without touching a running engine. Call before `startup`.
    pub fn set_engine_proxy(&self, settings: &DockerProxySettings) {
        self.host.set_engine_proxy(settings);
    }

    /// Applies saved proxy settings immediately: rewrites `daemon.json` and
    /// restarts a running engine when the config changed.
    pub fn apply_proxy(
        &self,
        settings: DockerProxySettings,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<(ProxyApplyOutcome, RuntimeHostSnapshot), RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            let outcome = host.apply_proxy(&settings).await?;
            Ok((outcome, host.snapshot(&integrations).await?))
        })
    }

    /// Launch-time entry point: reads state and, like Docker Desktop, starts
    /// an installed engine automatically.
    pub fn startup(
        &self,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.startup(&integrations).await
        })
    }

    /// Called synchronously from the app's quit hook. Gracefully stops a
    /// dockerd owned by this process, bounded by `limit`. If another
    /// operation (e.g. an install) holds the lock, the session is simply
    /// dropped with the runtime.
    pub fn shutdown_blocking(&self, limit: Duration) {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.runtime.block_on(async move {
            let Ok(_guard) = lock.try_lock() else {
                return;
            };
            let _ = tokio::time::timeout(limit, host.shutdown()).await;
        });
    }

    /// Async counterpart of [`Self::shutdown_blocking`] for the quit
    /// animation: waits for an in-flight operation (e.g. a start) to finish
    /// before stopping, all bounded by `limit`.
    pub fn shutdown(&self, limit: Duration) -> JoinHandle<()> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _ = tokio::time::timeout(limit, async move {
                let _guard = lock.lock().await;
                host.shutdown().await
            })
            .await;
        })
    }

    pub fn snapshot(
        &self,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.snapshot(&integrations).await
        })
    }

    pub fn install(
        &self,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.install().await?;
            host.snapshot_after_integration(&integrations).await
        })
    }

    pub fn start(
        &self,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.start().await?;
            host.snapshot_after_integration(&integrations).await
        })
    }

    pub fn stop(
        &self,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.stop().await?;
            host.snapshot(&integrations).await
        })
    }

    pub fn set_integration(
        &self,
        distribution: String,
        enabled: bool,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.set_integration(&distribution, enabled).await?;
            host.snapshot(&integrations).await
        })
    }
}

/// How often the idle monitor samples the engine.
#[cfg(windows)]
const RESOURCE_SAVER_POLL: Duration = Duration::from_secs(10);

/// Enters Resource Saver once the engine has had no running containers and
/// no Windows pipe clients for the configured threshold. Skips a sample
/// whenever another operation holds the lock, so it never interrupts an
/// install, restart or user-initiated stop.
#[cfg(windows)]
async fn resource_saver_monitor(host: Arc<wsl::WslHost>, lock: Arc<Mutex<()>>) {
    let saver = host.resource_saver();
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::time::sleep(RESOURCE_SAVER_POLL).await;
        if !saver.enabled()
            || saver.is_active()
            || !host.session_running()
            || saver.connections() > 0
        {
            idle_since = None;
            continue;
        }
        let Ok(_guard) = lock.try_lock() else {
            idle_since = None;
            continue;
        };
        if !matches!(host.running_containers().await, Ok(0)) {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= saver.threshold() && saver.connections() == 0 {
            host.enter_resource_saver().await;
            idle_since = None;
        }
    }
}
