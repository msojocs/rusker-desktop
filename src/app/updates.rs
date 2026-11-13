//! Update I/O runs on a lazily created Tokio runtime, independently of Engine
//! connectivity. Keep the downloaded TempDir alive until the user leaves the
//! app or starts another check/download.

use std::sync::Arc;

use gpui_kit::{
    base::Disableable as _, component::button::ButtonVariants as _, div,
    prelude::FluentBuilder as _, App, Context, Div, ParentElement as _, Styled as _, Task,
};
use rusker_desktop::update::{
    DownloadProgress, DownloadedUpdate, ReleaseUpdate, UpdateChannel, UpdateService,
    CURRENT_VERSION,
};
use tokio::{runtime::Runtime, task::AbortHandle};

use super::{button, detail_row, muted, RuskerApp};

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateOperation {
    Check,
    Download,
}

pub(super) struct UpdateState {
    channel: UpdateChannel,
    runtime: Option<Arc<Runtime>>,
    operation: Option<UpdateOperation>,
    available: Option<ReleaseUpdate>,
    downloaded: Option<Arc<DownloadedUpdate>>,
    saved_path: Option<std::path::PathBuf>,
    progress: DownloadProgress,
    checked: bool,
    error: Option<String>,
    worker: Option<AbortHandle>,
    task: Option<Task<()>>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            channel: if CURRENT_VERSION.contains("-preview.") {
                UpdateChannel::Preview
            } else {
                UpdateChannel::Stable
            },
            runtime: None,
            operation: None,
            available: None,
            downloaded: None,
            saved_path: None,
            progress: DownloadProgress::default(),
            checked: false,
            error: None,
            worker: None,
            task: None,
        }
    }
}

impl Drop for UpdateState {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl UpdateState {
    fn client(&mut self) -> Result<(Arc<Runtime>, UpdateService), String> {
        if self.runtime.is_none() {
            self.runtime = Some(Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("rusker-updates")
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?,
            ));
        }
        let service = UpdateService::new().map_err(|error| error.to_string())?;
        Ok((self.runtime.as_ref().expect("initialized").clone(), service))
    }
}

impl RuskerApp {
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if self.updates.operation.is_some() {
            return;
        }
        self.updates.error = None;
        self.updates.checked = false;
        self.updates.available = None;
        self.updates.downloaded = None;
        self.updates.saved_path = None;
        let (runtime, service) = match self.updates.client() {
            Ok(client) => client,
            Err(error) => {
                self.updates.error = Some(error);
                cx.notify();
                return;
            }
        };
        let channel = self.updates.channel;
        self.updates.operation = Some(UpdateOperation::Check);
        let handle = runtime.spawn(async move { service.check(channel).await });
        self.updates.worker = Some(handle.abort_handle());
        self.updates.task = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            _ = this.update(cx, |this, cx| {
                this.updates.operation = None;
                this.updates.worker = None;
                match result {
                    Ok(Ok(update)) => {
                        this.updates.available = update;
                        this.updates.checked = true;
                    }
                    Ok(Err(error)) => this.updates.error = Some(error.to_string()),
                    Err(error) => this.updates.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn download_update(&mut self, cx: &mut Context<Self>) {
        if self.updates.operation.is_some() {
            return;
        }
        let Some(update) = self.updates.available.clone() else {
            return;
        };
        self.updates.error = None;
        self.updates.downloaded = None;
        self.updates.saved_path = None;
        let (runtime, service) = match self.updates.client() {
            Ok(client) => client,
            Err(error) => {
                self.updates.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.updates.progress = DownloadProgress {
            downloaded_bytes: 0,
            total_bytes: update.size,
        };
        self.updates.operation = Some(UpdateOperation::Download);
        let (sender, mut receiver) = tokio::sync::watch::channel(self.updates.progress);
        let handle = runtime.spawn(async move { service.download(update, sender).await });
        self.updates.worker = Some(handle.abort_handle());
        self.updates.task = Some(cx.spawn(async move |this, cx| {
            while receiver.changed().await.is_ok() {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let progress = *receiver.borrow_and_update();
                if this
                    .update(cx, |this, cx| {
                        this.updates.progress = progress;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let result = handle.await;
            _ = this.update(cx, |this, cx| {
                this.updates.operation = None;
                this.updates.worker = None;
                match result {
                    Ok(Ok(downloaded)) => this.updates.downloaded = Some(Arc::new(downloaded)),
                    Ok(Err(error)) => this.updates.error = Some(error.to_string()),
                    Err(error) => this.updates.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn cancel_update_download(&mut self, cx: &mut Context<Self>) {
        if self.updates.operation != Some(UpdateOperation::Download) {
            return;
        }
        if let Some(worker) = self.updates.worker.take() {
            worker.abort();
        }
        self.updates.task = None;
        self.updates.operation = None;
        self.updates.progress = DownloadProgress::default();
        cx.notify();
    }

    fn save_downloaded_update(&mut self, cx: &mut Context<Self>) {
        let Some(downloaded) = self.updates.downloaded.clone() else {
            return;
        };
        let Some(runtime) = self.updates.runtime.clone() else {
            return;
        };
        let directory = directories::UserDirs::new()
            .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let name = downloaded
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let receiver = cx.prompt_for_new_path(&directory, name.as_deref());
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let destination = path.clone();
            // Hold the verified source alive even if another check starts.
            // Copy to a sibling temporary file before replacing the destination.
            let result = runtime
                .spawn_blocking(move || -> Result<(), String> {
                    use std::io::Write as _;
                    if destination.parent() == downloaded.path.parent() {
                        return Err(t!("updates.save_elsewhere").into_owned());
                    }
                    let parent = destination.parent().ok_or("Invalid destination")?;
                    let mut temporary = tempfile::NamedTempFile::new_in(parent)
                        .map_err(|error| error.to_string())?;
                    let mut source =
                        std::fs::File::open(&downloaded.path).map_err(|error| error.to_string())?;
                    std::io::copy(&mut source, temporary.as_file_mut())
                        .map_err(|error| error.to_string())?;
                    temporary.flush().map_err(|error| error.to_string())?;
                    temporary
                        .as_file()
                        .sync_all()
                        .map_err(|error| error.to_string())?;
                    temporary
                        .persist(&destination)
                        .map_err(|error| error.to_string())?;
                    Ok(())
                })
                .await;
            _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {
                        cx.reveal_path(&path);
                        this.updates.saved_path = Some(path);
                    }
                    Ok(Err(error)) => this.updates.error = Some(error),
                    Err(error) => this.updates.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_update_settings(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.updates;
        let supported = cfg!(all(target_os = "windows", target_arch = "x86_64"));
        let busy = state.operation.is_some();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(detail_row(
                t!("updates.current_version"),
                CURRENT_VERSION,
                cx,
            ))
            .child(muted(t!("updates.source"), cx))
            .child(
                div().flex().gap_2().children(
                    [
                        (UpdateChannel::Stable, "stable", t!("updates.stable")),
                        (UpdateChannel::Preview, "preview", t!("updates.preview")),
                    ]
                    .map(|(channel, id, label)| {
                        let selected = matches!(
                            (state.channel, channel),
                            (UpdateChannel::Stable, UpdateChannel::Stable)
                                | (UpdateChannel::Preview, UpdateChannel::Preview)
                        );
                        let control = button(id)
                            .label(label.into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.updates.operation.is_some() {
                                    return;
                                }
                                this.updates.channel = channel;
                                this.updates.available = None;
                                this.updates.downloaded = None;
                                this.updates.saved_path = None;
                                this.updates.checked = false;
                                this.updates.error = None;
                                cx.notify();
                            }));
                        if selected {
                            control.primary()
                        } else {
                            control.outline()
                        }
                    }),
                ),
            )
            .child(muted(t!("updates.channel_hint"), cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("check-update")
                            .label(if state.operation == Some(UpdateOperation::Check) {
                                t!("updates.checking").into_owned()
                            } else {
                                t!("updates.check").into_owned()
                            })
                            .outline()
                            .disabled(busy || !supported)
                            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
                    )
                    .child(
                        button("release-page")
                            .label(t!("updates.releases").into_owned())
                            .outline()
                            .on_click(|_, _, cx: &mut App| {
                                cx.open_url("https://github.com/msojocs/rusker-desktop/releases");
                            }),
                    ),
            )
            .when(!supported, |this| {
                this.child(muted(t!("updates.unsupported"), cx))
            })
            .when(state.checked && state.available.is_none(), |this| {
                this.child(muted(t!("updates.up_to_date"), cx))
            })
            .when_some(state.available.as_ref(), |this, update| {
                let release_url = update.release_url.clone();
                this.child(detail_row(
                    t!("updates.available_version"),
                    &update.version,
                    cx,
                ))
                .child(muted(
                    format!(
                        "{} · {}",
                        update.asset_name,
                        rusker_desktop::format::format_bytes(update.size)
                    ),
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("download-update")
                                .label(t!("updates.download").into_owned())
                                .primary()
                                .disabled(busy || state.downloaded.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.download_update(cx))),
                        )
                        .child(
                            button("release-notes")
                                .label(t!("updates.release_notes").into_owned())
                                .outline()
                                .on_click(move |_, _, cx| cx.open_url(&release_url)),
                        ),
                )
            })
            .when(state.operation == Some(UpdateOperation::Download), |this| {
                this.child(muted(
                    t!(
                        "updates.progress",
                        downloaded =
                            rusker_desktop::format::format_bytes(state.progress.downloaded_bytes),
                        total = rusker_desktop::format::format_bytes(state.progress.total_bytes)
                    ),
                    cx,
                ))
                .child(
                    button("cancel-update-download")
                        .label(t!("updates.cancel").into_owned())
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_update_download(cx))),
                )
            })
            .when_some(state.downloaded.as_ref(), |this, downloaded| {
                let path = downloaded.path.clone();
                this.child(muted(t!("updates.downloaded"), cx)).child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("save-update")
                                .label(t!("updates.save").into_owned())
                                .primary()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_downloaded_update(cx)),
                                ),
                        )
                        .child(
                            button("reveal-update")
                                .label(t!("updates.reveal").into_owned())
                                .outline()
                                .on_click(move |_, _, cx| cx.reveal_path(&path)),
                        ),
                )
            })
            .when_some(state.saved_path.as_ref(), |this, path| {
                this.child(muted(t!("updates.saved", path = path.display()), cx))
            })
            .when_some(state.error.as_ref(), |this, error| {
                this.child(muted(t!("updates.failed", error = error), cx))
            })
    }
}
