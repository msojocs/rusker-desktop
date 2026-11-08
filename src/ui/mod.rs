pub mod stats_chart;
pub mod terminal;
pub mod theme;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::{
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::{Cancel, Confirm},
    ActiveTheme as _,
};
use gpui_kit::{
    div, px, App, Div, ElementId, InteractiveElement as _, ParentElement as _, SharedString,
    Styled as _,
};

/// Shared constructor so every application button uses the pointing-hand cursor.
pub fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id).cursor_pointer()
}

/// Pointer-enabled alert buttons, dispatching through their own dialog's focus path.
pub fn confirmation_footer(label: &'static str, variant: ButtonVariant, cx: &mut App) -> Div {
    let anchor = cx.focus_handle();
    let cancel = anchor.clone();
    div()
        .flex()
        .justify_end()
        .gap_2()
        .child(div().absolute().size_0().track_focus(&anchor))
        .child(
            button("cancel")
                .outline()
                .label("取消")
                .on_click(move |_, window, cx| cancel.dispatch_action(&Cancel, window, cx)),
        )
        .child(
            button("ok")
                .with_variant(variant)
                .label(label)
                .on_click(move |_, window, cx| {
                    anchor.dispatch_action(&Confirm { secondary: false }, window, cx);
                }),
        )
}

pub fn page_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_2xl().font_semibold().child(title.into()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(description.into()),
        )
}

pub fn stat_card(
    title: impl Into<SharedString>,
    value: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .flex_1()
        .min_w(px(150.))
        .p_5()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(title.into()),
        )
        .child(div().text_3xl().font_semibold().child(value.into()))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(detail.into()),
        )
}

pub fn detail_row(label: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .gap_4()
        .text_sm()
        .child(
            div()
                .w(px(104.))
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(value.into()))
}

pub fn empty_state(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .w_full()
        .p_10()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(div().text_lg().font_medium().child(title.into()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(description.into()),
        )
}

pub fn section_panel(cx: &App) -> Div {
    div()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
}

pub fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}
