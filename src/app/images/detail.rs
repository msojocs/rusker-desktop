//! Image detail: real Engine history on the left, metadata / analysis state
//! on the right. No image content is uploaded to an analysis service.

use gpui_kit::component::{
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    menu::DropdownMenu as _,
    tab::{Tab, TabBar},
    ActiveTheme as _, Icon, IconName, Sizable as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div,
    prelude::FluentBuilder as _,
    px, Anchor, ClipboardItem, Context, Div, InteractiveElement as _, ParentElement as _, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task,
};
use rusker_desktop::{
    format::{format_bytes, format_relative, format_timestamp, short_id},
    image::ImageRow,
    model::{ImageDetail, ImageLayer, Revision},
};

use super::{image_menu, now, RuskerApp};
use crate::{
    app::{await_engine, list_action_button, PointerCursor as _},
    ui::{detail_row, empty_state, muted},
};

#[derive(Default)]
pub(super) struct ImageDetailState {
    data: Option<ImageDetail>,
    loading: bool,
    error: Option<String>,
    revision: Revision,
    task: Option<Task<()>>,
    tab: usize,
    selected_layer: Option<usize>,
}

impl RuskerApp {
    pub(in crate::app) fn reset_image_detail(&mut self) {
        self.images.detail.revision.advance();
        self.images.detail.task = None;
        self.images.detail.data = None;
        self.images.detail.loading = false;
        self.images.detail.error = None;
        self.images.detail.tab = 0;
        self.images.detail.selected_layer = None;
    }

    pub(in crate::app) fn close_image_detail(&mut self, cx: &mut Context<Self>) {
        self.images.selected = None;
        self.reset_image_detail();
        cx.notify();
    }

    pub(in crate::app) fn open_image_detail(&mut self, row: ImageRow, cx: &mut Context<Self>) {
        self.reset_image_detail();
        self.images.selected = Some(row);
        self.load_image_detail(cx);
    }

    fn load_image_detail(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.images.selected.as_ref() else {
            return;
        };
        let id = row.id.clone();
        let revision = self.images.detail.revision.advance();
        self.images.detail.data = None;
        self.images.detail.error = None;
        self.images.detail.selected_layer = None;
        let Some(engine) = self.engine.clone() else {
            self.images.detail.loading = false;
            self.images.detail.error = Some("容器引擎不可用，请连接引擎后重试。".into());
            cx.notify();
            return;
        };
        self.images.detail.loading = true;
        let handle = engine.image_detail(id.clone());
        self.images.detail.task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.images.detail.revision.is_current(revision)
                    || this.images.selected.as_ref().map(|row| &row.id) != Some(&id)
                {
                    return;
                }
                this.images.detail.loading = false;
                match result {
                    Ok(detail) => this.images.detail.data = Some(detail),
                    Err(error) => this.images.detail.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_image_detail_header(&self, row: &ImageRow, cx: &mut Context<Self>) -> Div {
        let title = row.reference.clone().unwrap_or_else(|| short_id(&row.id));
        let busy = self.busy.is_some();
        let run_target = row.target().to_string();
        let remove_row = row.clone();
        let menu_row = row.clone();
        let menu_weak = cx.weak_entity();
        let copy_id = row.id.clone();

        div()
            .px_6()
            .py_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .flex_wrap()
            .gap_4()
            .child(
                Button::new("image-detail-back")
                    .ghost()
                    .small()
                    .pointer(true)
                    .icon(IconName::ChevronLeft)
                    .tooltip("返回镜像列表")
                    .on_click(cx.listener(|this, _, _, cx| this.close_image_detail(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_2xl()
                            .font_semibold()
                            .truncate()
                            .child(title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .child(
                                div()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(short_id(&row.id)),
                            )
                            .child(
                                list_action_button("copy-image-detail-id", cx.theme().primary, cx)
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .tooltip("复制完整镜像 ID")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_id.clone(),
                                        ));
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_5()
                    .child(header_fact(
                        "创建时间",
                        format_relative(row.created, now()),
                        cx,
                    ))
                    .child(header_fact("大小", format_bytes(row.size), cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("run-image-detail")
                            .primary()
                            .pointer(!busy)
                            .icon(IconName::Play)
                            .label("运行")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_create_container_dialog(
                                    Some(run_target.clone()),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("image-detail-more")
                            .outline()
                            .pointer(!busy)
                            .icon(IconName::ChevronDown)
                            .tooltip("更多镜像操作")
                            .disabled(busy)
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                image_menu(menu, &menu_row, menu_weak.clone())
                            }),
                    )
                    .child(
                        Button::new("remove-image-detail")
                            .danger()
                            .pointer(!busy)
                            .icon(AssetIcon::Trash)
                            .tooltip("删除镜像")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_remove_image_row(&remove_row, window, cx);
                            })),
                    ),
            )
    }

    fn render_image_detail_layers_panel(&self, cx: &mut Context<Self>) -> Div {
        let detail = &self.images.detail;
        let busy = self.busy.is_some();
        let layers_title = match &detail.data {
            Some(data) => format!("镜像层（{}）", data.layers.len()),
            None => "镜像层".into(),
        };
        div()
            .flex_1()
            .min_w(px(280.))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(52.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().font_semibold().child(layers_title))
                    .child(
                        list_action_button("refresh-image-detail", cx.theme().foreground, cx)
                            .small()
                            .icon(IconName::RotateCw)
                            .tooltip("刷新镜像详情")
                            .loading(detail.loading)
                            .disabled(detail.loading || busy)
                            .on_click(cx.listener(|this, _, _, cx| this.load_image_detail(cx))),
                    ),
            )
            .child(self.render_image_layers(cx))
    }

    fn render_image_detail_information(&self, row: &ImageRow, cx: &mut Context<Self>) -> Div {
        let detail = &self.images.detail;
        let tab_weak = cx.weak_entity();
        div()
            .flex_1()
            .min_w(px(280.))
            .h_full()
            .flex()
            .flex_col()
            .child(
                TabBar::new("image-detail-tabs")
                    .underline()
                    .h(px(52.))
                    .selected_index(detail.tab)
                    .child(Tab::new().label("漏洞"))
                    .child(Tab::new().label("软件包"))
                    .child(Tab::new().label("镜像信息"))
                    .on_click(move |index, _, cx| {
                        _ = tab_weak.update(cx, |this, cx| {
                            this.images.detail.tab = *index;
                            cx.notify();
                        });
                    }),
            )
            .child(if detail.tab == 2 {
                self.render_image_metadata(row, cx)
            } else {
                self.render_image_analysis_placeholder(cx)
            })
    }

    // GPUI builders are large values. Separate the sections so their debug
    // temporaries do not accumulate in one frame on Windows' 1 MiB UI stack.
    pub(super) fn render_image_detail(&self, row: &ImageRow, cx: &mut Context<Self>) -> Div {
        let title = row.reference.clone().unwrap_or_else(|| short_id(&row.id));
        let back = cx.weak_entity();
        let header = self.render_image_detail_header(row, cx);
        let layers = self.render_image_detail_layers_panel(cx);
        let information = self.render_image_detail_information(row, cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().px_6().pt_4().child(
                    Breadcrumb::new()
                        .child(
                            BreadcrumbItem::new("镜像")
                                .text_color(cx.theme().primary)
                                .on_click(move |_, _, cx| {
                                    _ = back.update(cx, |this, cx| this.close_image_detail(cx));
                                }),
                        )
                        .child(BreadcrumbItem::new(title)),
                ),
            )
            .child(header)
            .child(
                div()
                    .id("image-detail-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .px_6()
                    .pt_4()
                    .pb_6()
                    .flex()
                    .gap_6()
                    .child(layers)
                    .child(information),
            )
    }

    fn render_image_layers(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let detail = &self.images.detail;
        let panel = div()
            .id("image-layer-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if detail.loading {
            return panel.child(empty_state(
                "正在读取镜像层…",
                "从容器引擎获取构建历史。",
                cx,
            ));
        }
        if let Some(error) = &detail.error {
            return panel.child(
                div()
                    .py_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error.clone()),
                    )
                    .child(
                        Button::new("retry-image-detail")
                            .outline()
                            .pointer(true)
                            .label("重试")
                            .on_click(cx.listener(|this, _, _, cx| this.load_image_detail(cx))),
                    ),
            );
        }
        let Some(data) = &detail.data else {
            return panel;
        };
        if data.layers.is_empty() {
            return panel.child(empty_state(
                "没有镜像层历史",
                "引擎未返回该镜像的构建历史。",
                cx,
            ));
        }
        panel.children(
            data.layers
                .iter()
                .enumerate()
                .rev()
                .map(|(index, layer)| self.render_image_layer(index, data.layers.len(), layer, cx)),
        )
    }

    fn render_image_layer(
        &self,
        index: usize,
        count: usize,
        layer: &ImageLayer,
        cx: &mut Context<Self>,
    ) -> Div {
        let selected = self.images.detail.selected_layer == Some(index);
        let command = if layer.created_by.trim().is_empty() {
            "（未提供构建命令）".to_string()
        } else {
            layer.created_by.clone()
        };
        let copy_command = command.clone();
        div()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .id(("image-layer", index))
                    .px_2()
                    .py_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(cx.theme().accent))
                    .hover(|this| this.bg(cx.theme().accent))
                    .child(
                        div()
                            .w(px(28.))
                            .flex_shrink_0()
                            .text_color(cx.theme().muted_foreground)
                            .child((count - index - 1).to_string()),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(command.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_right()
                            .child(format_bytes(layer.size)),
                    )
                    .child(
                        Icon::new(if selected {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.images.detail.selected_layer =
                            if selected { None } else { Some(index) };
                        cx.notify();
                    })),
            )
            .when(selected, |this| {
                this.child(
                    div()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(cx.theme().secondary)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(muted("构建命令", cx))
                                .child(
                                    list_action_button(
                                        ("copy-image-layer", index),
                                        cx.theme().primary,
                                        cx,
                                    )
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .tooltip("复制完整构建命令")
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_command.clone(),
                                            ));
                                        },
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(command),
                        )
                        .child(detail_row("创建时间", format_timestamp(layer.created), cx))
                        .when(!layer.id.is_empty(), |this| {
                            this.child(detail_row("层 ID", layer.id.clone(), cx))
                        })
                        .when(!layer.comment.is_empty(), |this| {
                            this.child(detail_row("备注", layer.comment.clone(), cx))
                        }),
                )
            })
    }

    fn render_image_analysis_placeholder(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let packages = self.images.detail.tab == 1;
        div()
            .id("image-analysis-state")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .p_6()
            .gap_4()
            .text_center()
            .child(
                div()
                    .size(px(64.))
                    .rounded_full()
                    .bg(cx.theme().secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(AssetIcon::Layers2).size(px(32.)).text_color(cx.theme().muted_foreground)),
            )
            .child(div().text_lg().font_semibold().child(if packages {
                "尚未生成软件包清单"
            } else {
                "此镜像尚未进行漏洞分析"
            }))
            .child(muted(if packages {
                "软件包清单需要通过 SBOM 分析工具生成，无法仅从镜像构建历史中获取。"
            } else {
                "Rusker 尚未接入镜像安全扫描服务，当前无法提供漏洞检测结果。未分析不代表没有漏洞。"
            }, cx))
            .child(muted("镜像不会被自动上传至第三方服务。", cx))
            .child(
                Button::new("show-image-metadata")
                    .outline()
                    .pointer(true)
                    .label("查看镜像信息")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.images.detail.tab = 2;
                        cx.notify();
                    })),
            )
    }

    fn render_image_metadata(&self, row: &ImageRow, cx: &mut Context<Self>) -> Stateful<Div> {
        let image = self.snapshot.images.iter().find(|image| image.id == row.id);
        let tags = image
            .map(|image| {
                image
                    .tags
                    .iter()
                    .filter(|tag| !tag.is_empty() && *tag != "<none>:<none>")
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|tags| !tags.is_empty())
            .unwrap_or_else(|| "无标签".into());
        let digests = image
            .map(|image| image.digests.join("\n"))
            .filter(|digests| !digests.is_empty())
            .unwrap_or_else(|| "—".into());
        let platform = self
            .images
            .detail
            .data
            .as_ref()
            .map(|data| format!("{} / {}", data.os, data.architecture))
            .unwrap_or_else(|| "—".into());
        div()
            .id("image-metadata-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(detail_row("镜像 ID", row.id.clone(), cx))
            .child(detail_row("平台", platform, cx))
            .child(detail_row("标签", tags, cx))
            .child(detail_row("摘要", digests, cx))
            .child(detail_row("创建时间", format_timestamp(row.created), cx))
            .child(detail_row("大小", format_bytes(row.size), cx))
            .child(detail_row("使用的容器", row.containers.to_string(), cx))
    }
}

fn header_fact(label: &'static str, value: String, cx: &gpui_kit::App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().font_semibold().child(label))
        .child(muted(value, cx))
}
