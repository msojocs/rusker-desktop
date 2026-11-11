//! Images page (Docker Desktop-style): one row per tag with usage, sortable
//! columns, filter, run / pull / delete per row, batch delete and cleanup.

mod detail;
mod pull;
mod table;

pub(super) use table::ImageTable;

use std::collections::HashSet;

use gpui_kit::component::{
    button::{ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::Input,
    menu::{DropdownMenu as _, PopupMenuItem},
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div,
    prelude::FluentBuilder as _,
    px, Anchor, ClipboardItem, Context, Div, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
};
use rusker_desktop::{
    format::{format_bytes, format_relative, short_id},
    image::{
        distinct_images, image_disk_usage, image_row_matches, image_rows, unused_image_targets,
        ImageFilter, ImageRow, ImageSort, ImageSortKey,
    },
    model::ContainerSummary,
};
use time::OffsetDateTime;

use super::{form_field, list_action_button, Page, PointerCursor as _, RuskerApp};
use crate::ui::{button, confirmation_footer, empty_state, muted};

const CHECK_COLUMN: f32 = 32.;
const DOT_COLUMN: f32 = 28.;
const TAG_COLUMN: f32 = 180.;
const ID_COLUMN: f32 = 130.;
const CREATED_COLUMN: f32 = 110.;
const SIZE_COLUMN: f32 = 110.;
const ACTIONS_COLUMN: f32 = 112.;
const USAGE_BAR_WIDTH: f32 = 240.;

#[derive(Default)]
pub(super) struct ImageState {
    pub(super) selected: Option<ImageRow>,
    detail: detail::ImageDetailState,
    /// Row keys (`repo:tag` or ID) ticked for batch deletion.
    checked: HashSet<String>,
    filter: ImageFilter,
    sort: ImageSort,
    /// Unix time of the last successful snapshot.
    last_refresh: Option<i64>,
    pull_error: Option<String>,
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

impl RuskerApp {
    // ---- state ------------------------------------------------------------

    /// Called whenever a snapshot is applied: drops ticks of vanished rows.
    pub(super) fn after_image_snapshot(&mut self) {
        self.images.last_refresh = Some(now());
        let keys: HashSet<String> = image_rows(&self.snapshot.images, &self.snapshot.containers)
            .into_iter()
            .map(|row| row.key)
            .collect();
        self.images.checked.retain(|key| keys.contains(key));
        if let Some(selected) = self.images.selected.as_ref() {
            let rows = self.image_rows();
            self.images.selected = rows
                .iter()
                .find(|row| row.id == selected.id && row.key == selected.key)
                .or_else(|| rows.iter().find(|row| row.id == selected.id))
                .cloned();
        }
        if self.images.selected.is_none() {
            self.reset_image_detail();
        }
    }

    pub(super) fn open_container_image(
        &mut self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) {
        let row = rusker_desktop::image::container_image(&self.snapshot.images, container)
            .and_then(|image| {
                let rows = self.image_rows();
                rows.iter()
                    .find(|row| {
                        row.id == image.id
                            && row.reference.as_deref() == Some(container.image.as_str())
                    })
                    .or_else(|| rows.iter().find(|row| row.id == image.id))
                    .cloned()
            });
        if let Some(row) = row {
            self.navigate(Page::Images, cx);
            self.open_image_detail(row, cx);
        } else {
            self.notify_error(t!("images.error.image_missing"), cx);
        }
        cx.notify();
    }

    fn image_rows(&self) -> Vec<ImageRow> {
        image_rows(&self.snapshot.images, &self.snapshot.containers)
    }

    // ---- operations -------------------------------------------------------

    fn confirm_remove_image_row(
        &mut self,
        row: &ImageRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let shared_tags = self
            .snapshot
            .images
            .iter()
            .find(|image| image.id == row.id)
            .map(|image| {
                image
                    .tags
                    .iter()
                    .filter(|tag| !tag.contains("<none>"))
                    .count()
            })
            .unwrap_or(0);
        let description = if row.in_use() {
            t!("images.remove.in_use")
        } else if row.reference.is_some() && shared_tags > 1 {
            t!("images.remove.untag")
        } else {
            t!("images.remove.permanent")
        }
        .to_string();
        let target = row.target().to_string();
        let title = t!(
            "images.remove.title",
            name = row.reference.clone().unwrap_or_else(|| short_id(&row.id))
        )
        .to_string();
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let target = target.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text(t!("images.action.delete").into_owned())
                .cancel_text(t!("images.action.cancel").into_owned())
                .ok_variant(ButtonVariant::Danger)
                .footer(confirmation_footer(
                    t!("images.action.delete").into_owned(),
                    ButtonVariant::Danger,
                    cx,
                ))
                .on_ok(move |_, _, cx| {
                    let target = target.clone();
                    _ = weak.update(cx, |this, cx| this.remove_image(target, cx));
                    true
                })
        });
    }

    fn confirm_remove_images(
        &mut self,
        targets: Vec<String>,
        unused_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() || self.busy.is_some() {
            return;
        }
        let mut listed: Vec<String> = targets
            .iter()
            .take(8)
            .map(|target| {
                if target.starts_with("sha256:") {
                    t!("images.remove_many.untagged", id = short_id(target)).into_owned()
                } else {
                    target.clone()
                }
            })
            .collect();
        if targets.len() > listed.len() {
            listed.push("…".into());
        }
        let mut description = if unused_only {
            t!("images.remove_many.unused_intro").into_owned()
        } else {
            t!("images.remove_many.intro").into_owned()
        };
        description.push('\n');
        description.push_str(&listed.join("\n"));
        if !unused_only {
            description.push('\n');
            description.push_str(&t!("images.remove_many.in_use_note"));
        }
        let title = if unused_only {
            t!("images.remove_many.prune_title", count = targets.len())
        } else {
            t!("images.remove_many.title", count = targets.len())
        }
        .into_owned();
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let targets = targets.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text(t!("images.action.delete").into_owned())
                .cancel_text(t!("images.action.cancel").into_owned())
                .ok_variant(ButtonVariant::Danger)
                .footer(confirmation_footer(
                    t!("images.action.delete").into_owned(),
                    ButtonVariant::Danger,
                    cx,
                ))
                .on_ok(move |_, _, cx| {
                    let targets = targets.clone();
                    _ = weak.update(cx, |this, cx| this.remove_images(targets, cx));
                    true
                })
        });
    }

    fn remove_images(&mut self, targets: Vec<String>, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.notify_error(t!("images.error.engine_unavailable"), cx);
            cx.notify();
            return;
        };
        let total = targets.len();
        let handle = engine.remove_images(targets);
        self.refresh_revision.advance();
        self.busy = Some(t!("images.remove_many.busy", count = total).into_owned());
        self.loading = false;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = super::await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok((snapshot, failures)) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot, cx);
                        if failures.is_empty() {
                            this.notify_success(t!("images.remove_many.done", count = total), cx);
                        } else {
                            this.notify_error(
                                t!(
                                    "images.remove_many.partial",
                                    removed = total - failures.len(),
                                    failed = failures.len(),
                                    failures = failures.join("\n")
                                ),
                                cx,
                            );
                        }
                    }
                    Err(error) => this.notify_error(error.to_string(), cx),
                }
                cx.notify();
            });
        }));
    }

    fn open_pull_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.images.pull_error = None;
        self.pull_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        let input = self.pull_input.clone();
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let error = weak
                .upgrade()
                .and_then(|this| this.read(cx).images.pull_error.clone());
            let submit = weak.clone();
            let confirm = weak.clone();
            dialog
                .title(t!("images.pull.title").into_owned())
                .w(px(520.))
                .on_ok(move |_, window, cx| {
                    _ = confirm.update(cx, |this, cx| this.submit_pull(window, cx));
                    // submit_pull closes only after validation succeeds.
                    false
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(form_field(
                            t!("images.pull.reference"),
                            &input,
                            t!("images.pull.hint"),
                            cx,
                        ))
                        .when_some(error, |this, error| {
                            this.child(
                                div()
                                    .p_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().danger)
                                    .text_sm()
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        }),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            button("cancel-pull")
                                .outline()
                                .label(t!("images.action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            button("submit-pull")
                                .primary()
                                .icon(IconName::ArrowDown)
                                .label(t!("images.pull.submit").into_owned())
                                .on_click(move |_, window, cx| {
                                    _ = submit.update(cx, |this, cx| {
                                        this.submit_pull(window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn submit_pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reference = self.pull_input.read(cx).value().trim().to_string();
        let error = if reference.is_empty() {
            Some(t!("images.pull.error.empty").into_owned())
        } else if reference.chars().any(char::is_whitespace) {
            Some(t!("images.pull.error.whitespace").into_owned())
        } else {
            None
        };
        if let Some(error) = error {
            self.images.pull_error = Some(error);
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        self.pull_image(reference, window, cx);
    }

    /// Shows the containers created from the row's image on the Containers page.
    fn show_image_containers(
        &mut self,
        row: &ImageRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = row.reference.clone().unwrap_or_else(|| short_id(&row.id));
        self.search_input
            .update(cx, |state, cx| state.set_value(query, window, cx));
        self.navigate(Page::Containers, cx);
    }

    // ---- rendering --------------------------------------------------------

    pub(super) fn render_images(&self, cx: &mut Context<Self>) -> Div {
        if let Some(row) = &self.images.selected {
            return self.render_image_detail(row, cx);
        }
        self.render_image_list(cx)
    }

    // Keep list construction off the detail render call stack in debug builds.
    fn render_image_list(&self, cx: &mut Context<Self>) -> Div {
        let query = self.search_input.read(cx).value().to_string();
        let all_rows = self.image_rows();
        let mut rows: Vec<ImageRow> = all_rows
            .iter()
            .filter(|row| self.images.filter.accepts(row) && image_row_matches(row, &query))
            .cloned()
            .collect();
        self.images.sort.apply(&mut rows);
        let busy = self.busy.is_some();

        let (used, total) = image_disk_usage(&all_rows);
        let image_count = distinct_images(&all_rows);
        let unused = unused_image_targets(&all_rows);
        let mut checked: Vec<String> = self.images.checked.iter().cloned().collect();
        checked.sort();

        let usage = div()
            .flex()
            .items_end()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.usage_bar(used, total, cx))
                    .child(
                        div().text_sm().child(
                            t!(
                                "images.usage.in_use",
                                used = format_bytes(used),
                                total = format_bytes(total)
                            )
                            .into_owned(),
                        ),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .child(t!("images.usage.count", count = image_count).into_owned()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(match self.images.last_refresh {
                        Some(time) => {
                            t!("images.refresh.last", time = format_relative(time, now()))
                                .into_owned()
                        }
                        None => t!("images.refresh.never").into_owned(),
                    }),
            )
            .child(
                list_action_button("refresh-images", cx.theme().foreground, cx)
                    .small()
                    .icon(IconName::RotateCw)
                    .tooltip(t!("images.action.refresh").into_owned())
                    .loading(self.loading)
                    .disabled(self.loading || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );

        let filter = self.images.filter;
        let weak = cx.weak_entity();
        let toolbar = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div().w(px(320.)).child(
                    Input::new(&self.search_input)
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true),
                ),
            )
            .child(
                list_action_button("image-filter", cx.theme().foreground, cx)
                    .icon(Icon::new(AssetIcon::ListFilter))
                    .when(filter != ImageFilter::All, |button| {
                        button.label(filter.label().into_owned())
                    })
                    .tooltip(t!("images.action.filter").into_owned())
                    .dropdown_menu_with_anchor(Anchor::TopLeft, move |mut menu, _, _| {
                        for option in ImageFilter::ALL {
                            let weak = weak.clone();
                            menu = menu.item(
                                PopupMenuItem::new(option.label().into_owned())
                                    .checked(option == filter)
                                    .on_click(move |_, _, cx| {
                                        _ = weak.update(cx, |this, cx| {
                                            this.images.filter = option;
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        menu.min_w(px(160.))
                    }),
            )
            .child(div().flex_1())
            .when(!checked.is_empty(), |this| {
                let count = checked.len();
                this.child(
                    list_action_button("clear-image-selection", cx.theme().foreground, cx)
                        .label(t!("images.action.clear_selection").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.images.checked.clear();
                            cx.notify();
                        })),
                )
                .child(
                    button("remove-checked-images")
                        .danger()
                        .pointer(!busy)
                        .icon(AssetIcon::Trash)
                        .label(t!("images.action.delete_selected", count = count).into_owned())
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove_images(checked.clone(), false, window, cx);
                        })),
                )
            })
            .child(
                button("prune-images")
                    .outline()
                    .pointer(!busy && !unused.is_empty())
                    .icon(AssetIcon::BrushCleaning)
                    .label(t!("images.action.prune", count = unused.len()).into_owned())
                    .tooltip(t!("images.action.prune_tooltip").into_owned())
                    .disabled(busy || unused.is_empty())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove_images(unused.clone(), true, window, cx);
                    })),
            )
            .child(
                button("open-pull-image")
                    .primary()
                    .pointer(!busy)
                    .icon(IconName::ArrowDown)
                    .label(t!("images.pull.title").into_owned())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.open_pull_dialog(window, cx))),
            );

        let shown = rows.len();
        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(usage)
            .child(toolbar)
            .child(self.render_image_table(rows, all_rows.is_empty(), cx))
            .child(div().flex().justify_end().child(muted(
                t!("images.list.shown", count = shown).into_owned(),
                cx,
            )))
    }

    fn usage_bar(&self, used: u64, total: u64, cx: &mut Context<Self>) -> Div {
        let ratio = if total == 0 {
            0.
        } else {
            (used as f64 / total as f64).clamp(0., 1.) as f32
        };
        div()
            .w(px(USAGE_BAR_WIDTH))
            .h(px(4.))
            .rounded_full()
            .bg(cx.theme().border)
            .child(
                div()
                    .h_full()
                    .w(px(USAGE_BAR_WIDTH * ratio))
                    .rounded_full()
                    .bg(cx.theme().success),
            )
    }

    fn render_image_table(
        &self,
        rows: Vec<ImageRow>,
        no_images: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        ImageTable::render(&self.images_table, rows, no_images, cx)
    }
}

fn image_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    row: &ImageRow,
    weak: gpui_kit::WeakEntity<RuskerApp>,
) -> gpui_kit::component::menu::PopupMenu {
    let detail_row = row.clone();
    let detail_weak = weak.clone();
    let containers_row = row.clone();
    let containers_weak = weak.clone();
    let pull_reference = row.reference.clone();
    let id = row.id.clone();
    let reference = row.reference.clone();
    menu.item(
        image_menu_item(
            t!("images.menu.view_details").into_owned(),
            AssetIcon::Layers2,
            true,
        )
        .on_click(move |_, _, cx| {
            _ = detail_weak.update(cx, |this, cx| {
                this.open_image_detail(detail_row.clone(), cx);
            });
        }),
    )
    .item(
        image_menu_item(
            if row.in_use() {
                t!("images.menu.view_containers_count", count = row.containers).into_owned()
            } else {
                t!("images.menu.view_containers").into_owned()
            },
            AssetIcon::Container,
            row.in_use(),
        )
        .on_click(move |_, window, cx| {
            _ = containers_weak.update(cx, |this, cx| {
                this.show_image_containers(&containers_row, window, cx);
            });
        }),
    )
    .item(
        image_menu_item(
            t!("images.menu.pull_latest").into_owned(),
            AssetIcon::Download,
            pull_reference.is_some(),
        )
        .on_click(move |_, window, cx| {
            if let Some(reference) = pull_reference.clone() {
                _ = weak.update(cx, |this, cx| this.pull_image(reference, window, cx));
            }
        }),
    )
    .separator()
    .item(
        image_menu_item(t!("images.menu.copy_id").into_owned(), IconName::Copy, true).on_click(
            move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
            },
        ),
    )
    .item(
        image_menu_item(
            t!("images.menu.copy_reference").into_owned(),
            IconName::Copy,
            reference.is_some(),
        )
        .on_click(move |_, _, cx| {
            if let Some(reference) = reference.clone() {
                cx.write_to_clipboard(ClipboardItem::new_string(reference));
            }
        }),
    )
    .min_w(px(200.))
}

/// Custom content lets the cursor cover the icon, label and row padding;
/// the popup menu still owns activation, keyboard navigation and dismissal.
fn image_menu_item(
    label: impl Into<SharedString>,
    icon: impl Into<Icon>,
    enabled: bool,
) -> PopupMenuItem {
    let label = label.into();
    let icon = icon.into();
    PopupMenuItem::element(move |_, _| {
        div()
            .flex_1()
            .h(px(26.))
            .mx_neg_2()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .when(enabled, |this| this.cursor_pointer())
            .when(!enabled, |this| this.cursor_default())
            .child(icon.clone().xsmall())
            .child(label.clone())
    })
    .disabled(!enabled)
}

fn usage_dot(in_use: bool, cx: &gpui_kit::App) -> Div {
    let dot = div().size(px(10.)).rounded_full().border_1();
    if in_use {
        dot.border_color(cx.theme().success).bg(cx.theme().success)
    } else {
        dot.border_color(cx.theme().muted_foreground)
    }
}
