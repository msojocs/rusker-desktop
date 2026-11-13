use gpui_kit::component::{
    scroll::Scrollbar,
    table::{Column, ColumnSort, DataTable, TableDelegate, TableState},
    Size,
};
use gpui_kit::{App, AppContext as _, Entity, Pixels, Stateful, WeakEntity};

use std::rc::Rc;

use super::*;
use crate::app::{
    table_control_cell, table_control_header_cell, table_horizontal_scrollbar, TABLE_CHECK_COLUMN,
    TABLE_STATUS_COLUMN,
};

pub(crate) struct ImageTables {
    main: Entity<TableState<ImageTable>>,
    actions: Entity<TableState<ImageActionsTable>>,
}

impl ImageTables {
    pub(crate) fn new(
        weak: WeakEntity<RuskerApp>,
        window: &mut Window,
        cx: &mut Context<RuskerApp>,
    ) -> Self {
        let main = cx.new(|cx| {
            TableState::new(ImageTable::new(weak.clone()), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
        });
        let vertical_scroll_handle = main.read(cx).vertical_scroll_handle.clone();
        let actions = cx.new(|cx| {
            let mut state = TableState::new(
                ImageActionsTable {
                    weak,
                    rows: Rc::new(Vec::new()),
                },
                window,
                cx,
            )
            .row_selectable(false)
            .col_selectable(false)
            .col_movable(false)
            .sortable(false);
            state.vertical_scroll_handle = vertical_scroll_handle;
            state
        });
        Self { main, actions }
    }

    pub(crate) fn render(
        &self,
        rows: Vec<ImageRow>,
        no_images: bool,
        cx: &mut Context<RuskerApp>,
    ) -> Div {
        let rows = Rc::new(rows);
        self.main.update(cx, |table, _| {
            let delegate = table.delegate_mut();
            delegate.rows = rows.clone();
            delegate.now = now();
            delegate.no_images = no_images;
        });
        self.actions
            .update(cx, |table, _| table.delegate_mut().rows = rows);
        let horizontal_scroll = self.main.read(cx).horizontal_scroll_handle.clone();
        let horizontal_content_width = {
            let table = self.main.read(cx);
            (0..ImageTable::COLUMNS_COUNT)
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
                                .fixed_cols_border(false)
                                .with_size(Size::Large)
                                .header_row_height(px(40.))
                                .scrollbar_visible(true, false)
                                .accessibility_label(t!("images.title")),
                        ),
                    )
                    .child(table_horizontal_scrollbar(
                        &horizontal_scroll,
                        Some(horizontal_content_width),
                    )),
            )
            .child(
                div()
                    .w(px(ACTIONS_COLUMN))
                    .flex_shrink_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div().flex_1().min_h_0().child(
                            DataTable::new(&self.actions)
                                .bordered(false)
                                .with_size(Size::Large)
                                .header_row_height(px(40.))
                                .scrollbar_visible(false, false)
                                .accessibility_label(t!("images.column.actions")),
                        ),
                    )
                    .child(div().h(Scrollbar::width()).flex_shrink_0()),
            )
    }
}

struct ImageActionsTable {
    weak: WeakEntity<RuskerApp>,
    rows: Rc<Vec<ImageRow>>,
}

impl TableDelegate for ImageActionsTable {
    fn columns_count(&self, _: &App) -> usize {
        1
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, _: usize, _: &App) -> Column {
        Column::new("actions", t!("images.column.actions"))
            .width(px(ACTIONS_COLUMN))
            .resizable(false)
            .selectable(false)
            .p_0()
    }

    fn render_th(
        &mut self,
        _: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .h(px(40.))
            .flex()
            .items_center()
            .pl_2()
            .child(t!("images.column.actions"))
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let row = &self.rows[row_ix];
        let checked = self
            .weak
            .upgrade()
            .is_some_and(|app| app.read(cx).images.checked.contains(&row.key));
        div()
            .id(("image-action-row", row_ix))
            .text_sm()
            .when(row.in_use(), |this| this.font_semibold())
            .when(checked, |this| this.bg(cx.theme().accent))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        _: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let busy = self
            .weak
            .upgrade()
            .is_some_and(|app| app.read(cx).busy.is_some());
        let run_target = row.target().to_string();
        let run_weak = self.weak.clone();
        let menu_row = row.clone();
        let menu_weak = self.weak.clone();
        let remove_row = row.clone();
        let remove_weak = self.weak.clone();
        div()
            .px_2()
            .h(Size::Large.table_row_height())
            .flex()
            .items_center()
            .gap_1()
            .child(
                list_action_button(("run-image", row_ix), cx.theme().primary, cx)
                    .small()
                    .icon(IconName::Play)
                    .tooltip(t!("images.action.run").into_owned())
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        _ = run_weak.update(cx, |this, cx| {
                            this.open_create_container_dialog(Some(run_target.clone()), window, cx);
                        });
                    }),
            )
            .child(
                list_action_button(("image-more", row_ix), cx.theme().foreground, cx)
                    .small()
                    .icon(IconName::EllipsisVertical)
                    .disabled(busy)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        image_menu(menu, &menu_row, menu_weak.clone())
                    }),
            )
            .child(
                list_action_button(("remove-image", row_ix), cx.theme().danger, cx)
                    .small()
                    .icon(AssetIcon::Trash)
                    .tooltip(t!("images.action.delete").into_owned())
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        _ = remove_weak.update(cx, |this, cx| {
                            this.confirm_remove_image_row(&remove_row, window, cx);
                        });
                    }),
            )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }

    fn render_last_empty_col(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }
}

struct ImageTable {
    weak: WeakEntity<RuskerApp>,
    rows: Rc<Vec<ImageRow>>,
    sort: ImageSort,
    now: i64,
    no_images: bool,
}

impl ImageTable {
    const COLUMNS_COUNT: usize = 7;

    fn new(weak: WeakEntity<RuskerApp>) -> Self {
        Self {
            weak,
            rows: Rc::new(Vec::new()),
            sort: ImageSort::default(),
            now: 0,
            no_images: true,
        }
    }

    fn sort_column(&self, key: ImageSortKey) -> ColumnSort {
        if self.sort.key != key {
            ColumnSort::Default
        } else if self.sort.descending {
            ColumnSort::Descending
        } else {
            ColumnSort::Ascending
        }
    }
}

impl TableDelegate for ImageTable {
    fn columns_count(&self, _: &App) -> usize {
        Self::COLUMNS_COUNT
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let control = |key, width| {
            Column::new(key, "")
                .width(px(width))
                .resizable(false)
                .selectable(false)
                .p_0()
        };
        match col_ix {
            0 => control("check", TABLE_CHECK_COLUMN),
            1 => control("usage", TABLE_STATUS_COLUMN),
            2 => Column::new("name", t!("images.column.name"))
                .width(px(240.))
                .min_width(px(120.))
                .sort(self.sort_column(ImageSortKey::Name)),
            3 => Column::new("tag", t!("images.column.tag"))
                .width(px(TAG_COLUMN))
                .min_width(px(80.))
                .sort(self.sort_column(ImageSortKey::Tag)),
            4 => Column::new("id", t!("images.column.id"))
                .width(px(ID_COLUMN))
                .min_width(px(80.)),
            5 => Column::new("created", t!("images.column.created"))
                .width(px(CREATED_COLUMN))
                .min_width(px(80.))
                .sort(self.sort_column(ImageSortKey::Created)),
            6 => Column::new("size", t!("images.column.size"))
                .width(px(SIZE_COLUMN))
                .min_width(px(80.))
                .text_right()
                .sort(self.sort_column(ImageSortKey::Size)),
            _ => unreachable!(),
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let key = match col_ix {
            2 => ImageSortKey::Name,
            3 => ImageSortKey::Tag,
            5 => ImageSortKey::Created,
            6 => ImageSortKey::Size,
            _ => return,
        };
        self.sort = ImageSort {
            key,
            descending: match sort {
                ColumnSort::Ascending => false,
                ColumnSort::Descending => true,
                ColumnSort::Default => {
                    matches!(key, ImageSortKey::Created | ImageSortKey::Size)
                }
            },
        };
        self.sort.apply(Rc::make_mut(&mut self.rows).as_mut_slice());
        _ = self.weak.update(cx, |this, cx| {
            this.images.sort = self.sort;
            cx.notify();
        });
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = match col_ix {
            0 => table_control_header_cell(self.column(col_ix, cx).width, cx),
            1 => table_control_cell()
                .w(self.column(col_ix, cx).width)
                .flex_shrink_0(),
            _ => div().size_full(),
        };
        if col_ix != 0 {
            return cell
                .child(self.column(col_ix, cx).name.clone())
                .into_any_element();
        }
        let visible: Vec<String> = self.rows.iter().map(|row| row.key.clone()).collect();
        let all_checked = !visible.is_empty()
            && self.weak.upgrade().is_some_and(|app| {
                visible
                    .iter()
                    .all(|key| app.read(cx).images.checked.contains(key))
            });
        let weak = self.weak.clone();
        cell.child(
            Checkbox::new("check-all-images")
                .checked(all_checked)
                .tooltip(t!("images.list.select_all").into_owned())
                .disabled(visible.is_empty())
                .on_click(move |checked: &bool, _, cx| {
                    _ = weak.update(cx, |this, cx| {
                        for key in &visible {
                            if *checked {
                                this.images.checked.insert(key.clone());
                            } else {
                                this.images.checked.remove(key);
                            }
                        }
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
        let checked = self
            .weak
            .upgrade()
            .is_some_and(|app| app.read(cx).images.checked.contains(&row.key));
        div()
            .id(("image-table-row", row_ix))
            .text_sm()
            .when(row.in_use(), |this| this.font_semibold())
            .when(checked, |this| this.bg(cx.theme().accent))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = self.rows[row_ix].clone();
        let none = |text: &str| text == rusker_desktop::image::NONE_LABEL;
        match col_ix {
            0 => {
                let key = row.key.clone();
                let weak = self.weak.clone();
                let checked = self
                    .weak
                    .upgrade()
                    .is_some_and(|app| app.read(cx).images.checked.contains(&row.key));
                table_control_cell()
                    .child(
                        Checkbox::new(("check-image", row_ix))
                            .checked(checked)
                            .on_click(move |checked: &bool, _, cx| {
                                _ = weak.update(cx, |this, cx| {
                                    if *checked {
                                        this.images.checked.insert(key.clone());
                                    } else {
                                        this.images.checked.remove(&key);
                                    }
                                    cx.notify();
                                });
                            }),
                    )
                    .into_any_element()
            }
            1 => {
                let in_use = row.in_use();
                table_control_cell()
                    .id(("image-usage", row_ix))
                    .child(usage_dot(in_use, cx))
                    .tooltip(move |window, cx| {
                        let text: SharedString = if in_use {
                            t!("images.usage.used_by", count = row.containers)
                                .into_owned()
                                .into()
                        } else {
                            t!("images.usage.unused").into_owned().into()
                        };
                        gpui_kit::component::tooltip::Tooltip::new(text).build(window, cx)
                    })
                    .into_any_element()
            }
            2 => {
                let weak = self.weak.clone();
                let detail = row.clone();
                div()
                    .id(("open-image-detail", row_ix))
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .text_color(cx.theme().primary)
                    .hover(|this| this.underline())
                    .when(none(&row.repository), |this| {
                        this.text_color(cx.theme().muted_foreground)
                    })
                    .child(row.repository.clone())
                    .on_click(move |_, _, cx| {
                        _ = weak.update(cx, |this, cx| this.open_image_detail(detail.clone(), cx));
                    })
                    .into_any_element()
            }
            3 => div()
                .truncate()
                .when(none(&row.tag), |this| {
                    this.text_color(cx.theme().muted_foreground)
                })
                .child(row.tag.clone())
                .into_any_element(),
            4 => div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_xs()
                .child(short_id(&row.id))
                .into_any_element(),
            5 => div()
                .child(format_relative(row.created, self.now))
                .into_any_element(),
            6 => div()
                .text_right()
                .child(format_bytes(row.size))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.no_images {
            empty_state(
                t!("images.empty.title").into_owned(),
                t!("images.empty.description").into_owned(),
                cx,
            )
            .into_any_element()
        } else {
            empty_state(
                t!("images.empty.no_match").into_owned(),
                t!("images.empty.no_match_hint").into_owned(),
                cx,
            )
            .into_any_element()
        }
    }
}
