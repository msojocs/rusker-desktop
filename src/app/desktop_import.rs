//! Session-only desktop engine source discovery and import UI. Import tasks
//! have their own lifetime; `busy` keeps ordinary engine mutations out.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopImportGroup {
    project: Option<String>,
    members: Vec<usize>,
}

/// Compose services are selected as a unit; non-Compose containers remain
/// independent entries. Kept UI-framework-free so the grouping contract can be tested.
fn desktop_import_groups(containers: &[DesktopContainer]) -> Vec<DesktopImportGroup> {
    let mut groups = Vec::new();
    let mut projects = HashMap::new();
    for (index, container) in containers.iter().enumerate() {
        if let Some(project) = container
            .compose_project
            .as_ref()
            .filter(|name| !name.is_empty())
        {
            let group_index = *projects.entry(project.clone()).or_insert_with(|| {
                groups.push(DesktopImportGroup {
                    project: Some(project.clone()),
                    members: Vec::new(),
                });
                groups.len() - 1
            });
            groups[group_index].members.push(index);
        } else {
            groups.push(DesktopImportGroup {
                project: None,
                members: vec![index],
            });
        }
    }
    groups
}

fn desktop_group_importable(group: &DesktopImportGroup, blocked: &[bool]) -> bool {
    group.members.iter().all(|&index| !blocked[index])
}

fn desktop_selection_state(
    groups: &[DesktopImportGroup],
    containers: &[DesktopContainer],
    checked: &HashSet<String>,
) -> gpui_kit::base::CheckboxState {
    let mut total = 0;
    let mut selected = 0;
    for index in groups.iter().flat_map(|group| &group.members) {
        total += 1;
        selected += usize::from(checked.contains(&containers[*index].id));
    }
    match (selected, total) {
        (0, _) => gpui_kit::base::CheckboxState::Unchecked,
        (selected, total) if selected == total => gpui_kit::base::CheckboxState::Checked,
        _ => gpui_kit::base::CheckboxState::Indeterminate,
    }
}

fn desktop_group_selected(
    group: &DesktopImportGroup,
    containers: &[DesktopContainer],
    checked: &HashSet<String>,
) -> bool {
    group
        .members
        .iter()
        .all(|&index| checked.contains(&containers[index].id))
}

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

impl DesktopImportState {
    fn finish_import(&mut self, result: Result<DesktopImportReport, String>) {
        self.running = false;
        self.progress.message = t!("desktop_import.progress.finished").into_owned();
        match result {
            Ok(report) => {
                self.progress.completed = self.progress.total;
                if report.failures.is_empty() {
                    self.checked.clear();
                    self.scanned = false;
                    self.containers.clear();
                }
                self.report = Some(report);
            }
            Err(error) => self.error = Some(error),
        }
    }
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
        if let Some(project) = container.compose_project.as_deref() {
            if self.connected
                && self
                    .snapshot
                    .containers
                    .iter()
                    .any(|target| target.compose_project.as_deref() == Some(project))
            {
                return Some(
                    t!("desktop_import.blocked.project_conflict", project = project).into_owned(),
                );
            }
        }
        None
    }

    fn selected_desktop_containers(&self) -> Vec<String> {
        let containers = &self.desktop_import.containers;
        let blocked: Vec<bool> = containers
            .iter()
            .map(|container| self.desktop_container_blocked(container).is_some())
            .collect();
        desktop_import_groups(containers)
            .into_iter()
            .filter(|group| {
                desktop_group_importable(group, &blocked)
                    && desktop_group_selected(group, containers, &self.desktop_import.checked)
            })
            .flat_map(|group| {
                group
                    .members
                    .into_iter()
                    .map(|index| containers[index].id.clone())
            })
            .collect()
    }

    fn scan_desktop_containers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Source discovery deliberately does not require a connected target.
        if !cfg!(windows) || self.desktop_import_conflict() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.open_desktop_import_error(
                t!("desktop_import.error.engine_unavailable").into_owned(),
                window,
                cx,
            );
            return;
        };
        self.desktop_import.scanning = true;
        let source = self.desktop_import.source;
        let handle = engine.containers_from_desktop(source);
        self.desktop_import.scan_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update_in(cx, |this, window, cx| {
                this.desktop_import.scanning = false;
                match result {
                    Ok(containers) => {
                        this.desktop_import.containers = containers;
                        this.desktop_import.checked.clear();
                        this.desktop_import.scanned = true;
                    }
                    Err(error) => {
                        this.open_desktop_import_error(
                            t!(
                                "desktop_import.error.scan_failed",
                                source = source.label(),
                                error = error,
                                hint = desktop_source_hint(source)
                            )
                            .into_owned(),
                            window,
                            cx,
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
            self.open_desktop_import_error(
                t!("desktop_import.error.source_changed").into_owned(),
                window,
                cx,
            );
            return;
        }
        // Recheck after the confirmation dialog: runtime/connection/selection
        // may have changed while it was open. The backend validates again too.
        if let Some(reason) = self.desktop_import_unavailable() {
            self.open_desktop_import_error(reason, window, cx);
            return;
        }
        let eligible = self.selected_desktop_containers();
        if ids.is_empty() || ids.iter().any(|id| !eligible.contains(id)) {
            self.open_desktop_import_error(
                t!("desktop_import.error.selection_changed").into_owned(),
                window,
                cx,
            );
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
            _ = this.update_in(cx, |this, _, cx| {
                this.busy = None;
                // Keep the source list and selection on failure, including partial
                // failures. Refresh the target so conflicts still block retries.
                this.desktop_import.finish_import(result.map_err(|error| {
                    t!("desktop_import.error.import_failed", error = error).into_owned()
                }));
                // The progress dialog becomes the result dialog and stays open.
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_desktop_import_error(
        &self,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.open_alert_dialog(cx, move |dialog, _, _| {
            dialog
                .title(t!("desktop_import.result.error_title").into_owned())
                .description(error.clone())
                .footer(desktop_import_footer())
        });
    }

    fn render_desktop_import_result(&self, cx: &gpui_kit::App) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .when_some(self.desktop_import.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .when_some(self.desktop_import.report.as_ref(), |this, report| {
                this.child(muted(t!("desktop_import.report.hint"), cx))
                    .child(
                        div()
                            .id("desktop-import-results")
                            .max_h(px(400.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(report.imported.iter().map(|item| {
                                div().text_sm().text_color(cx.theme().success).child(
                                    t!("desktop_import.report.success", item = item).to_string(),
                                )
                            }))
                            .children(report.failures.iter().map(|item| {
                                div().text_sm().text_color(cx.theme().danger).child(
                                    t!("desktop_import.report.failure", item = item).to_string(),
                                )
                            })),
                    )
            })
    }

    fn open_desktop_import_progress(&self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = weak.upgrade() else {
                return dialog;
            };
            let (progress, running, indicator, title, result) = {
                let app = this.read(cx);
                let title = if app.desktop_import.running {
                    t!(
                        "desktop_import.progress.title",
                        source = app.desktop_import.source.label()
                    )
                    .into_owned()
                } else if let Some(report) = &app.desktop_import.report {
                    t!(
                        "desktop_import.result.summary",
                        succeeded = report.imported.len(),
                        failed = report.failures.len()
                    )
                    .into_owned()
                } else {
                    t!("desktop_import.result.error_title").into_owned()
                };
                (
                    app.desktop_import.progress.clone(),
                    app.desktop_import.running,
                    app.loading_ring(px(20.), None),
                    title,
                    app.render_desktop_import_result(cx),
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
                .title(title)
                .w(px(520.))
                .keyboard(!running)
                .close_button(!running)
                .overlay_closable(!running)
                .on_cancel(move |_, _, cx| {
                    cancel
                        .upgrade()
                        .is_none_or(|this| !this.read(cx).desktop_import.running)
                })
                .when(!running, |dialog| {
                    dialog.child(result).footer(desktop_import_footer())
                })
                .when(running, |dialog| {
                    dialog.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(muted(t!("desktop_import.running.notice"), cx))
                            .child(
                                div().flex().items_center().gap_3().child(indicator).child(
                                    div().flex_1().min_w_0().child(progress.message.clone()),
                                ),
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
                                    .child(format!(
                                        "{completed}/{total} · {:.0}%",
                                        fraction * 100.
                                    )),
                            ),
                    )
                })
        });
    }

    pub(super) fn render_desktop_import(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.desktop_import;
        let scan_blocked = self.engine.is_none() || self.desktop_import_conflict();
        let unavailable = self.desktop_import_unavailable();
        let selected = self.selected_desktop_containers();
        let import_blocked = unavailable.is_some() || selected.is_empty();
        let groups = desktop_import_groups(&state.containers);
        let blocked: Vec<bool> = state
            .containers
            .iter()
            .map(|container| self.desktop_container_blocked(container).is_some())
            .collect();
        let eligible: Vec<DesktopImportGroup> = groups
            .iter()
            .filter(|group| desktop_group_importable(group, &blocked))
            .cloned()
            .collect();
        let selection_state = desktop_selection_state(&eligible, &state.containers, &state.checked);
        let selection_disabled = eligible.is_empty() || state.running || state.scanning;
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
                    .on_click(cx.listener(|this, _, window, cx| this.scan_desktop_containers(window, cx))))
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
                    .child(gpui_kit::base::Checkbox::new("check-all-desktop-containers")
                        .state(selection_state)
                        .disabled(selection_disabled)
                        .accessibility_label(t!("desktop_import.select_all").into_owned())
                        .flex().items_center().gap_2().text_sm()
                        .when(selection_disabled, |this| this.opacity(0.5))
                        .child(div().size(px(16.)).flex_shrink_0().border_1().rounded(px(4.))
                            .flex().items_center().justify_center()
                            .border_color(cx.theme().input)
                            .bg(cx.theme().input_background())
                            .when(selection_state != gpui_kit::base::CheckboxState::Unchecked, |this| {
                                this.bg(cx.theme().primary).border_color(cx.theme().primary)
                                    .child(gpui_kit::component::Icon::new(if selection_state == gpui_kit::base::CheckboxState::Indeterminate {
                                        IconName::Minus
                                    } else {
                                        IconName::Check
                                    }).size(px(12.)).text_color(cx.theme().primary_foreground))
                            }))
                        .child(t!("desktop_import.select_all").into_owned())
                        .on_change({
                            let weak = cx.weak_entity();
                            move |next, _, _, cx| {
                                _ = weak.update(cx, |this, cx| {
                                    if this.desktop_import_running() || this.desktop_import_scanning() {
                                        return;
                                    }
                                    for group in &eligible {
                                        for &index in &group.members {
                                            let id = &this.desktop_import.containers[index].id;
                                            if next == gpui_kit::base::CheckboxState::Checked {
                                                this.desktop_import.checked.insert(id.clone());
                                            } else {
                                                this.desktop_import.checked.remove(id);
                                            }
                                        }
                                    }
                                    cx.notify();
                                });
                            }
                        }))
                    // Stop bubbling outside the viewport, after its native scroll handler ran.
                    .child(div().on_scroll(|_, _, cx| cx.stop_propagation())
                        .child(div().id("desktop-source-containers").max_h(px(360.)).overflow_y_scroll()
                        .flex().flex_col().gap_3()
                        .children(groups.iter().map(|group| {
                            let group_blocked = !desktop_group_importable(group, &blocked);
                            div().flex().flex_col().gap_2().children(group.project.as_ref().map(|project| {
                                div().font_semibold().text_sm().child(t!("desktop_import.group.title", project = project).to_string())
                            }))
                            .children(group.members.iter().map(|&index| {
                                let container = &state.containers[index];
                                let ids: Vec<String> = group.members.iter().map(|&member| state.containers[member].id.clone()).collect();
                                let reason = self.desktop_container_blocked(container);
                                let disabled = group_blocked || state.running || state.scanning;
                                div().p_3().border_1().border_color(cx.theme().border).rounded_md()
                                    .flex_shrink_0().flex().flex_col().gap_2()
                                    .child(Checkbox::new(("check-desktop-container", index))
                                        .label(container.name.clone())
                                        .checked(state.checked.contains(&container.id))
                                        .disabled(disabled)
                                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                            if this.desktop_import_running() || this.desktop_import_scanning() { return; }
                                            for id in &ids {
                                                if *checked { this.desktop_import.checked.insert(id.clone()); }
                                                else { this.desktop_import.checked.remove(id); }
                                            }
                                            cx.notify();
                                        })))
                                    .when_some(container.compose_service.clone(), |this, service| this.child(muted(t!("desktop_import.container.service", service = service), cx)))
                                    .child(muted(t!("desktop_import.container.meta", image = container.image, state = container.state), cx))
                                    .child(muted(t!("desktop_import.container.source_id", id = container.id), cx))
                                    .when_some(reason, |this, reason| this.child(div().text_sm()
                                        .text_color(cx.theme().warning).child(t!("desktop_import.container.blocked", reason = reason).to_string())))
                            }))
                        })))))
            })
            .when(state.running, |this| this.child(div().flex().flex_col().gap_2()
                .child(div().flex().items_center().gap_2()
                    .child(self.loading_ring(px(16.), None))
                    .child(t!("desktop_import.running.notice").to_string()))
                .child(div().text_sm().child(state.progress.message.clone()))))
    }
}

fn desktop_import_footer() -> Div {
    div().flex().justify_end().child(
        button("close-desktop-import")
            .primary()
            .label(t!("desktop_import.action.close"))
            .on_click(|_, window, cx| window.close_dialog(cx)),
    )
}

/// Localized connection hint for a desktop import source. The engine-side
/// `connection_hint()` returns a fixed Chinese string; this maps to i18n keys.
fn desktop_source_hint(source: DesktopImportSource) -> std::borrow::Cow<'static, str> {
    match source {
        DesktopImportSource::DockerDesktop => t!("desktop_import.hint.docker_desktop"),
        DesktopImportSource::RancherDesktop => t!("desktop_import.hint.rancher_desktop"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, project: Option<&str>) -> DesktopContainer {
        DesktopContainer {
            id: id.into(),
            compose_project: project.map(str::to_owned),
            ..Default::default()
        }
    }

    fn import_state() -> DesktopImportState {
        DesktopImportState {
            containers: vec![container("a", None), container("b", None)],
            checked: HashSet::from(["a".into(), "b".into()]),
            scanned: true,
            running: true,
            progress: DesktopImportProgress {
                total: 2,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn failed_import_preserves_source_list_and_selection() {
        let mut state = import_state();
        state.finish_import(Err("import failed".into()));

        assert!(!state.running);
        assert!(state.scanned);
        assert_eq!(state.containers.len(), 2);
        assert_eq!(state.checked, HashSet::from(["a".into(), "b".into()]));
        assert_eq!(state.error.as_deref(), Some("import failed"));
        assert!(state.report.is_none());
    }

    #[test]
    fn reported_failures_preserve_source_list_and_selection() {
        for imported in [vec![], vec!["a".into()]] {
            let mut state = import_state();
            state.finish_import(Ok(DesktopImportReport {
                imported,
                failures: vec!["b: failed".into()],
            }));

            assert!(!state.running);
            assert!(state.scanned);
            assert_eq!(state.containers.len(), 2);
            assert_eq!(state.checked, HashSet::from(["a".into(), "b".into()]));
            assert_eq!(state.report.as_ref().unwrap().failures, ["b: failed"]);
            assert!(state.error.is_none());
        }
    }

    #[test]
    fn successful_import_clears_source_list_and_keeps_report() {
        let mut state = import_state();
        state.finish_import(Ok(DesktopImportReport {
            imported: vec!["a".into(), "b".into()],
            failures: vec![],
        }));

        assert!(!state.running);
        assert!(!state.scanned);
        assert!(state.containers.is_empty());
        assert!(state.checked.is_empty());
        assert_eq!(state.progress.completed, 2);
        assert_eq!(state.report.as_ref().unwrap().imported, ["a", "b"]);
        assert!(state.error.is_none());
    }

    #[test]
    fn compose_members_are_grouped_and_regular_containers_stay_individual() {
        let containers = vec![
            container("a", Some("shop")),
            container("solo", None),
            container("b", Some("shop")),
            container("c", Some("other")),
        ];
        let groups = desktop_import_groups(&containers);
        assert_eq!(groups.len(), 3);
        assert_eq!(
            groups[0],
            DesktopImportGroup {
                project: Some("shop".into()),
                members: vec![0, 2]
            }
        );
        assert_eq!(
            groups[1],
            DesktopImportGroup {
                project: None,
                members: vec![1]
            }
        );
        assert_eq!(
            groups[2],
            DesktopImportGroup {
                project: Some("other".into()),
                members: vec![3]
            }
        );
    }

    #[test]
    fn select_all_reflects_none_partial_and_all_eligible_containers() {
        use gpui_kit::base::CheckboxState;

        let containers = vec![
            container("a", None),
            container("b", None),
            container("blocked", None),
        ];
        let groups = desktop_import_groups(&containers);
        let eligible = &groups[..2];
        let mut checked = HashSet::new();
        assert_eq!(
            desktop_selection_state(eligible, &containers, &checked),
            CheckboxState::Unchecked
        );
        checked.insert("a".into());
        assert_eq!(
            desktop_selection_state(eligible, &containers, &checked),
            CheckboxState::Indeterminate
        );
        checked.insert("b".into());
        assert_eq!(
            desktop_selection_state(eligible, &containers, &checked),
            CheckboxState::Checked
        );
        checked.clear();
        checked.insert("blocked".into());
        assert_eq!(
            desktop_selection_state(eligible, &containers, &checked),
            CheckboxState::Unchecked
        );
        assert_eq!(
            desktop_selection_state(&[], &containers, &checked),
            CheckboxState::Unchecked
        );
    }

    #[test]
    fn select_all_is_indeterminate_for_a_selected_compose_group() {
        use gpui_kit::base::CheckboxState;

        let containers = vec![
            container("web", Some("shop")),
            container("db", Some("shop")),
            container("solo", None),
        ];
        let groups = desktop_import_groups(&containers);
        let checked = HashSet::from(["web".into(), "db".into()]);
        assert_eq!(
            desktop_selection_state(&groups, &containers, &checked),
            CheckboxState::Indeterminate
        );
    }

    #[test]
    fn compose_selection_and_blocking_apply_to_the_whole_group() {
        let containers = vec![
            container("a", Some("shop")),
            container("b", Some("shop")),
            container("solo", None),
        ];
        let groups = desktop_import_groups(&containers);
        let mut checked = HashSet::new();
        checked.insert("a".to_owned());
        assert!(!desktop_group_selected(&groups[0], &containers, &checked));
        checked.insert("b".to_owned());
        assert!(desktop_group_selected(&groups[0], &containers, &checked));
        assert!(desktop_group_importable(&groups[0], &[false, false, true]));
        assert!(!desktop_group_importable(&groups[0], &[false, true, false]));
        assert!(desktop_group_importable(&groups[1], &[true, true, false]));
    }
}
