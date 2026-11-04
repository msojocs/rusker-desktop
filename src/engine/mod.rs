mod docker;

use std::{future::Future, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::model::{ContainerAction, CreateContainerRequest, EngineSnapshot};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("无法连接容器引擎：{0}")]
    Connect(String),
    #[error("{action}失败：{message}")]
    Operation { action: String, message: String },
    #[error("{action}超时（{seconds} 秒）")]
    Timeout { action: String, seconds: u64 },
    #[error("无法创建后台运行时：{0}")]
    Runtime(String),
    #[error("后台任务异常结束：{0}")]
    Task(String),
}

impl EngineError {
    pub(crate) fn operation(action: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self::Operation {
            action: action.into(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct EngineService {
    runtime: Arc<Runtime>,
}

impl EngineService {
    pub fn new() -> Result<Self, EngineError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rusker-engine")
            .build()
            .map_err(|error| EngineError::Runtime(error.to_string()))?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    pub fn snapshot(&self) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async {
            let engine = docker::DockerEngine::connect().await?;
            engine.snapshot().await
        })
    }

    pub fn create_container(
        &self,
        request: CreateContainerRequest,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.create_and_run(request).await?;
            engine.snapshot().await
        })
    }

    pub fn container_action(
        &self,
        id: String,
        action: ContainerAction,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.container_action(&id, action).await?;
            engine.snapshot().await
        })
    }

    pub fn container_logs(&self, id: String) -> JoinHandle<Result<String, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.container_logs(&id).await
        })
    }

    pub fn remove_image(&self, id: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.remove_image(&id).await?;
            engine.snapshot().await
        })
    }

    pub fn pull_image(&self, reference: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.pull_image(&reference).await?;
            engine.snapshot().await
        })
    }
}

pub(crate) async fn with_timeout<F, T>(
    action: &str,
    duration: Duration,
    future: F,
) -> Result<T, EngineError>
where
    F: Future<Output = Result<T, bollard::errors::Error>>,
{
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(EngineError::operation(action, error)),
        Err(_) => Err(EngineError::Timeout {
            action: action.into(),
            seconds: duration.as_secs(),
        }),
    }
}
