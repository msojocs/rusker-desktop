use gpui_kit::component::{
    table::{Column, DataTable, TableDelegate, TableState},
    Size,
};
use gpui_kit::{App, Entity, Stateful, WeakEntity};

use super::*;
use crate::app::table_horizontal_scrollbar;

pub(crate) struct VolumeTable {
    weak: WeakEntity<RuskerApp>,
    rows: Vec<VolumeSummary>,
    no_volumes: bool,
}

impl VolumeTable {
    pub(crate) fn new(weak: WeakEntity<RuskerApp>) -> Self {
        Self {
            weak,
            rows: Vec::new(),
            no_volumes: true,
        }
    }

    pub(crate) fn render(
        table: &Entity<TableState<Self>>,
        rows: Vec<VolumeSummary>,
        no_volumes: bool,
        cx: &mut Context<RuskerApp>,
    ) -> Div {
        table.update(cx, |table, _| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.no_volumes = no_volumes;
        });
        let horizontal_scroll = table.read(cx).horizontal_scroll_handle.clone();
        div()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div().flex_1().min_h_0().child(
                    DataTable::new(table)
                        .bordered(false)
                        .with_size(Size::Large)
                        .header_row_height(px(40.))
                        .scrollbar_visible(true, false)
                        .accessibility_label(t!("volumes.title")),
                ),
            )
            .child(table_horizontal_scrollbar(&horizontal_scroll, None))
    }
}

impl TableDelegate for VolumeTable {
    fn columns_count(&self, _: &App) -> usize {
        6
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let fixed = |key, width| {
            Column::new(key, "")
                .width(px(width))
                .fixed_left()
                .resizable(false)
                .selectable(false)
                .p_0()
        };
        match col_ix {
            0 => fixed("check", 32.),
            1 => fixed("usage", 28.),
            2 => Column::new("name", t!("volumes.column.name"))
                .width(px(320.))
                .min_width(px(NAME_COLUMN_MIN)),
            3 => Column::new("created", t!("volumes.column.created"))
                .width(px(DATE_COLUMN))
                .min_width(px(120.)),
            4 => Column::new("size", t!("volumes.column.size"))
                .width(px(SIZE_COLUMN))
                .min_width(px(80.))
                .text_right(),
            5 => Column::new("actions", t!("volumes.column.actions"))
                .width(px(92.))
                .resizable(false)
                .selectable(false)
                .p_0(),
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
            return div()
                .size_full()
                .child(self.column(col_ix, cx).name.clone())
                .into_any_element();
        }
        let visible_names: Vec<String> = self.rows.iter().map(|row| row.name.clone()).collect();
        let all_checked = !visible_names.is_empty()
            && self.weak.upgrade().is_some_and(|app| {
                visible_names
                    .iter()
                    .all(|name| app.read(cx).volumes.checked.contains(name))
            });
        let weak = self.weak.clone();
        div()
            .h_full()
            .flex()
            .items_center()
            .child(
                Checkbox::new("check-all-volumes")
                    .pointer(!visible_names.is_empty())
                    .checked(all_checked)
                    .tooltip(t!("volumes.list.select_all"))
                    .disabled(visible_names.is_empty())
                    .on_click(move |checked: &bool, _, cx| {
                        _ = weak.update(cx, |this, cx| {
                            for name in &visible_names {
                                if *checked {
                                    this.volumes.checked.insert(name.clone());
                                } else {
                                    this.volumes.checked.remove(name);
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
        let row = &self.rows[row_ix];
        let (checked, in_use) = self.weak.upgrade().map_or((false, false), |app| {
            let app = app.read(cx);
            (
                app.volumes.checked.contains(&row.name),
                app.snapshot.volume_in_use(&row.name),
            )
        });
        let name = row.name.clone();
        let weak = self.weak.clone();
        div()
            .id(("volume-table-row", row_ix))
            .text_sm()
            .cursor_pointer()
            .when(in_use, |this| this.font_semibold())
            .when(checked, |this| this.bg(cx.theme().accent))
            .on_click(move |_, _, cx| {
                _ = weak.update(cx, |this, cx| this.open_volume(name.clone(), cx));
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = self.rows[row_ix].clone();
        match col_ix {
            0 => {
                let checked = self
                    .weak
                    .upgrade()
                    .is_some_and(|app| app.read(cx).volumes.checked.contains(&row.name));
                let name = row.name.clone();
                let weak = self.weak.clone();
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        Checkbox::new(("check-volume", row_ix))
                            .pointer(true)
                            .checked(checked)
                            .on_click(move |checked: &bool, _, cx| {
                                cx.stop_propagation();
                                _ = weak.update(cx, |this, cx| {
                                    if *checked {
                                        this.volumes.checked.insert(name.clone());
                                    } else {
                                        this.volumes.checked.remove(&name);
                                    }
                                    cx.notify();
                                });
                            }),
                    )
                    .into_any_element()
            }
            1 => {
                let in_use = self
                    .weak
                    .upgrade()
                    .is_some_and(|app| app.read(cx).snapshot.volume_in_use(&row.name));
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(usage_dot(in_use, cx))
                    .into_any_element()
            }
            2 => div()
                .min_w_0()
                .truncate()
                .text_color(cx.theme().primary)
                .child(row.name.clone())
                .into_any_element(),
            3 => div()
                .child(format_timestamp(row.created))
                .into_any_element(),
            4 => {
                let size = self.weak.upgrade().map_or_else(
                    || "—".into(),
                    |app| app.read(cx).volume_size_label(&row.name),
                );
                div().text_right().child(size).into_any_element()
            }
            5 => {
                let busy = self
                    .weak
                    .upgrade()
                    .is_some_and(|app| app.read(cx).busy.is_some());
                let clone_source = row.name.clone();
                let clone_weak = self.weak.clone();
                let remove_name = row.name.clone();
                let remove_weak = self.weak.clone();
                div()
                    .px_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        list_action_button(("clone-volume", row_ix), cx.theme().primary, cx)
                            .small()
                            .icon(IconName::Copy)
                            .tooltip(t!("volumes.action.clone"))
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                _ = clone_weak.update(cx, |this, cx| {
                                    this.open_clone_volume_dialog(clone_source.clone(), window, cx);
                                });
                            }),
                    )
                    .child(
                        list_action_button(("remove-volume", row_ix), cx.theme().danger, cx)
                            .small()
                            .icon(AssetIcon::Trash)
                            .tooltip(t!("volumes.action.delete"))
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                _ = remove_weak.update(cx, |this, cx| {
                                    this.confirm_remove_volume(remove_name.clone(), window, cx);
                                });
                            }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        empty_state(
            if self.no_volumes {
                t!("volumes.list.empty")
            } else {
                t!("volumes.list.no_match")
            },
            t!("volumes.list.empty_hint"),
            cx,
        )
        .into_any_element()
    }
}
