//! Native terminal surface: VT parsing, cell painting and platform text/IME input.
use std::{num::NonZeroU16, ops::Range};

use crate::ui::button;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    button::ButtonVariants as _,
    input::{Input, InputEvent, InputState},
    Icon, IconName, Sizable as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    canvas, component::ActiveTheme as _, div, fill, point, px, rgb, size, App, AppContext as _,
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable as _, Font, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    Keystroke, MouseButton, ParentElement as _, Pixels, Point, Render, ScrollDelta, SharedString,
    Styled as _, TextAlign, TextRun, UTF16Selection, Window,
};
use tokio::sync::{mpsc, watch};

const FONT_SIZE: f32 = 14.;
const LINE_HEIGHT: f32 = 20.;
const PADDING: f32 = 12.;

#[derive(Default)]
struct TerminalReplies {
    pending: Vec<String>,
}

impl vt100::Callbacks for TerminalReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        prefix: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        code: char,
    ) {
        // Answer only terminal status queries. Never honor clipboard/host commands.
        if self.pending.len() >= 32 {
            return;
        }
        let param = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        let reply = match (prefix, code, param) {
            (None, 'n', 5) => "\x1b[0n".into(),
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            (None, 'c', 0) => "\x1b[?1;2c".into(),
            _ => return,
        };
        self.pending.push(reply);
    }
}

pub struct TerminalView {
    parser: vt100::Parser<TerminalReplies>,
    focus: FocusHandle,
    input: Option<mpsc::Sender<String>>,
    resize: Option<watch::Sender<(u16, u16)>>,
    composition: String,
    error: Option<String>,
    search_input: Option<gpui_kit::Entity<InputState>>,
    search_subscription: Option<gpui_kit::Subscription>,
    search_query: String,
    search_regex: bool,
    search_pattern: Option<regex::Regex>,
    search_error: Option<String>,
    search_matches: Vec<Range<usize>>,
    search_active: Option<usize>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                NonZeroU16::new(24).unwrap(),
                NonZeroU16::new(80).unwrap(),
                2000,
                TerminalReplies::default(),
            ),
            focus: cx.focus_handle(),
            input: None,
            resize: None,
            composition: String::new(),
            error: None,
            search_input: None,
            search_subscription: None,
            search_query: String::new(),
            search_regex: false,
            search_pattern: None,
            search_error: None,
            search_matches: Vec::new(),
            search_active: None,
            cursor_bounds: None,
        }
    }

    pub fn connect(
        &mut self,
        input: mpsc::Sender<String>,
        resize: watch::Sender<(u16, u16)>,
        cx: &mut Context<Self>,
    ) {
        let (rows, cols) = self.parser.screen().size();
        resize.send_replace((rows.get(), cols.get()));
        self.input = Some(input);
        self.resize = Some(resize);
        self.error = None;
        cx.notify();
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.input = None;
        self.resize = None;
        self.composition.clear();
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    pub fn process(&mut self, text: &str, cx: &mut Context<Self>) {
        self.parser.process(text.as_bytes());
        let replies = std::mem::take(&mut self.parser.callbacks_mut().pending);
        for reply in replies {
            self.send(reply, cx);
        }
        self.refresh_search();
        cx.notify();
    }

    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.is_some() {
            self.close_search(window, cx);
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("ui.terminal_search_placeholder"))
        });
        self.search_subscription =
            Some(
                cx.subscribe_in(&input, window, |this, input, event, _, cx| match event {
                    InputEvent::Change => {
                        this.search_query = input.read(cx).value().to_string();
                        this.compile_search();
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } => this.navigate_search(*shift, cx),
                    _ => {}
                }),
            );
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.search_input = Some(input);
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_subscription = None;
        self.search_input = None;
        self.search_query.clear();
        self.search_pattern = None;
        self.search_error = None;
        self.search_matches.clear();
        self.search_active = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn compile_search(&mut self) {
        self.search_active = None;
        self.search_error = None;
        self.search_pattern = match compile_search_pattern(&self.search_query, self.search_regex) {
            Ok(pattern) => pattern,
            Err(error) => {
                self.search_error = Some(error.to_string());
                None
            }
        };
        self.refresh_search();
        self.reveal_search_match();
    }

    fn refresh_search(&mut self) {
        self.search_matches = self
            .search_pattern
            .as_ref()
            .map_or_else(Vec::new, |pattern| {
                find_terminal_matches(self.parser.screen(), pattern)
            });
        self.search_active = if self.search_matches.is_empty() {
            None
        } else {
            Some(
                self.search_active
                    .unwrap_or(0)
                    .min(self.search_matches.len() - 1),
            )
        };
    }

    fn reveal_search_match(&mut self) {
        let Some(range) = self
            .search_active
            .and_then(|index| self.search_matches.get(index))
        else {
            return;
        };
        let screen = self.parser.screen_mut();
        let row = range.start / usize::from(screen.size().1.get());
        screen.set_scrollback(screen.scrollback_len().saturating_sub(row));
    }

    fn navigate_search(&mut self, previous: bool, cx: &mut Context<Self>) {
        let count = self.search_matches.len();
        if count == 0 {
            return;
        }
        let current = self.search_active.unwrap_or(0);
        self.search_active = Some(if previous {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        });
        self.reveal_search_match();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        // Clear the viewport without resetting terminal modes or cursor position.
        self.parser.process(b"\x1b[2J\x1b[3J");
        self.refresh_search();
        cx.notify();
    }

    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(input) = &self.input else {
            return;
        };
        if text.len() > 64 * 1024 {
            self.error = Some(t!("ui.paste_too_large").into_owned());
        } else if let Err(error) = input.try_send(text) {
            self.error = Some(t!("ui.terminal_input_not_sent", error = error).into_owned());
        } else {
            self.error = None;
            self.parser.screen_mut().set_scrollback(0);
        }
        cx.notify();
    }

    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = if self.parser.screen().bracketed_paste() {
            // Prevent clipboard text from escaping bracketed-paste mode.
            format!("\x1b[200~{}\x1b[201~", text.replace('\u{1b}', ""))
        } else {
            text.replace('\n', "\r")
        };
        self.send(text, cx);
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if key.modifiers.control && key.key == "f" {
            self.toggle_search(window, cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        let paste = (key.modifiers.control && key.modifiers.shift && key.key == "v")
            || (key.modifiers.platform && key.key == "v")
            || (key.modifiers.shift && key.key == "insert");
        if paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.paste_text(&text, cx);
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        // Printable text goes through EntityInputHandler, not key_down: this
        // avoids duplicate characters and preserves keyboard layouts and IME.
        if self.composition.is_empty() {
            if let Some(text) = key_sequence(key, self.parser.screen().application_cursor()) {
                self.send(text, cx);
                cx.stop_propagation();
                window.prevent_default();
            }
        }
    }

    fn resize_to(&mut self, rows: u16, cols: u16, cx: &mut Context<Self>) {
        let rows = NonZeroU16::new(rows.max(1)).unwrap();
        let cols = NonZeroU16::new(cols.max(1)).unwrap();
        if self.parser.screen().size() != (rows, cols) {
            self.parser.set_size(rows, cols);
            self.refresh_search();
            if self.search_input.is_some() {
                self.reveal_search_match();
            }
            if let Some(resize) = &self.resize {
                resize.send_replace((rows.get(), cols.get()));
            }
            cx.notify();
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let foreground = cx.theme().foreground;
        let background = cx.theme().background;
        let font = Font {
            family: cx.theme().mono_font_family.clone(),
            ..Default::default()
        };
        let surface = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .track_focus(&self.focus)
            .key_context("ContainerTerminal")
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus(window, cx)),
            )
            .on_scroll(cx.listener(|this, event: &gpui_kit::ScrollEvent, _, cx| {
                let delta = match event.delta {
                    ScrollDelta::Lines(lines) => lines.y,
                    ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / LINE_HEIGHT,
                };
                let screen = this.parser.screen_mut();
                let offset =
                    (screen.scrollback() as isize + (delta * 3.).round() as isize).max(0) as usize;
                screen.set_scrollback(offset);
                cx.stop_propagation();
                cx.notify();
            }))
            .children(self.error.as_ref().map(|error| {
                div()
                    .px_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone())
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let run = TextRun {
                            len: 1,
                            font: font.clone(),
                            color: foreground,
                            ..Default::default()
                        };
                        let width = window
                            .text_system()
                            .shape_line("M".into(), px(FONT_SIZE), &[run], None)
                            .width
                            .max(px(1.));
                        let rows = ((f32::from(bounds.size.height) - PADDING * 2.) / LINE_HEIGHT)
                            .floor()
                            .clamp(1., 300.) as u16;
                        let cols = ((f32::from(bounds.size.width) - PADDING * 2.)
                            / f32::from(width))
                        .floor()
                        .clamp(1., 500.) as u16;
                        if entity.read(cx).parser.screen().size()
                            != (
                                NonZeroU16::new(rows).unwrap(),
                                NonZeroU16::new(cols).unwrap(),
                            )
                        {
                            let entity = entity.clone();
                            cx.defer(move |cx| {
                                entity.update(cx, |this, cx| this.resize_to(rows, cols, cx))
                            });
                        }
                        (width, font)
                    },
                    move |bounds, (width, font), window, cx| {
                        let terminal = paint_entity.read(cx);
                        let screen = terminal.parser.screen().clone();
                        let focus = terminal.focus.clone();
                        let cursor = screen.cursor_position();
                        let origin = bounds.origin + point(px(PADDING), px(PADDING));
                        let cursor_bounds = Bounds::new(
                            origin
                                + point(width * cursor.1 as f32, px(LINE_HEIGHT * cursor.0 as f32)),
                            size(width, px(LINE_HEIGHT)),
                        );
                        let focused = focus.is_focused(window);
                        let composition = terminal.composition.clone();
                        let search_matches = terminal.search_matches.clone();
                        let search_active = terminal.search_active;
                        let (rows, cols) = screen.size();
                        for row in 0..rows.get() {
                            for col in 0..cols.get() {
                                let Some(cell) = screen.cell(row, col) else {
                                    continue;
                                };
                                if cell.is_wide_continuation() {
                                    continue;
                                }
                                let cell_width = width * if cell.is_wide() { 2. } else { 1. };
                                let position = origin
                                    + point(width * col as f32, px(LINE_HEIGHT * row as f32));
                                let mut fg = terminal_color(cell.fgcolor(), foreground);
                                let mut bg = terminal_color(cell.bgcolor(), background);
                                if cell.inverse() {
                                    std::mem::swap(&mut fg, &mut bg);
                                }
                                let position_index = (screen.scrollback_len()
                                    - screen.scrollback()
                                    + usize::from(row))
                                    * usize::from(cols.get())
                                    + usize::from(col);
                                let search_match = search_matches
                                    .partition_point(|range| range.end <= position_index);
                                if search_matches
                                    .get(search_match)
                                    .is_some_and(|range| range.contains(&position_index))
                                {
                                    let active = search_active == Some(search_match);
                                    bg = if active {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().accent
                                    };
                                    fg = if active {
                                        cx.theme().primary_foreground
                                    } else {
                                        cx.theme().accent_foreground
                                    };
                                }
                                let caret = focused
                                    && screen.scrollback() == 0
                                    && !screen.hide_cursor()
                                    && cursor == (row, col);
                                if caret {
                                    std::mem::swap(&mut fg, &mut bg);
                                }
                                if bg != background {
                                    window.paint_quad(fill(
                                        Bounds::new(position, size(cell_width, px(LINE_HEIGHT))),
                                        bg,
                                    ));
                                }
                                if cell.has_contents() {
                                    let text: SharedString = cell.contents().to_string().into();
                                    let mut cell_font = font.clone();
                                    if cell.bold() {
                                        cell_font.weight = FontWeight::BOLD;
                                    }
                                    let run = TextRun {
                                        len: text.len(),
                                        font: cell_font,
                                        color: fg,
                                        ..Default::default()
                                    };
                                    let line = window.text_system().shape_line(
                                        text,
                                        px(FONT_SIZE),
                                        &[run],
                                        Some(width),
                                    );
                                    _ = line.paint(
                                        position,
                                        px(LINE_HEIGHT),
                                        TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }
                        if !composition.is_empty() {
                            let run = TextRun {
                                len: composition.len(),
                                font,
                                color: foreground,
                                ..Default::default()
                            };
                            let line = window.text_system().shape_line(
                                composition.into(),
                                px(FONT_SIZE),
                                &[run],
                                Some(width),
                            );
                            window.paint_quad(fill(
                                Bounds::new(
                                    cursor_bounds.origin,
                                    size(line.width, px(LINE_HEIGHT)),
                                ),
                                background,
                            ));
                            _ = line.paint(
                                cursor_bounds.origin,
                                px(LINE_HEIGHT),
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, paint_entity.clone()),
                            cx,
                        );
                        paint_entity.update(cx, |terminal, _| {
                            terminal.cursor_bounds = Some(cursor_bounds)
                        });
                    },
                )
                .size_full(),
            );
        let search_panel = self.search_input.clone().map(|input| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .w(px(480.))
                .max_w_full()
                .occlude()
                .bg(background)
                .border_1()
                .border_color(cx.theme().border)
                .shadow_md()
                .flex()
                .flex_col()
                .items_end()
                .child(
                    div()
                        .w(px(480.))
                        .max_w_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .p_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                this.close_search(window, cx);
                                cx.stop_propagation();
                                window.prevent_default();
                            }
                        }))
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&input)
                                    .small()
                                    .prefix(Icon::new(IconName::Search).small()),
                            ),
                        )
                        .child(
                            button("terminal-search-regex")
                                .small()
                                .label("Reg")
                                .when(self.search_regex, |button| button.primary())
                                .when(!self.search_regex, |button| button.ghost())
                                .tooltip(t!("ui.regex"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.search_regex = !this.search_regex;
                                    this.compile_search();
                                    if let Some(input) = &this.search_input {
                                        window.focus(&input.read(cx).focus_handle(cx), cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{}/{}",
                                    self.search_active.map_or(0, |index| index + 1),
                                    self.search_matches.len()
                                )),
                        )
                        .child(
                            button("terminal-search-previous")
                                .ghost()
                                .small()
                                .icon(IconName::ChevronLeft)
                                .tooltip(t!("ui.previous_match"))
                                .disabled(self.search_matches.is_empty())
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.navigate_search(true, cx)),
                                ),
                        )
                        .child(
                            button("terminal-search-next")
                                .ghost()
                                .small()
                                .icon(IconName::ChevronRight)
                                .tooltip(t!("ui.next_match"))
                                .disabled(self.search_matches.is_empty())
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.navigate_search(false, cx)),
                                ),
                        )
                        .child(
                            button("terminal-search-close")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .tooltip(t!("ui.close_search"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.close_search(window, cx)
                                    }),
                                ),
                        ),
                )
                .when_some(self.search_error.clone(), |view, error| {
                    view.child(
                        div()
                            .w(px(480.))
                            .max_w_full()
                            .px_2()
                            .pb_2()
                            .text_xs()
                            .text_color(cx.theme().danger)
                            .child(t!("ui.invalid_regex", error = error).into_owned()),
                    )
                })
        });
        div()
            .relative()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_w_0().child(surface))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_1()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .child(
                                button("terminal-search")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Search)
                                    .tooltip(t!("ui.search_ctrl_f"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_search(window, cx)
                                    })),
                            )
                            .child(
                                button("terminal-copy")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Copy)
                                    .tooltip(t!("ui.copy_output"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            this.contents(),
                                        ))
                                    })),
                            )
                            .child(
                                button("terminal-clear")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Delete)
                                    .tooltip(t!("ui.clear_screen"))
                                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                            ),
                    ),
            )
            .children(search_panel)
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        *actual = Some(0..self.composition.encode_utf16().count());
        Some(self.composition.clone())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.composition.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.composition.is_empty()).then(|| 0..self.composition.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.composition.clear();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composition.clear();
        self.send(text.into(), cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composition = text.into();
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.input.is_some()
    }
    fn paste(&mut self, item: ClipboardItem, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.paste_text(&text, cx);
        }
    }
}

fn compile_search_pattern(query: &str, regex: bool) -> Result<Option<regex::Regex>, regex::Error> {
    if query.is_empty() {
        return Ok(None);
    }
    let pattern = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    regex::RegexBuilder::new(&pattern)
        .multi_line(true)
        .build()
        .map(Some)
}

fn find_terminal_matches(screen: &vt100::Screen, pattern: &regex::Regex) -> Vec<Range<usize>> {
    let mut screen = screen.clone();
    let (rows, cols) = screen.size();
    let rows = usize::from(rows.get());
    let cols = usize::from(cols.get());
    let history = screen.scrollback_len();
    let total = history + rows;
    let mut text = String::new();
    let mut positions = Vec::new();
    let mut start = 0;
    while start < total {
        let offset = history.saturating_sub(start);
        screen.set_scrollback(offset);
        let viewport_start = history - offset;
        for row in start - viewport_start..rows {
            // Omit unused right-hand padding so regex anchors match logical line ends.
            let row_cols = if screen.row_wrapped(row as u16) {
                cols
            } else {
                (0..cols)
                    .rfind(|col| {
                        screen
                            .cell(row as u16, *col as u16)
                            .is_some_and(|cell| cell.has_contents() || cell.is_wide_continuation())
                    })
                    .map_or(0, |col| col + 1)
            };
            for col in 0..row_cols {
                let cell = screen.cell(row as u16, col as u16).unwrap();
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                let value = if contents.is_empty() { " " } else { contents };
                let position = (viewport_start + row) * cols + col;
                let end = position + if cell.is_wide() { 2 } else { 1 };
                positions.extend(std::iter::repeat_n(position..end, value.len()));
                text.push_str(value);
            }
            if !screen.row_wrapped(row as u16) {
                text.push('\n');
                let end = (viewport_start + row + 1) * cols;
                positions.push(end..end);
            }
        }
        start = viewport_start + rows;
    }
    pattern
        .find_iter(&text)
        // Zero-width matches have no terminal cell to highlight or navigate to.
        .filter(|matched| !matched.is_empty())
        .map(|matched| positions[matched.start()].start..positions[matched.end() - 1].end)
        .collect()
}

fn key_sequence(key: &Keystroke, application: bool) -> Option<String> {
    if key.modifiers.platform {
        return None;
    }
    let value = if key.modifiers.control && key.key.len() == 1 {
        let byte = key.key.as_bytes()[0].to_ascii_uppercase();
        match byte {
            b'@'..=b'_' => ((byte & 0x1f) as char).to_string(),
            b' ' => "\0".into(),
            _ => return None,
        }
    } else {
        match key.key.as_str() {
            "enter" => "\r",
            "backspace" => "\x7f",
            "escape" => "\x1b",
            "tab" if key.modifiers.shift => "\x1b[Z",
            "tab" => "\t",
            "up" if application => "\x1bOA",
            "down" if application => "\x1bOB",
            "right" if application => "\x1bOC",
            "left" if application => "\x1bOD",
            "up" => "\x1b[A",
            "down" => "\x1b[B",
            "right" => "\x1b[C",
            "left" => "\x1b[D",
            "home" => "\x1b[H",
            "end" => "\x1b[F",
            "delete" => "\x1b[3~",
            "insert" => "\x1b[2~",
            "pageup" => "\x1b[5~",
            "pagedown" => "\x1b[6~",
            "f1" => "\x1bOP",
            "f2" => "\x1bOQ",
            "f3" => "\x1bOR",
            "f4" => "\x1bOS",
            "f5" => "\x1b[15~",
            "f6" => "\x1b[17~",
            "f7" => "\x1b[18~",
            "f8" => "\x1b[19~",
            "f9" => "\x1b[20~",
            "f10" => "\x1b[21~",
            "f11" => "\x1b[23~",
            "f12" => "\x1b[24~",
            _ if key.modifiers.alt => {
                return key.key_char.as_ref().map(|text| format!("\x1b{text}"))
            }
            _ => return None,
        }
        .into()
    };
    Some(value)
}

fn terminal_color(color: vt100::Color, default: Hsla) -> Hsla {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => {
            rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
        }
        vt100::Color::Idx(index) => {
            const ANSI: [u32; 16] = [
                0x1e1e1e, 0xcd3131, 0x0dbc79, 0xe5b567, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5,
                0x666666, 0xf14c4c, 0x23d18b, 0xf5d76e, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
            ];
            let value = match index {
                0..=15 => ANSI[index as usize],
                16..=231 => {
                    let n = u32::from(index - 16);
                    let ramp = [0, 95, 135, 175, 215, 255];
                    (ramp[(n / 36) as usize] << 16)
                        | (ramp[((n / 6) % 6) as usize] << 8)
                        | ramp[(n % 6) as usize]
                }
                _ => {
                    let gray = 8 + u32::from(index - 232) * 10;
                    (gray << 16) | (gray << 8) | gray
                }
            };
            rgb(value).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_literal_matches(screen: &vt100::Screen, query: &str) -> Vec<Range<usize>> {
        compile_search_pattern(query, false)
            .unwrap()
            .map_or_else(Vec::new, |pattern| find_terminal_matches(screen, &pattern))
    }

    #[test]
    fn regex_search_handles_anchors_unicode_and_invalid_patterns() {
        let mut parser = vt100::Parser::default();
        parser.process("foo123\r\n你好42\r\nfoo.456".as_bytes());
        let pattern = compile_search_pattern(r"^foo\d+$", true).unwrap().unwrap();
        assert_eq!(
            find_terminal_matches(parser.screen(), &pattern),
            std::slice::from_ref(&(0..6))
        );
        let pattern = compile_search_pattern(r"你好\d+", true).unwrap().unwrap();
        assert_eq!(
            find_terminal_matches(parser.screen(), &pattern),
            std::slice::from_ref(&(80..86))
        );
        assert_eq!(
            find_literal_matches(parser.screen(), "foo."),
            std::slice::from_ref(&(160..164))
        );
        assert!(compile_search_pattern("[", true).is_err());
        assert!(compile_search_pattern("", true).unwrap().is_none());
        let pattern = compile_search_pattern("^", true).unwrap().unwrap();
        assert!(find_terminal_matches(parser.screen(), &pattern).is_empty());
    }
    #[test]
    fn search_maps_unicode_and_wide_cells_to_terminal_columns() {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(3).unwrap(),
            NonZeroU16::new(20).unwrap(),
            10,
        );
        parser.process("a你好a你好".as_bytes());
        assert_eq!(find_literal_matches(parser.screen(), "你好"), [1..5, 6..10]);
        assert!(find_literal_matches(parser.screen(), "").is_empty());
        assert!(find_literal_matches(parser.screen(), "missing").is_empty());
    }

    #[test]
    fn search_includes_history_without_changing_the_viewport() {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(2).unwrap(),
            NonZeroU16::new(10).unwrap(),
            10,
        );
        parser.process(b"old\r\nnext\r\nlast");
        assert_eq!(parser.screen().scrollback_len(), 1);
        assert_eq!(
            find_literal_matches(parser.screen(), "old"),
            std::slice::from_ref(&(0..3))
        );
        assert_eq!(
            find_literal_matches(parser.screen(), "last"),
            std::slice::from_ref(&(20..24))
        );
        assert_eq!(parser.screen().scrollback(), 0);
        parser.screen_mut().set_scrollback(1);
        assert_eq!(
            find_literal_matches(parser.screen(), "last"),
            std::slice::from_ref(&(20..24))
        );
        assert_eq!(parser.screen().scrollback(), 1);
    }

    #[test]
    fn search_crosses_soft_wraps_but_not_hard_line_breaks() {
        let mut parser =
            vt100::Parser::new(NonZeroU16::new(3).unwrap(), NonZeroU16::new(5).unwrap(), 10);
        parser.process(b"hello world");
        assert_eq!(
            find_literal_matches(parser.screen(), "lo wo"),
            std::slice::from_ref(&(3..8))
        );
        let mut parser = vt100::Parser::default();
        parser.process(b"hello\r\nworld");
        assert!(find_literal_matches(parser.screen(), "helloworld").is_empty());
        parser.process(b"\x1b[2J\x1b[3J");
        assert!(find_literal_matches(parser.screen(), "hello").is_empty());
    }

    #[test]
    fn keys_encode_terminal_controls() {
        for (key, expected) in [
            ("ctrl-c", "\x03"),
            ("ctrl-d", "\x04"),
            ("backspace", "\x7f"),
            ("enter", "\r"),
            ("up", "\x1b[A"),
            ("tab", "\t"),
        ] {
            assert_eq!(
                key_sequence(&Keystroke::parse(key).unwrap(), false).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            key_sequence(&Keystroke::parse("up").unwrap(), true).as_deref(),
            Some("\x1bOA")
        );
        assert!(key_sequence(&Keystroke::parse("a").unwrap(), false).is_none());
    }
    #[test]
    fn terminal_status_queries_are_answered_without_executing_host_commands() {
        let mut parser = vt100::Parser::new_with_callbacks(
            NonZeroU16::new(24).unwrap(),
            NonZeroU16::new(80).unwrap(),
            0,
            TerminalReplies::default(),
        );
        parser.process(b"abc\x1b[6n\x1b[5n\x1b[c\x1b]52;c;c2VjcmV0\x07");
        assert_eq!(
            parser.callbacks().pending,
            ["\x1b[1;4R", "\x1b[0n", "\x1b[?1;2c"]
        );
    }

    #[test]
    fn terminal_handles_redraw_and_alternate_screen() {
        let mut parser = vt100::Parser::default();
        parser.process(b"old\rnew\x1b[K");
        assert_eq!(parser.screen().contents(), "new");
        parser.process(b"\x1b[?1049hfull screen\x1b[?1049l");
        assert_eq!(parser.screen().contents(), "new");
    }
}
