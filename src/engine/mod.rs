mod container_detail;
mod docker;
mod pull;

pub use container_detail::{ContainerShell, ContainerStats};
pub use pull::{ImagePullLayer, ImagePullProgress};

use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    model::{ContainerAction, CreateContainerRequest, EngineSnapshot, ImageDetail},
    volume::DirectoryListing,
};

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

    /// Creates and starts the container; returns its ID with a fresh snapshot.
    pub fn create_container(
        &self,
        request: CreateContainerRequest,
    ) -> JoinHandle<Result<(String, EngineSnapshot), EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            let id = engine.create_and_run(request).await?;
            Ok((id, engine.snapshot().await?))
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

    /// Opens a persistent TTY `/bin/sh -i` with `TERM=xterm-256color`.
    /// Input/output strings carry terminal control bytes unchanged (invalid
    /// output UTF-8 is replaced). `resize` sends `(rows, cols)`, default 24x80.
    /// Retain the returned value while in use; dropping it aborts backend IO
    /// (not a guaranteed remote process-tree kill).
    pub fn open_container_shell(
        &self,
        id: String,
    ) -> JoinHandle<Result<ContainerShell, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.open_container_shell(&id).await
        })
    }

    /// Reads a single absolute POSIX directory without modifying its contents.
    /// Requires `/bin/sh`, GNU/BusyBox-compatible `find` and `stat -c` in the
    /// running container. At most 5000 entries; directory sizes are unmeasured
    /// (zero), not recursive `du` results. Non-UTF-8 filenames are an error.
    pub fn list_container_directory(
        &self,
        id: String,
        path: String,
    ) -> JoinHandle<Result<DirectoryListing, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.list_container_directory(&id, &path).await
        })
    }

    /// Returns one non-streaming sample, with Docker's previous CPU sample.
    pub fn container_stats(&self, id: String) -> JoinHandle<Result<ContainerStats, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.container_stats(&id).await
        })
    }

    /// Removes each container independently and returns the fresh snapshot
    /// with one error message per failed container.
    pub fn remove_containers(
        &self,
        ids: Vec<String>,
    ) -> JoinHandle<Result<(EngineSnapshot, Vec<String>), EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            let failures = engine.remove_containers(&ids).await;
            Ok((engine.snapshot().await?, failures))
        })
    }

    pub fn image_detail(&self, id: String) -> JoinHandle<Result<ImageDetail, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.image_detail(&id).await
        })
    }

    pub fn remove_image(&self, id: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.remove_image(&id).await?;
            engine.snapshot().await
        })
    }

    /// Removes each reference (`repo:tag` or ID) independently and returns
    /// the fresh snapshot with one error message per failed reference.
    pub fn remove_images(
        &self,
        references: Vec<String>,
    ) -> JoinHandle<Result<(EngineSnapshot, Vec<String>), EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            let failures = engine.remove_images(&references).await;
            Ok((engine.snapshot().await?, failures))
        })
    }

    pub fn pull_image(&self, reference: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.pull_image(&reference).await?;
            engine.snapshot().await
        })
    }

    pub fn pull_image_with_progress(
        &self,
        reference: String,
        progress: tokio::sync::watch::Sender<ImagePullProgress>,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine
                .pull_image_with_progress(&reference, |response| {
                    progress.send_modify(|state| state.update(response));
                })
                .await?;
            engine.snapshot().await
        })
    }

    pub fn volume_sizes(&self) -> JoinHandle<Result<HashMap<String, u64>, EngineError>> {
        self.spawn(async {
            let engine = docker::DockerEngine::connect().await?;
            engine.volume_sizes().await
        })
    }

    /// `name: None` lets the engine generate a name.
    pub fn create_volume(
        &self,
        name: Option<String>,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.create_volume(name).await?;
            engine.snapshot().await
        })
    }

    pub fn remove_volume(&self, name: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.remove_volume(&name).await?;
            engine.snapshot().await
        })
    }

    /// Removes each volume independently and returns the fresh snapshot
    /// together with one error message per volume that was not removed.
    pub fn remove_volumes(
        &self,
        names: Vec<String>,
    ) -> JoinHandle<Result<(EngineSnapshot, Vec<String>), EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            let failures = engine.remove_volumes(&names).await;
            Ok((engine.snapshot().await?, failures))
        })
    }

    /// Saves the volume as a tar file; returns the number of bytes written.
    pub fn export_volume(
        &self,
        name: String,
        destination: PathBuf,
    ) -> JoinHandle<Result<u64, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.export_volume(&name, &destination).await
        })
    }

    pub fn clone_volume(
        &self,
        source: String,
        target: String,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.clone_volume(&source, &target).await?;
            engine.snapshot().await
        })
    }

    pub fn empty_volume(&self, name: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.empty_volume(&name).await?;
            engine.snapshot().await
        })
    }

    pub fn import_volume(
        &self,
        name: String,
        archive: PathBuf,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.import_volume(&name, &archive).await?;
            engine.snapshot().await
        })
    }

    /// Lists a directory inside the volume; `relative` is `""` for the root.
    pub fn list_volume_directory(
        &self,
        name: String,
        relative: String,
    ) -> JoinHandle<Result<DirectoryListing, EngineError>> {
        self.spawn(async move {
            let engine = docker::DockerEngine::connect().await?;
            engine.list_volume_directory(&name, &relative).await
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
