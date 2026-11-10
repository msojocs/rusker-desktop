//! "Run a new container" dialog (Docker Desktop-style): image header,
//! collapsible optional settings with name, ports, bind mounts, environment
//! variables, command and GPU.

use gpui_kit::component::{
    button::ButtonVariants as _,
    input::{Input, InputState},
    switch::Switch,
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div,
    prelude::FluentBuilder as _,
    px, AppContext as _, Context, Div, Entity, InteractiveElement as _, ParentElement as _,
    PathPromptOptions, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    WeakEntity, Window,
};
use rusker_desktop::container::{merge_env, ContainerDraft};

use super::{form_field, Page, RuskerApp};
use crate::ui::button;

type InputPair = (Entity<InputState>, Entity<InputState>);

#[derive(Default)]
pub(super) struct RunForm {
    /// Image chosen on the Images page; shown as text instead of an input.
    image: Option<String>,
    options_open: bool,
    mounts: Vec<InputPair>,
    env: Vec<InputPair>,
    /// The container is being created and started; the dialog shows a
    /// spinner and stays open until it succeeds or fails.
    pub(super) submitting: bool,
    task: Option<Task<()>>,
}

#[derive(Clone, Copy)]
enum RowList {
    Mounts,
    Env,
}

impl RowList {
    fn rows(self, form: &mut RunForm) -> &mut Vec<InputPair> {
        match self {
            Self::Mounts => &mut form.mounts,
            Self::Env => &mut form.env,
        }
    }

    fn placeholders(self) -> (SharedString, SharedString) {
        let (left, right) = match self {
            Self::Mounts => (
                t!("run.placeholder.mount_source"),
                t!("run.placeholder.mount_target"),
            ),
            Self::Env => (
                t!("run.placeholder.env_name"),
                t!("run.placeholder.env_value"),
            ),
        };
        (left.into_owned().into(), right.into_owned().into())
    }

    fn id(self) -> &'static str {
        match self {
            Self::Mounts => "mount",
            Self::Env => "env",
        }
    }
}

fn values(rows: &[InputPair], cx: &gpui_kit::App) -> Vec<(String, String)> {
    rows.iter()
        .map(|(left, right)| {
            (
                left.read(cx).value().to_string(),
                right.read(cx).value().to_string(),
            )
        })
        .collect()
}

impl RuskerApp {
    fn new_row(list: RowList, window: &mut Window, cx: &mut Context<Self>) -> InputPair {
        let (left, right) = list.placeholders();
        (
            cx.new(|cx| InputState::new(window, cx).placeholder(left)),
            cx.new(|cx| InputState::new(window, cx).placeholder(right)),
        )
    }

    /// Re-applies row placeholders after a language switch.
    pub(super) fn apply_run_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for list in [RowList::Mounts, RowList::Env] {
            let (left, right) = list.placeholders();
            for (left_input, right_input) in list.rows(&mut self.run).clone() {
                for (input, placeholder) in
                    [(left_input, left.clone()), (right_input, right.clone())]
                {
                    input.update(cx, |input, cx| {
                        input.set_placeholder(placeholder, window, cx)
                    });
                }
            }
        }
    }

    fn add_run_row(&mut self, list: RowList, window: &mut Window, cx: &mut Context<Self>) {
        let row = Self::new_row(list, window, cx);
        list.rows(&mut self.run).push(row);
        cx.notify();
    }

    fn remove_run_row(&mut self, list: RowList, index: usize, cx: &mut Context<Self>) {
        let rows = list.rows(&mut self.run);
        if index < rows.len() && rows.len() > 1 {
            rows.remove(index);
            cx.notify();
        }
    }

    fn browse_mount_source(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((source, _)) = self.run.mounts.get(index).cloned() else {
            return;
        };
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("run.browse_prompt").into_owned().into()),
        });
        let window = self.window_handle;
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let value = path.display().to_string();
            _ = cx.update_window(window, |_, window, cx| {
                source.update(cx, |state, cx| state.set_value(value, window, cx));
            });
        })
        .detach();
    }

    /// `image` comes from the Images page "run" action and is fixed; without
    /// it (Containers page) the image reference is entered in the form.
    pub(super) fn open_create_container_dialog(
        &mut self,
        image: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.create_error = None;
        self.create_nvidia_gpu = false;
        for input in [
            &self.create_image_input,
            &self.create_name_input,
            &self.create_command_input,
            &self.create_ports_input,
        ] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.run = RunForm {
            image,
            options_open: true,
            mounts: vec![Self::new_row(RowList::Mounts, window, cx)],
            env: vec![Self::new_row(RowList::Env, window, cx)],
            submitting: false,
            task: None,
        };

        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = weak.upgrade() else {
                return dialog;
            };
            let submitting = this.read(cx).run.submitting;
            let body = this.read(cx).render_run_form(weak.clone(), cx);
            let submit = weak.clone();
            let confirm = weak.clone();
            dialog
                .w(px(600.))
                .on_ok(move |_, window, cx| {
                    _ = confirm.update(cx, |this, cx| {
                        this.submit_create_container(window, cx);
                    });
                    // Keep the form open through validation and async submission.
                    false
                })
                .close_button(!submitting)
                .overlay_closable(!submitting)
                .child(body)
                .footer(
                    div()
                        .w_full()
                        .pt_4()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            button("cancel-create")
                                .outline()
                                .label(t!("run.cancel").into_owned())
                                .disabled(submitting)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            button("submit-create")
                                .primary()
                                .icon(IconName::Play)
                                .label(if submitting {
                                    t!("run.submitting").into_owned()
                                } else {
                                    t!("run.submit").into_owned()
                                })
                                .disabled(submitting)
                                .on_click(move |_, window, cx| {
                                    _ = submit.update(cx, |this, cx| {
                                        this.submit_create_container(window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn render_run_form(
        &self,
        weak: WeakEntity<Self>,
        cx: &gpui_kit::App,
    ) -> gpui_kit::Stateful<Div> {
        let open = self.run.options_open;
        let toggle_weak = weak.clone();
        let header = div()
            .flex()
            .items_center()
            .gap_4()
            .pb_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .size(px(56.))
                    .flex_shrink_0()
                    .rounded_lg()
                    .bg(cx.theme().primary.opacity(0.12))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(AssetIcon::Package)
                            .size(px(32.))
                            .text_color(cx.theme().primary),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t!("run.title").into_owned()),
                    )
                    .when_some(self.run.image.clone(), |this, image| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(image),
                        )
                    }),
            );

        let mut form = div().flex().flex_col().gap_4().child(header);
        if self.run.submitting {
            let image = match &self.run.image {
                Some(image) => image.clone(),
                None => self.create_image_input.read(cx).value().trim().to_string(),
            };
            return div().id("run-form").child(
                form.child(
                    div()
                        .py(px(48.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .child(self.loading_ring(px(40.), Some(cx.theme().primary)))
                        .child(
                            div()
                                .text_base()
                                .font_medium()
                                .child(t!("run.creating").into_owned()),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(image),
                        ),
                ),
            );
        }
        if self.run.image.is_none() {
            form = form.child(form_field(
                t!("run.field.image"),
                &self.create_image_input,
                t!("run.hint.image"),
                cx,
            ));
        }
        form = form.child(
            div()
                .id("toggle-run-options")
                .flex()
                .items_center()
                .justify_between()
                .cursor_pointer()
                .child(
                    div()
                        .text_base()
                        .font_medium()
                        .child(t!("run.optional_settings").into_owned()),
                )
                .child(Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                }))
                .on_click(move |_, _, cx| {
                    _ = toggle_weak.update(cx, |this, cx| {
                        this.run.options_open = !this.run.options_open;
                        cx.notify();
                    });
                }),
        );

        if open {
            form = form
                .child(form_field(
                    t!("run.field.name"),
                    &self.create_name_input,
                    t!("run.hint.name"),
                    cx,
                ))
                .child(form_field(
                    t!("run.field.ports"),
                    &self.create_ports_input,
                    t!("run.hint.ports"),
                    cx,
                ))
                .child(self.render_row_section(
                    t!("run.field.volumes").into_owned(),
                    t!("run.hint.volumes").into_owned(),
                    RowList::Mounts,
                    weak.clone(),
                    cx,
                ))
                .child(self.render_row_section(
                    t!("run.field.env").into_owned(),
                    t!("run.hint.env").into_owned(),
                    RowList::Env,
                    weak.clone(),
                    cx,
                ))
                .child(form_field(
                    t!("run.field.command"),
                    &self.create_command_input,
                    t!("run.hint.command"),
                    cx,
                ))
                .child(self.render_gpu_switch(weak, cx));
        }

        if let Some(error) = self.create_error.clone() {
            form = form.child(
                div()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().danger)
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error),
            );
        }
        div()
            .id("run-form")
            .max_h(px(560.))
            .overflow_y_scroll()
            .child(form)
    }

    fn render_row_section(
        &self,
        title: String,
        hint: String,
        list: RowList,
        weak: WeakEntity<Self>,
        cx: &gpui_kit::App,
    ) -> Div {
        let rows = match list {
            RowList::Mounts => &self.run.mounts,
            RowList::Env => &self.run.env,
        };
        let count = rows.len();
        let id = list.id();
        let rows = rows.iter().enumerate().map(|(index, (left, right))| {
            let mut left_input = Input::new(left);
            if matches!(list, RowList::Mounts) {
                let browse_weak = weak.clone();
                left_input = left_input.suffix(
                    button(SharedString::from(format!("{id}-browse-{index}")))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Ellipsis).text_color(cx.theme().primary))
                        .tooltip(t!("run.browse_folder").into_owned())
                        .on_click(move |_, _, cx| {
                            _ = browse_weak.update(cx, |this, cx| {
                                this.browse_mount_source(index, cx);
                            });
                        }),
                );
            }
            let add_weak = weak.clone();
            let remove_weak = weak.clone();
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().min_w_0().child(left_input))
                .child(div().flex_1().min_w_0().child(Input::new(right)))
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .flex()
                        .gap_1()
                        .child(
                            button(SharedString::from(format!("{id}-remove-{index}")))
                                .ghost()
                                .small()
                                .icon(IconName::Minus)
                                .tooltip(t!("run.remove_row").into_owned())
                                .when(count <= 1, |button| button.invisible())
                                .on_click(move |_, _, cx| {
                                    _ = remove_weak.update(cx, |this, cx| {
                                        this.remove_run_row(list, index, cx);
                                    });
                                }),
                        )
                        .when(index + 1 == count, |this| {
                            this.child(
                                button(SharedString::from(format!("{id}-add")))
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(IconName::Plus).text_color(cx.theme().primary))
                                    .tooltip(t!("run.add_row").into_owned())
                                    .on_click(move |_, window, cx| {
                                        _ = add_weak.update(cx, |this, cx| {
                                            this.add_run_row(list, window, cx);
                                        });
                                    }),
                            )
                        }),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().font_medium().child(title))
            .children(rows)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
    }

    fn render_gpu_switch(&self, weak: WeakEntity<Self>, cx: &gpui_kit::App) -> Div {
        let available = self.nvidia_gpu_available();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                Switch::new("create-nvidia-gpu")
                    .label(t!("run.gpu.label").into_owned())
                    .checked(self.create_nvidia_gpu)
                    .disabled(!available)
                    .on_change(move |enabled, _, cx| {
                        _ = weak.update(cx, |this, cx| {
                            this.create_nvidia_gpu = *enabled;
                            cx.notify();
                        });
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if available {
                        t!("run.gpu.hint").into_owned()
                    } else {
                        t!("run.gpu.unavailable").into_owned()
                    }),
            )
    }

    fn submit_create_container(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let image = match &self.run.image {
            Some(image) => image.clone(),
            None => self.create_image_input.read(cx).value().to_string(),
        };
        let draft = ContainerDraft {
            image,
            name: self.create_name_input.read(cx).value().to_string(),
            command: self.create_command_input.read(cx).value().to_string(),
            ports: self.create_ports_input.read(cx).value().to_string(),
            nvidia_gpu: self.create_nvidia_gpu,
            mounts: values(&self.run.mounts, cx),
            env: values(&self.run.env, cx),
        };
        let mut request = match draft.parse() {
            Ok(request) => request,
            Err(error) => {
                self.create_error = Some(error);
                self.run.options_open = true;
                cx.notify();
                return;
            }
        };
        merge_env(
            &mut request.env,
            self.settings.container_proxy_resolved().container_env(),
        );
        let Some(engine) = self.engine.clone() else {
            self.create_error = Some(t!("run.error.engine_unavailable").into_owned());
            cx.notify();
            return;
        };

        self.create_error = None;
        self.run.submitting = true;
        // Block other operations and drop in-flight refreshes, which could
        // otherwise overwrite the snapshot that contains the new container.
        self.refresh_revision.advance();
        self.busy = Some(t!("run.creating").into_owned());
        cx.notify();
        let handle = engine.create_container(request);
        let window = self.window_handle;
        self.run.task = Some(cx.spawn(async move |this, cx| {
            let result = super::await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.run.submitting = false;
                match result {
                    Ok((id, snapshot)) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot);
                        this.after_volume_snapshot(cx);
                        let search = this.search_input.clone();
                        _ = cx.update_window(window, |_, window, cx| {
                            window.close_dialog(cx);
                            // Make sure the new container is visible in the list.
                            search.update(cx, |state, cx| state.set_value("", window, cx));
                        });
                        this.navigate(Page::Containers, cx);
                        this.select_container(id, cx);
                        this.notify_success(t!("run.notice.created"), cx);
                    }
                    // Keep the dialog and its input so the user can fix it.
                    Err(error) => this.create_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }
}
