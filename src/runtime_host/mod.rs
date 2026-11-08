mod data_disk;
#[cfg(any(windows, test))]
mod docker_api_transport;
mod engine_proxy;
mod http_proxy;
mod lan_forward;
mod path_translation;
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

pub use engine_proxy::{ENGINE_PROXY_PORT, ENGINE_PROXY_URL};
pub use lan_forward::{LanForwardState, Protocol as ForwardProtocol};
pub use wsl::{
    same_path, validate_daemon_config, EngineRecovery, NvidiaGpuStatus, ProxyApplyOutcome,
    RuntimeHostSnapshot, WslDistribution, MANAGED_DISTRO_NAME, MANAGED_DOCKER_HOST, MANAGED_PIPE,
    WSL_DOCKER_HOST,
};

use crate::settings::{DockerProxySettings, NetworkSettings, ResourceSaverSettings};

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

/// Result of [`RuntimeHostService::apply_integrations`]: the refreshed
/// snapshot, the integrations actually in effect, and per-change failure
/// messages.
pub type ApplyIntegrationsOutcome = (RuntimeHostSnapshot, BTreeMap<String, bool>, Vec<String>);

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
            runtime.spawn(lan_forward::serve(host.clone()));
            runtime.spawn(engine_proxy::serve(host.clone()));
            runtime.spawn(engine_watchdog(host.clone(), operation_lock.clone()));
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
        if !settings.enabled {
            let host = self.host.clone();
            let lock = self.operation_lock.clone();
            self.spawn(async move {
                let _guard = lock.lock().await;
                if let Err(error) = host.resume_resource_saver().await {
                    host.proxy_status().set_error(error.to_string());
                }
            });
        }
    }

    /// Whether the engine is currently paused by Resource Saver.
    pub fn resource_saver_active(&self) -> bool {
        self.host.resource_saver().is_active()
    }

    /// Whether this process holds a live engine session.
    pub fn engine_session_running(&self) -> bool {
        self.host.session_running()
    }

    /// NAT-mode LAN forwarding of published ports, for the settings page.
    pub fn lan_forward_state(&self) -> LanForwardState {
        self.host.lan_forward_status().get()
    }

    /// Automatic restart of a lost engine, and how many restarts have
    /// succeeded (the app reconnects when that number changes).
    pub fn engine_recovery(&self) -> (EngineRecovery, u64) {
        self.host.recovery_status().get()
    }

    /// Docker Desktop's "Enable host networking": also forward the ports
    /// `--net=host` containers listen on. Takes effect immediately.
    pub fn set_host_networking(&self, enabled: bool) {
        self.host.lan_forward_status().set_host_networking(enabled);
    }

    /// Default directory for the Docker data disk.
    pub fn default_disk_location(&self) -> PathBuf {
        self.host.default_disk_location()
    }

    /// Where a new Docker data disk is created (None = default location).
    pub fn set_data_location(&self, location: Option<PathBuf>) {
        self.host.set_data_location(location);
    }

    /// Moves the Docker data disk into the `target` directory.
    pub fn move_disk(
        &self,
        target: PathBuf,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<RuntimeHostSnapshot, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            let warnings = host.move_disk(Path::new(&target)).await?;
            let mut snapshot = host.snapshot(&integrations).await?;
            snapshot.warnings.extend(warnings);
            Ok(snapshot)
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

    /// Like [`Self::set_engine_proxy`], for the network settings.
    pub fn set_engine_network(&self, settings: &NetworkSettings) {
        self.host.set_engine_network(settings);
    }

    /// Applies saved Engine settings (proxy, network) immediately: rewrites
    /// `daemon.json` and restarts a running engine when the config changed.
    /// `daemon_config` also replaces the rest of `daemon.json` (the settings
    /// editor).
    pub fn apply_engine_config(
        &self,
        proxy: DockerProxySettings,
        network: NetworkSettings,
        daemon_config: Option<String>,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<(ProxyApplyOutcome, RuntimeHostSnapshot), RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            let outcome = host
                .apply_engine_config(&proxy, &network, daemon_config.as_deref())
                .await?;
            Ok((outcome, host.snapshot(&integrations).await?))
        })
    }

    /// The managed `daemon.json` without the keys owned by the proxy and
    /// network settings, for the settings editor.
    pub fn read_daemon_config(&self) -> JoinHandle<Result<String, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            host.read_daemon_config().await
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
            host.resource_saver().set_integrations(&integrations);
            host.snapshot(&integrations).await
        })
    }

    /// Applies each `(distribution, enabled)` change in order. A failed
    /// change is reverted in the returned integrations (the state actually in
    /// effect) and reported as a message instead of aborting the rest.
    pub fn apply_integrations(
        &self,
        changes: Vec<(String, bool)>,
        integrations: BTreeMap<String, bool>,
    ) -> JoinHandle<Result<ApplyIntegrationsOutcome, RuntimeHostError>> {
        let host = self.host.clone();
        let lock = self.operation_lock.clone();
        self.spawn(async move {
            let _guard = lock.lock().await;
            let mut effective = integrations;
            let mut failures = Vec::new();
            for (distribution, enabled) in changes {
                if let Err(error) = host.set_integration(&distribution, enabled).await {
                    let action = if enabled { "启用" } else { "停用" };
                    failures.push(format!("{action} {distribution} 集成失败：{error}"));
                    if enabled {
                        effective.remove(&distribution);
                    } else {
                        effective.insert(distribution, true);
                    }
                }
            }
            host.resource_saver().set_integrations(&effective);
            let snapshot = host.snapshot(&effective).await?;
            Ok((snapshot, effective, failures))
        })
    }
}

/// How often the watchdog checks for a lost engine.
#[cfg(windows)]
const WATCHDOG_POLL: Duration = Duration::from_secs(3);
/// Consecutive failed restarts before the watchdog gives up.
#[cfg(windows)]
const MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// Restarts the engine when its session disappears without a deliberate
/// stop (`wsl --shutdown`, a terminated distribution, a dockerd crash).
/// Runs under the operation lock, so it waits for an apply or disk move
/// that is restarting the engine itself; backs off between failed attempts
/// and gives up after [`MAX_RECOVERY_ATTEMPTS`].
#[cfg(windows)]
async fn engine_watchdog(host: Arc<wsl::WslHost>, lock: Arc<Mutex<()>>) {
    let mut attempt = 0u32;
    loop {
        tokio::time::sleep(WATCHDOG_POLL * 2u32.pow(attempt)).await;
        if !host.engine_lost() {
            attempt = 0;
            continue;
        }
        let _guard = lock.lock().await;
        if !host.engine_lost() {
            attempt = 0;
            continue;
        }
        attempt += 1;
        match host.recover(attempt).await {
            Ok(()) => attempt = 0,
            Err(error) if attempt >= MAX_RECOVERY_ATTEMPTS => {
                host.give_up_recovery(format!(
                    "Engine 意外退出，自动重启 {attempt} 次均失败：{error}"
                ));
                attempt = 0;
            }
            Err(_) => {}
        }
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
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        tokio::time::sleep(RESOURCE_SAVER_POLL).await;
        let saver = host.resource_saver();
        let needs_resume = saver.is_active() && !saver.can_pause();
        if !saver.can_pause() && !needs_resume
            || !needs_resume
                && (saver.is_active() || !host.session_running() || saver.connections() > 0)
        {
            idle_since = None;
            continue;
        }
        let Ok(_guard) = lock.try_lock() else {
            idle_since = None;
            continue;
        };
        if needs_resume {
            if let Err(error) = host.resume_resource_saver().await {
                host.proxy_status().set_error(error.to_string());
            }
            idle_since = None;
            continue;
        }
        if !matches!(host.running_containers().await, Ok(0)) {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= saver.threshold() && saver.connections() == 0 {
            if let Err(error) = host.enter_resource_saver().await {
                host.proxy_status().set_error(error.to_string());
            }
            idle_since = None;
        }
    }
}
