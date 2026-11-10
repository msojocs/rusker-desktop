mod connection;
mod container_detail;
mod docker;
mod pull;
mod replace;
mod stats_history;

pub use container_detail::{ContainerShell, ContainerStats, ContainerStatsBatch};
pub use docker::{
    DesktopContainer, DesktopImportProgress, DesktopImportReport, DesktopImportSource,
};
pub use pull::{ImagePullLayer, ImagePullProgress};
pub use stats_history::{StatsCache, StatsHistory, StatsSample, STATS_SAMPLE_INTERVAL};

use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{runtime::Runtime, task::JoinHandle};

use crate::{
    model::{ContainerAction, CreateContainerRequest, EngineSnapshot, ImageDetail},
    volume::DirectoryListing,
};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("{}", t!("engine.error.connect", detail = .0))]
    Connect(String),
    #[error("{}", t!("engine.error.operation", action = action, message = message))]
    Operation { action: String, message: String },
    #[error("{}", t!("engine.error.timeout", action = action, seconds = seconds))]
    Timeout { action: String, seconds: u64 },
    #[error("{}", t!("engine.error.runtime", detail = .0))]
    Runtime(String),
    #[error("{}", t!("engine.error.task", detail = .0))]
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
    clients: Arc<connection::ClientCache<docker::DockerEngine>>,
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
            clients: Arc::new(connection::ClientCache::default()),
        })
    }

    fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    fn spawn_engine<F, T>(
        &self,
        operation: impl FnOnce(Arc<docker::DockerEngine>) -> F + Send + 'static,
    ) -> JoinHandle<Result<T, EngineError>>
    where
        F: Future<Output = Result<T, EngineError>> + Send + 'static,
        T: Send + 'static,
    {
        let clients = self.clients.clone();
        self.spawn(async move {
            let _timing = crate::diagnostics::PerfSpan::new("engine.operation");
            let engine = clients
                .get(
                    std::env::var_os("DOCKER_HOST"),
                    docker::DockerEngine::connect,
                )
                .await?;
            // The initialization mutex is no longer held: unrelated operations
            // and long-running streams execute concurrently.
            let result = operation(engine.clone()).await;
            if result.is_err() {
                clients.invalidate(&engine).await;
            }
            // Never replay a mutation after a transport error: the daemon may
            // already have applied it. The next user operation reconnects.
            result
        })
    }

    pub fn desktop_containers(&self) -> JoinHandle<Result<Vec<DesktopContainer>, EngineError>> {
        self.containers_from_desktop(DesktopImportSource::DockerDesktop)
    }

    pub fn containers_from_desktop(
        &self,
        source: DesktopImportSource,
    ) -> JoinHandle<Result<Vec<DesktopContainer>, EngineError>> {
        self.spawn(async move { docker::DockerEngine::desktop_containers(source).await })
    }

    pub fn import_desktop_containers(
        &self,
        ids: Vec<String>,
        progress: tokio::sync::watch::Sender<DesktopImportProgress>,
    ) -> JoinHandle<Result<DesktopImportReport, EngineError>> {
        self.import_from_desktop(DesktopImportSource::DockerDesktop, ids, progress)
    }

    pub fn import_from_desktop(
        &self,
        source: DesktopImportSource,
        ids: Vec<String>,
        progress: tokio::sync::watch::Sender<DesktopImportProgress>,
    ) -> JoinHandle<Result<DesktopImportReport, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine
                .import_desktop_containers(source, ids, progress)
                .await
        })
    }

    pub fn snapshot(&self) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(|engine| async move { engine.snapshot().await })
    }

    /// Creates and starts the container; returns its ID with a fresh snapshot.
    pub fn create_container(
        &self,
        request: CreateContainerRequest,
    ) -> JoinHandle<Result<(String, EngineSnapshot), EngineError>> {
        self.spawn_engine(move |engine| async move {
            let id = engine.create_and_run(request).await?;
            Ok((id, engine.snapshot().await?))
        })
    }

    /// Recreates a standalone container with its inspected configuration.
    pub fn replace_container(
        &self,
        id: String,
        image: Option<String>,
    ) -> JoinHandle<Result<(String, EngineSnapshot), EngineError>> {
        self.spawn_engine(move |engine| async move {
            let new_id = engine.replace_container(&id, image).await?;
            Ok((new_id, engine.snapshot().await?))
        })
    }

    pub fn container_action(
        &self,
        id: String,
        action: ContainerAction,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine.container_action(&id, action).await?;
            engine.snapshot().await
        })
    }

    /// Full Docker inspect response as formatted JSON, also available for stopped containers.
    pub fn container_inspect(&self, id: String) -> JoinHandle<Result<String, EngineError>> {
        self.spawn_engine(move |engine| async move { engine.container_inspect(&id).await })
    }

    pub fn container_logs(&self, id: String) -> JoinHandle<Result<String, EngineError>> {
        self.spawn_engine(move |engine| async move { engine.container_logs(&id).await })
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
        self.spawn_engine(move |engine| async move { engine.open_container_shell(&id).await })
    }

    /// Reads a single absolute POSIX directory without modifying its contents.
    /// Requires `/bin/sh`, GNU/BusyBox-compatible `find`, `stat -c`, `tr`,
    /// `head`, and `xargs -0` in the running container. At most 5000 entries;
    /// directory sizes are unmeasured
    /// (zero), not recursive `du` results. Non-UTF-8 filenames are an error.
    pub fn list_container_directory(
        &self,
        id: String,
        path: String,
    ) -> JoinHandle<Result<DirectoryListing, EngineError>> {
        self.spawn_engine(
            move |engine| async move { engine.list_container_directory(&id, &path).await },
        )
    }

    /// Lists all containers and concurrently samples those currently running.
    pub fn sample_container_stats(&self) -> JoinHandle<Result<ContainerStatsBatch, EngineError>> {
        self.spawn_engine(|engine| async move { engine.sample_container_stats().await })
    }

    /// Returns one non-streaming sample, with Docker's previous CPU sample.
    pub fn container_stats(&self, id: String) -> JoinHandle<Result<ContainerStats, EngineError>> {
        self.spawn_engine(move |engine| async move { engine.container_stats(&id).await })
    }

    /// Removes each container independently and returns the fresh snapshot
    /// with one error message per failed container.
    pub fn remove_containers(
        &self,
        ids: Vec<String>,
    ) -> JoinHandle<Result<(EngineSnapshot, Vec<String>), EngineError>> {
        self.spawn_engine(move |engine| async move {
            let failures = engine.remove_containers(&ids).await;
            Ok((engine.snapshot().await?, failures))
        })
    }

    pub fn image_detail(&self, id: String) -> JoinHandle<Result<ImageDetail, EngineError>> {
        self.spawn_engine(move |engine| async move { engine.image_detail(&id).await })
    }

    pub fn remove_image(&self, id: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
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
        self.spawn_engine(move |engine| async move {
            let failures = engine.remove_images(&references).await;
            Ok((engine.snapshot().await?, failures))
        })
    }

    pub fn pull_image(&self, reference: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine.pull_image(&reference).await?;
            engine.snapshot().await
        })
    }

    pub fn pull_image_with_progress(
        &self,
        reference: String,
        progress: tokio::sync::watch::Sender<ImagePullProgress>,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine
                .pull_image_with_progress(&reference, |response| {
                    progress.send_modify(|state| state.update(response));
                })
                .await?;
            engine.snapshot().await
        })
    }

    pub fn volume_sizes(&self) -> JoinHandle<Result<HashMap<String, u64>, EngineError>> {
        self.spawn_engine(|engine| async move { engine.volume_sizes().await })
    }

    /// `name: None` lets the engine generate a name.
    pub fn create_volume(
        &self,
        name: Option<String>,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine.create_volume(name).await?;
            engine.snapshot().await
        })
    }

    pub fn remove_volume(&self, name: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
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
        self.spawn_engine(move |engine| async move {
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
        self.spawn_engine(
            move |engine| async move { engine.export_volume(&name, &destination).await },
        )
    }

    pub fn clone_volume(
        &self,
        source: String,
        target: String,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine.clone_volume(&source, &target).await?;
            engine.snapshot().await
        })
    }

    pub fn empty_volume(&self, name: String) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
            engine.empty_volume(&name).await?;
            engine.snapshot().await
        })
    }

    pub fn import_volume(
        &self,
        name: String,
        archive: PathBuf,
    ) -> JoinHandle<Result<EngineSnapshot, EngineError>> {
        self.spawn_engine(move |engine| async move {
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
        self.spawn_engine(move |engine| async move {
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
