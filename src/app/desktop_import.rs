//! Session-only desktop engine source discovery and import UI. Import tasks
//! have their own lifetime; `busy` keeps ordinary engine mutations out.

use std::{collections::HashSet, time::Duration};

use gpui_kit::{
    base::{Disableable as _, StyledExt as _},
    component::{
        button::{ButtonVariant, ButtonVariants as _},
        checkbox::Checkbox,
        ActiveTheme as _, IconName, WindowExt as _,
    },
    div,
    prelude::FluentBuilder as _,
    px, relative, Context, Div, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Task, Window,
};
use rusker_desktop::engine::{
    DesktopContainer, DesktopImportProgress, DesktopImportReport, DesktopImportSource,
};

use super::{await_engine, callout, CalloutKind, PointerCursor as _, RuskerApp};
use crate::ui::{button, confirmation_footer, muted};

#[derive(Default)]
pub(super) struct DesktopImportState {
    source: DesktopImportSource,
    containers: Vec<DesktopContainer>,
    checked: HashSet<String>,
    scanned: bool,
    scanning: bool,
    running: bool,
    progress: DesktopImportProgress,
    error: Option<String>,
    report: Option<DesktopImportReport>,
    scan_task: Option<Task<()>>,
    import_task: Option<Task<()>>,
}

impl RuskerApp {
    fn select_desktop_source(&mut self, source: DesktopImportSource, cx: &mut Context<Self>) {
        if self.desktop_import_conflict() || self.desktop_import.source == source {
            return;
        }
        // Selection, results and confirmations always belong to a single source.
        self.desktop_import = DesktopImportState {
            source,
            ..Default::default()
        };
        cx.notify();
    }

    pub(super) fn desktop_import_running(&self) -> bool {
        self.desktop_import.running
    }

    pub(super) fn desktop_import_scanning(&self) -> bool {
        self.desktop_import.scanning
    }

    fn desktop_import_conflict(&self) -> bool {
        self.desktop_import.running
            || self.desktop_import.scanning
            || self.busy.is_some()
            || self.runtime_busy.is_some()
            || self.settings_applying
            || self.engine_transition.is_some()
            || self.recovering()
            || self.quitting
    }

    fn desktop_import_unavailable(&self) -> Option<String> {
        if !self.manages_engine() {
            Some(t!("desktop_import.unavailable.not_managed").into_owned())
        } else if !self.connected {
            Some(t!("desktop_import.unavailable.not_connected").into_owned())
        } else if self.desktop_import_conflict() {
            Some(t!("desktop_import.unavailable.busy").into_owned())
        } else {
            None
        }
    }

    fn desktop_container_blocked(&self, container: &DesktopContainer) -> Option<String> {
        if let Some(reason) = &container.blocked_reason {
            return Some(reason.clone());
        }
        if !matches!(container.state.as_str(), "created" | "exited") {
            return Some(
                t!(
                    "desktop_import.blocked.stop_first",
                    source = self.desktop_import.source.label()
                )
                .into_owned(),
            );
        }
        if self.connected
            && self
                .snapshot
                .containers
                .iter()
                .any(|target| target.name == container.name)
        {
            return Some(t!("desktop_import.blocked.name_conflict").into_owned());
        }
        None
    }

    fn selected_desktop_containers(&self) -> Vec<String> {
        self.desktop_import
            .containers
            .iter()
            .filter(|container| {
                self.desktop_import.checked.contains(&container.id)
                    && self.desktop_container_blocked(container).is_none()
            })
            .map(|container| container.id.clone())
            .collect()
    }

    fn scan_desktop_containers(&mut self, cx: &mut Context<Self>) {
        // Source discovery deliberately does not require a connected target.
        if !cfg!(windows) || self.desktop_import_conflict() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.desktop_import.error =
                Some(t!("desktop_import.error.engine_unavailable").into_owned());
            cx.notify();
            return;
        };
        self.desktop_import.scanning = true;
        self.desktop_import.scanned = false;
        self.desktop_import.error = None;
        self.desktop_import.checked.clear();
        self.desktop_import.containers.clear();
        let source = self.desktop_import.source;
        let handle = engine.containers_from_desktop(source);
        self.desktop_import.scan_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                this.desktop_import.scanning = false;
                match result {
                    Ok(containers) => {
                        this.desktop_import.containers = containers;
                        this.desktop_import.scanned = true;
                    }
                    Err(error) => {
                        this.desktop_import.error = Some(
                            t!(
                                "desktop_import.error.scan_failed",
                                source = source.label(),
                                error = error,
                                hint = desktop_source_hint(source)
                            )
                            .into_owned(),
                        );
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_desktop_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.desktop_import_unavailable().is_some() {
            return;
        }
        let ids = self.selected_desktop_containers();
        if ids.is_empty() {
            return;
        }
        let names = self
            .desktop_import
            .containers
            .iter()
            .filter(|container| ids.contains(&container.id))
            .map(|container| container.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let source = self.desktop_import.source;
        let title = t!(
            "desktop_import.confirm.title",
            count = ids.len(),
            source = source.label()
        )
        .into_owned();
        let description = t!("desktop_import.confirm.description", names = names).into_owned();
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let ids = ids.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text(t!("desktop_import.confirm.ok"))
                .cancel_text(t!("desktop_import.confirm.cancel"))
                .footer(confirmation_footer(
                    t!("desktop_import.confirm.ok"),
                    ButtonVariant::Primary,
                    cx,
                ))
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    _ = weak.update(cx, |this, cx| {
                        this.start_desktop_import(source, ids.clone(), window, cx)
                    });
                    false
                })
        });
    }

    fn start_desktop_import(
        &mut self,
        source: DesktopImportSource,
        ids: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if source != self.desktop_import.source {
            self.desktop_import.error =
                Some(t!("desktop_import.error.source_changed").into_owned());
            cx.notify();
            return;
        }
        // Recheck after the confirmation dialog: runtime/connection/selection
        // may have changed while it was open. The backend validates again too.
        if let Some(reason) = self.desktop_import_unavailable() {
            self.desktop_import.error = Some(reason);
            cx.notify();
            return;
        }
        let eligible = self.selected_desktop_containers();
        if ids.is_empty() || ids.iter().any(|id| !eligible.contains(id)) {
            self.desktop_import.error =
                Some(t!("desktop_import.error.selection_changed").into_owned());
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        self.refresh_revision.advance();
        self.loading = false;
        self.desktop_import.running = true;
        self.desktop_import.error = None;
        self.desktop_import.report = None;
        let total = ids.len();
        self.desktop_import.progress = DesktopImportProgress {
            total,
            message: t!("desktop_import.progress.preparing").into_owned(),
            ..Default::default()
        };
        self.busy = Some(
            t!(
                "desktop_import.progress.importing_from",
                source = source.label()
            )
            .into_owned(),
        );
        self.open_desktop_import_progress(window, cx);
        let (sender, mut receiver) = tokio::sync::watch::channel(DesktopImportProgress::default());
        let handle = engine.import_from_desktop(source, ids, sender);
        self.desktop_import.import_task = Some(cx.spawn_in(window, async move |this, cx| {
            while receiver.changed().await.is_ok() {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let progress = receiver.borrow_and_update().clone();
                if this
                    .update_in(cx, |this, _, cx| {
                        this.desktop_import.progress = progress;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let result = await_engine(handle).await;
            _ = this.update_in(cx, |this, window, cx| {
                this.desktop_import.running = false;
                this.busy = None;
                this.desktop_import.checked.clear();
                // Source/target state may have changed; require a fresh scan
                // before another import, but keep the complete result visible.
                this.desktop_import.scanned = false;
                this.desktop_import.containers.clear();
                match result {
                    Ok(report) => {
                        let summary = t!(
                            "desktop_import.result.summary",
                            succeeded = report.imported.len(),
                            failed = report.failures.len()
                        )
                        .into_owned();
                        this.desktop_import.progress.message = summary.clone();
                        this.desktop_import.progress.completed = this.desktop_import.progress.total;
                        if report.failures.is_empty() {
                            this.notify_success(summary, cx);
                        } else {
                            this.notify_error(
                                t!("desktop_import.result.see_details", summary = summary),
                                cx,
                            );
                        }
                        this.desktop_import.report = Some(report);
                    }
                    Err(error) => {
                        this.desktop_import.progress.message =
                            t!("desktop_import.progress.finished").into_owned();
                        this.desktop_import.error = Some(
                            t!("desktop_import.error.import_failed", error = error).into_owned(),
                        );
                        this.notify_error(
                            t!(
                                "desktop_import.error.import_failed_short",
                                source = source.label()
                            ),
                            cx,
                        );
                    }
                }
                // Also refresh after partial failure to show everything created.
                window.close_dialog(cx);
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_desktop_import_progress(&self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = weak.upgrade() else {
                return dialog;
            };
            let (progress, running, source, indicator) = {
                let app = this.read(cx);
                (
                    app.desktop_import.progress.clone(),
                    app.desktop_import.running,
                    app.desktop_import.source,
                    app.loading_ring(px(20.), None),
                )
            };
            let total = progress.total;
            let completed = progress.completed.min(total);
            let fraction = if total == 0 {
                0.
            } else {
                completed as f32 / total as f32
            };
            let cancel = weak.clone();
            dialog
                .title(t!("desktop_import.progress.title", source = source.label()).into_owned())
                .w(px(520.))
                .keyboard(!running)
                .on_cancel(move |_, _, cx| {
                    cancel
                        .upgrade()
                        .is_none_or(|this| !this.read(cx).desktop_import.running)
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(muted(t!("desktop_import.running.notice"), cx))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(indicator)
                                .child(div().flex_1().min_w_0().child(progress.message.clone())),
                        )
                        .child(
                            div()
                                .h(px(8.))
                                .w_full()
                                .rounded_full()
                                .bg(cx.theme().secondary)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(fraction))
                                        .rounded_full()
                                        .bg(cx.theme().primary),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("desktop_import.progress.overall").into_owned())
                                .child(format!("{completed}/{total} · {:.0}%", fraction * 100.)),
                        ),
                )
        });
    }

    pub(super) fn render_desktop_import(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.desktop_import;
        let scan_blocked = self.engine.is_none() || self.desktop_import_conflict();
        let unavailable = self.desktop_import_unavailable();
        let selected = self.selected_desktop_containers();
        let import_blocked = unavailable.is_some() || selected.is_empty();
        let eligible: Vec<String> = state
            .containers
            .iter()
            .filter(|container| self.desktop_container_blocked(container).is_none())
            .map(|container| container.id.clone())
            .collect();
        let all_checked =
            !eligible.is_empty() && eligible.iter().all(|id| state.checked.contains(id));
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(muted(t!("desktop_import.intro"), cx))
            .child(div().flex().flex_wrap().items_center().gap_3()
                .child(muted(t!("desktop_import.source_label"), cx))
                .children([DesktopImportSource::DockerDesktop, DesktopImportSource::RancherDesktop].into_iter().enumerate().map(|(index, source)| {
                    button(("desktop-import-source", index))
                        .outline()
                        .label(if source == state.source { format!("✓ {}", source.label()) } else { source.label().into() })
                        .disabled(self.desktop_import_conflict())
                        .on_click(cx.listener(move |this, _, _, cx| this.select_desktop_source(source, cx)))
                })))
            .child(muted(desktop_source_hint(state.source), cx))
            .child(callout(
                CalloutKind::Warning,
                t!("desktop_import.callout.title"),
                muted(t!("desktop_import.callout.body"), cx),
                cx,
            ))
            .child(div().flex().flex_wrap().items_center().gap_3()
                .child(button("scan-desktop-containers")
                    .outline()
                    .icon(IconName::RefreshCw)
                    .label(if state.scanning {
                        t!("desktop_import.action.scanning").into_owned()
                    } else {
                        t!("desktop_import.action.scan", source = state.source.label()).into_owned()
                    })
                    .disabled(scan_blocked)
                    .pointer(!scan_blocked)
                    .on_click(cx.listener(|this, _, _, cx| this.scan_desktop_containers(cx))))
                .child(button("import-desktop-containers")
                    .primary()
                    .label(t!("desktop_import.action.import_selected", count = selected.len()))
                    .disabled(import_blocked)
                    .pointer(!import_blocked)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_desktop_import(window, cx)))))
            .when_some(unavailable, |this, reason| this.child(muted(reason, cx)))
            .when(state.scanning, |this| this.child(div().flex().items_center().gap_2()
                .child(self.loading_ring(px(16.), None))
                .child(muted(t!("desktop_import.progress.reading", source = state.source.label()), cx))))
            .when(state.scanned && state.containers.is_empty(), |this| {
                this.child(muted(t!("desktop_import.empty", source = state.source.label()), cx))
            })
            .when(!state.containers.is_empty(), |this| {
                this.child(div().flex().flex_col().gap_3()
                    .child(Checkbox::new("check-all-desktop-containers")
                        .label(t!("desktop_import.select_all").into_owned())
                        .checked(all_checked)
                        .disabled(eligible.is_empty() || state.running || state.scanning)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if this.desktop_import_running() || this.desktop_import_scanning() {
                                return;
                            }
                            for id in &eligible {
                                if *checked {
                                    this.desktop_import.checked.insert(id.clone());
                                } else {
                                    this.desktop_import.checked.remove(id);
                                }
                            }
                            cx.notify();
                        })))
                    // Stop bubbling outside the viewport, after its native scroll handler ran.
                    .child(div().on_scroll(|_, _, cx| cx.stop_propagation())
                        .child(div().id("desktop-source-containers").max_h(px(360.)).overflow_y_scroll()
                        .flex().flex_col().gap_3()
                        .children(state.containers.iter().enumerate().map(|(index, container)| {
                            let id = container.id.clone();
                            let blocked = self.desktop_container_blocked(container);
                            let disabled = blocked.is_some() || state.running || state.scanning;
                            div().p_3().border_1().border_color(cx.theme().border).rounded_md()
                                .flex_shrink_0().flex().flex_col().gap_2()
                                .child(Checkbox::new(("check-desktop-container", index))
                                    .label(container.name.clone())
                                    .checked(state.checked.contains(&container.id))
                                    .disabled(disabled)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        if this.desktop_import_running() || this.desktop_import_scanning() {
                                            return;
                                        }
                                        if *checked {
                                            this.desktop_import.checked.insert(id.clone());
                                        } else {
                                            this.desktop_import.checked.remove(&id);
                                        }
                                        cx.notify();
                                    })))
                                .child(muted(t!("desktop_import.container.meta", image = container.image, state = container.state), cx))
                                .child(muted(t!("desktop_import.container.source_id", id = container.id), cx))
                                .when_some(blocked, |this, reason| this.child(div().text_sm()
                                    .text_color(cx.theme().warning).child(t!("desktop_import.container.blocked", reason = reason).to_string())))
                        })))))
            })
            .when(state.running, |this| this.child(div().flex().flex_col().gap_2()
                .child(div().flex().items_center().gap_2()
                    .child(self.loading_ring(px(16.), None))
                    .child(t!("desktop_import.running.notice").to_string()))
                .child(div().text_sm().child(state.progress.message.clone()))))
            .when_some(state.error.clone(), |this, error| this.child(div().text_sm()
                .text_color(cx.theme().danger).child(error)))
            .when_some(state.report.as_ref(), |this, report| {
                this.child(div().flex().flex_col().gap_3()
                    .child(div().font_semibold().child(
                        t!(
                            "desktop_import.report.title",
                            succeeded = report.imported.len(),
                            failed = report.failures.len()
                        )
                        .to_string(),
                    ))
                    .child(muted(t!("desktop_import.report.hint"), cx))
                    .child(div().on_scroll(|_, _, cx| cx.stop_propagation())
                        .child(div().id("desktop-import-results").max_h(px(400.)).overflow_y_scroll()
                        .flex().flex_col().gap_2()
                        .children(report.imported.iter().map(|item| div().text_sm()
                            .text_color(cx.theme().success).child(t!("desktop_import.report.success", item = item).to_string())))
                        .children(report.failures.iter().map(|item| div().text_sm()
                            .text_color(cx.theme().danger).child(t!("desktop_import.report.failure", item = item).to_string()))))))
            })
    }
}

/// Localized connection hint for a desktop import source. The engine-side
/// `connection_hint()` returns a fixed Chinese string; this maps to i18n keys.
fn desktop_source_hint(source: DesktopImportSource) -> std::borrow::Cow<'static, str> {
    match source {
        DesktopImportSource::DockerDesktop => t!("desktop_import.hint.docker_desktop"),
        DesktopImportSource::RancherDesktop => t!("desktop_import.hint.rancher_desktop"),
    }
}
