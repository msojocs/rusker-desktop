//! Compose project detail: service list, recent logs and project-wide actions.

use futures_util::future::join_all;

use super::*;

impl RuskerApp {
    pub(super) fn select_compose_project(&mut self, project: String, cx: &mut Context<Self>) {
        self.reset_container_tools();
        self.selected_container = None;
        self.compose_config_view = None;
        self.selected_compose_project = Some(project.clone());
        self.load_compose_logs(&project, cx);
    }

    pub(super) fn close_compose_project(&mut self, cx: &mut Context<Self>) {
        self.selected_compose_project = None;
        self.compose_config_view = None;
        self.logs_revision.advance();
        self.logs_loading = false;
        self.logs.clear();
        self.logs_error = None;
        cx.notify();
    }

    fn load_compose_logs(&mut self, project: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let services: Vec<(String, String)> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container.compose_project.as_deref() == Some(project))
            .map(|container| (container.id.clone(), compose_service_name(container)))
            .collect();
        let revision = self.logs_revision.advance();
        self.logs_loading = true;
        self.logs_error = None;
        self.logs.clear();
        cx.notify();
        self._logs_task = Some(cx.spawn(async move |this, cx| {
            let handles = services
                .iter()
                .map(|(id, _)| engine.container_logs(id.clone()));
            let results = join_all(handles).await;
            let mut combined = String::new();
            let mut errors = Vec::new();
            for ((_, service), result) in services.into_iter().zip(results) {
                match result {
                    Ok(Ok(logs)) => combined.push_str(&prefix_service_logs(&service, &logs)),
                    Ok(Err(error)) => errors.push(format!("{service}: {error}")),
                    Err(error) => errors.push(format!("{service}: {}", error)),
                }
            }
            _ = this.update(cx, |this, cx| {
                if !this.logs_revision.is_current(revision) {
                    return;
                }
                this.logs_loading = false;
                if combined.is_empty() && !errors.is_empty() {
                    this.logs_error = Some(errors.join("\n"));
                } else {
                    this.logs = rusker_desktop::format::bound_logs(&combined);
                    this.container_logs_scroll.scroll_to_bottom();
                    if !errors.is_empty() {
                        this.notify_error(errors.join("\n"), cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn start_compose_project_action(
        &mut self,
        project: String,
        action: ContainerAction,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.notify_error(t!("app.error.engine_unavailable"), cx);
            cx.notify();
            return;
        };
        let ids: Vec<String> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container.compose_project.as_deref() == Some(project.as_str()))
            .filter(|container| match action {
                ContainerAction::Start => !container.is_running(),
                ContainerAction::Stop => container.is_running(),
                ContainerAction::Restart | ContainerAction::Remove => true,
            })
            .map(|container| container.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let handle = engine.container_actions(ids, action);
        let revision = self.refresh_revision.advance();
        self.busy = Some(t!("app.operation_progress", operation = action.label()).into());
        self.loading = false;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.busy = None;
                match result {
                    Ok((snapshot, failures)) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot);
                        this.after_volume_snapshot(cx);
                        this.sync_container_tools(cx);
                        if failures.is_empty() {
                            this.notify_success(
                                t!("app.operation_done", operation = action.label()),
                                cx,
                            );
                        } else {
                            this.notify_error(failures.join("\n"), cx);
                        }
                    }
                    Err(error) => this.notify_error(error.to_string(), cx),
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_compose_project_detail(
        &self,
        project: &str,
        containers: &[ContainerSummary],
        cx: &mut Context<Self>,
    ) -> Div {
        if let Some(view) = &self.compose_config_view {
            return self.render_compose_config_detail(project, view, cx);
        }
        let busy = self.busy.is_some();
        let running = containers
            .iter()
            .filter(|container| container.is_running())
            .count();
        let working_dir = containers.iter().find_map(|container| {
            container
                .compose_working_dir
                .clone()
                .filter(|path| !path.is_empty())
        });
        let config_project = project.to_string();
        let start_project = project.to_string();
        let stop_project = project.to_string();
        let refresh_project = project.to_string();
        let remove_containers = containers.to_vec();
        let logs_for_copy = self.logs.clone();
        let weak = cx.weak_entity();

        let service_rows: Vec<AnyElement> = containers
            .iter()
            .map(|container| {
                self.render_compose_service_row(container, busy, cx)
                    .into_any_element()
            })
            .collect();

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().px_6().pt_4().child(
                    Breadcrumb::new()
                        .child(
                            BreadcrumbItem::new(t!("app.page.containers").into_owned())
                                .text_color(cx.theme().primary)
                                .on_click(move |_, _, cx| {
                                    _ = weak.update(cx, |this, cx| this.close_compose_project(cx));
                                }),
                        )
                        .child(BreadcrumbItem::new(project.to_string())),
                ),
            )
            .child(
                div()
                    .px_6()
                    .py_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        button("compose-project-back")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .tooltip(t!("app.back_to_containers"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_compose_project(cx))),
                    )
                    .child(
                        Icon::new(AssetIcon::Layers2)
                            .size(px(40.))
                            .text_color(if running > 0 {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            }),
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
                                    .text_2xl()
                                    .font_semibold()
                                    .truncate()
                                    .child(project.to_string()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        t!(
                                            "app.containers.summary",
                                            running = running,
                                            total = containers.len()
                                        )
                                        .into_owned(),
                                    ),
                            )
                            .when_some(working_dir, |this, path| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(path),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                button("view-compose-config")
                                    .outline()
                                    .text_color(cx.theme().primary)
                                    .border_color(cx.theme().primary.opacity(0.5))
                                    .label(t!("app.compose.view_config"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_compose_config(&config_project, cx);
                                    })),
                            )
                            .child(
                                button("start-compose-project")
                                    .primary()
                                    .icon(IconName::Play)
                                    .tooltip(t!("app.start"))
                                    .disabled(
                                        busy || containers.is_empty()
                                            || running == containers.len(),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_compose_project_action(
                                            start_project.clone(),
                                            ContainerAction::Start,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                button("stop-compose-project")
                                    .secondary()
                                    .icon(IconName::Square)
                                    .tooltip(t!("app.stop"))
                                    .disabled(busy || running == 0)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_compose_project_action(
                                            stop_project.clone(),
                                            ContainerAction::Stop,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                button("remove-compose-project")
                                    .danger()
                                    .icon(AssetIcon::Trash)
                                    .tooltip(t!("app.delete"))
                                    .disabled(busy || containers.is_empty())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_remove_containers(
                                            remove_containers.clone(),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_6()
                    .pb_6()
                    .flex()
                    .gap_4()
                    .child(
                        section_panel(cx)
                            .bg(gpui_kit::transparent_black())
                            .w(px(400.))
                            .flex_shrink_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_4()
                                    .py_3()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .font_semibold()
                                    .child(t!("app.compose.services")),
                            )
                            .child(
                                div()
                                    .id("compose-service-list")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .children(service_rows),
                            ),
                    )
                    .child(
                        section_panel(cx)
                            .bg(gpui_kit::transparent_black())
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .flex()
                                    .items_center()
                                    .child(div().flex_1().font_semibold().child(t!("app.tab.logs")))
                                    .child(
                                        button("copy-compose-logs")
                                            .ghost()
                                            .small()
                                            .icon(IconName::Copy)
                                            .tooltip(t!("app.copy_logs"))
                                            .disabled(self.logs.is_empty())
                                            .on_click(move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    logs_for_copy.clone(),
                                                ));
                                            }),
                                    )
                                    .child(
                                        button("refresh-compose-logs")
                                            .ghost()
                                            .small()
                                            .icon(IconName::RotateCw)
                                            .tooltip(t!("app.refresh_logs"))
                                            .disabled(self.logs_loading)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.load_compose_logs(&refresh_project, cx);
                                            })),
                                    ),
                            )
                            .child(self.render_compose_logs(cx)),
                    ),
            )
    }

    fn render_compose_service_row(
        &self,
        container: &ContainerSummary,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let running = container.is_running();
        let detail_id = container.id.clone();
        let toggle_id = container.id.clone();
        let state_color = if running {
            cx.theme().success
        } else if container.state.eq_ignore_ascii_case("paused") {
            cx.theme().warning
        } else {
            cx.theme().muted_foreground
        };
        div()
            .id((ElementId::from("compose-service-row"), container.id.clone()))
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_container(detail_id.clone(), cx);
            }))
            .child(
                div()
                    .size(px(8.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(state_color),
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
                            .truncate()
                            .font_semibold()
                            .child(compose_service_name(container)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(cx.theme().primary)
                            .child(container.image.clone()),
                    ),
            )
            .child(
                list_action_button(
                    (
                        ElementId::from("toggle-compose-service"),
                        container.id.clone(),
                    ),
                    cx.theme().primary,
                    cx,
                )
                .small()
                .icon(if running {
                    IconName::Square
                } else {
                    IconName::Play
                })
                .tooltip(if running {
                    t!("app.stop")
                } else {
                    t!("app.start")
                })
                .disabled(busy)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.start_container_action(
                        toggle_id.clone(),
                        if running {
                            ContainerAction::Stop
                        } else {
                            ContainerAction::Start
                        },
                        cx,
                    );
                })),
            )
    }

    fn render_compose_logs(&self, cx: &mut Context<Self>) -> Div {
        let is_placeholder =
            self.logs_loading || self.logs_error.is_some() || self.logs.trim().is_empty();
        let text = if self.logs_loading {
            t!("app.logs.loading").to_string()
        } else if let Some(error) = &self.logs_error {
            error.clone()
        } else if self.logs.trim().is_empty() {
            t!("app.compose.logs.empty").to_string()
        } else {
            self.logs.clone()
        };
        let color = if self.logs_error.is_some() {
            cx.theme().danger
        } else if is_placeholder {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                div()
                    .id("compose-log-scroll")
                    .size_full()
                    .overflow_scroll()
                    .track_scroll(&self.container_logs_scroll)
                    .child(
                        div()
                            .p_4()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(color)
                            .when(!is_placeholder, |this| this.whitespace_nowrap())
                            .child(text),
                    ),
            )
            .when(!is_placeholder, |this| {
                this.child(
                    div().absolute().bottom_4().right_6().child(
                        button("compose-logs-bottom")
                            .outline()
                            .icon(IconName::ArrowDown)
                            .tooltip(t!("app.scroll_to_bottom"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.container_logs_scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                )
            })
    }
}

fn compose_service_name(container: &ContainerSummary) -> String {
    container
        .compose_service
        .clone()
        .filter(|service| !service.is_empty())
        .unwrap_or_else(|| container_list_name(container, true))
}

fn prefix_service_logs(service: &str, logs: &str) -> String {
    let mut output = String::new();
    for line in logs.lines() {
        output.push_str(service);
        output.push_str(" | ");
        output.push_str(line);
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::prefix_service_logs;

    #[test]
    fn service_logs_prefix_every_line_without_adding_an_empty_record() {
        assert_eq!(
            prefix_service_logs("web", "one\ntwo\n"),
            "web | one\nweb | two\n"
        );
        assert_eq!(prefix_service_logs("web", ""), "");
    }
}
