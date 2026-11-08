//! Live tools on the container detail page. All IO stays on the engine runtime.
use std::collections::VecDeque;

use crate::ui::{
    stats_chart::{stats_chart, ChartPoint, ChartUnit},
    terminal::TerminalView,
};
use gpui_kit::Context;
use rusker_desktop::{
    engine::ContainerStats,
    model::{Revision, VolumeEntryKind},
    volume::DirectoryListing,
};

use super::*;

const STATS_HISTORY_LIMIT: usize = 300;
const STATS_HISTORY_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct StatsSample {
    at: Instant,
    timestamp: i64,
    stats: ContainerStats,
    connected: bool,
}

#[derive(Default)]
struct StatsHistory {
    samples: VecDeque<StatsSample>,
    interrupted: bool,
}

impl StatsHistory {
    fn push(&mut self, stats: ContainerStats, at: Instant, timestamp: i64) {
        let connected = !self.interrupted
            && self.samples.back().is_some_and(|previous| {
                at.saturating_duration_since(previous.at) <= Duration::from_secs(10)
                    && stats.network_rx >= previous.stats.network_rx
                    && stats.network_tx >= previous.stats.network_tx
                    && stats.block_read >= previous.stats.block_read
                    && stats.block_write >= previous.stats.block_write
            });
        self.samples.push_back(StatsSample {
            at,
            timestamp,
            stats,
            connected,
        });
        self.interrupted = false;
        while self.samples.len() > STATS_HISTORY_LIMIT
            || self.samples.front().is_some_and(|sample| {
                at.saturating_duration_since(sample.at) > STATS_HISTORY_WINDOW
            })
        {
            self.samples.pop_front();
        }
    }

    fn points(&self, values: impl Fn(&ContainerStats) -> [Option<f64>; 2]) -> Vec<ChartPoint> {
        let Some(first) = self.samples.front() else {
            return Vec::new();
        };
        self.samples
            .iter()
            .map(|sample| ChartPoint {
                seconds: sample.at.saturating_duration_since(first.at).as_secs_f64(),
                timestamp: sample.timestamp,
                values: values(&sample.stats),
                connected: sample.connected,
            })
            .collect()
    }
}

const DIRECTORY_CACHE_LIMIT: usize = 8;
const DIRECTORY_CACHE_BYTES: usize = 16 * 1024 * 1024;
const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(30);

struct CachedDirectory {
    container: String,
    path: String,
    listing: DirectoryListing,
    at: Instant,
    bytes: usize,
}

/// Small LRU shared across detail selections, never across engine disconnects.
#[derive(Default)]
struct DirectoryCache {
    entries: VecDeque<CachedDirectory>,
}

impl DirectoryCache {
    fn get(
        &mut self,
        container: &str,
        path: &str,
        now: Instant,
        refresh: bool,
    ) -> Option<DirectoryListing> {
        // A refresh never reads cached contents, even before their TTL expires.
        if refresh {
            return None;
        }
        self.entries
            .retain(|entry| now.saturating_duration_since(entry.at) < DIRECTORY_CACHE_TTL);
        let index = self
            .entries
            .iter()
            .position(|entry| entry.container == container && entry.path == path)?;
        let entry = self.entries.remove(index)?;
        let listing = entry.listing.clone();
        self.entries.push_back(entry);
        Some(listing)
    }

    fn insert(&mut self, container: String, path: String, listing: DirectoryListing, at: Instant) {
        self.entries
            .retain(|entry| entry.container != container || entry.path != path);
        let bytes = container.len()
            + path.len()
            + listing
                .entries
                .iter()
                .map(|entry| std::mem::size_of_val(entry) + entry.name.len() + entry.mode.len())
                .sum::<usize>();
        if bytes > DIRECTORY_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= DIRECTORY_CACHE_LIMIT
            || self.entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes
                > DIRECTORY_CACHE_BYTES
        {
            self.entries.pop_front();
        }
        self.entries.push_back(CachedDirectory {
            container,
            path,
            listing,
            at,
            bytes,
        });
    }
}

#[derive(Default)]
pub(super) struct ContainerTools {
    shell_connecting: bool,
    shell_error: Option<String>,
    shell_task: Option<Task<()>>,
    directory: String,
    listing: Option<DirectoryListing>,
    files_loading: bool,
    files_error: Option<String>,
    files_revision: Revision,
    files_task: Option<Task<()>>,
    files_cache: DirectoryCache,
    files_cached: bool,
    stats: Option<ContainerStats>,
    stats_error: Option<String>,
    stats_updated: Option<Instant>,
    stats_task: Option<Task<()>>,
    stats_history: StatsHistory,
    stats_table: bool,
}

impl RuskerApp {
    pub(super) fn reset_container_tools(&mut self) {
        // Keep the request clock monotonic even across A -> B -> A selections.
        let mut files_revision = self.container_tools.files_revision;
        files_revision.advance();
        let files_cache = std::mem::take(&mut self.container_tools.files_cache);
        self.container_tools = ContainerTools {
            files_revision,
            files_cache,
            ..Default::default()
        };
        self.container_terminal = None;
    }

    pub(super) fn stop_container_live_tools(&mut self) {
        let tools = &mut self.container_tools;
        tools.shell_task = None;
        tools.shell_connecting = false;
        tools.stats_task = None;
        tools.stats = None;
        tools.stats_updated = None;
        tools.stats_history = StatsHistory::default();
        tools.stats_error = None;
        tools.files_revision.advance();
        tools.files_task = None;
        tools.files_loading = false;
        tools.listing = None;
        tools.files_cache = DirectoryCache::default();
        tools.files_cached = false;
        self.container_terminal = None;
    }

    pub(super) fn sync_container_tools(&mut self, cx: &mut Context<Self>) {
        if !self.selected_container_running() {
            self.stop_container_live_tools();
        } else {
            self.select_container_tab(self.container_tab, cx);
        }
    }

    pub(super) fn select_container_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.container_tab = tab.min(5);
        if tab != 5 {
            self.container_tools.stats_task = None;
            self.container_tools.stats_history.interrupted = true;
        }
        match tab {
            3 if self.container_tools.shell_task.is_none() => self.connect_container_shell(cx),
            4 if self.container_tools.listing.is_none() && !self.container_tools.files_loading => {
                let path = if self.container_tools.directory.is_empty() {
                    "/".into()
                } else {
                    self.container_tools.directory.clone()
                };
                self.load_container_directory(path, cx);
            }
            5 => self.start_container_stats(cx),
            _ => {}
        }
        cx.notify();
    }

    fn selected_container_running(&self) -> bool {
        self.connected
            && self.page == Page::Containers
            && self.snapshot.containers.iter().any(|container| {
                Some(container.id.as_str()) == self.selected_container.as_deref()
                    && container.is_running()
            })
    }

    fn connect_container_shell(&mut self, cx: &mut Context<Self>) {
        if !self.selected_container_running() || self.container_tools.shell_connecting {
            return;
        }
        let (Some(engine), Some(id)) = (self.engine.clone(), self.selected_container.clone())
        else {
            return;
        };
        self.container_tools.shell_task = None;
        self.container_terminal = Some(cx.new(TerminalView::new));
        self.container_tools.shell_error = None;
        self.container_tools.shell_connecting = true;
        let handle = engine.open_container_shell(id.clone());
        self.container_tools.shell_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            let mut shell = match result {
                Ok(shell) => shell,
                Err(error) => {
                    _ = this.update(cx, |this, cx| {
                        if this.selected_container.as_ref() == Some(&id) {
                            this.container_tools.shell_connecting = false;
                            this.container_tools.shell_error = Some(error.to_string());
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let valid = this
                .update(cx, |this, cx| {
                    if this.selected_container.as_ref() != Some(&id)
                        || !this.selected_container_running()
                    {
                        return false;
                    }
                    this.container_tools.shell_connecting = false;
                    if let Some(terminal) = &this.container_terminal {
                        terminal.update(cx, |terminal, cx| {
                            terminal.connect(shell.input.clone(), shell.resize.clone(), cx)
                        });
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !valid {
                return;
            }
            while let Some(output) = shell.output.recv().await {
                let valid = this
                    .update(cx, |this, cx| {
                        if this.selected_container.as_ref() != Some(&id) {
                            return false;
                        }
                        this.append_shell_output(&output, cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !valid {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                if this.selected_container.as_ref() == Some(&id) {
                    this.append_shell_output("\r\n[Shell 会话已结束]\r\n", cx);
                    if let Some(terminal) = &this.container_terminal {
                        terminal.update(cx, |terminal, cx| terminal.disconnect(cx));
                    }
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn append_shell_output(&mut self, output: &str, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.container_terminal {
            terminal.update(cx, |terminal, cx| terminal.process(output, cx));
        }
    }

    fn load_container_directory(&mut self, path: String, cx: &mut Context<Self>) {
        self.request_container_directory(path, false, cx);
    }

    fn request_container_directory(&mut self, path: String, refresh: bool, cx: &mut Context<Self>) {
        let (Some(engine), Some(id)) = (self.engine.clone(), self.selected_container.clone())
        else {
            return;
        };
        if !self.selected_container_running() {
            return;
        }
        let revision = self.container_tools.files_revision.advance();
        let same_path = self.container_tools.directory == path;
        let tools = &mut self.container_tools;
        tools.files_task = None;
        tools.directory = path.clone();
        if !same_path {
            tools.listing = None;
        }
        tools.files_error = None;
        tools.files_loading = false;
        tools.files_cached = false;
        if let Some(listing) = tools.files_cache.get(&id, &path, Instant::now(), refresh) {
            tools.listing = Some(listing);
            tools.files_cached = true;
            cx.notify();
            return;
        }
        tools.files_loading = true;
        let fill_span = rusker_desktop::diagnostics::PerfSpan::new("directory.ui.fill");
        let handle = engine.list_container_directory(id.clone(), path.clone());
        self.container_tools.files_task = Some(cx.spawn(async move |this, cx| {
            let _fill_span = fill_span;
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if this.selected_container.as_ref() != Some(&id)
                    || !this.container_tools.files_revision.is_current(revision)
                    || this.container_tools.directory != path
                    || !this.selected_container_running()
                {
                    return;
                }
                this.container_tools.files_loading = false;
                match result {
                    Ok(listing) => {
                        this.container_tools.files_cache.insert(
                            id,
                            path,
                            listing.clone(),
                            Instant::now(),
                        );
                        this.container_tools.listing = Some(listing);
                    }
                    Err(error) => this.container_tools.files_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn start_container_stats(&mut self, cx: &mut Context<Self>) {
        if self.container_tools.stats_task.is_some() || !self.selected_container_running() {
            return;
        }
        let (Some(engine), Some(id)) = (self.engine.clone(), self.selected_container.clone())
        else {
            return;
        };
        self.container_tools.stats_error = None;
        self.container_tools.stats_task = Some(cx.spawn(async move |this, cx| loop {
            let active = this
                .update(cx, |this, _| {
                    this.page == Page::Containers
                        && this.container_tab == 5
                        && this.selected_container.as_ref() == Some(&id)
                        && this.selected_container_running()
                })
                .unwrap_or(false);
            if !active {
                break;
            }
            let result = await_engine(engine.container_stats(id.clone())).await;
            let active = this
                .update(cx, |this, cx| {
                    if this.selected_container.as_ref() != Some(&id)
                        || this.container_tab != 5
                        || !this.selected_container_running()
                    {
                        return false;
                    }
                    match result {
                        Ok(stats) => {
                            let now = Instant::now();
                            this.container_tools.stats_history.push(
                                stats.clone(),
                                now,
                                OffsetDateTime::now_utc().unix_timestamp(),
                            );
                            this.container_tools.stats = Some(stats);
                            this.container_tools.stats_error = None;
                            this.container_tools.stats_updated = Some(now);
                        }
                        Err(error) => {
                            this.container_tools.stats_error = Some(error.to_string());
                            this.container_tools.stats_history.interrupted = true;
                        }
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !active {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }));
    }

    pub(super) fn render_container_exec_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> Div {
        let tools = &self.container_tools;
        let output = self
            .container_terminal
            .as_ref()
            .map(|terminal| terminal.read(cx).contents())
            .unwrap_or_default();
        div()
            .flex_1()
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
                    .child(
                        muted(
                            if tools.shell_connecting {
                                "正在连接 /bin/sh…"
                            } else {
                                "点击终端直接输入 · Ctrl+C 中断 · Ctrl+Shift+V 粘贴"
                            },
                            cx,
                        )
                        .flex_1(),
                    )
                    .child(
                        button("shell-copy")
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .tooltip("复制输出")
                            .disabled(output.is_empty())
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(output.clone()))
                            }),
                    )
                    .child(
                        button("shell-clear")
                            .ghost()
                            .small()
                            .label("清屏")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(terminal) = &this.container_terminal {
                                    terminal.update(cx, |terminal, cx| terminal.clear(cx));
                                }
                            })),
                    )
                    .child(
                        button("shell-reconnect")
                            .outline()
                            .small()
                            .label("重新连接")
                            .disabled(!container.is_running() || tools.shell_connecting)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.connect_container_shell(cx)),
                            ),
                    ),
            )
            .when(!container.is_running(), |view| {
                view.child(muted("请先启动容器再连接命令行。", cx).px_4().py_2())
            })
            .when_some(tools.shell_error.clone(), |view, error| {
                view.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when_some(self.container_terminal.clone(), |view, terminal| {
                view.child(div().flex_1().min_h_0().child(terminal))
            })
    }

    pub(super) fn render_container_files_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> Div {
        let tools = &self.container_tools;
        let path = if tools.directory.is_empty() {
            "/"
        } else {
            &tools.directory
        };
        let parent = directory_parent(path);
        let refresh_path = path.to_string();
        let mut content = div().flex().flex_col();
        if !container.is_running() {
            content = content.child(empty_state(
                "容器未运行",
                "启动容器后可浏览文件；需要 sh、find、stat、tr、head 和 xargs。",
                cx,
            ));
        } else {
            if tools.files_loading {
                content = content.child(
                    muted(
                        if tools.listing.is_some() {
                            "正在刷新目录，暂时保留上次内容…"
                        } else {
                            "正在读取目录…"
                        },
                        cx,
                    )
                    .p_4(),
                );
            }
            if let Some(error) = &tools.files_error {
                content = content.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(format!("读取失败（如有内容则为上次结果）：{error}")),
                );
            }
            if let Some(listing) = &tools.listing {
                content = content.child(
                    div()
                        .px_4()
                        .py_2()
                        .flex()
                        .gap_4()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(div().flex_1().child("名称"))
                        .child(div().w(px(100.)).child("大小"))
                        .child(div().w(px(190.)).child("修改时间"))
                        .child(div().w(px(110.)).child("权限")),
                );
                for (index, entry) in listing.entries.iter().enumerate() {
                    let directory = entry.kind == VolumeEntryKind::Directory;
                    let child_path = format!("{}/{}", path.trim_end_matches('/'), entry.name);
                    content = content.child(
                        div()
                            .id(("container-file", index))
                            .px_4()
                            .py_3()
                            .flex()
                            .items_center()
                            .gap_4()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .when(directory, |row| {
                                row.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.load_container_directory(child_path.clone(), cx)
                                    }))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Icon::new(if directory {
                                            IconName::Folder
                                        } else {
                                            IconName::File
                                        })
                                        .small()
                                        .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(div().truncate().text_sm().child(entry.name.clone())),
                            )
                            .child(div().w(px(100.)).text_sm().child(if directory {
                                "—".into()
                            } else {
                                format_bytes(entry.size)
                            }))
                            .child(
                                div()
                                    .w(px(190.))
                                    .text_xs()
                                    .child(format_timestamp(entry.modified)),
                            )
                            .child(
                                div()
                                    .w(px(110.))
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(entry.mode.clone()),
                            ),
                    );
                }
                if listing.entries.is_empty() {
                    content = content.child(empty_state("空目录", "此目录没有文件。", cx));
                }
                if listing.truncated {
                    content = content.child(
                        muted("目录过大，仅显示扫描到的前 5000 项（非全目录排序）。", cx).p_4(),
                    );
                }
            }
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .p_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        button("container-file-parent")
                            .ghost()
                            .small()
                            .icon(IconName::ArrowUp)
                            .tooltip("上一级")
                            .disabled(path == "/" || !container.is_running())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.load_container_directory(parent.clone(), cx)
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(path.to_string()),
                    )
                    .child(muted(
                        if tools.files_loading && tools.listing.is_some() {
                            "只读 · 刷新中"
                        } else if tools.files_cached {
                            "只读 · 缓存（30 秒）"
                        } else {
                            "只读"
                        },
                        cx,
                    ))
                    .child(
                        button("container-files-refresh")
                            .ghost()
                            .small()
                            .icon(IconName::RotateCw)
                            .tooltip("刷新目录")
                            .disabled(tools.files_loading || !container.is_running())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request_container_directory(refresh_path.clone(), true, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("container-files-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .child(content),
            )
    }

    pub(super) fn render_container_stats_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tools = &self.container_tools;
        let mut view = div()
            .id("container-stats-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4();
        if !container.is_running() {
            return view.child(empty_state(
                "容器未运行",
                "启动容器后可查看实时资源使用情况。",
                cx,
            ));
        }
        view = view.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_3()
                .child(
                    muted(
                        "最近 10 分钟 · 最多 300 个采样点 · 采样完成后间隔 2 秒更新",
                        cx,
                    )
                    .flex_1(),
                )
                .child(
                    button("container-stats-view")
                        .outline()
                        .small()
                        .label(if tools.stats_table {
                            "显示图表"
                        } else {
                            "采样数据"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.container_tools.stats_table = !this.container_tools.stats_table;
                            cx.notify();
                        })),
                ),
        );
        if let Some(error) = &tools.stats_error {
            view = view.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(format!("采样失败（自动重试）：{error}")),
            );
        }
        if let Some(stats) = &tools.stats {
            let cpu = format_cpu(stats.cpu_percent);
            let history = &tools.stats_history;
            if tools.stats_table {
                let mut table = div()
                    .id("container-stats-table")
                    .overflow_x_scroll()
                    .flex()
                    .flex_col()
                    .child(stats_table_row(
                        [
                            "时间 (UTC)",
                            "CPU",
                            "内存",
                            "磁盘读取",
                            "磁盘写入",
                            "网络接收",
                            "网络发送",
                            "PID",
                        ]
                        .map(str::to_owned),
                        cx,
                    ));
                for sample in history.samples.iter().rev() {
                    let stats = &sample.stats;
                    table = table.child(stats_table_row(
                        [
                            format_timestamp(sample.timestamp),
                            format_cpu(stats.cpu_percent),
                            format_bytes(stats.memory_bytes),
                            format_bytes(stats.block_read),
                            format_bytes(stats.block_write),
                            format_bytes(stats.network_rx),
                            format_bytes(stats.network_tx),
                            stats.pids.to_string(),
                        ],
                        cx,
                    ));
                }
                view = view.child(table);
            } else {
                view = view.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .flex_shrink_0()
                        .gap_4()
                        .child(stats_chart(
                            "container-cpu-chart",
                            "CPU 使用率",
                            "100% 表示占用一个逻辑 CPU".into(),
                            vec![("CPU", cpu)],
                            history.points(|s| [s.cpu_percent, None]),
                            ChartUnit::Percent,
                            cx,
                        ))
                        .child(stats_chart(
                            "container-memory-chart",
                            "内存使用量",
                            format!("上限 {}", format_bytes(stats.memory_limit)),
                            vec![("内存", format_bytes(stats.memory_bytes))],
                            history.points(|s| [Some(s.memory_bytes as f64), None]),
                            ChartUnit::Bytes,
                            cx,
                        ))
                        .child(stats_chart(
                            "container-disk-chart",
                            "磁盘读写",
                            "自容器启动以来的累计值".into(),
                            vec![
                                ("读取", format_bytes(stats.block_read)),
                                ("写入", format_bytes(stats.block_write)),
                            ],
                            history.points(|s| {
                                [Some(s.block_read as f64), Some(s.block_write as f64)]
                            }),
                            ChartUnit::Bytes,
                            cx,
                        ))
                        .child(stats_chart(
                            "container-network-chart",
                            "网络 I/O",
                            "自容器启动以来的累计值".into(),
                            vec![
                                ("接收", format_bytes(stats.network_rx)),
                                ("发送", format_bytes(stats.network_tx)),
                            ],
                            history
                                .points(|s| [Some(s.network_rx as f64), Some(s.network_tx as f64)]),
                            ChartUnit::Bytes,
                            cx,
                        )),
                );
            }
            view = view.child(muted("网络和磁盘 I/O 为累计字节数，不是每秒速率。", cx));
            view = view.child(muted(
                format!(
                    "当前 PID：{} · 保留 {} 个采样点 · 时间轴为本地时间（24 小时制） · 未采样区间断开",
                    stats.pids,
                    history.samples.len(),
                ),
                cx,
            ));
            if let Some(updated) = tools.stats_updated {
                view = view.child(muted(
                    format!("最近成功采样：{} 秒前", updated.elapsed().as_secs()),
                    cx,
                ));
            }
        } else if tools.stats_error.is_none() {
            view = view.child(muted("正在采集资源统计…", cx));
        }
        view
    }
}

fn format_cpu(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}%"))
        .unwrap_or_else(|| "—".into())
}

fn stats_table_row(cells: [String; 8], cx: &App) -> Div {
    div()
        .flex()
        .min_w(px(1000.))
        .py_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .text_xs()
        .children(cells.into_iter().enumerate().map(|(index, text)| {
            div()
                .w(px(if index == 0 { 200. } else { 114. }))
                .flex_shrink_0()
                .px_2()
                .child(text)
        }))
}

fn directory_parent(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
        .unwrap_or("/")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        directory_parent, DirectoryCache, StatsHistory, DIRECTORY_CACHE_BYTES,
        DIRECTORY_CACHE_LIMIT, DIRECTORY_CACHE_TTL, STATS_HISTORY_LIMIT,
    };
    use rusker_desktop::{
        engine::ContainerStats,
        model::{VolumeEntry, VolumeEntryKind},
        volume::DirectoryListing,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn directory_cache_keys_include_container_and_expire() {
        let now = Instant::now();
        let mut cache = DirectoryCache::default();
        let listing = DirectoryListing {
            truncated: true,
            ..Default::default()
        };
        cache.insert("a".into(), "/".into(), listing.clone(), now);
        assert!(cache.get("a", "/", now, true).is_none());
        assert!(cache.get("b", "/", now, false).is_none());
        assert!(cache.get("a", "/other", now, false).is_none());
        assert_eq!(cache.get("a", "/", now, false), Some(listing));
        assert!(cache
            .get("a", "/", now + DIRECTORY_CACHE_TTL, false)
            .is_none());
    }

    #[test]
    fn directory_cache_is_lru_and_bounded_by_bytes() {
        let now = Instant::now();
        let mut cache = DirectoryCache::default();
        for index in 0..DIRECTORY_CACHE_LIMIT {
            cache.insert(
                "a".into(),
                format!("/{index}"),
                DirectoryListing::default(),
                now,
            );
        }
        assert!(cache.get("a", "/0", now, false).is_some());
        cache.insert("b".into(), "/".into(), DirectoryListing::default(), now);
        assert!(cache.get("a", "/1", now, false).is_none());
        assert!(cache.get("a", "/0", now, false).is_some());
        let listing = DirectoryListing {
            entries: vec![VolumeEntry {
                name: "x".repeat(DIRECTORY_CACHE_BYTES),
                size: 0,
                kind: VolumeEntryKind::File,
                modified: 0,
                mode: String::new(),
            }],
            truncated: false,
        };
        cache.insert("large".into(), "/".into(), listing, now);
        assert!(cache.get("large", "/", now, false).is_none());
        assert!(cache.entries.len() <= DIRECTORY_CACHE_LIMIT);
        assert!(
            cache.entries.iter().map(|entry| entry.bytes).sum::<usize>() <= DIRECTORY_CACHE_BYTES
        );
    }

    #[test]
    fn directory_request_clock_survives_selection_reset() {
        let mut tools = super::ContainerTools::default();
        let old_request = tools.files_revision.advance();
        let mut revision = tools.files_revision;
        revision.advance();
        tools = super::ContainerTools {
            files_revision: revision,
            ..Default::default()
        };
        let next_request = tools.files_revision.advance();
        assert!(!tools.files_revision.is_current(old_request));
        assert!(tools.files_revision.is_current(next_request));
    }

    #[test]
    fn stats_history_bounds_memory_and_time() {
        let start = Instant::now();
        let mut history = StatsHistory::default();
        for index in 0..400 {
            history.push(
                ContainerStats::default(),
                start + Duration::from_secs(index),
                index as i64,
            );
        }
        assert_eq!(history.samples.len(), STATS_HISTORY_LIMIT);
        history.push(
            ContainerStats::default(),
            start + Duration::from_secs(1100),
            1100,
        );
        assert_eq!(history.samples.len(), 1);
        assert!(!history.samples[0].connected);
    }

    #[test]
    fn stats_history_uses_real_spacing_and_preserves_missing_cpu() {
        let start = Instant::now();
        let mut history = StatsHistory::default();
        history.push(ContainerStats::default(), start, 100);
        history.push(
            ContainerStats {
                cpu_percent: Some(120.5),
                ..Default::default()
            },
            start + Duration::from_secs(3),
            103,
        );
        let points = history.points(|stats| [stats.cpu_percent, None]);
        assert_eq!(points[0].seconds, 0.);
        assert_eq!(points[0].values[0], None);
        assert_eq!(points[1].seconds, 3.);
        assert_eq!(points[1].values[0], Some(120.5));
        assert!(points[1].connected);
    }

    #[test]
    fn stats_history_breaks_on_pause_failure_or_counter_reset() {
        let start = Instant::now();
        let mut history = StatsHistory::default();
        let stats = ContainerStats {
            network_rx: 100,
            ..Default::default()
        };
        history.push(stats.clone(), start, 100);
        history.interrupted = true;
        history.push(stats.clone(), start + Duration::from_secs(2), 102);
        assert!(!history.samples[1].connected);
        history.push(stats, start + Duration::from_secs(4), 104);
        assert!(history.samples[2].connected);
        history.push(
            ContainerStats::default(),
            start + Duration::from_secs(6),
            106,
        );
        assert!(!history.samples[3].connected);
    }

    #[test]
    fn parent_directory_stays_at_root() {
        for (path, expected) in [
            ("/", "/"),
            ("/etc", "/"),
            ("/etc/nginx", "/etc"),
            ("/etc/nginx/", "/etc"),
        ] {
            assert_eq!(directory_parent(path), expected);
        }
    }
}
