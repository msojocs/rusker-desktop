//! Read-only Compose source files, loaded from the paths recorded by Compose.

use std::{
    fs::File,
    io::{self, Read as _},
    path::Path,
};

use super::*;

const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct ConfigFile {
    path: String,
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn resolve_path(path: &str, working_dir: Option<&str>) -> String {
    if is_absolute(path) {
        return path.to_string();
    }
    let Some(directory) = working_dir.filter(|directory| !directory.is_empty()) else {
        return path.to_string();
    };
    let separator = if directory.contains('\\') { '\\' } else { '/' };
    format!(
        "{}{separator}{path}",
        directory.trim_end_matches(['/', '\\'])
    )
}

fn config_files(containers: &[ContainerSummary]) -> Vec<ConfigFile> {
    let mut paths = HashSet::new();
    let mut files = Vec::new();
    for container in containers {
        let Some(label) = &container.compose_config_files else {
            continue;
        };
        for path in label
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            let path = resolve_path(path, container.compose_working_dir.as_deref());
            if paths.insert(path.clone()) {
                files.push(ConfigFile { path });
            }
        }
    }
    files
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Compose running in WSL may record the Windows source as `/mnt/d/...`.
#[cfg(any(windows, test))]
fn windows_drive_path(path: &str) -> Option<PathBuf> {
    let rest = path
        .strip_prefix("/mnt/")
        .or_else(|| path.strip_prefix("/run/desktop/mnt/host/"))?;
    let bytes = rest.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() || (bytes.len() > 1 && bytes[1] != b'/')
    {
        return None;
    }
    let drive = char::from(bytes[0]).to_ascii_uppercase();
    let tail = rest[1..].trim_start_matches('/').replace('/', "\\");
    Some(PathBuf::from(format!("{drive}:\\{tail}")))
}

fn local_path(path: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = windows_drive_path(path) {
        return path;
    }
    PathBuf::from(path)
}

fn vscode_project_url(containers: &[ContainerSummary]) -> Result<String, String> {
    let directory = containers
        .iter()
        .filter_map(|container| container.compose_working_dir.as_deref())
        .find(|path| !path.trim().is_empty())
        .map(local_path)
        .or_else(|| {
            config_files(containers)
                .first()
                .and_then(|file| local_path(&file.path).parent().map(Path::to_path_buf))
        })
        .ok_or_else(|| t!("app.compose.workspace_unavailable").into_owned())?;
    let file_url = url::Url::from_directory_path(&directory)
        .map_err(|_| t!("app.compose.config_not_local").into_owned())?;
    // VSCode's URI handler accepts folder paths, including escaped spaces and Unicode.
    let host = file_url
        .host_str()
        .map(|host| format!("//{host}"))
        .unwrap_or_default();
    Ok(format!("vscode://file{host}{}", file_url.path()))
}

fn read_config(path: &Path) -> Result<String, String> {
    // Never resolve a relative label against the application's working directory.
    if !path.is_absolute() {
        return Err(t!("app.compose.config_not_local").into_owned());
    }
    let read = || -> io::Result<Vec<u8>> {
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let bytes =
        read().map_err(|error| t!("app.compose.config_read_failed", error = error).into_owned())?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(t!("app.compose.config_too_large").into_owned());
    }
    let text =
        String::from_utf8(bytes).map_err(|_| t!("app.compose.config_invalid_utf8").into_owned())?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

pub(super) struct ComposeConfigView {
    project: String,
    working_dir: Option<String>,
    files: Vec<ConfigFile>,
    selected: Option<usize>,
    expanded: bool,
    content: Option<String>,
    error: Option<String>,
    loading: bool,
    editor: Option<Entity<EditorState>>,
    revision: Revision,
    _task: Option<Task<()>>,
}

impl ComposeConfigView {
    fn new(project: String, containers: &[ContainerSummary], cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            project,
            working_dir: containers.iter().find_map(|container| {
                container
                    .compose_working_dir
                    .clone()
                    .filter(|path| !path.is_empty())
            }),
            files: config_files(containers),
            selected: None,
            expanded: true,
            content: None,
            error: None,
            loading: false,
            editor: None,
            revision: Revision::default(),
            _task: None,
        };
        if !view.files.is_empty() {
            view.load_file(0, cx);
        }
        view
    }

    fn load_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(index) else {
            return;
        };
        let path = local_path(&file.path);
        let revision = self.revision.advance();
        self.selected = Some(index);
        self.content = None;
        self.error = None;
        self.editor = None;
        self.loading = true;
        let read = cx
            .background_executor()
            .spawn(async move { read_config(&path) });
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            _ = this.update(cx, |this, cx| {
                if !this.revision.is_current(revision) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(content) => this.content = Some(content),
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for ComposeConfigView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut files = div()
            .id("compose-config-files")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .id("compose-config-folder")
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().accent))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if self.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small(),
                    )
                    .child(Icon::new(IconName::Folder).small())
                    .child(div().min_w_0().truncate().child(self.project.clone())),
            );
        if self.expanded {
            for (index, file) in self.files.iter().enumerate() {
                let selected = self.selected == Some(index);
                let tooltip = file.path.clone();
                files = files.child(
                    div()
                        .id(("compose-config-file", index))
                        .pl_10()
                        .pr_4()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .when(selected, |style| {
                            style.bg(cx.theme().accent).text_color(cx.theme().primary)
                        })
                        .hover(|style| style.bg(cx.theme().accent))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.selected != Some(index) {
                                this.load_file(index, cx);
                            }
                        }))
                        .child(Icon::new(IconName::FileText).small())
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(file_name(&file.path).to_string()),
                        ),
                );
            }
        }

        let content = if let Some(text) = &self.content {
            let editor = self.editor.get_or_insert_with(|| {
                cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language("yaml")
                        .line_number(true)
                        .soft_wrap(false)
                        .folding(true)
                        .scroll_beyond_last_line(Some(0))
                        .default_value(text.clone())
                })
            });
            div().size_full().child(
                Editor::new(editor)
                    .readonly(true)
                    .bordered(false)
                    .size_full(),
            )
        } else {
            let message = if self.loading {
                t!("app.compose.config_loading").into_owned()
            } else if let Some(error) = &self.error {
                error.clone()
            } else {
                t!("app.compose.config_empty").into_owned()
            };
            div()
                .p_4()
                .text_sm()
                .text_color(if self.error.is_some() {
                    cx.theme().danger
                } else {
                    cx.theme().muted_foreground
                })
                .child(message)
        };
        let selected_path = self
            .selected
            .and_then(|index| self.files.get(index))
            .map(|file| file.path.clone())
            .unwrap_or_default();
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(px(300.))
                    .flex_shrink_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(div().px_4().py_3().font_semibold().child(t!("app.name")))
                    .child(files),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(div().flex_1().min_w_0().truncate().child(selected_path))
                            .child(
                                button("copy-compose-config")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Copy)
                                    .tooltip(t!("app.compose.copy_config"))
                                    .disabled(self.content.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(content) = &this.content {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                content.clone(),
                                            ));
                                        }
                                    })),
                            )
                            .child(
                                button("reload-compose-config")
                                    .ghost()
                                    .small()
                                    .icon(IconName::RefreshCw)
                                    .tooltip(t!("app.refresh"))
                                    .disabled(self.loading || self.selected.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(index) = this.selected {
                                            this.load_file(index, cx);
                                        }
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_w_0().min_h_0().child(content)),
            )
    }
}

impl RuskerApp {
    pub(super) fn open_compose_in_vscode(&mut self, project: &str, cx: &mut Context<Self>) {
        let containers: Vec<_> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container.compose_project.as_deref() == Some(project))
            .cloned()
            .collect();
        match vscode_project_url(&containers) {
            Ok(url) => cx.open_url(&url),
            Err(error) => self.notify_error(error, cx),
        }
    }

    pub(super) fn open_compose_config(&mut self, project: &str, cx: &mut Context<Self>) {
        let containers: Vec<_> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container.compose_project.as_deref() == Some(project))
            .cloned()
            .collect();
        self.compose_config_view =
            Some(cx.new(|cx| ComposeConfigView::new(project.to_string(), &containers, cx)));
        cx.notify();
    }

    fn close_compose_config(&mut self, cx: &mut Context<Self>) {
        self.compose_config_view = None;
        cx.notify();
    }

    pub(super) fn render_compose_config_detail(
        &self,
        project: &str,
        view: &Entity<ComposeConfigView>,
        cx: &mut Context<Self>,
    ) -> Div {
        let weak = cx.weak_entity();
        let project_weak = weak.clone();
        let subtitle = view
            .read(cx)
            .working_dir
            .as_ref()
            .map_or_else(|| project.to_string(), |path| format!("{project}: {path}"));
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
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
                        .child(
                            BreadcrumbItem::new(project.to_string())
                                .text_color(cx.theme().primary)
                                .on_click(move |_, _, cx| {
                                    _ = project_weak
                                        .update(cx, |this, cx| this.close_compose_config(cx));
                                }),
                        )
                        .child(BreadcrumbItem::new(
                            t!("app.compose.config_title").into_owned(),
                        )),
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
                        button("compose-config-back")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .tooltip(t!("app.compose.back_to_project"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_compose_config(cx))),
                    )
                    .child(
                        Icon::new(AssetIcon::Layers2)
                            .size(px(40.))
                            .text_color(cx.theme().muted_foreground),
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
                                    .child(t!("app.compose.config_title")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .px_6()
                    .pb_6()
                    .flex()
                    .child(view.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_opens_the_project_folder_and_escapes_special_characters() {
        let root = std::env::temp_dir().join("compose 中文 # %2F");
        let container = ContainerSummary {
            compose_working_dir: Some(root.to_str().unwrap().to_string()),
            ..Default::default()
        };
        let url = vscode_project_url(&[container]).unwrap();
        assert!(url.starts_with("vscode://file/"));
        assert!(url.contains("%20"));
        assert!(url.contains("%23"));
        assert!(url.contains("%252F"));
        assert!(!url.contains('中'));
        let fallback = ContainerSummary {
            compose_config_files: Some(root.join("compose.yaml").to_str().unwrap().to_string()),
            ..Default::default()
        };
        assert_eq!(vscode_project_url(&[fallback]).unwrap(), url);
        assert!(vscode_project_url(&[ContainerSummary::default()]).is_err());
        assert!(vscode_project_url(&[ContainerSummary {
            compose_working_dir: Some("relative/project".into()),
            ..Default::default()
        }])
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn vscode_translates_wsl_drive_paths_to_windows_folders() {
        let container = ContainerSummary {
            compose_working_dir: Some("/mnt/d/temp/project1".into()),
            ..Default::default()
        };
        assert_eq!(
            vscode_project_url(&[container]).unwrap(),
            "vscode://file/D:/temp/project1/"
        );
    }

    #[test]
    fn collects_all_project_files_in_compose_order_without_duplicate_service_labels() {
        let container = ContainerSummary {
            compose_working_dir: Some("/srv/project".into()),
            compose_config_files: Some(
                "compose.yaml, /srv/project/override.yaml, ,compose.yaml".into(),
            ),
            ..Default::default()
        };
        let files = config_files(&[container.clone(), container]);
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/srv/project/compose.yaml", "/srv/project/override.yaml"]
        );
        assert!(config_files(&[ContainerSummary::default()]).is_empty());
    }

    #[test]
    fn resolves_native_paths_and_wsl_drive_labels() {
        assert_eq!(
            resolve_path("compose.yaml", Some(r"D:\temp\project1")),
            r"D:\temp\project1\compose.yaml"
        );
        assert_eq!(
            resolve_path(r"C:\other\compose.yaml", Some("/srv/project")),
            r"C:\other\compose.yaml"
        );
        assert_eq!(resolve_path("compose.yaml", None), "compose.yaml");
        assert_eq!(file_name(r"D:\temp\compose.yaml"), "compose.yaml");
        assert_eq!(file_name("/srv/compose.yaml"), "compose.yaml");
        assert_eq!(
            windows_drive_path("/mnt/d/temp/project1/compose.yaml"),
            Some(PathBuf::from(r"D:\temp\project1\compose.yaml"))
        );
        assert_eq!(
            windows_drive_path("/run/desktop/mnt/host/c/project/compose.yaml"),
            Some(PathBuf::from(r"C:\project\compose.yaml"))
        );
        assert_eq!(windows_drive_path("/mnt/data/compose.yaml"), None);
    }

    #[test]
    fn reads_real_files_including_empty_and_bom_and_reports_missing_or_invalid_sources() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("compose.yaml");
        std::fs::write(
            &path,
            "\u{feff}services:\r\n  nginx:\r\n    image: nginx\r\n",
        )
        .unwrap();
        assert_eq!(
            read_config(&path).unwrap(),
            "services:\r\n  nginx:\r\n    image: nginx\r\n"
        );
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_config(&path).unwrap(), "");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(read_config(&path).is_err());
        assert!(read_config(&temp.path().join("missing.yaml")).is_err());
        assert!(read_config(Path::new("compose.yaml")).is_err());
        assert!(read_config(temp.path()).is_err());
    }

    #[test]
    fn oversized_config_is_rejected_without_truncating_the_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("compose.yaml");
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();
        assert!(read_config(&path).is_err());
    }
}
