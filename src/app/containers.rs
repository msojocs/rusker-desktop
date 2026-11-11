use std::rc::Rc;

use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::{AnyElement, WeakEntity};

use super::*;

#[derive(Clone, Copy)]
pub(super) enum ContainerEmpty {
    All,
    Running,
    Filtered,
}

pub(super) struct ContainerTables {
    main: Entity<TableState<ContainerTable>>,
    actions: Entity<TableState<ContainerActionsTable>>,
}

impl ContainerTables {
    pub(super) fn new(
        weak: WeakEntity<RuskerApp>,
        window: &mut Window,
        cx: &mut Context<RuskerApp>,
    ) -> Self {
        let main = cx.new(|cx| {
            TableState::new(ContainerTable::new(weak.clone()), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
        });
        let vertical_scroll_handle = main.read(cx).vertical_scroll_handle.clone();
        let actions = cx.new(|cx| {
            let mut state = TableState::new(ContainerActionsTable::new(weak), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false);
            state.vertical_scroll_handle = vertical_scroll_handle;
            state
        });
        Self { main, actions }
    }

    pub(super) fn render(
        &self,
        rows: Vec<ContainerListItem>,
        empty: ContainerEmpty,
        collapsed: &HashSet<String>,
        cx: &mut Context<RuskerApp>,
    ) -> Div {
        let rows = Rc::new(expand_container_rows(rows, collapsed));
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.main.update(cx, |table, _| {
            let delegate = table.delegate_mut();
            delegate.rows = rows.clone();
            delegate.now = now;
            delegate.empty = empty;
        });
        self.actions.update(cx, |table, _| {
            table.delegate_mut().rows = rows;
        });

        let horizontal_scroll = self.main.read(cx).horizontal_scroll_handle.clone();
        let horizontal_content_width = {
            let table = self.main.read(cx);
            // Include DataTable's default 12px trailing cell.
            (0..ContainerTable::COLUMNS_COUNT)
                .filter_map(|col_ix| table.col_width(col_ix))
                .sum::<Pixels>()
                + px(12.)
        };
        div()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .flex_1()
            .min_h_0()
            .flex()
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div().flex_1().min_h_0().child(
                            DataTable::new(&self.main)
                                .bordered(false)
                                .with_size(Size::Large)
                                .header_row_height(Size::Large.table_row_height())
                                .scrollbar_visible(true, false)
                                .accessibility_label(t!("app.page.containers")),
                        ),
                    )
                    .child(table_horizontal_scrollbar(
                        &horizontal_scroll,
                        Some(horizontal_content_width),
                    )),
            )
            .child(
                div()
                    .w(px(CONTAINER_ACTIONS_COLUMN))
                    .flex_shrink_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .child(
                        div().flex_1().min_h_0().child(
                            DataTable::new(&self.actions)
                                .bordered(false)
                                .with_size(Size::Large)
                                .header_row_height(Size::Large.table_row_height())
                                .scrollbar_visible(false, false)
                                .accessibility_label(t!("app.actions")),
                        ),
                    )
                    .child(div().h(Scrollbar::width()).flex_shrink_0()),
            )
    }
}

fn expand_container_rows(
    rows: Vec<ContainerListItem>,
    collapsed: &HashSet<String>,
) -> Vec<ContainerListItem> {
    rows.into_iter()
        .flat_map(|row| match row {
            ContainerListItem::ComposeProject {
                project,
                containers,
            } => {
                let is_collapsed = collapsed.contains(&project);
                let mut items = vec![ContainerListItem::ComposeProject {
                    project,
                    containers: containers.clone(),
                }];
                if !is_collapsed {
                    items.extend(
                        containers
                            .into_iter()
                            .map(|container| ContainerListItem::Container(Box::new(container))),
                    );
                }
                items
            }
            row => vec![row],
        })
        .collect()
}

struct ContainerTable {
    weak: WeakEntity<RuskerApp>,
    rows: Rc<Vec<ContainerListItem>>,
    now: i64,
    empty: ContainerEmpty,
}

impl ContainerTable {
    const COLUMNS_COUNT: usize = 10;

    fn new(weak: WeakEntity<RuskerApp>) -> Self {
        Self {
            weak,
            rows: Rc::new(Vec::new()),
            now: 0,
            empty: ContainerEmpty::All,
        }
    }

    fn row(&self, row_ix: usize) -> &ContainerListItem {
        &self.rows[row_ix]
    }

    fn container_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        container: &ContainerSummary,
        nested: bool,
        cx: &mut Context<TableState<Self>>,
    ) -> AnyElement {
        let Some(app) = self.weak.upgrade() else {
            return div().into_any_element();
        };
        let app = app.read(cx);
        let running = container.is_running();
        let paused = container.state.eq_ignore_ascii_case("paused");
        match col_ix {
            0 => {
                let id = container.id.clone();
                let weak = self.weak.clone();
                centered_container_cell()
                    .child(
                        Checkbox::new(("check-container", row_ix))
                            .checked(app.containers_checked.contains(&container.id))
                            .on_click(move |checked: &bool, _, cx| {
                                _ = weak.update(cx, |this, cx| {
                                    if *checked {
                                        this.containers_checked.insert(id.clone());
                                    } else {
                                        this.containers_checked.remove(&id);
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                });
                            }),
                    )
                    .into_any_element()
            }
            1 => div().into_any_element(),
            2 => {
                let tooltip = if container.status.trim().is_empty() {
                    container.state.clone()
                } else {
                    container.status.clone()
                };
                centered_container_cell()
                    .id(("container-state", row_ix))
                    .child(state_dot(running, paused, cx))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .into_any_element()
            }
            3 => div()
                .min_w_0()
                .truncate()
                .when(nested, |this| this.pl_4())
                .child(container_list_name(container, nested).to_owned())
                .into_any_element(),
            4 => {
                let id = container.id.clone();
                div()
                    .id(("copy-container-id", row_ix))
                    .cursor_pointer()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .hover(|style| style.underline())
                    .tooltip(|window, cx| {
                        Tooltip::new(t!("app.copy_container_id").into_owned()).build(window, cx)
                    })
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                    })
                    .child(short_id(&container.id))
                    .into_any_element()
            }
            5 => {
                let image_container = container.clone();
                let weak = self.weak.clone();
                div()
                    .id(("open-container-image", row_ix))
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .text_color(cx.theme().primary)
                    .hover(|style| style.underline())
                    .tooltip(|window, cx| {
                        Tooltip::new(t!("app.view_image").into_owned()).build(window, cx)
                    })
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        _ = weak.update(cx, |this, cx| {
                            this.open_container_image(&image_container, cx);
                        });
                    })
                    .child(container.image.clone())
                    .into_any_element()
            }
            6 => div()
                .truncate()
                .child(format_port_bindings(&container.ports))
                .into_any_element(),
            7 => {
                let usage = if running {
                    app.container_stats
                        .latest(&container.id)
                        .and_then(|stats| stats.cpu_percent)
                        .map_or_else(
                            || "N/A".into(),
                            |value| format_cpu_usage(value, app.snapshot.engine.cpus),
                        )
                } else {
                    "—".into()
                };
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(usage)
                    .into_any_element()
            }
            8 => {
                let usage = if running {
                    app.container_stats.latest(&container.id).map_or_else(
                        || "N/A".into(),
                        |stats| format_memory_usage(stats.memory_bytes, stats.memory_limit),
                    )
                } else {
                    "—".into()
                };
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(usage)
                    .into_any_element()
            }
            9 => div()
                .child(format_relative(container.created, self.now))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn compose_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        project: &str,
        containers: &[ContainerSummary],
        cx: &mut Context<TableState<Self>>,
    ) -> AnyElement {
        let Some(app) = self.weak.upgrade() else {
            return div().into_any_element();
        };
        let app = app.read(cx);
        let collapsed = app.compose_projects_collapsed.contains(project);
        let running = containers
            .iter()
            .filter(|container| container.is_running())
            .count();
        let paused = containers
            .iter()
            .any(|container| container.state.eq_ignore_ascii_case("paused"));
        match col_ix {
            0 => {
                let visible_ids: Vec<String> = containers
                    .iter()
                    .map(|container| container.id.clone())
                    .collect();
                let all_checked = !containers.is_empty()
                    && containers
                        .iter()
                        .all(|container| app.containers_checked.contains(&container.id));
                let weak = self.weak.clone();
                centered_container_cell()
                    .child(
                        Checkbox::new((
                            ElementId::from("check-compose-project"),
                            project.to_string(),
                        ))
                        .checked(all_checked)
                        .on_click(move |checked: &bool, _, cx| {
                            _ = weak.update(cx, |this, cx| {
                                for id in &visible_ids {
                                    if *checked {
                                        this.containers_checked.insert(id.clone());
                                    } else {
                                        this.containers_checked.remove(id);
                                    }
                                }
                                cx.stop_propagation();
                                cx.notify();
                            });
                        }),
                    )
                    .into_any_element()
            }
            1 => {
                let tooltip = if collapsed {
                    t!("app.containers.expand_compose").into_owned()
                } else {
                    t!("app.containers.collapse_compose").into_owned()
                };
                centered_container_cell()
                    .id((
                        ElementId::from("compose-project-toggle"),
                        project.to_string(),
                    ))
                    .text_color(cx.theme().primary)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .small(),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .into_any_element()
            }
            2 => {
                let tooltip = t!(
                    "app.containers.summary",
                    running = running,
                    total = containers.len()
                )
                .into_owned();
                centered_container_cell()
                    .id((
                        ElementId::from("compose-project-state"),
                        project.to_string(),
                    ))
                    .child(state_dot(running > 0, paused, cx))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .into_any_element()
            }
            3 => {
                let open_project = project.to_string();
                let weak = self.weak.clone();
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id((ElementId::from("open-compose-project"), project.to_string()))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .font_semibold()
                            .text_color(cx.theme().primary)
                            .hover(|this| this.underline())
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                _ = weak.update(cx, |this, cx| {
                                    this.select_compose_project(open_project.clone(), cx);
                                });
                            })
                            .child(project.to_string()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                t!("app.containers.compose_services", count = containers.len())
                                    .into_owned(),
                            ),
                    )
                    .into_any_element()
            }
            4..=6 => div()
                .text_color(cx.theme().muted_foreground)
                .child("—")
                .into_any_element(),
            7 => {
                let latest_stats: Vec<_> = containers
                    .iter()
                    .filter(|container| container.is_running())
                    .filter_map(|container| app.container_stats.latest(&container.id))
                    .collect();
                let cpu_total = latest_stats.iter().try_fold(0.0, |total, stats| {
                    stats.cpu_percent.map(|value| total + value)
                });
                let usage = if running == 0 {
                    "—".into()
                } else {
                    cpu_total.map_or_else(
                        || "N/A".into(),
                        |total| format_cpu_usage(total, app.snapshot.engine.cpus),
                    )
                };
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(usage)
                    .into_any_element()
            }
            8 => {
                let latest_stats: Vec<_> = containers
                    .iter()
                    .filter(|container| container.is_running())
                    .filter_map(|container| app.container_stats.latest(&container.id))
                    .collect();
                let usage = if running == 0 {
                    "—".into()
                } else if latest_stats.is_empty() {
                    "N/A".into()
                } else {
                    let used = latest_stats.iter().fold(0u64, |total, stats| {
                        total.saturating_add(stats.memory_bytes)
                    });
                    format_memory_usage(used, app.snapshot.engine.memory_bytes)
                };
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(usage)
                    .into_any_element()
            }
            9 => {
                let created = containers
                    .iter()
                    .map(|container| container.created)
                    .max()
                    .unwrap_or_default();
                div()
                    .child(format_relative(created, self.now))
                    .into_any_element()
            }
            _ => {
                let _ = row_ix;
                div().into_any_element()
            }
        }
    }
}

impl TableDelegate for ContainerTable {
    fn columns_count(&self, _: &App) -> usize {
        Self::COLUMNS_COUNT
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let narrow = |key, name, width| {
            Column::new(key, name)
                .width(px(width))
                .resizable(false)
                .selectable(false)
                .p_0()
        };
        match col_ix {
            0 => narrow("check", "", CONTAINER_CHECK_COLUMN),
            1 => narrow("expand", "", CONTAINER_EXPAND_COLUMN),
            2 => narrow("status", "", CONTAINER_STATUS_COLUMN),
            3 => Column::new("name", t!("app.name"))
                .width(px(CONTAINER_NAME_COLUMN))
                .min_width(px(80.)),
            4 => Column::new("id", t!("app.container_id"))
                .width(px(CONTAINER_ID_COLUMN))
                .min_width(px(72.)),
            5 => Column::new("image", t!("app.image"))
                .width(px(CONTAINER_IMAGE_COLUMN))
                .min_width(px(80.)),
            6 => Column::new("ports", t!("app.ports"))
                .width(px(CONTAINER_PORTS_COLUMN))
                .min_width(px(64.)),
            7 => Column::new("cpu", t!("app.containers.cpu"))
                .width(px(CONTAINER_CPU_COLUMN))
                .min_width(px(96.)),
            8 => Column::new("memory", t!("app.containers.memory"))
                .width(px(CONTAINER_MEMORY_COLUMN))
                .min_width(px(96.)),
            9 => Column::new("created", t!("app.created"))
                .width(px(CONTAINER_CREATED_COLUMN))
                .min_width(px(72.)),
            _ => unreachable!(),
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if col_ix != 0 {
            return centered_container_cell()
                .child(self.column(col_ix, cx).name.clone())
                .into_any_element();
        }
        let visible_ids: Vec<String> = self
            .rows
            .iter()
            .flat_map(|row| match row {
                ContainerListItem::Container(container) => vec![container.id.clone()],
                ContainerListItem::ComposeProject { containers, .. } => containers
                    .iter()
                    .map(|container| container.id.clone())
                    .collect(),
            })
            .collect();
        let all_checked = !visible_ids.is_empty()
            && self.weak.upgrade().is_some_and(|app| {
                visible_ids
                    .iter()
                    .all(|id| app.read(cx).containers_checked.contains(id))
            });
        let weak = self.weak.clone();
        centered_container_cell()
            .child(
                Checkbox::new("check-all-containers")
                    .checked(all_checked)
                    .tooltip(t!("app.select_all"))
                    .disabled(visible_ids.is_empty())
                    .on_click(move |checked: &bool, _, cx| {
                        _ = weak.update(cx, |this, cx| {
                            for id in &visible_ids {
                                if *checked {
                                    this.containers_checked.insert(id.clone());
                                } else {
                                    this.containers_checked.remove(id);
                                }
                            }
                            cx.stop_propagation();
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let row = self.row(row_ix).clone();
        let weak = self.weak.clone();
        let checked = match &row {
            ContainerListItem::Container(container) => self
                .weak
                .upgrade()
                .is_some_and(|app| app.read(cx).containers_checked.contains(&container.id)),
            ContainerListItem::ComposeProject { .. } => false,
        };
        div()
            .id(("container-table-row", row_ix))
            .text_sm()
            .cursor_pointer()
            .when(checked, |this| this.bg(cx.theme().accent))
            .when(
                matches!(row, ContainerListItem::ComposeProject { .. }),
                |this| this.bg(cx.theme().secondary),
            )
            .on_click(move |_, _, cx| activate_container_row(&weak, &row, cx))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match self.row(row_ix).clone() {
            ContainerListItem::Container(container) => {
                let nested = self.rows.iter().any(|row| match row {
                    ContainerListItem::ComposeProject { containers, .. } => {
                        containers.iter().any(|nested| nested.id == container.id)
                    }
                    _ => false,
                });
                self.container_cell(row_ix, col_ix, &container, nested, cx)
            }
            ContainerListItem::ComposeProject {
                project,
                containers,
            } => self.compose_cell(row_ix, col_ix, &project, &containers, cx),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (title, hint) = match self.empty {
            ContainerEmpty::All => (
                t!("app.empty.no_containers"),
                t!("app.empty.no_containers_hint"),
            ),
            ContainerEmpty::Running => {
                (t!("app.empty.no_running"), t!("app.empty.no_running_hint"))
            }
            ContainerEmpty::Filtered => {
                (t!("app.empty.no_matches"), t!("app.empty.no_matches_hint"))
            }
        };
        empty_state(title, hint, cx).into_any_element()
    }
}

struct ContainerActionsTable {
    weak: WeakEntity<RuskerApp>,
    rows: Rc<Vec<ContainerListItem>>,
}

impl ContainerActionsTable {
    fn new(weak: WeakEntity<RuskerApp>) -> Self {
        Self {
            weak,
            rows: Rc::new(Vec::new()),
        }
    }
}

impl TableDelegate for ContainerActionsTable {
    fn columns_count(&self, _: &App) -> usize {
        1
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, _: usize, _: &App) -> Column {
        Column::new("actions", t!("app.actions"))
            .width(px(CONTAINER_ACTIONS_COLUMN))
            .resizable(false)
            .selectable(false)
            .p_0()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        centered_container_cell().child(self.column(col_ix, cx).name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let row = self.rows[row_ix].clone();
        let weak = self.weak.clone();
        let checked = match &row {
            ContainerListItem::Container(container) => self
                .weak
                .upgrade()
                .is_some_and(|app| app.read(cx).containers_checked.contains(&container.id)),
            ContainerListItem::ComposeProject { .. } => false,
        };
        div()
            .id(("container-action-row", row_ix))
            .text_sm()
            .cursor_pointer()
            .when(checked, |this| this.bg(cx.theme().accent))
            .when(
                matches!(row, ContainerListItem::ComposeProject { .. }),
                |this| this.bg(cx.theme().secondary),
            )
            .on_click(move |_, _, cx| activate_container_row(&weak, &row, cx))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        _: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let ContainerListItem::Container(container) = &self.rows[row_ix] else {
            return div().into_any_element();
        };
        let container = container.clone();
        let running = container.is_running();
        let busy = self
            .weak
            .upgrade()
            .is_some_and(|app| app.read(cx).busy.is_some());
        let toggle_id = container.id.clone();
        let toggle_weak = self.weak.clone();
        let menu_container = container.clone();
        let menu_weak = self.weak.clone();
        let remove_container = container.clone();
        let remove_weak = self.weak.clone();
        centered_container_cell()
            .px_2()
            .gap_1()
            .child(
                list_action_button(("toggle-container", row_ix), cx.theme().primary, cx)
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
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        _ = toggle_weak.update(cx, |this, cx| {
                            this.start_container_action(
                                toggle_id.clone(),
                                if running {
                                    ContainerAction::Stop
                                } else {
                                    ContainerAction::Start
                                },
                                cx,
                            );
                        });
                    }),
            )
            .child(
                list_action_button(("container-more", row_ix), cx.theme().primary, cx)
                    .small()
                    .icon(IconName::EllipsisVertical)
                    .disabled(busy)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        container_menu(menu, &menu_container, menu_weak.clone())
                    }),
            )
            .child(
                list_action_button(("remove-container-row", row_ix), cx.theme().danger, cx)
                    .small()
                    .icon(Icon::new(AssetIcon::Trash).text_color(cx.theme().danger))
                    .tooltip(t!("app.delete"))
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        _ = remove_weak.update(cx, |this, cx| {
                            this.confirm_remove_container(&remove_container, window, cx);
                        });
                    }),
            )
            .into_any_element()
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().size_full().into_any_element()
    }
}

fn activate_container_row(weak: &WeakEntity<RuskerApp>, row: &ContainerListItem, cx: &mut App) {
    match row {
        ContainerListItem::Container(container) => {
            let id = container.id.clone();
            _ = weak.update(cx, |this, cx| this.select_container(id, cx));
        }
        ContainerListItem::ComposeProject { project, .. } => {
            let project = project.clone();
            _ = weak.update(cx, |this, cx| {
                if !this.compose_projects_collapsed.remove(&project) {
                    this.compose_projects_collapsed.insert(project);
                }
                cx.notify();
            });
        }
    }
}

fn centered_container_cell() -> Div {
    // Percentage heights are not resolved on the table's first layout pass.
    // Use the same definite height as Size::Large so controls start centered.
    div()
        .h(Size::Large.table_row_height())
        .flex()
        .items_center()
}

fn state_dot(running: bool, paused: bool, cx: &App) -> Div {
    let color = if running {
        cx.theme().success
    } else if paused {
        cx.theme().warning
    } else {
        cx.theme().muted_foreground
    };
    if running || paused {
        div().size(px(8.)).rounded_full().bg(color)
    } else {
        div()
            .size(px(8.))
            .rounded_full()
            .border_1()
            .border_color(color)
    }
}

#[cfg(test)]
mod tests {
    use super::expand_container_rows;
    use crate::app::{group_container_list, ContainerListItem};
    use rusker_desktop::model::ContainerSummary;
    use std::collections::HashSet;

    fn container(id: &str) -> ContainerSummary {
        ContainerSummary {
            id: id.into(),
            name: id.into(),
            compose_project: Some("shop".into()),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn expanded_compose_rows_include_the_group_and_each_service() {
        let grouped = group_container_list(vec![container("web"), container("database")]);
        let expanded = expand_container_rows(grouped.clone(), &HashSet::new());
        assert_eq!(expanded.len(), 3);
        assert!(matches!(
            &expanded[0],
            ContainerListItem::ComposeProject { .. }
        ));
        assert!(matches!(
            &expanded[1],
            ContainerListItem::Container(container) if container.id == "web"
        ));
        assert!(matches!(
            &expanded[2],
            ContainerListItem::Container(container) if container.id == "database"
        ));

        let collapsed = expand_container_rows(grouped, &HashSet::from(["shop".into()]));
        assert_eq!(collapsed.len(), 1);
        assert!(matches!(
            &collapsed[0],
            ContainerListItem::ComposeProject { .. }
        ));
    }
}
