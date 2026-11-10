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
    px, Context, Div, InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, Task, Window,
};
use rusker_desktop::engine::{DesktopContainer, DesktopImportReport, DesktopImportSource};

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
    progress: String,
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

    fn desktop_import_unavailable(&self) -> Option<&'static str> {
        if !self.manages_engine() {
            Some("仅支持导入到 Rusker 管理的 Engine；请取消 DOCKER_HOST 后重新启动 Rusker。")
        } else if !self.connected {
            Some("目标 Engine 尚未连接。可以先扫描来源；连接 Rusker Engine 后才能导入。")
        } else if self.desktop_import_conflict() {
            Some("扫描、导入或其他操作正在进行，请等待完成后再导入。")
        } else {
            None
        }
    }

    fn desktop_container_blocked(&self, container: &DesktopContainer) -> Option<String> {
        if let Some(reason) = &container.blocked_reason {
            return Some(reason.clone());
        }
        if !matches!(container.state.as_str(), "created" | "exited") {
            return Some(format!(
                "请先在 {} 中停止此容器，再重新扫描。",
                self.desktop_import.source.label()
            ));
        }
        if self.connected
            && self
                .snapshot
                .containers
                .iter()
                .any(|target| target.name == container.name)
        {
            return Some("目标 Engine 已有同名容器，不会覆盖；请先处理名称冲突。".into());
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
            self.desktop_import.error = Some("容器引擎服务不可用，请重启 Rusker".into());
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
                        this.desktop_import.error = Some(format!(
                            "扫描 {} 失败：{error}。{}。",
                            source.label(),
                            source.connection_hint()
                        ));
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
        let title = format!("导入 {} 个 {} 容器？", ids.len(), source.label());
        let description = format!(
            "{names}\n\n将复制容器 rootfs、配置及本地卷数据到 Rusker，可能耗时较长。目标磁盘和用于 rootfs 临时归档的系统盘均需足够空间。文件系统镜像会被扁平化，不保留原镜像分层历史。\n\n源容器必须保持停止，源容器与原卷数据不会被修改；卷复制会创建并清理只读辅助容器，可能准备 busybox 镜像。正在被其他运行中容器使用的源卷会阻止导入。同批共享源卷映射为同一个新卷。\n\n不会覆盖目标同名容器，新容器不会自动启动。bind 目录、自定义网络或容器依赖不受支持，会阻止导入。导入期间不能退出 Rusker、修改目标 Engine 或应用设置。"
        );
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let ids = ids.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("确认导入")
                .cancel_text("取消")
                .footer(confirmation_footer("确认导入", ButtonVariant::Primary, cx))
                .on_ok(move |_, _, cx| {
                    _ = weak.update(cx, |this, cx| {
                        this.start_desktop_import(source, ids.clone(), cx)
                    });
                    true
                })
        });
    }

    fn start_desktop_import(
        &mut self,
        source: DesktopImportSource,
        ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if source != self.desktop_import.source {
            self.desktop_import.error = Some("导入来源已变化，请重新扫描并确认。".into());
            cx.notify();
            return;
        }
        // Recheck after the confirmation dialog: runtime/connection/selection
        // may have changed while it was open. The backend validates again too.
        if let Some(reason) = self.desktop_import_unavailable() {
            self.desktop_import.error = Some(reason.into());
            cx.notify();
            return;
        }
        let eligible = self.selected_desktop_containers();
        if ids.is_empty() || ids.iter().any(|id| !eligible.contains(id)) {
            self.desktop_import.error = Some("所选容器状态已变化，请重新扫描并选择。".into());
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
        self.desktop_import.progress = "正在准备导入…".into();
        self.busy = Some(format!("正在从 {} 导入容器…", source.label()));
        let (sender, mut receiver) = tokio::sync::watch::channel(String::new());
        let handle = engine.import_from_desktop(source, ids, sender);
        self.desktop_import.import_task = Some(cx.spawn(async move |this, cx| {
            while receiver.changed().await.is_ok() {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let progress = receiver.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.desktop_import.progress = progress;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                this.desktop_import.running = false;
                this.busy = None;
                this.desktop_import.checked.clear();
                // Source/target state may have changed; require a fresh scan
                // before another import, but keep the complete result visible.
                this.desktop_import.scanned = false;
                this.desktop_import.containers.clear();
                match result {
                    Ok(report) => {
                        let summary = format!(
                            "导入完成：{} 项成功，{} 项失败",
                            report.imported.len(),
                            report.failures.len()
                        );
                        this.desktop_import.progress = summary.clone();
                        if report.failures.is_empty() {
                            this.notify_success(summary, cx);
                        } else {
                            this.notify_error(format!("{summary}；请在设置中查看完整逐项结果"), cx);
                        }
                        this.desktop_import.report = Some(report);
                    }
                    Err(error) => {
                        this.desktop_import.progress = "导入已结束".into();
                        this.desktop_import.error = Some(format!(
                            "导入失败：{error}。目标可能已有部分数据，请查看目标容器和卷后再重试。"
                        ));
                        this.notify_error(
                            format!("{} 导入失败，请在设置中查看详情", source.label()),
                            cx,
                        );
                    }
                }
                // Also refresh after partial failure to show everything created.
                this.refresh(cx);
                cx.notify();
            });
        }));
        cx.notify();
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
            .child(muted("从 Docker Desktop 或 Rancher Desktop 的 Linux Engine 复制已停止的容器到 Rusker。扫描只读取源信息，不要求目标 Engine 已连接。", cx))
            .child(div().flex().flex_wrap().items_center().gap_3()
                .child(muted("导入来源", cx))
                .children([DesktopImportSource::DockerDesktop, DesktopImportSource::RancherDesktop].into_iter().enumerate().map(|(index, source)| {
                    button(("desktop-import-source", index))
                        .outline()
                        .label(if source == state.source { format!("✓ {}", source.label()) } else { source.label().into() })
                        .disabled(self.desktop_import_conflict())
                        .on_click(cx.listener(move |this, _, _, cx| this.select_desktop_source(source, cx)))
                })))
            .child(muted(state.source.connection_hint(), cx))
            .child(callout(
                CalloutKind::Warning,
                "复制导入，不是迁移或同步",
                muted("复制 rootfs、配置与本地卷；文件系统镜像会扁平化，不保留原镜像分层历史。源容器与原卷数据保持不变；卷复制会创建/清理只读辅助容器，可能准备 busybox 镜像。请为目标数据及系统盘上的 rootfs 临时归档预留空间。目标同名容器不会被覆盖，新容器保持停止。bind 目录、自定义网络、容器依赖，以及其他运行中容器正在使用的源卷会阻止导入。同批共享源卷映射到同一个新卷。导入期间保持源容器停止。", cx),
                cx,
            ))
            .child(div().flex().flex_wrap().items_center().gap_3()
                .child(button("scan-desktop-containers")
                    .outline()
                    .icon(IconName::RefreshCw)
                    .label(if state.scanning { "扫描中…".to_string() } else { format!("扫描 {}", state.source.label()) })
                    .disabled(scan_blocked)
                    .pointer(!scan_blocked)
                    .on_click(cx.listener(|this, _, _, cx| this.scan_desktop_containers(cx))))
                .child(button("import-desktop-containers")
                    .primary()
                    .label(format!("导入所选（{}）", selected.len()))
                    .disabled(import_blocked)
                    .pointer(!import_blocked)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_desktop_import(window, cx)))))
            .when_some(unavailable, |this, reason| this.child(muted(reason, cx)))
            .when(state.scanning, |this| this.child(div().flex().items_center().gap_2()
                .child(self.loading_ring(px(16.), None))
                .child(muted(format!("正在读取 {} 容器…", state.source.label()), cx))))
            .when(state.scanned && state.containers.is_empty(), |this| {
                this.child(muted(format!("{} 中没有容器。", state.source.label()), cx))
            })
            .when(!state.containers.is_empty(), |this| {
                this.child(div().flex().flex_col().gap_3()
                    .child(Checkbox::new("check-all-desktop-containers")
                        .label("全选可导入容器")
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
                                .child(muted(format!("镜像：{} · 状态：{}", container.image, container.state), cx))
                                .child(muted(format!("源 ID：{}", container.id), cx))
                                .when_some(blocked, |this, reason| this.child(div().text_sm()
                                    .text_color(cx.theme().warning).child(format!("不能导入：{reason}"))))
                        })))))
            })
            .when(state.running, |this| this.child(div().flex().flex_col().gap_2()
                .child(div().flex().items_center().gap_2()
                    .child(self.loading_ring(px(16.), None))
                    .child("正在导入，请勿退出 Rusker 或启动源容器"))
                .child(div().text_sm().child(state.progress.clone()))))
            .when_some(state.error.clone(), |this, error| this.child(div().text_sm()
                .text_color(cx.theme().danger).child(error)))
            .when_some(state.report.as_ref(), |this, report| {
                this.child(div().flex().flex_col().gap_3()
                    .child(div().font_semibold().child(format!(
                        "最近一次导入结果：{} 项成功，{} 项失败", report.imported.len(), report.failures.len()
                    )))
                    .child(muted("成功创建的容器不会自动启动；请到容器页检查后手动启动。以下为完整逐项结果，仅在本次会话保留。", cx))
                    .child(div().on_scroll(|_, _, cx| cx.stop_propagation())
                        .child(div().id("desktop-import-results").max_h(px(400.)).overflow_y_scroll()
                        .flex().flex_col().gap_2()
                        .children(report.imported.iter().map(|item| div().text_sm()
                            .text_color(cx.theme().success).child(format!("成功：{item}"))))
                        .children(report.failures.iter().map(|item| div().text_sm()
                            .text_color(cx.theme().danger).child(format!("失败：{item}")))))))
            })
    }
}
