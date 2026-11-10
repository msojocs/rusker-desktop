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
                .ok_or_else(|| t!("docker_context_ui.error.config_dir").into_owned())
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
        let title = t!("docker_context_ui.prompt.title").into_owned();
        let description = t!(
            "docker_context_ui.prompt.description",
            current = current,
            context = RUSKER_CONTEXT,
            host = MANAGED_DOCKER_HOST
        )
        .into_owned();
        let weak = cx.weak_entity();
        let window = self.window_handle;
        cx.defer(move |cx| {
            _ = window.update(cx, |_, window, cx| {
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let skip = weak.clone();
                    let switch = weak.clone();
                    dialog
                        .title(title.clone())
                        .description(description.clone())
                        .footer(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    button("docker-context-skip")
                                        .ghost()
                                        .label(t!("docker_context_ui.prompt.dont_ask"))
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
                                        .label(t!("docker_context_ui.prompt.later"))
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    button("docker-context-switch")
                                        .primary()
                                        .label(t!("docker_context_ui.prompt.switch"))
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
            .ok_or_else(|| t!("docker_context_ui.error.config_dir").into_owned())
            .and_then(|path| use_rusker_context(&path, MANAGED_DOCKER_HOST));
        match result {
            Ok(()) => self.notify_success(
                t!(
                    "docker_context_ui.notice.switched",
                    context = RUSKER_CONTEXT
                ),
                cx,
            ),
            Err(error) => self.notify_error(
                t!("docker_context_ui.error.switch_failed", error = error),
                cx,
            ),
        }
        self.load_docker_context();
        cx.notify();
    }

    fn restore_docker_context(&mut self, cx: &mut Context<Self>) {
        let result = docker_client_config_path()
            .ok_or_else(|| t!("docker_context_ui.error.config_dir").into_owned())
            .and_then(|path| restore_previous_context(&path));
        match result {
            Ok(previous) => self.notify_success(
                t!("docker_context_ui.notice.restored", context = previous),
                cx,
            ),
            Err(error) => self.notify_error(
                t!("docker_context_ui.error.restore_failed", error = error),
                cx,
            ),
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
            None => (
                t!("docker_context_ui.status.loading").into_owned(),
                false,
                None,
            ),
            Some(Ok(CliContext::Rusker { previous })) => (
                t!(
                    "docker_context_ui.status.using_rusker",
                    context = RUSKER_CONTEXT
                )
                .into_owned(),
                false,
                Some(previous.clone()),
            ),
            Some(Ok(CliContext::Other(current))) => (
                t!("docker_context_ui.status.using_other", current = current).into_owned(),
                true,
                None,
            ),
            Some(Ok(CliContext::Overridden(variable))) => (
                t!("docker_context_ui.status.overridden", variable = variable).into_owned(),
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
                    .child(settings_label(t!("docker_context_ui.setting.label"), cx))
                    .child(muted(status, cx)),
            )
            .when(can_switch, |this| {
                this.child(
                    button("switch-docker-context")
                        .outline()
                        .small()
                        .label(t!(
                            "docker_context_ui.action.switch_to",
                            context = RUSKER_CONTEXT
                        ))
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
                        .label(t!("docker_context_ui.action.restore", context = previous))
                        .on_click(cx.listener(|this, _, _, cx| this.restore_docker_context(cx))),
                )
            })
    }
}
