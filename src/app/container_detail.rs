//! Live tools on the container detail page. All IO stays on the engine runtime.
use std::collections::VecDeque;

use crate::ui::{
    stats_chart::{stats_chart, ChartPoint, ChartUnit, StatsHover},
    terminal::TerminalView,
};
use gpui_kit::{component::scroll::ScrollableElement as _, Context};
use rusker_desktop::{
    engine::{StatsCache, StatsHistory, STATS_SAMPLE_INTERVAL},
    model::{Revision, VolumeEntryKind},
    volume::DirectoryListing,
};

use super::*;

/// Independent of the selected container/page. Only the backend cache expires.
#[derive(Default)]
pub(super) struct ContainerStatsState {
    cache: StatsCache,
    task: Option<Task<()>>,
    revision: Revision,
}

fn stats_points(
    history: &StatsHistory,
    values: impl Fn(&rusker_desktop::engine::ContainerStats) -> [Option<f64>; 2],
) -> Vec<ChartPoint> {
    let Some(first) = history.samples.front() else {
        return Vec::new();
    };
    history
        .samples
        .iter()
        .map(|sample| ChartPoint {
            seconds: sample.at.saturating_duration_since(first.at).as_secs_f64(),
            timestamp: sample.timestamp,
            values: values(&sample.stats),
            connected: sample.connected,
        })
        .collect()
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
struct InspectFields {
    env: Vec<(String, String)>,
    ports: Vec<(String, String)>,
}

fn inspect_fields(json: &str) -> Result<InspectFields, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let env = value
        .pointer("/Config/Env")
        .and_then(|env| env.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_str())
        .map(|entry| {
            let (name, value) = entry.split_once('=').unwrap_or((entry, ""));
            (name.to_string(), value.to_string())
        })
        .collect();
    let mut ports = std::collections::BTreeMap::<String, Vec<String>>::new();
    for pointer in [
        "/Config/ExposedPorts",
        "/HostConfig/PortBindings",
        "/NetworkSettings/Ports",
    ] {
        if let Some(object) = value.pointer(pointer).and_then(|ports| ports.as_object()) {
            for port in object.keys() {
                ports.entry(port.clone()).or_default();
            }
        }
    }
    for (port, bindings) in &mut ports {
        // Prefer the daemon's actual bindings (including dynamically allocated
        // host ports), falling back to configured bindings for stopped containers.
        let runtime = value
            .pointer("/NetworkSettings/Ports")
            .and_then(|ports| ports.get(port))
            .and_then(|bindings| bindings.as_array())
            .filter(|bindings| !bindings.is_empty());
        let configured = value
            .pointer("/HostConfig/PortBindings")
            .and_then(|ports| ports.get(port))
            .and_then(|bindings| bindings.as_array());
        for binding in runtime.or(configured).into_iter().flatten() {
            let Some(host_port) = binding.get("HostPort").and_then(|port| port.as_str()) else {
                continue;
            };
            let host_port = if host_port.is_empty() || host_port == "0" {
                "自动分配"
            } else {
                host_port
            };
            let ip = binding
                .get("HostIp")
                .and_then(|ip| ip.as_str())
                .filter(|ip| !ip.is_empty())
                .unwrap_or("0.0.0.0");
            let address = if ip.contains(':') {
                format!("[{ip}]:{host_port}")
            } else {
                format!("{ip}:{host_port}")
            };
            if !bindings.contains(&address) {
                bindings.push(address);
            }
        }
    }
    Ok(InspectFields {
        env,
        ports: ports
            .into_iter()
            .map(|(port, bindings)| {
                (
                    port,
                    if bindings.is_empty() {
                        "未绑定".into()
                    } else {
                        bindings.join(", ")
                    },
                )
            })
            .collect(),
    })
}

#[derive(Default)]
pub(super) struct ContainerTools {
    pub(super) inspect_summary: bool,
    inspect_loading: bool,
    inspect_error: Option<String>,
    inspect_revision: Revision,
    inspect_task: Option<Task<()>>,
    inspect_view: Option<Entity<crate::ui::json_view::JsonView>>,
    inspect_fields: Option<InspectFields>,
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
    stats_error: Option<String>,
    stats_history: StatsHistory,
    stats_hover: Option<Entity<StatsHover>>,
}

impl RuskerApp {
    pub(super) fn reset_container_tools(&mut self) {
        // Keep the request clock monotonic even across A -> B -> A selections.
        let mut files_revision = self.container_tools.files_revision;
        files_revision.advance();
        let mut inspect_revision = self.container_tools.inspect_revision;
        inspect_revision.advance();
        let files_cache = std::mem::take(&mut self.container_tools.files_cache);
        self.container_tools = ContainerTools {
            inspect_revision,
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
        // Statistics belong to the cache/open view, not to the live shell/files.
        tools.stats_error = None;
        tools.stats_hover = None;
        tools.files_revision.advance();
        tools.files_task = None;
        tools.files_loading = false;
        tools.listing = None;
        tools.files_cache = DirectoryCache::default();
        tools.files_cached = false;
        self.container_terminal = None;
    }

    pub(super) fn sync_container_tools(&mut self, cx: &mut Context<Self>) {
        self.start_background_container_stats(cx);
        if !self.selected_container_running() {
            self.stop_container_live_tools();
        } else {
            self.select_container_tab(self.container_tab, cx);
        }
    }

    pub(super) fn select_container_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        let tab = tab.min(5);
        let entering_stats = tab == 5 && self.container_tab != 5;
        self.container_tab = tab;
        if tab == 5 && self.container_tools.stats_hover.is_none() {
            let hover = cx.new(|_| StatsHover::default());
            cx.observe(&hover, |_, _, cx| cx.notify()).detach();
            self.container_tools.stats_hover = Some(hover);
        }
        if entering_stats {
            self.load_cached_container_stats();
        } else if tab != 5 {
            self.container_tools.stats_hover = None;
            self.container_tools.stats_history = StatsHistory::default();
            self.container_tools.stats_error = None;
        }
        match tab {
            1 if self.container_tools.inspect_view.is_none()
                && !self.container_tools.inspect_loading =>
            {
                self.load_container_inspect(cx);
            }
            3 if self.container_tools.shell_task.is_none() => self.connect_container_shell(cx),
            4 if self.container_tools.listing.is_none() && !self.container_tools.files_loading => {
                let path = if self.container_tools.directory.is_empty() {
                    "/".into()
                } else {
                    self.container_tools.directory.clone()
                };
                self.load_container_directory(path, cx);
            }
            5 => self.start_background_container_stats(cx),
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn load_container_inspect(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_container.clone() else {
            return;
        };
        let revision = self.container_tools.inspect_revision.advance();
        self.container_tools.inspect_task = None;
        self.container_tools.inspect_error = None;
        let Some(engine) = self.engine.clone() else {
            self.container_tools.inspect_loading = false;
            self.container_tools.inspect_error = Some("容器引擎不可用，请连接引擎后重试。".into());
            cx.notify();
            return;
        };
        self.container_tools.inspect_loading = true;
        let handle = engine.container_inspect(id.clone());
        self.container_tools.inspect_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if this.selected_container.as_ref() != Some(&id)
                    || !this.container_tools.inspect_revision.is_current(revision)
                {
                    return;
                }
                this.container_tools.inspect_loading = false;
                match result {
                    Ok(json) => match inspect_fields(&json) {
                        Ok(fields) => {
                            this.container_tools.inspect_fields = Some(fields);
                            this.container_tools.inspect_view =
                                Some(cx.new(|_| crate::ui::json_view::JsonView::new(json)));
                        }
                        Err(error) => {
                            this.container_tools.inspect_error =
                                Some(format!("解析容器详情失败：{error}"))
                        }
                    },
                    Err(error) => this.container_tools.inspect_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_container_inspect_tab(&self, cx: &mut Context<Self>) -> Div {
        let tools = &self.container_tools;
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .when(tools.inspect_loading, |view| {
                view.child(muted("正在读取容器详情…", cx).p_4())
            })
            .when_some(tools.inspect_error.as_ref(), |view, error| {
                view.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                )
            })
            .when_some(tools.inspect_view.clone(), |view, json| view.child(json))
    }

    pub(super) fn render_container_inspect_fields(&self, cx: &mut Context<Self>) -> Div {
        let tools = &self.container_tools;
        let mut view = div()
            .flex()
            .flex_col()
            .gap_4()
            .when(tools.inspect_loading, |view| {
                view.child(muted("正在读取容器详情…", cx))
            })
            .when_some(tools.inspect_error.as_ref(), |view, error| {
                view.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                )
            });
        let Some(fields) = &tools.inspect_fields else {
            return view;
        };
        let header = |left: &'static str, right: &'static str| {
            div()
                .flex()
                .gap_4()
                .px_4()
                .py_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(div().w(px(220.)).flex_shrink_0().child(left))
                .child(div().flex_1().min_w_0().child(right))
        };
        let mut env = section_panel(cx)
            .bg(cx.theme().background)
            .flex()
            .flex_col()
            .child(div().px_4().py_3().font_semibold().child("环境变量"))
            .child(header("变量名", "变量值").child(div().w(px(32.))));
        for (index, (name, value)) in fields.env.iter().enumerate() {
            let entry = format!("{name}={value}");
            env = env.child(
                div()
                    .flex()
                    .items_start()
                    .gap_4()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(div().w(px(220.)).flex_shrink_0().child(name.clone()))
                    .child(div().flex_1().min_w_0().child(value.clone()))
                    .child(
                        button(("copy-inspect-env", index))
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .tooltip("复制环境变量（名称=值）")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(entry.clone()))
                            }),
                    ),
            );
        }
        if fields.env.is_empty() {
            env = env.child(muted("没有环境变量。", cx).px_4().py_3());
        }
        let mut ports = section_panel(cx)
            .bg(cx.theme().background)
            .flex()
            .flex_col()
            .child(div().px_4().py_3().font_semibold().child("端口"))
            .child(header("镜像暴露的端口", "绑定的端口"));
        for (exposed, bound) in &fields.ports {
            ports = ports.child(
                div()
                    .flex()
                    .gap_4()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(div().w(px(220.)).flex_shrink_0().child(exposed.clone()))
                    .child(div().flex_1().min_w_0().child(bound.clone())),
            );
        }
        if fields.ports.is_empty() {
            ports = ports.child(muted("没有暴露或绑定的端口。", cx).px_4().py_3());
        }
        view = view.child(env).child(ports);
        view
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
        if !self.selected_container_running()
            || self.container_tools.shell_task.is_some()
            || self.container_tools.shell_connecting
        {
            return;
        }
        let (Some(engine), Some(id)) = (self.engine.clone(), self.selected_container.clone())
        else {
            return;
        };
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

    fn load_cached_container_stats(&mut self) {
        self.container_stats.cache.prune(Instant::now());
        self.container_tools.stats_history = self
            .selected_container
            .as_deref()
            .and_then(|id| self.container_stats.cache.history(id))
            .cloned()
            .unwrap_or_default();
        self.container_tools.stats_error = self
            .selected_container
            .as_deref()
            .and_then(|id| self.container_stats.cache.error(id))
            .map(str::to_owned);
    }

    /// Called for every authoritative snapshot, including deletion and stop actions.
    pub(super) fn reconcile_container_stats(&mut self, containers: &[ContainerSummary]) {
        self.container_stats.revision.advance();
        self.container_stats
            .cache
            .reconcile(containers, Instant::now());
    }

    fn stats_view_open(&self) -> bool {
        self.page == Page::Containers
            && self.selected_container.is_some()
            && self.container_tab == 5
    }

    fn can_sample_container_stats(&self) -> bool {
        self.connected
            && !self.quitting
            && self.engine_transition.is_none()
            && self.runtime_busy.is_none()
            && !self.resource_saver_active()
    }

    fn start_background_container_stats(&mut self, cx: &mut Context<Self>) {
        if self.container_stats.task.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        self.container_stats.task = Some(cx.spawn(async move |this, cx| loop {
            let started = Instant::now();
            let revision = match this.update(cx, |this, _| {
                this.container_stats.cache.prune(started);
                if this.can_sample_container_stats() {
                    Some((
                        this.container_stats.revision.current(),
                        this.refresh_revision.current(),
                    ))
                } else {
                    this.container_stats.cache.interrupt_all();
                    None
                }
            }) {
                Ok(revision) => revision,
                Err(_) => return,
            };
            if let Some((revision, refresh_revision)) = revision {
                let result = await_engine(engine.sample_container_stats()).await;
                if this
                    .update(cx, |this, cx| {
                        // A mutation/refresh/lifecycle transition won the race. Do not let
                        // an old list or delayed sample resurrect a removed/stopped container.
                        if !this.container_stats.revision.is_current(revision)
                            || !this.refresh_revision.is_current(refresh_revision)
                            || !this.can_sample_container_stats()
                        {
                            this.container_stats.cache.interrupt_all();
                            return;
                        }
                        let open = this.stats_view_open();
                        match result {
                            Ok(batch) => {
                                if this.snapshot.containers != batch.containers {
                                    let mut snapshot = this.snapshot.clone();
                                    snapshot.containers = batch.containers;
                                    this.apply_snapshot(snapshot);
                                    this.sync_container_tools(cx);
                                } else {
                                    this.container_stats
                                        .cache
                                        .reconcile(&batch.containers, Instant::now());
                                }
                                for (id, result) in batch.samples {
                                    match result {
                                        Ok(sample) => {
                                            if let Some(sample) = this.container_stats.cache.push(
                                                &id,
                                                sample.stats,
                                                sample.at,
                                                sample.timestamp,
                                            ) {
                                                if open
                                                    && this.selected_container.as_ref() == Some(&id)
                                                {
                                                    this.container_tools
                                                        .stats_history
                                                        .append(sample);
                                                    this.container_tools.stats_error = None;
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            this.container_stats.cache.fail(&id, error.to_string());
                                            if open && this.selected_container.as_ref() == Some(&id)
                                            {
                                                this.container_tools.stats_error = this
                                                    .container_stats
                                                    .cache
                                                    .error(&id)
                                                    .map(str::to_owned);
                                            }
                                        }
                                    }
                                }
                                // Requests can take long enough for earlier samples to expire.
                                this.container_stats.cache.prune(Instant::now());
                            }
                            Err(error) => {
                                this.container_stats.cache.interrupt_all();
                                if open {
                                    this.container_tools.stats_error = Some(error.to_string());
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Fixed start-to-start cadence, not ten seconds after request completion.
            // Slow requests skip missed ticks rather than accumulating overlapping IO.
            cx.background_executor()
                .timer(STATS_SAMPLE_INTERVAL.saturating_sub(started.elapsed()))
                .await;
        }));
    }

    pub(super) fn render_container_exec_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> Div {
        let tools = &self.container_tools;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .when(tools.shell_connecting, |view| {
                view.child(muted("正在连接 /bin/sh…", cx).px_4().py_2())
            })
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
                    .overflow_y_scrollbar()
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
            .id("container-stats")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .p_4()
            .flex()
            .flex_col()
            .gap_4();
        if !container.is_running() {
            return view.child(empty_state(
                "容器未运行",
                "启动容器后可查看资源统计。历史缓存仍按一小时规则保留和清理。",
                cx,
            ));
        }
        if let Some(error) = &tools.stats_error {
            view = view.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(format!("采样失败（自动重试）：{error}")),
            );
        }
        if let Some(latest) = tools.stats_history.samples.back() {
            let stats = &latest.stats;
            let cpu = format_cpu(stats.cpu_percent);
            let history = &tools.stats_history;
            let Some(hover) = &tools.stats_hover else {
                return view;
            };
            view = view.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .grid()
                    .grid_cols(2)
                    .grid_rows(2)
                    .gap_4()
                    .child(stats_chart(
                        "container-cpu-chart",
                        "CPU 使用率",
                        "100% 表示占用一个逻辑 CPU".into(),
                        vec![("CPU", cpu)],
                        stats_points(history, |s| [s.cpu_percent, None]),
                        ChartUnit::Percent,
                        hover.clone(),
                    ))
                    .child(stats_chart(
                        "container-memory-chart",
                        "内存使用量",
                        format!("上限 {}", format_bytes(stats.memory_limit)),
                        vec![("内存", format_bytes(stats.memory_bytes))],
                        stats_points(history, |s| [Some(s.memory_bytes as f64), None]),
                        ChartUnit::Bytes,
                        hover.clone(),
                    ))
                    .child(stats_chart(
                        "container-disk-chart",
                        "磁盘读写",
                        "自容器启动以来的累计值".into(),
                        vec![
                            ("读取", format_bytes(stats.block_read)),
                            ("写入", format_bytes(stats.block_write)),
                        ],
                        stats_points(history, |s| {
                            [Some(s.block_read as f64), Some(s.block_write as f64)]
                        }),
                        ChartUnit::Bytes,
                        hover.clone(),
                    ))
                    .child(stats_chart(
                        "container-network-chart",
                        "网络 I/O",
                        "自容器启动以来的累计值".into(),
                        vec![
                            ("接收", format_bytes(stats.network_rx)),
                            ("发送", format_bytes(stats.network_tx)),
                        ],
                        stats_points(history, |s| {
                            [Some(s.network_rx as f64), Some(s.network_tx as f64)]
                        }),
                        ChartUnit::Bytes,
                        hover.clone(),
                    )),
            );
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
        directory_parent, stats_points, DirectoryCache, StatsHistory, DIRECTORY_CACHE_BYTES,
        DIRECTORY_CACHE_LIMIT, DIRECTORY_CACHE_TTL,
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
    fn inspect_fields_preserve_env_values_and_resolve_port_bindings() {
        let fields = super::inspect_fields(&serde_json::json!({
            "Config": {"Env": ["TOKEN=a=b", "EMPTY=", "BARE"], "ExposedPorts": {"80/tcp": {}, "53/udp": {}}},
            "HostConfig": {"PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "0"}], "443/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8443"}]}},
            "NetworkSettings": {"Ports": {"80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "32768"}, {"HostIp": "::", "HostPort": "32768"}], "53/udp": null}}
        }).to_string()).unwrap();
        assert_eq!(
            fields.env,
            [
                ("TOKEN".into(), "a=b".into()),
                ("EMPTY".into(), "".into()),
                ("BARE".into(), "".into())
            ]
        );
        assert!(fields
            .ports
            .contains(&("80/tcp".into(), "0.0.0.0:32768, [::]:32768".into())));
        assert!(fields.ports.contains(&("53/udp".into(), "未绑定".into())));
        assert!(fields
            .ports
            .contains(&("443/tcp".into(), "127.0.0.1:8443".into())));
    }

    #[test]
    fn inspect_fields_handle_missing_data_and_pending_dynamic_ports() {
        let fields = super::inspect_fields("{}").unwrap();
        assert!(fields.env.is_empty());
        assert!(fields.ports.is_empty());
        let fields = super::inspect_fields(r#"{"HostConfig":{"PortBindings":{"80/tcp":[{"HostIp":"","HostPort":""}]}},"NetworkSettings":{"Ports":{"80/tcp":null}}}"#).unwrap();
        assert_eq!(fields.ports, [("80/tcp".into(), "0.0.0.0:自动分配".into())]);
    }

    #[test]
    fn inspect_defaults_to_json_without_loading() {
        let tools = super::ContainerTools::default();
        assert!(!tools.inspect_summary);
        assert!(!tools.inspect_loading);
        assert!(tools.inspect_view.is_none());
    }

    #[test]
    fn inspect_request_clock_survives_selection_reset() {
        let mut tools = super::ContainerTools::default();
        let old_request = tools.inspect_revision.advance();
        let mut revision = tools.inspect_revision;
        revision.advance();
        tools = super::ContainerTools {
            inspect_revision: revision,
            ..Default::default()
        };
        let next_request = tools.inspect_revision.advance();
        assert!(!tools.inspect_revision.is_current(old_request));
        assert!(tools.inspect_revision.is_current(next_request));
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
        let points = stats_points(&history, |stats| [stats.cpu_percent, None]);
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
        history.interrupt();
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
