//! Image pulls keep one bottom-right notification alive while the engine
//! streams layer progress. Updating its content entity avoids replaying the
//! notification's enter animation on every event.

use std::time::Duration;

use gpui_kit::{
    base::StyledExt as _,
    component::{
        notification::{Notification, NotificationDelivery},
        ActiveTheme as _, WindowExt as _,
    },
    div, px, Anchor, AppContext as _, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
};
use rusker_desktop::{engine::ImagePullProgress, format::format_bytes};

use super::super::{await_engine, RuskerApp};

struct ImagePullNotification;

struct PullProgressView {
    progress: ImagePullProgress,
}

impl Render for PullProgressView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = &self.progress;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .text_xs()
            .child(div().text_color(cx.theme().muted_foreground).child(
                if progress.status.is_empty() {
                    "正在连接镜像仓库…".to_string()
                } else {
                    progress.status.clone()
                },
            ))
            .child(
                div()
                    .id("image-pull-layers")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(progress.layers.iter().map(|(id, layer)| {
                        let bytes = match (layer.current, layer.total) {
                            (Some(current), Some(total)) if total > 0 => format!(
                                " · {} / {} ({:.0}%)",
                                format_bytes(current),
                                format_bytes(total),
                                (current as f64 / total as f64 * 100.).min(100.),
                            ),
                            (Some(current), _) => format!(" · {}", format_bytes(current)),
                            _ => String::new(),
                        };
                        div()
                            .flex()
                            .flex_col()
                            .child(div().font_medium().child(id.clone()))
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}{bytes}", layer.status)),
                            )
                    })),
            )
    }
}

impl RuskerApp {
    /// `reference` must already be validated (non-empty, no whitespace).
    pub(super) fn pull_image(
        &mut self,
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.notify_error("容器引擎不可用", cx);
            cx.notify();
            return;
        };
        let label = format!("拉取镜像 {reference}");
        let revision = self.refresh_revision.advance();
        self.busy = Some(format!("{label}…"));
        self.loading = false;

        let progress_view = cx.new(|_| PullProgressView {
            progress: ImagePullProgress::default(),
        });
        let content = progress_view.clone();
        window.push_notification(
            Notification::new()
                .id::<ImagePullNotification>()
                .title("正在拉取镜像")
                .message(reference.clone())
                .placement(Anchor::BottomRight)
                .delivery(NotificationDelivery::InApp)
                .autohide(false)
                .content(move |_, _, _| content.clone().into_any_element()),
            cx,
        );
        cx.notify();

        let (sender, mut receiver) = tokio::sync::watch::channel(ImagePullProgress::default());
        let handle = engine.pull_image_with_progress(reference.clone(), sender);
        self._action_task = Some(cx.spawn_in(window, async move |this, cx| {
            while receiver.changed().await.is_ok() {
                // Coalesce bursts of layer updates without blocking the engine.
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let progress = receiver.borrow_and_update().clone();
                if this
                    .update_in(cx, |_, _, cx| {
                        progress_view.update(cx, |view, cx| {
                            view.progress = progress;
                            cx.notify();
                        });
                    })
                    .is_err()
                {
                    return;
                }
            }
            let result = await_engine(handle).await;
            _ = this.update_in(cx, |this, window, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.busy = None;
                let notification = match result {
                    Ok(snapshot) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot);
                        this.after_volume_snapshot(cx);
                        Notification::success(reference).title("镜像拉取完成")
                    }
                    Err(error) => {
                        let message = error.to_string();
                        Notification::error(message)
                            .title("镜像拉取失败")
                            .autohide(false)
                    }
                };
                window.push_notification(
                    notification
                        .id::<ImagePullNotification>()
                        .placement(Anchor::BottomRight)
                        .delivery(NotificationDelivery::InApp),
                    cx,
                );
                cx.notify();
            });
        }));
    }
}
