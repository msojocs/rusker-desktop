#[cfg(windows)]
mod proxy;
mod wsl;

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{runtime::Runtime, sync::Mutex, task::JoinHandle};

pub use wsl::{
    NvidiaGpuStatus, ProxyApplyOutcome, RuntimeHostSnapshot, WslDistribution, MANAGED_DISTRO_NAME,
    MANAGED_DOCKER_HOST, MANAGED_PIPE, WSL_DOCKER_HOST,
};

use crate::settings::DockerProxySettings;

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
        let host = wsl::WslHost::discover()?;
        #[cfg(windows)]
        runtime.spawn(proxy::serve(host.session(), host.proxy_status()));
        Ok(Self {
            runtime: Arc::new(runtime),
            host: Arc::new(host),
            operation_lock: Arc::new(Mutex::new(())),
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
