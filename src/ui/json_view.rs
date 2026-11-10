//! Read-only inspect JSON with structural (not substring-based) block navigation.
use crate::ui::button;
use gpui_kit::component::{
    button::ButtonVariants as _,
    input::{Editor, EditorState, Position},
    ActiveTheme as _, IconName, Sizable as _,
};
use gpui_kit::{
    div, point, prelude::FluentBuilder as _, px, AppContext as _, ClipboardItem, Context, Entity,
    IntoElement, ParentElement as _, Pixels, Render, Styled as _, Window,
};

#[derive(Debug, PartialEq, Eq)]
struct JsonBlock {
    label: &'static str,
    row: u32,
}

// Prefer actual inspect fields over similarly named values in Labels/Env.
const BLOCKS: &[(&str, &[&str])] = &[
    ("Platform", &["Platform"]),
    ("Cmd", &["Config", "Cmd"]),
    ("State", &["State"]),
    ("Image", &["Image"]),
    ("PortBindings", &["HostConfig", "PortBindings"]),
    ("Runtime", &["HostConfig", "Runtime"]),
    ("Mounts", &["Mounts"]),
    ("Volumes", &["Config", "Volumes"]),
    ("Env", &["Config", "Env"]),
    ("Labels", &["Config", "Labels"]),
    ("Networks", &["NetworkSettings", "Networks"]),
];

/// The input is pretty-printed JSON with two-space indentation. Track enclosing
/// keys by depth, decoding escaped property names with serde rather than regex.
fn json_blocks(json: &str) -> Vec<JsonBlock> {
    let mut paths = Vec::<String>::new();
    let mut found = vec![None; BLOCKS.len()];
    for (row, line) in json.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('"') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let depth = indent / 2;
        if depth == 0 || depth > paths.len() + 1 {
            continue;
        }
        // Decode only the property name, including escaped quotes/colons.
        let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<String>();
        let Some(Ok(name)) = stream.next() else {
            continue;
        };
        if !trimmed[stream.byte_offset()..].starts_with(':') {
            continue;
        }
        paths.truncate(depth - 1);
        paths.push(name);
        for (index, (_, path)) in BLOCKS.iter().enumerate() {
            if paths.iter().map(String::as_str).eq(path.iter().copied()) {
                found[index] = Some(row as u32);
            }
        }
    }
    BLOCKS
        .iter()
        .zip(found)
        .filter_map(|((label, _), row)| row.map(|row| JsonBlock { label, row }))
        .collect()
}

/// Center the whole target line, not its top edge. The editor clamps the
/// resulting offset to the document's valid scroll range during layout.
fn centered_row_offset(row: usize, line_height: Pixels, viewport_height: Pixels) -> Pixels {
    (viewport_height / 2. - line_height * (row as f32 + 0.5)).min(px(0.))
}

pub struct JsonView {
    json: String,
    blocks: Vec<JsonBlock>,
    editor: Option<Entity<EditorState>>,
    selected: Option<usize>,
}

impl JsonView {
    pub fn new(json: String) -> Self {
        let blocks = json_blocks(&json);
        Self {
            json,
            blocks,
            editor: None,
            selected: None,
        }
    }
}

impl Render for JsonView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self
            .editor
            .get_or_insert_with(|| {
                cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language("json")
                        .line_number(true)
                        .soft_wrap(false)
                        .folding(false)
                        .scroll_beyond_last_line(Some(0))
                        .default_value(self.json.clone())
                })
            })
            .clone();
        let mut navigation = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_3()
            .flex_shrink_0();
        for (index, block) in self.blocks.iter().enumerate() {
            let row = block.row;
            navigation = navigation.child(
                button(("inspect-block", index))
                    .outline()
                    .small()
                    .label(block.label)
                    .when(self.selected == Some(index), |button| button.primary())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = Some(index);
                        if let Some(editor) = &this.editor {
                            editor.update(cx, |state, cx| {
                                state.set_cursor_position(Position::new(row, 0), window, cx);
                                if let Some(line_height) = state.line_height() {
                                    let display_row = state
                                        .display_map()
                                        .buffer_line_to_display_row(row as usize);
                                    let y = centered_row_offset(
                                        display_row,
                                        line_height,
                                        state.input_bounds().size.height,
                                    );
                                    // Override the cursor's minimal reveal with a centered
                                    // viewport, without carrying over horizontal scrolling.
                                    state.set_scroll_offset(point(px(0.), y), cx);
                                }
                            });
                        }
                        cx.notify();
                    })),
            );
        }
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                navigation.child(
                    button("copy-container-inspect")
                        .ghost()
                        .small()
                        .icon(IconName::Copy)
                        .tooltip(t!("ui.copy_json"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(this.json.clone()));
                        })),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Editor::new(&editor).readonly(true).size_full()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_line_center_matches_the_viewport_center() {
        let line_height = px(20.);
        let viewport_height = px(400.);
        let offset = centered_row_offset(30, line_height, viewport_height);
        assert_eq!(offset, px(-410.));
        assert_eq!(offset + line_height * 30.5, viewport_height / 2.);
        assert_eq!(centered_row_offset(30, line_height, px(200.)), px(-510.));
    }

    #[test]
    fn centering_near_the_top_does_not_scroll_past_the_document_start() {
        assert_eq!(centered_row_offset(0, px(20.), px(400.)), px(0.));
        assert_eq!(centered_row_offset(5, px(20.), px(400.)), px(0.));
    }

    #[test]
    fn blocks_follow_paths_not_duplicate_keys_or_values() {
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "State": {"Status": "running"},
            "Image": "sha256:abc",
            "Config": {"Cmd": ["sh"], "Env": [], "Labels": {"State": "not a block", "Networks": "ignored"}},
            "HostConfig": {"PortBindings": {}, "Runtime": "runc"},
            "NetworkSettings": {"Networks": {"bridge": {}}}
        })).unwrap();
        let blocks = json_blocks(&json);
        for block in &blocks {
            let line = json.lines().nth(block.row as usize).unwrap();
            assert!(line
                .trim_start()
                .starts_with(&format!("\"{}\":", block.label)));
        }
        assert_eq!(
            blocks.iter().map(|block| block.label).collect::<Vec<_>>(),
            [
                "Cmd",
                "State",
                "Image",
                "PortBindings",
                "Runtime",
                "Env",
                "Labels",
                "Networks"
            ]
        );
        let state = blocks.iter().find(|block| block.label == "State").unwrap();
        assert!(json
            .lines()
            .nth(state.row as usize)
            .unwrap()
            .starts_with("  \"State\""));
    }

    #[test]
    fn absent_blocks_are_not_offered_and_null_fields_can_be_reached() {
        assert!(json_blocks("{}").is_empty());
        let blocks = json_blocks("{\n  \"Config\": {\n    \"Cmd\": null\n  }\n}");
        assert_eq!(
            blocks,
            [JsonBlock {
                label: "Cmd",
                row: 2
            }]
        );
    }
}
