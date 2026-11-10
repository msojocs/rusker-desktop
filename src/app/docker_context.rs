//! Offers to point the Docker CLI at the managed engine (`docker context use
//! rusker`) once it is running, and shows the current context in settings.

use gpui_kit::{
    base::Disableable as _,
    component::{button::ButtonVariants as _, Sizable as _, WindowExt as _},
    div,
    prelude::FluentBuilder as _,
    Context, Div, ParentElement as _, Styled as _,
};
use rusker_desktop::{
    docker_context::{
        current_context, restore_previous_context, use_rusker_context, CliContext, RUSKER_CONTEXT,
    },
    runtime_host::MANAGED_DOCKER_HOST,
    settings::docker_client_config_path,
};

use super::{settings_label, PointerCursor as _, RuskerApp};
use crate::ui::{button, muted};

impl RuskerApp {
    /// Re-reads the CLI's current context for the settings page.
    pub(super) fn load_docker_context(&mut self) {
        self.docker_context = Some(
            docker_client_config_path()
                .ok_or_else(|| "无法确定 Docker 客户端配置目录".to_string())
                .and_then(|path| current_context(&path)),
        );
    }

    /// Called whenever the engine connects; asks at most once per session,
    /// and only when the CLI uses another context that could be switched.
    pub(super) fn maybe_prompt_docker_context(&mut self, cx: &mut Context<Self>) {
        if !self.manages_engine()
            || self.docker_context_prompted
            || self.settings.skip_docker_context_prompt
        {
            return;
        }
        self.docker_context_prompted = true;
        self.load_docker_context();
        let Some(Ok(CliContext::Other(current))) = self.docker_context.clone() else {
            return;
        };
        let title = "将 Docker CLI 切换到 Rusker？";
        let description = format!(
            "Docker CLI 当前使用“{current}”上下文，docker 命令不会连接 Rusker 的 Engine。\n\n切换后将创建并使用“{RUSKER_CONTEXT}”上下文（{MANAGED_DOCKER_HOST}）。这对当前 Windows 用户的所有 Docker CLI 生效，包括其他软件自带的 docker.exe；可随时执行 docker context use {current} 切回。"
        );
        let weak = cx.weak_entity();
        let window = self.window_handle;
        cx.defer(move |cx| {
            _ = window.update(cx, |_, window, cx| {
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let skip = weak.clone();
                    let switch = weak.clone();
                    dialog.title(title).description(description.clone()).footer(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button("docker-context-skip")
                                    .ghost()
                                    .label("不再提示")
                                    .on_click(move |_, window, cx| {
                                        _ = skip.update(cx, |this, cx| {
                                            this.skip_docker_context_prompt(cx);
                                        });
                                        window.close_dialog(cx);
                                    }),
                            )
                            .child(div().flex_1())
                            .child(
                                button("docker-context-later")
                                    .outline()
                                    .label("暂不")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                button("docker-context-switch")
                                    .primary()
                                    .label("切换")
                                    .on_click(move |_, window, cx| {
                                        _ = switch.update(cx, |this, cx| {
                                            this.switch_docker_context(cx);
                                        });
                                        window.close_dialog(cx);
                                    }),
                            ),
                    )
                });
            });
        });
    }

    fn switch_docker_context(&mut self, cx: &mut Context<Self>) {
        let result = docker_client_config_path()
            .ok_or_else(|| "无法确定 Docker 客户端配置目录".to_string())
            .and_then(|path| use_rusker_context(&path, MANAGED_DOCKER_HOST));
        match result {
            Ok(()) => {
                self.notify_success(format!("Docker CLI 已切换到“{RUSKER_CONTEXT}”上下文"), cx)
            }
            Err(error) => self.notify_error(format!("切换 Docker CLI 上下文失败：{error}"), cx),
        }
        self.load_docker_context();
        cx.notify();
    }

    fn restore_docker_context(&mut self, cx: &mut Context<Self>) {
        let result = docker_client_config_path()
            .ok_or_else(|| "无法确定 Docker 客户端配置目录".to_string())
            .and_then(|path| restore_previous_context(&path));
        match result {
            Ok(previous) => self.notify_success(format!("Docker CLI 已切回“{previous}”上下文"), cx),
            Err(error) => self.notify_error(format!("切回 Docker CLI 上下文失败：{error}"), cx),
        }
        self.load_docker_context();
        cx.notify();
    }

    /// Persists "don't ask again" without touching other unsaved edits.
    fn skip_docker_context_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            return;
        };
        let mut saved = self.settings.clone();
        saved.skip_docker_context_prompt = true;
        match store.save(&saved) {
            Ok(()) => {
                self.settings = saved;
                self.settings_draft.skip_docker_context_prompt = true;
            }
            Err(error) => self.notify_error(error.to_string(), cx),
        }
        cx.notify();
    }

    /// "Docker CLI" row of the Docker Engine settings section.
    pub(super) fn render_docker_context_setting(&self, cx: &mut Context<Self>) -> Div {
        let (status, can_switch, previous) = match &self.docker_context {
            None => ("读取中…".to_string(), false, None),
            Some(Ok(CliContext::Rusker { previous })) => (
                format!("docker 命令已使用“{RUSKER_CONTEXT}”上下文连接 Rusker。"),
                false,
                Some(previous.clone()),
            ),
            Some(Ok(CliContext::Other(current))) => (
                format!("docker 命令当前使用“{current}”上下文，不会连接 Rusker。"),
                true,
                None,
            ),
            Some(Ok(CliContext::Overridden(variable))) => (
                format!("环境变量 {variable} 决定 docker 命令连接的 Engine，切换上下文不会生效。"),
                false,
                None,
            ),
            Some(Err(error)) => (error.clone(), false, None),
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(settings_label("Docker CLI 上下文", cx))
                    .child(muted(status, cx)),
            )
            .when(can_switch, |this| {
                this.child(
                    button("switch-docker-context")
                        .outline()
                        .small()
                        .label(format!("切换到 {RUSKER_CONTEXT}"))
                        .disabled(!self.manages_engine())
                        .pointer(self.manages_engine())
                        .on_click(cx.listener(|this, _, _, cx| this.switch_docker_context(cx))),
                )
            })
            .when_some(previous, |this, previous| {
                this.child(
                    button("restore-docker-context")
                        .outline()
                        .small()
                        .label(format!("切回 {previous}"))
                        .on_click(cx.listener(|this, _, _, cx| this.restore_docker_context(cx))),
                )
            })
    }
}
