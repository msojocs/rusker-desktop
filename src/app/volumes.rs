//! Volumes page: list, create, clone, delete (single, batch, unused), and a
//! detail view that browses the stored data, imports, exports or empties it.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use gpui_kit::component::{
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::Input,
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div,
    prelude::FluentBuilder as _,
    px, AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window,
};
use rusker_desktop::{
    engine::EngineError,
    format::{format_bytes, format_timestamp},
    model::{Revision, VolumeEntry, VolumeEntryKind, VolumeSummary},
    volume::{
        clone_name, export_file_name, join_relative, unused_volumes, validate_volume_name,
        volume_matches, DirectoryListing, HELPER_IMAGE, MAX_DIRECTORY_ENTRIES,
    },
};
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use super::{
    await_engine, brand_color, form_field, list_action_button, Page, PointerCursor as _, RuskerApp,
};
use crate::ui::{detail_row, empty_state, muted, page_header, section_panel};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum VolumeTab {
    #[default]
    Data,
    Containers,
    Exports,
}

enum DirState {
    Loading,
    Loaded(DirectoryListing),
    Failed(String),
}

/// A finished export; kept for the current session only.
struct ExportRecord {
    volume: String,
    path: PathBuf,
    size: u64,
    finished: i64,
}

#[derive(Default)]
pub(super) struct VolumeState {
    sizes: HashMap<String, u64>,
    sizes_loading: bool,
    sizes_error: Option<String>,
    sizes_revision: Revision,
    sizes_task: Option<Task<()>>,
    /// Volume whose detail view is open.
    selected: Option<String>,
    tab: VolumeTab,
    /// Directory listings keyed by path relative to the volume root.
    tree: HashMap<String, DirState>,
    expanded: HashSet<String>,
    tree_revision: Revision,
    tree_tasks: HashMap<String, Task<()>>,
    dialog_error: Option<String>,
    prompt_task: Option<Task<()>>,
    /// Volumes ticked in the list for batch deletion.
    checked: HashSet<String>,
    exports: Vec<ExportRecord>,
}

enum TreeRow {
    Entry {
        path: String,
        entry: VolumeEntry,
        depth: usize,
        expanded: bool,
    },
    Message {
        depth: usize,
        text: String,
        is_error: bool,
    },
}

const NAME_COLUMN_MIN: f32 = 240.;
const SIZE_COLUMN: f32 = 110.;
const DATE_COLUMN: f32 = 190.;
const MODE_COLUMN: f32 = 120.;
const INDENT: f32 = 20.;

impl RuskerApp {
    // ---- state and background loading -------------------------------------

    /// Called after every successful snapshot: refreshes sizes and the open
    /// volume's tree while the Volumes page is visible.
    pub(super) fn after_volume_snapshot(&mut self, cx: &mut Context<Self>) {
        if self.page != Page::Volumes {
            return;
        }
        self.load_volume_sizes(cx);
        if self.volumes.selected.is_some() {
            self.reload_volume_tree(cx);
        }
    }

    /// Drops the detail view when its volume disappeared from the engine.
    pub(super) fn prune_volume_selection(&mut self) {
        let existing: HashSet<&str> = self
            .snapshot
            .volumes
            .iter()
            .map(|volume| volume.name.as_str())
            .collect();
        self.volumes
            .checked
            .retain(|name| existing.contains(name.as_str()));
        let Some(selected) = self.volumes.selected.as_ref() else {
            return;
        };
        if !existing.contains(selected.as_str()) {
            self.close_volume();
        }
    }

    /// Like `run_snapshot_operation`, for volume operations whose result is
    /// not just a snapshot. `on_done` runs on the UI thread on success.
    fn run_volume_task<T: Send + 'static>(
        &mut self,
        label: String,
        handle: JoinHandle<Result<T, EngineError>>,
        on_done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        // Invalidate an in-flight refresh so it cannot overwrite our result.
        self.refresh_revision.advance();
        self.busy = Some(format!("{label}…"));
        self.loading = false;
        self.error = None;
        self.notice = None;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(value) => on_done(this, value, cx),
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn confirm_remove_volumes(
        &mut self,
        names: Vec<String>,
        unused_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if names.is_empty() || self.busy.is_some() {
            return;
        }
        let in_use = names
            .iter()
            .filter(|name| self.snapshot.volume_in_use(name))
            .count();
        let mut listed: Vec<&str> = names.iter().map(String::as_str).take(8).collect();
        if names.len() > listed.len() {
            listed.push("…");
        }
        let mut description = if unused_only {
            "以下卷未被任何容器（包括已停止的容器）挂载，将被永久删除：\n".to_string()
        } else {
            "以下卷及其数据将被永久删除，此操作不可撤销：\n".to_string()
        };
        description.push_str(&listed.join("\n"));
        if in_use > 0 {
            description.push_str(&format!(
                "\n其中 {in_use} 个卷正被容器挂载，Engine 会拒绝删除它们，其余卷照常删除。"
            ));
        }
        let title = if unused_only {
            format!("清理 {} 个未使用的卷？", names.len())
        } else {
            format!("删除 {} 个卷？", names.len())
        };
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let names = names.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("删除")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let names = names.clone();
                    _ = weak.update(cx, |this, cx| this.remove_volumes(names, cx));
                    true
                })
        });
    }

    fn remove_volumes(&mut self, names: Vec<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let total = names.len();
        let handle = engine.remove_volumes(names);
        self.run_volume_task(
            format!("删除 {total} 个卷"),
            handle,
            move |this, (snapshot, failures), cx| {
                this.connected = true;
                this.apply_snapshot(snapshot);
                if failures.is_empty() {
                    this.notice = Some(format!("已删除 {total} 个卷"));
                } else {
                    this.error = Some(format!(
                        "已删除 {} 个卷，{} 个失败：\n{}",
                        total - failures.len(),
                        failures.len(),
                        failures.join("\n")
                    ));
                }
                this.after_volume_snapshot(cx);
            },
            cx,
        );
    }

    /// Asks where to save the export, then streams the volume into it.
    fn prompt_export_volume(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let directory = directories::UserDirs::new()
            .and_then(|dirs| {
                dirs.download_dir()
                    .map(Path::to_path_buf)
                    .or_else(|| Some(dirs.home_dir().to_path_buf()))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = export_file_name(&name, OffsetDateTime::now_utc());
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        self.volumes.prompt_task = Some(cx.spawn_in(window, async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(path)) => path,
                Ok(Err(error)) => {
                    _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("无法打开保存对话框：{error}"));
                        cx.notify();
                    });
                    None
                }
                Err(_) => None,
            };
            if let Some(path) = path {
                _ = this.update(cx, |this, cx| this.export_volume(name, path, cx));
            }
        }));
    }

    fn export_volume(&mut self, name: String, path: PathBuf, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let handle = engine.export_volume(name.clone(), path.clone());
        self.run_volume_task(
            format!("导出卷 {name}"),
            handle,
            move |this, size, _| {
                this.notice = Some(format!(
                    "已导出卷 {name} 到 {}（{}）",
                    path.display(),
                    format_bytes(size)
                ));
                this.volumes.exports.insert(
                    0,
                    ExportRecord {
                        volume: name,
                        path,
                        size,
                        finished: OffsetDateTime::now_utc().unix_timestamp(),
                    },
                );
            },
            cx,
        );
    }

    fn load_volume_sizes(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let revision = self.volumes.sizes_revision.advance();
        self.volumes.sizes_loading = true;
        cx.notify();
        let handle = engine.volume_sizes();
        self.volumes.sizes_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.volumes.sizes_revision.is_current(revision) {
                    return;
                }
                this.volumes.sizes_loading = false;
                match result {
                    Ok(sizes) => {
                        this.volumes.sizes = sizes;
                        this.volumes.sizes_error = None;
                    }
                    Err(error) => this.volumes.sizes_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn open_volume(&mut self, name: String, cx: &mut Context<Self>) {
        self.volumes.selected = Some(name);
        self.volumes.tab = VolumeTab::Data;
        self.volumes.expanded.clear();
        self.reload_volume_tree(cx);
    }

    fn close_volume(&mut self) {
        self.volumes.selected = None;
        self.volumes.tree_revision.advance();
        self.volumes.tree.clear();
        self.volumes.tree_tasks.clear();
        self.volumes.expanded.clear();
    }

    /// Re-reads the root and every expanded directory of the open volume.
    fn reload_volume_tree(&mut self, cx: &mut Context<Self>) {
        self.volumes.tree_revision.advance();
        self.volumes.tree.clear();
        self.volumes.tree_tasks.clear();
        let mut directories: Vec<String> = self.volumes.expanded.iter().cloned().collect();
        directories.sort();
        self.load_volume_directory(String::new(), cx);
        for directory in directories {
            self.load_volume_directory(directory, cx);
        }
    }

    fn load_volume_directory(&mut self, directory: String, cx: &mut Context<Self>) {
        let (Some(engine), Some(name)) = (self.engine.clone(), self.volumes.selected.clone())
        else {
            return;
        };
        let revision = self.volumes.tree_revision.current();
        self.volumes
            .tree
            .insert(directory.clone(), DirState::Loading);
        cx.notify();
        let handle = engine.list_volume_directory(name, directory.clone());
        let key = directory.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.volumes.tree_revision.is_current(revision) {
                    return;
                }
                let state = match result {
                    Ok(listing) => DirState::Loaded(listing),
                    Err(error) => DirState::Failed(error.to_string()),
                };
                this.volumes.tree.insert(directory, state);
                cx.notify();
            });
        });
        self.volumes.tree_tasks.insert(key, task);
    }

    fn toggle_volume_directory(&mut self, path: String, cx: &mut Context<Self>) {
        if self.volumes.expanded.remove(&path) {
            cx.notify();
            return;
        }
        self.volumes.expanded.insert(path.clone());
        match self.volumes.tree.get(&path) {
            Some(DirState::Loading | DirState::Loaded(_)) => cx.notify(),
            _ => self.load_volume_directory(path, cx),
        }
    }

    // ---- operations -------------------------------------------------------

    fn open_create_volume_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.volumes.dialog_error = None;
        self.volume_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        let input = self.volume_name_input.clone();
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let error = weak
                .upgrade()
                .and_then(|this| this.read(cx).volumes.dialog_error.clone());
            let submit = weak.clone();
            dialog
                .title("创建卷")
                .w(px(480.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(form_field(
                            "卷名称",
                            &input,
                            "留空则由 Engine 生成随机名称。可包含字母、数字、下划线、点和连字符。",
                            cx,
                        ))
                        .when_some(error, |this, error| this.child(dialog_error(error, cx))),
                )
                .footer(dialog_footer(
                    "submit-create-volume",
                    "创建",
                    IconName::Plus,
                    move |window, cx| {
                        _ = submit.update(cx, |this, cx| this.submit_create_volume(window, cx));
                    },
                ))
        });
    }

    fn submit_create_volume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let name = self.volume_name_input.read(cx).value().trim().to_string();
        let name = (!name.is_empty()).then_some(name);
        if let Some(name) = &name {
            if let Err(error) = validate_volume_name(name) {
                self.volumes.dialog_error = Some(error);
                cx.notify();
                return;
            }
            if self
                .snapshot
                .volumes
                .iter()
                .any(|volume| &volume.name == name)
            {
                self.volumes.dialog_error = Some(format!("卷 {name} 已存在"));
                cx.notify();
                return;
            }
        }
        let Some(engine) = self.engine.clone() else {
            self.volumes.dialog_error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        window.close_dialog(cx);
        let label = match &name {
            Some(name) => format!("创建卷 {name}"),
            None => "创建卷".into(),
        };
        let handle = engine.create_volume(name);
        self.run_snapshot_operation(label, handle, cx);
    }

    fn open_clone_volume_dialog(
        &mut self,
        source: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.volumes.dialog_error = None;
        let suggestion = clone_name(&source, &self.snapshot.volumes);
        self.volume_name_input
            .update(cx, |state, cx| state.set_value(suggestion, window, cx));
        let in_use = !self.snapshot.volume_users(&source).is_empty();
        let input = self.volume_name_input.clone();
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let error = weak
                .upgrade()
                .and_then(|this| this.read(cx).volumes.dialog_error.clone());
            let submit = weak.clone();
            let submit_source = source.clone();
            let mut hint = format!(
                "创建新卷并通过临时 {HELPER_IMAGE} 容器复制全部数据，保留属主、权限和时间戳。"
            );
            if in_use {
                hint.push_str("该卷正被容器使用，建议先停止相关容器以保证数据一致。");
            }
            dialog
                .title(format!("克隆卷 {source}"))
                .w(px(520.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(form_field(
                            "新卷名称 *",
                            &input,
                            "可包含字母、数字、下划线、点和连字符。",
                            cx,
                        ))
                        .child(muted(hint, cx))
                        .when_some(error, |this, error| this.child(dialog_error(error, cx))),
                )
                .footer(dialog_footer(
                    "submit-clone-volume",
                    "克隆",
                    IconName::Copy,
                    move |window, cx| {
                        let source = submit_source.clone();
                        _ = submit
                            .update(cx, |this, cx| this.submit_clone_volume(source, window, cx));
                    },
                ))
        });
    }

    fn submit_clone_volume(&mut self, source: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let target = self.volume_name_input.read(cx).value().trim().to_string();
        if let Err(error) = validate_volume_name(&target) {
            self.volumes.dialog_error = Some(error);
            cx.notify();
            return;
        }
        if self
            .snapshot
            .volumes
            .iter()
            .any(|volume| volume.name == target)
        {
            self.volumes.dialog_error = Some(format!("卷 {target} 已存在"));
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.volumes.dialog_error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        window.close_dialog(cx);
        let label = format!("克隆卷 {source} 为 {target}");
        let handle = engine.clone_volume(source, target);
        self.run_snapshot_operation(label, handle, cx);
    }

    fn confirm_remove_volume(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let users = self.snapshot.volume_users(&name).len();
        let description = if users > 0 {
            format!(
                "该卷正被 {users} 个容器挂载（含已停止的容器）。Engine 会拒绝删除仍被引用的卷，请先删除这些容器。"
            )
        } else {
            "此操作不可撤销，卷中的全部数据将被永久删除。".to_string()
        };
        let title = format!("删除卷 {name}？");
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let name = name.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("删除")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let name = name.clone();
                    _ = weak.update(cx, |this, cx| this.remove_volume(name, cx));
                    true
                })
        });
    }

    fn remove_volume(&mut self, name: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let label = format!("删除卷 {name}");
        let handle = engine.remove_volume(name);
        self.run_snapshot_operation(label, handle, cx);
    }

    fn confirm_empty_volume(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let running = self
            .snapshot
            .volume_users(&name)
            .iter()
            .filter(|container| container.is_running())
            .count();
        let mut description =
            "将永久删除卷内的全部文件和目录，卷本身保留。此操作不可撤销。".to_string();
        if running > 0 {
            description.push_str(&format!(
                "\n该卷正被 {running} 个运行中的容器使用，清空可能导致这些容器异常，建议先停止。"
            ));
        }
        let title = format!("清空卷 {name}？");
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let name = name.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("清空")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let name = name.clone();
                    _ = weak.update(cx, |this, cx| this.empty_volume(name, cx));
                    true
                })
        });
    }

    fn empty_volume(&mut self, name: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let label = format!("清空卷 {name}");
        let handle = engine.empty_volume(name);
        self.run_snapshot_operation(label, handle, cx);
    }

    /// Asks for a local archive, then confirms the import.
    fn prompt_import_volume(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("导入".into()),
        });
        self.volumes.prompt_task = Some(cx.spawn_in(window, async move |this, cx| {
            let path = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(error)) => {
                    _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("无法打开文件选择器：{error}"));
                        cx.notify();
                    });
                    None
                }
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                this.confirm_import_volume(name, path, window, cx);
            });
        }));
    }

    fn confirm_import_volume(
        &mut self,
        name: String,
        archive: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let description = format!(
            "将 {} 解压到卷根目录。支持 tar、tar.gz、tar.bz2 和 tar.xz；同名文件会被覆盖，其他已有文件保留；归档中的属主与权限会保留。",
            archive.display()
        );
        let title = format!("导入数据到卷 {name}？");
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let name = name.clone();
            let archive = archive.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("导入")
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let name = name.clone();
                    let archive = archive.clone();
                    _ = weak.update(cx, |this, cx| this.import_volume(name, archive, cx));
                    true
                })
        });
    }

    fn import_volume(&mut self, name: String, archive: PathBuf, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let label = format!("导入数据到卷 {name}");
        let handle = engine.import_volume(name, archive);
        self.run_snapshot_operation(label, handle, cx);
    }

    // ---- rendering --------------------------------------------------------

    pub(super) fn render_volumes(&self, cx: &mut Context<Self>) -> Div {
        let selected = self.volumes.selected.as_ref().and_then(|name| {
            self.snapshot
                .volumes
                .iter()
                .find(|volume| &volume.name == name)
        });
        match selected {
            Some(volume) => self.render_volume_detail(volume, cx),
            None => self.render_volume_list(cx),
        }
    }

    fn volume_size_label(&self, name: &str) -> String {
        match self.volumes.sizes.get(name) {
            Some(size) => format_bytes(*size),
            None if self.volumes.sizes_loading => "统计中…".into(),
            None => "—".into(),
        }
    }

    fn render_volume_list(&self, cx: &mut Context<Self>) -> Div {
        let query = self.search_input.read(cx).value().to_string();
        let volumes: Vec<&VolumeSummary> = self
            .snapshot
            .volumes
            .iter()
            .filter(|volume| volume_matches(volume, &query))
            .collect();
        let busy = self.busy.is_some();
        let visible_names: Vec<String> = volumes.iter().map(|volume| volume.name.clone()).collect();
        let all_checked = !visible_names.is_empty()
            && visible_names
                .iter()
                .all(|name| self.volumes.checked.contains(name));
        let mut checked: Vec<String> = self.volumes.checked.iter().cloned().collect();
        checked.sort();
        let unused: Vec<String> = unused_volumes(&self.snapshot.volumes, &self.snapshot.containers)
            .into_iter()
            .map(|volume| volume.name.clone())
            .collect();

        let header = div()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .text_sm()
            .font_semibold()
            .text_color(cx.theme().muted_foreground)
            .child(
                div().w(px(32.)).flex_shrink_0().child(
                    Checkbox::new("check-all-volumes")
                        .pointer(!visible_names.is_empty())
                        .checked(all_checked)
                        .tooltip("全选当前列表")
                        .disabled(visible_names.is_empty())
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            for name in &visible_names {
                                if *checked {
                                    this.volumes.checked.insert(name.clone());
                                } else {
                                    this.volumes.checked.remove(name);
                                }
                            }
                            cx.notify();
                        })),
                ),
            )
            .child(div().w(px(28.)).flex_shrink_0())
            .child(div().flex_1().min_w_0().child("名称"))
            .child(div().w(px(DATE_COLUMN)).flex_shrink_0().child("创建时间"))
            .child(
                div()
                    .w(px(SIZE_COLUMN))
                    .flex_shrink_0()
                    .text_right()
                    .pr_4()
                    .child("大小"),
            )
            .child(div().w(px(92.)).flex_shrink_0().child("操作"));

        let mut panel = div()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header);

        if volumes.is_empty() {
            panel = panel.child(empty_state(
                if self.snapshot.volumes.is_empty() {
                    "还没有卷"
                } else {
                    "没有匹配的卷"
                },
                "点击“创建卷”新建一个，或调整搜索条件。",
                cx,
            ));
        } else {
            let rows = volumes.iter().enumerate().map(|(index, volume)| {
                let in_use = self.snapshot.volume_in_use(&volume.name);
                let open_name = volume.name.clone();
                let clone_source = volume.name.clone();
                let remove_name = volume.name.clone();
                let check_name = volume.name.clone();
                let is_checked = self.volumes.checked.contains(&volume.name);
                div()
                    .px_4()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .flex()
                    .items_center()
                    .text_sm()
                    .when(in_use, |row| row.font_semibold())
                    .when(is_checked, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent))
                    .child(
                        div().w(px(32.)).flex_shrink_0().child(
                            Checkbox::new(("check-volume", index))
                                .pointer(true)
                                .checked(is_checked)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    if *checked {
                                        this.volumes.checked.insert(check_name.clone());
                                    } else {
                                        this.volumes.checked.remove(&check_name);
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div()
                            .id(("volume-row", index))
                            .flex_1()
                            .min_w_0()
                            .py_3()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_volume(open_name.clone(), cx);
                            }))
                            .child(
                                div()
                                    .w(px(28.))
                                    .flex_shrink_0()
                                    .child(usage_dot(in_use, cx)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .pr_3()
                                    .text_sm()
                                    .truncate()
                                    .child(volume.name.clone()),
                            )
                            .child(
                                div()
                                    .w(px(DATE_COLUMN))
                                    .flex_shrink_0()
                                    .child(format_timestamp(volume.created)),
                            )
                            .child(
                                div()
                                    .w(px(SIZE_COLUMN))
                                    .flex_shrink_0()
                                    .pr_4()
                                    .text_right()
                                    .text_sm()
                                    .child(self.volume_size_label(&volume.name)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(92.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                list_action_button(("clone-volume", index), cx.theme().primary, cx)
                                    .small()
                                    .icon(IconName::Copy)
                                    .tooltip("克隆")
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_clone_volume_dialog(
                                            clone_source.clone(),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                list_action_button(("remove-volume", index), cx.theme().danger, cx)
                                    .small()
                                    .icon(AssetIcon::Trash)
                                    .tooltip("删除")
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_remove_volume(remove_name.clone(), window, cx);
                                    })),
                            ),
                    )
            });
            panel = panel.child(
                div()
                    .id("volume-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            );
        }

        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(page_header(
                "卷",
                format!(
                    "共 {} 个，当前显示 {} 个。实心圆点表示正被容器使用。",
                    self.snapshot.volumes.len(),
                    volumes.len()
                ),
                cx,
            ))
            .child(
                div()
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
                    .child(div().flex_1())
                    .when(!checked.is_empty(), |this| {
                        let count = checked.len();
                        this.child(
                            list_action_button("clear-volume-selection", cx.theme().foreground, cx)
                                .label("取消选择")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.volumes.checked.clear();
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("remove-checked-volumes")
                                .danger()
                                .pointer(!busy)
                                .icon(AssetIcon::Trash)
                                .label(format!("删除所选（{count}）"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_remove_volumes(checked.clone(), false, window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("prune-volumes")
                            .outline()
                            .pointer(!busy && !unused.is_empty())
                            .icon(AssetIcon::BrushCleaning)
                            .label(format!("清理未使用（{}）", unused.len()))
                            .tooltip("删除所有未被任何容器挂载的卷")
                            .disabled(busy || unused.is_empty())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_remove_volumes(unused.clone(), true, window, cx);
                            })),
                    )
                    .child(
                        Button::new("open-create-volume")
                            .primary()
                            .pointer(!busy)
                            .icon(IconName::Plus)
                            .label("创建卷")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_create_volume_dialog(window, cx);
                            })),
                    ),
            )
            .when_some(self.volumes.sizes_error.clone(), |this, error| {
                this.child(muted(format!("卷大小统计失败：{error}"), cx))
            })
            .child(panel)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .child(muted(format!("显示 {} 项", volumes.len()), cx)),
            )
    }

    fn render_volume_detail(&self, volume: &VolumeSummary, cx: &mut Context<Self>) -> Div {
        let users = self.snapshot.volume_users(&volume.name);
        let busy = self.busy.is_some();
        let status = if users.is_empty() {
            "未使用".to_string()
        } else {
            format!("使用中（{} 个容器）", users.len())
        };
        let accent = brand_color(cx);
        let delete_name = volume.name.clone();
        let import_name = volume.name.clone();
        let empty_name = volume.name.clone();
        let clone_name = volume.name.clone();
        let export_name = volume.name.clone();

        let breadcrumb = div()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .id("volume-breadcrumb")
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(accent)
                    .cursor_pointer()
                    .hover(|style| style.underline())
                    .child(Icon::new(IconName::ArrowLeft).small())
                    .child("卷")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_volume();
                        cx.notify();
                    })),
            )
            .child(div().text_color(cx.theme().muted_foreground).child("/"))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(volume.name.clone()),
            );

        let title = div()
            .flex()
            .items_start()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(Icon::new(IconName::HardDrive).size(px(28.)))
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_semibold()
                                    .truncate()
                                    .child(volume.name.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(usage_dot(!users.is_empty(), cx))
                                    .child(status),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        Button::new("clone-volume-detail")
                            .outline()
                            .icon(IconName::Copy)
                            .label("克隆")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_clone_volume_dialog(clone_name.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("empty-volume")
                            .outline()
                            .icon(AssetIcon::Eraser)
                            .label("清空")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_empty_volume(empty_name.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("export-volume")
                            .outline()
                            .icon(AssetIcon::Download)
                            .label("导出")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.prompt_export_volume(export_name.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("import-volume")
                            .primary()
                            .icon(AssetIcon::Upload)
                            .label("导入")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.prompt_import_volume(import_name.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("remove-volume-detail")
                            .danger()
                            .icon(AssetIcon::Trash)
                            .label("删除")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_remove_volume(delete_name.clone(), window, cx);
                            })),
                    ),
            );

        let mut info = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(detail_row("驱动", volume.driver.clone(), cx))
            .child(detail_row("大小", self.volume_size_label(&volume.name), cx))
            .child(detail_row("创建于", format_timestamp(volume.created), cx))
            .child(detail_row("挂载点", volume.mountpoint.clone(), cx));
        if !volume.labels.is_empty() {
            let labels = volume
                .labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\n");
            info = info.child(detail_row("标签", labels, cx));
        }

        let tabs = div()
            .flex()
            .gap_6()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(self.volume_tab(VolumeTab::Data, "存储数据".into(), cx))
            .child(self.volume_tab(
                VolumeTab::Containers,
                format!("使用中的容器（{}）", users.len()).into(),
                cx,
            ))
            .child(
                self.volume_tab(
                    VolumeTab::Exports,
                    format!(
                        "导出（{}）",
                        self.volumes
                            .exports
                            .iter()
                            .filter(|record| record.volume == volume.name)
                            .count()
                    )
                    .into(),
                    cx,
                ),
            );

        let content = match self.volumes.tab {
            VolumeTab::Data => self.render_volume_data(cx),
            VolumeTab::Containers => self.render_volume_users(&volume.name, cx),
            VolumeTab::Exports => self.render_volume_exports(&volume.name, cx),
        };

        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(breadcrumb)
            .child(title)
            .child(info)
            .child(tabs)
            .child(content)
    }

    fn render_volume_exports(&self, volume: &str, cx: &mut Context<Self>) -> Div {
        let busy = self.busy.is_some();
        let export_name = volume.to_string();
        let running = self
            .snapshot
            .volume_users(volume)
            .iter()
            .filter(|container| container.is_running())
            .count();
        let records: Vec<&ExportRecord> = self
            .volumes
            .exports
            .iter()
            .filter(|record| record.volume == volume)
            .collect();

        let mut panel = section_panel(cx).flex_1().min_h_0().flex().flex_col();
        if records.is_empty() {
            panel = panel.child(empty_state(
                "还没有导出记录",
                "导出完成后会在这里列出文件，可在文件夹中定位。",
                cx,
            ));
        } else {
            let rows = records.into_iter().enumerate().map(|(index, record)| {
                let path = record.path.clone();
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(record.path.display().to_string()),
                    )
                    .child(
                        div()
                            .w(px(SIZE_COLUMN))
                            .flex_shrink_0()
                            .text_right()
                            .text_sm()
                            .child(format_bytes(record.size)),
                    )
                    .child(
                        div()
                            .w(px(DATE_COLUMN))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format_timestamp(record.finished)),
                    )
                    .child(
                        Button::new(("reveal-export", index))
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .tooltip("在文件夹中显示")
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
            });
            panel = panel.child(
                div()
                    .id("volume-exports")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            );
        }

        let mut hint = "导出为未压缩的 tar 文件，条目相对卷根目录并保留属主、权限和时间戳，可通过“导入”恢复到任意卷。记录只保留在本次运行期间。".to_string();
        if running > 0 {
            hint.push_str(&format!(
                "该卷正被 {running} 个运行中的容器使用，导出时若仍在写入，数据可能不一致，建议先停止。"
            ));
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(muted(hint, cx)))
                    .child(
                        Button::new("export-volume-tab")
                            .outline()
                            .small()
                            .icon(AssetIcon::Download)
                            .label("导出到文件…")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.prompt_export_volume(export_name.clone(), window, cx);
                            })),
                    ),
            )
            .child(panel)
    }

    fn volume_tab(
        &self,
        tab: VolumeTab,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.volumes.tab == tab;
        let accent = brand_color(cx);
        div()
            .id(SharedString::from(format!("volume-tab-{}", tab as u8)))
            .pb_2()
            .text_sm()
            .cursor_pointer()
            .border_b_2()
            .when(selected, |this| {
                this.border_color(accent).text_color(accent).font_semibold()
            })
            .when(!selected, |this| {
                this.border_color(gpui_kit::transparent_black())
                    .text_color(cx.theme().muted_foreground)
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.volumes.tab = tab;
                cx.notify();
            }))
    }

    fn render_volume_data(&self, cx: &mut Context<Self>) -> Div {
        let header = div()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(div().flex_1().min_w(px(NAME_COLUMN_MIN)).child("名称"))
            .child(
                div()
                    .w(px(SIZE_COLUMN))
                    .flex_shrink_0()
                    .text_right()
                    .pr_4()
                    .child("大小"),
            )
            .child(div().w(px(DATE_COLUMN)).flex_shrink_0().child("修改时间"))
            .child(div().w(px(MODE_COLUMN)).flex_shrink_0().child("权限"));

        let mut rows = Vec::new();
        self.collect_tree_rows("", 0, &mut rows);
        let root_loading = matches!(self.volumes.tree.get(""), Some(DirState::Loading) | None);

        let body: AnyElement = if rows.is_empty() {
            if root_loading {
                empty_state(
                    "正在读取卷内容…",
                    format!("首次使用会拉取 {HELPER_IMAGE} 辅助镜像。"),
                    cx,
                )
                .into_any_element()
            } else {
                empty_state("卷是空的", "点击“导入”从 tar 归档写入数据。", cx).into_any_element()
            }
        } else {
            div()
                .id("volume-tree")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(
                    rows.into_iter()
                        .enumerate()
                        .map(|(index, row)| self.render_tree_row(index, row, cx)),
                )
                .into_any_element()
        };

        let refreshing = self
            .volumes
            .tree
            .values()
            .any(|state| matches!(state, DirState::Loading));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div().flex_1().child(muted(
                            format!("卷以只读方式挂载到临时 {HELPER_IMAGE} 容器中读取，目录大小为磁盘占用。"),
                            cx,
                        )),
                    )
                    .child(
                        Button::new("reload-volume-tree")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .label(if refreshing { "读取中" } else { "刷新内容" })
                            .disabled(refreshing)
                            .on_click(cx.listener(|this, _, _, cx| this.reload_volume_tree(cx))),
                    ),
            )
            .child(
                section_panel(cx)
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(body),
            )
    }

    fn collect_tree_rows(&self, directory: &str, depth: usize, rows: &mut Vec<TreeRow>) {
        match self.volumes.tree.get(directory) {
            None | Some(DirState::Loading) => {
                if depth > 0 {
                    rows.push(TreeRow::Message {
                        depth,
                        text: "读取中…".into(),
                        is_error: false,
                    });
                }
            }
            Some(DirState::Failed(error)) => rows.push(TreeRow::Message {
                depth,
                text: error.clone(),
                is_error: true,
            }),
            Some(DirState::Loaded(listing)) => {
                if listing.entries.is_empty() && depth > 0 {
                    rows.push(TreeRow::Message {
                        depth,
                        text: "空目录".into(),
                        is_error: false,
                    });
                }
                for entry in &listing.entries {
                    let path = join_relative(directory, &entry.name);
                    let expanded = entry.kind == VolumeEntryKind::Directory
                        && self.volumes.expanded.contains(&path);
                    rows.push(TreeRow::Entry {
                        path: path.clone(),
                        entry: entry.clone(),
                        depth,
                        expanded,
                    });
                    if expanded {
                        self.collect_tree_rows(&path, depth + 1, rows);
                    }
                }
                if listing.truncated {
                    rows.push(TreeRow::Message {
                        depth,
                        text: format!("仅显示前 {MAX_DIRECTORY_ENTRIES} 项"),
                        is_error: false,
                    });
                }
            }
        }
    }

    fn render_tree_row(&self, index: usize, row: TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let base = div()
            .id(("volume-tree-row", index))
            .px_4()
            .py(px(6.))
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .text_sm();
        match row {
            TreeRow::Message {
                depth,
                text,
                is_error,
            } => base
                .child(
                    div()
                        .pl(px(depth as f32 * INDENT + 22.))
                        .text_xs()
                        .text_color(if is_error {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(text),
                )
                .into_any_element(),
            TreeRow::Entry {
                path,
                entry,
                depth,
                expanded,
            } => {
                let is_directory = entry.kind == VolumeEntryKind::Directory;
                let icon = match entry.kind {
                    VolumeEntryKind::Directory if expanded => IconName::FolderOpen,
                    VolumeEntryKind::Directory => IconName::Folder,
                    _ => IconName::File,
                };
                let name = if entry.kind == VolumeEntryKind::Symlink {
                    format!("{} ↗", entry.name)
                } else {
                    entry.name.clone()
                };
                base.when(is_directory, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(cx.theme().accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_volume_directory(path.clone(), cx);
                        }))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(NAME_COLUMN_MIN))
                        .pl(px(depth as f32 * INDENT))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .w(px(18.))
                                .flex_shrink_0()
                                .text_color(cx.theme().muted_foreground)
                                .when(is_directory, |this| {
                                    this.child(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .small(),
                                    )
                                }),
                        )
                        .child(
                            Icon::new(icon)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().ml_1().min_w_0().truncate().child(name)),
                )
                .child(
                    div()
                        .w(px(SIZE_COLUMN))
                        .flex_shrink_0()
                        .pr_4()
                        .text_right()
                        .child(format_bytes(entry.size)),
                )
                .child(
                    div()
                        .w(px(DATE_COLUMN))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format_timestamp(entry.modified)),
                )
                .child(
                    div()
                        .w(px(MODE_COLUMN))
                        .flex_shrink_0()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .child(entry.mode),
                )
                .into_any_element()
            }
        }
    }

    fn render_volume_users(&self, volume: &str, cx: &mut Context<Self>) -> Div {
        let users = self.snapshot.volume_users(volume);
        let panel = section_panel(cx).flex_1().min_h_0().flex().flex_col();
        if users.is_empty() {
            return panel.child(empty_state(
                "没有容器使用此卷",
                "卷未被任何容器（包括已停止的容器）挂载。",
                cx,
            ));
        }
        let rows = users.into_iter().enumerate().map(|(index, container)| {
            let id = container.id.clone();
            let state_color = if container.is_running() {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            };
            div()
                .id(("volume-user", index))
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .gap_3()
                .cursor_pointer()
                .hover(|row| row.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = Page::Containers;
                    this.select_container(id.clone(), cx);
                }))
                .child(
                    div()
                        .w(px(220.))
                        .flex_shrink_0()
                        .text_sm()
                        .truncate()
                        .child(container.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(container.image.clone()),
                )
                .child(
                    div()
                        .w(px(90.))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(state_color)
                        .child(container.state.clone()),
                )
        });
        panel.child(
            div()
                .id("volume-users")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows),
        )
    }
}

fn usage_dot(in_use: bool, cx: &gpui_kit::App) -> Div {
    let dot = div().size(px(10.)).rounded_full().border_1();
    if in_use {
        dot.border_color(cx.theme().success).bg(cx.theme().success)
    } else {
        dot.border_color(cx.theme().muted_foreground)
    }
}

fn dialog_error(error: String, cx: &gpui_kit::App) -> Div {
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().danger)
        .text_sm()
        .text_color(cx.theme().danger)
        .child(error)
}

fn dialog_footer(
    submit_id: &'static str,
    submit_label: &'static str,
    icon: IconName,
    on_submit: impl Fn(&mut Window, &mut gpui_kit::App) + 'static,
) -> Div {
    div()
        .flex()
        .justify_end()
        .gap_2()
        .child(
            Button::new(SharedString::from(format!("{submit_id}-cancel")))
                .outline()
                .label("取消")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
            Button::new(submit_id)
                .primary()
                .icon(icon)
                .label(submit_label)
                .on_click(move |_, window, cx| on_submit(window, cx)),
        )
}
