use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    button::{Button, ButtonVariant, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    skeleton::Skeleton,
    slider::{Slider, SliderEvent, SliderState},
    switch::Switch,
    window_border, ActiveTheme as _, Icon, IconName, Sizable as _, Size, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div, ease_out_quint, percentage,
    prelude::FluentBuilder as _,
    pulsating_between, px, rgb, Animation, AnimationExt as _, AnyElement, App, AppContext as _,
    Context, Div, Entity, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Transformation, Window,
    WindowAppearance, WindowControlArea,
};
use rusker_desktop::{
    container::ContainerDraft,
    engine::{EngineError, EngineService},
    format::{
        container_matches, format_bytes, format_ports, format_timestamp, image_matches, image_tags,
        short_id,
    },
    model::{ContainerAction, ContainerSummary, EngineSnapshot, ImageSummary, Revision},
    runtime_host::{
        same_path, ProxyApplyOutcome, RuntimeHostError, RuntimeHostService, RuntimeHostSnapshot,
        MANAGED_DOCKER_HOST, WSL_DOCKER_HOST,
    },
    settings::{
        AppSettings, DockerProxySettings, SettingsStore, ThemePreference, RESOURCE_SAVER_MAX_SECS,
        RESOURCE_SAVER_MIN_SECS,
    },
    tray::{TrayCommand, TrayMenu},
};
use tokio::task::JoinHandle;

use crate::ui::{
    detail_row, empty_state, message_banner, muted, page_header, section_panel, stat_card,
};

mod volumes;

use volumes::VolumeState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Containers,
    Images,
    Volumes,
    Settings,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Containers => "容器",
            Self::Images => "镜像",
            Self::Volumes => "卷",
            Self::Settings => "设置",
        }
    }
}

/// Sections of the settings page, in scroll order (Docker Desktop-style
/// right-hand navigation).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    VmResources,
    ResourceSaver,
    Proxies,
    WslIntegration,
}

impl SettingsSection {
    fn visible() -> Vec<Self> {
        let mut sections = vec![
            Self::General,
            Self::VmResources,
            Self::ResourceSaver,
            Self::Proxies,
        ];
        if cfg!(windows) {
            sections.push(Self::WslIntegration);
        }
        sections
    }

    fn title(self) -> &'static str {
        match self {
            Self::General => "常规",
            Self::VmResources => "VM 资源",
            Self::ResourceSaver => "资源节省",
            Self::Proxies => "代理",
            Self::WslIntegration => "WSL 集成",
        }
    }
}

const WSLCONFIG_DOCS_URL: &str =
    "https://learn.microsoft.com/windows/wsl/wsl-config#configuration-settings-for-wslconfig";

#[derive(Clone, Copy)]
enum RuntimeOperation {
    Install,
    Start,
    Stop,
}

impl RuntimeOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Install => "安装/修复 WSL2 环境",
            Self::Start => "启动 WSL2 环境",
            Self::Stop => "停止 WSL2 环境",
        }
    }
}

/// Engine lifecycle phases that get the full-page loading animation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EngineTransition {
    /// The managed WSL2 runtime is starting (launch, install or start).
    Starting,
    /// The runtime is ready (or external); waiting on the Docker API.
    Connecting,
    /// The managed runtime is shutting down.
    Stopping,
}

impl EngineTransition {
    fn key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Connecting => "connecting",
            Self::Stopping => "stopping",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Starting => "正在启动 Docker Engine…",
            Self::Connecting => "正在连接 Docker Engine…",
            Self::Stopping => "正在停止 Docker Engine…",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Starting | Self::Connecting => "Docker Engine 是运行容器的底层技术",
            Self::Stopping => "运行中的容器将被停止，Engine 关闭后可在设置中重新启动",
        }
    }
}

pub struct RuskerApp {
    engine: Option<EngineService>,
    /// Set while a managed runtime start/stop is in flight.
    engine_transition: Option<EngineTransition>,
    /// Quit requested; the engine is being stopped before exiting.
    quitting: bool,
    /// The managed engine was already stopped by the quit flow, so the
    /// blocking fallback in `on_app_quit` can be skipped.
    shutdown_done: bool,
    /// Clock for loading indicators. They are driven by hand instead of
    /// `with_animation`, which freezes when the OS asks for reduced motion
    /// (Windows "Animation effects" off) — a progress spinner must move.
    motion_epoch: Instant,
    page: Page,
    snapshot: EngineSnapshot,
    connected: bool,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    busy: Option<String>,
    selected_container: Option<String>,
    logs: String,
    logs_loading: bool,
    logs_error: Option<String>,
    search_input: Entity<InputState>,
    pull_input: Entity<InputState>,
    create_image_input: Entity<InputState>,
    create_name_input: Entity<InputState>,
    create_command_input: Entity<InputState>,
    create_ports_input: Entity<InputState>,
    create_nvidia_gpu: bool,
    create_error: Option<String>,
    volumes: VolumeState,
    volume_name_input: Entity<InputState>,
    settings: AppSettings,
    /// Edits not yet applied. Proxy addresses and the Resource Saver
    /// threshold live in their input states instead.
    settings_draft: AppSettings,
    settings_store: Option<SettingsStore>,
    settings_warning: Option<String>,
    settings_notice: Option<String>,
    settings_error: Option<String>,
    settings_scroll: ScrollHandle,
    settings_nav_target: Option<usize>,
    http_proxy_input: Entity<InputState>,
    https_proxy_input: Entity<InputState>,
    no_proxy_input: Entity<InputState>,
    saver_threshold_input: Entity<InputState>,
    saver_slider: Entity<SliderState>,
    /// Last Resource Saver state the UI reacted to.
    saver_seen: bool,
    _saver_watch_task: Task<()>,
    runtime_host: Option<RuntimeHostService>,
    runtime_snapshot: RuntimeHostSnapshot,
    runtime_busy: Option<String>,
    runtime_error: Option<String>,
    runtime_notice: Option<String>,
    tray: Option<TrayMenu>,
    _tray_task: Option<Task<()>>,
    refresh_revision: Revision,
    logs_revision: Revision,
    _refresh_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _logs_task: Option<Task<()>>,
    _runtime_task: Option<Task<()>>,
    _quit_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RuskerApp {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        settings_store: Option<SettingsStore>,
        settings: AppSettings,
        mut settings_warning: Option<String>,
    ) -> Self {
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索名称、ID、镜像或状态"));
        let pull_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例如：nginx:1.29 或 docker.io/library/nginx:latest")
        });
        let create_image_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例如：alpine:3.23 或 nginx:1.29"));
        let create_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("留空则由 Engine 自动命名"));
        let create_command_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("例如：sh -c \"echo hello; sleep 60\"")
        });
        let create_ports_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例如：8080:80, 8443:443"));
        let volume_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例如：postgres-data"));
        let http_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.docker_proxy.http_proxy.clone())
                .placeholder("例如：http://127.0.0.1:8080")
        });
        let https_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.docker_proxy.https_proxy.clone())
                .placeholder("通常与 HTTP 代理地址相同")
        });
        let no_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.docker_proxy.no_proxy.clone())
                .placeholder("localhost,127.0.0.1,::1")
        });
        let threshold = settings.resource_saver.threshold_secs;
        let saver_threshold_input =
            cx.new(|cx| InputState::new(window, cx).default_value(threshold.to_string()));
        let saver_slider = cx.new(|_| {
            SliderState::new()
                .min(RESOURCE_SAVER_MIN_SECS as f32)
                .max(RESOURCE_SAVER_MAX_SECS as f32)
                .step(30.)
                .default_value(threshold as f32)
        });
        let mut subscriptions = Vec::new();
        for input in [&http_proxy_input, &https_proxy_input, &no_proxy_input] {
            subscriptions.push(cx.subscribe_in(input, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.subscribe_in(
            &saver_threshold_input,
            window,
            |this, input, event, window, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if let Ok(seconds) = input.read(cx).value().trim().parse::<u64>() {
                    if (RESOURCE_SAVER_MIN_SECS..=RESOURCE_SAVER_MAX_SECS).contains(&seconds) {
                        this.saver_slider.update(cx, |slider, cx| {
                            slider.set_value(seconds as f32, window, cx);
                        });
                    }
                }
                cx.notify();
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &saver_slider,
            window,
            |this, _, event: &SliderEvent, window, cx| {
                let SliderEvent::Change(value) = event else {
                    return;
                };
                let seconds = value.start().round() as u64;
                this.saver_threshold_input.update(cx, |input, cx| {
                    input.set_value(seconds.to_string(), window, cx);
                });
                cx.notify();
            },
        ));
        let saver_watch_task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            if this
                .update(cx, |this, cx| this.sync_resource_saver(cx))
                .is_err()
            {
                break;
            }
        });
        let subscription = cx.subscribe_in(&search_input, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let weak = cx.weak_entity();
        let appearance_subscription = window.observe_window_appearance(move |_, app_cx| {
            let follow_system = weak
                .update(app_cx, |this, _| {
                    this.settings_draft.theme == ThemePreference::System
                })
                .unwrap_or(false);
            if follow_system {
                let mode = ThemeMode::from(app_cx.window_appearance());
                Theme::change(mode, None, app_cx);
            }
        });
        let (engine, error) = match EngineService::new() {
            Ok(engine) => (Some(engine), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let (runtime_host, runtime_error) = match RuntimeHostService::new() {
            Ok(host) => {
                host.set_engine_proxy(&settings.docker_proxy);
                host.set_resource_saver(&settings.resource_saver);
                host.set_data_location(disk_location_setting(&settings));
                (Some(host), None)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        // Like Docker Desktop, quitting the app stops the engine it owns.
        // Normally `request_quit` already did so with an animation; this is
        // the fallback for other quit paths (e.g. OS logoff). Runs
        // synchronously: GPUI only waits ~200 ms for quit futures.
        let quit_subscription = cx.on_app_quit(|this: &mut Self, _| {
            if !this.shutdown_done {
                if let Some(host) = this.runtime_host.clone() {
                    host.shutdown_blocking(Duration::from_secs(50));
                }
            }
            async {}
        });

        let (tray, tray_error) = match TrayMenu::new() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(error)),
        };
        let has_tray = tray.is_some();
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            if has_tray {
                window.minimize_window();
            } else {
                _ = weak.update(cx, |this, cx| this.request_quit(cx));
            }
            false
        });
        if let Some(error) = tray_error {
            let warning = match settings_warning.take() {
                Some(existing) => format!("{existing}\n{error}"),
                None => error,
            };
            settings_warning = Some(warning);
        }

        Self {
            engine,
            engine_transition: None,
            quitting: false,
            shutdown_done: false,
            motion_epoch: Instant::now(),
            page: Page::Overview,
            snapshot: EngineSnapshot::default(),
            connected: false,
            loading: false,
            error,
            notice: None,
            busy: None,
            selected_container: None,
            logs: String::new(),
            logs_loading: false,
            logs_error: None,
            search_input,
            pull_input,
            create_image_input,
            create_name_input,
            create_command_input,
            create_ports_input,
            create_nvidia_gpu: false,
            create_error: None,
            volumes: VolumeState::default(),
            volume_name_input,
            settings_draft: settings.clone(),
            settings,
            settings_store,
            settings_warning,
            settings_notice: None,
            settings_error: None,
            settings_scroll: ScrollHandle::new(),
            settings_nav_target: None,
            http_proxy_input,
            https_proxy_input,
            no_proxy_input,
            saver_threshold_input,
            saver_slider,
            saver_seen: false,
            _saver_watch_task: saver_watch_task,
            runtime_host,
            runtime_snapshot: RuntimeHostSnapshot::default(),
            runtime_busy: None,
            runtime_error,
            runtime_notice: None,
            tray,
            _tray_task: None,
            refresh_revision: Revision::default(),
            logs_revision: Revision::default(),
            _refresh_task: None,
            _action_task: None,
            _logs_task: None,
            _runtime_task: None,
            _quit_task: None,
            _subscriptions: {
                subscriptions.extend([subscription, appearance_subscription, quit_subscription]);
                subscriptions
            },
        }
    }

    pub fn start_tray_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tray.is_none() {
            return;
        }
        self._tray_task = Some(cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let running = this
                .update_in(cx, |this, window, cx| {
                    let commands = this
                        .tray
                        .as_ref()
                        .map(TrayMenu::drain_commands)
                        .unwrap_or_default();
                    for command in commands {
                        this.handle_tray_command(command, window, cx);
                    }
                    this.tray.is_some()
                })
                .unwrap_or(false);
            if !running {
                break;
            }
        }));
    }

    fn handle_tray_command(
        &mut self,
        command: TrayCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            TrayCommand::Show => window.activate_window(),
            TrayCommand::Minimize => window.minimize_window(),
            TrayCommand::Refresh => {
                window.activate_window();
                self.refresh(cx);
            }
            TrayCommand::Containers => {
                window.activate_window();
                self.navigate(Page::Containers, cx);
            }
            TrayCommand::Images => {
                window.activate_window();
                self.navigate(Page::Images, cx);
            }
            TrayCommand::Settings => {
                window.activate_window();
                self.navigate(Page::Settings, cx);
            }
            TrayCommand::Quit => {
                window.activate_window();
                self.request_quit(cx);
            }
        }
    }

    /// Quits the app, first stopping a managed engine with the stopping
    /// animation on screen instead of freezing in the quit hook.
    fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            cx.quit();
            return;
        };
        if !host.engine_session_running() && self.runtime_busy.is_none() {
            self.shutdown_done = true;
            cx.quit();
            return;
        }
        self.quitting = true;
        cx.notify();
        let handle = host.shutdown(Duration::from_secs(60));
        self._quit_task = Some(cx.spawn(async move |this, cx| {
            _ = handle.await;
            _ = this.update(cx, |this, cx| {
                this.shutdown_done = true;
                cx.quit();
            });
        }));
    }

    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        let entering_volumes = page == Page::Volumes && self.page != Page::Volumes;
        self.page = page;
        if page == Page::Settings {
            self.refresh_runtime_host(cx);
        }
        if entering_volumes && self.connected {
            self.after_volume_snapshot(cx);
        }
        cx.notify();
    }

    /// Launch-time hook: refreshes state and starts an installed managed
    /// engine, unless `DOCKER_HOST` points the app at another engine.
    pub fn startup_runtime_host(&mut self, cx: &mut Context<Self>) {
        if !cfg!(windows) || self.runtime_busy.is_some() {
            return;
        }
        if std::env::var_os("DOCKER_HOST").is_some() {
            self.refresh_runtime_host(cx);
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.runtime_busy = Some("正在检查并启动 WSL2 环境…".into());
        self.engine_transition = Some(EngineTransition::Starting);
        self.runtime_error = None;
        cx.notify();
        let handle = host.startup(self.settings.wsl.integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                this.engine_transition = None;
                match result {
                    Ok(snapshot) => {
                        let ready = snapshot.engine_ready;
                        this.runtime_snapshot = snapshot;
                        if ready {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => this.runtime_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    pub fn refresh_runtime_host(&mut self, cx: &mut Context<Self>) {
        if !cfg!(windows) || self.runtime_busy.is_some() {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.runtime_busy = Some("读取 WSL2 环境状态…".into());
        self.runtime_error = None;
        cx.notify();
        let handle = host.snapshot(self.settings.wsl.integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                match result {
                    Ok(snapshot) => {
                        // E.g. the engine just left Resource Saver mode.
                        let reconnect = snapshot.engine_ready && !this.connected;
                        this.runtime_snapshot = snapshot;
                        if reconnect {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => this.runtime_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn resource_saver_active(&self) -> bool {
        self.manages_engine()
            && self
                .runtime_host
                .as_ref()
                .is_some_and(RuntimeHostService::resource_saver_active)
    }

    /// Polled every few seconds: reacts when the backend enters Resource
    /// Saver mode, or leaves it because a client connected.
    fn sync_resource_saver(&mut self, cx: &mut Context<Self>) {
        let active = self.resource_saver_active();
        if active == self.saver_seen {
            return;
        }
        self.saver_seen = active;
        if active {
            self.connected = false;
            self.runtime_snapshot.engine_ready = false;
            self.runtime_snapshot.running = false;
            self.runtime_snapshot.resource_saver_active = true;
            cx.notify();
        } else {
            // Waits on the wake-up (it holds the operation lock), then
            // reconnects once the engine is ready.
            self.refresh_runtime_host(cx);
        }
    }

    fn run_runtime_operation(&mut self, operation: RuntimeOperation, cx: &mut Context<Self>) {
        if self.runtime_busy.is_some() {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            self.runtime_error = Some("WSL2 后台运行时不可用".into());
            cx.notify();
            return;
        };
        self.runtime_busy = Some(format!("{}…", operation.label()));
        self.engine_transition = Some(match operation {
            RuntimeOperation::Install | RuntimeOperation::Start => EngineTransition::Starting,
            RuntimeOperation::Stop => EngineTransition::Stopping,
        });
        self.runtime_error = None;
        self.runtime_notice = None;
        cx.notify();
        let integrations = self.settings.wsl.integrations.clone();
        let handle = match operation {
            RuntimeOperation::Install => host.install(integrations),
            RuntimeOperation::Start => host.start(integrations),
            RuntimeOperation::Stop => host.stop(integrations),
        };
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                this.engine_transition = None;
                match result {
                    Ok(snapshot) => {
                        let ready = snapshot.engine_ready;
                        this.runtime_snapshot = snapshot;
                        this.runtime_notice = Some(format!("{}完成", operation.label()));
                        if matches!(
                            operation,
                            RuntimeOperation::Install | RuntimeOperation::Start
                        ) && ready
                        {
                            this.refresh(cx);
                        }
                        if matches!(operation, RuntimeOperation::Stop) {
                            this.connected = false;
                        }
                    }
                    Err(error) => this.runtime_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn set_runtime_integration(
        &mut self,
        distribution: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if self.runtime_busy.is_some() {
            return;
        }
        if self.settings_store.is_none() {
            self.runtime_error = Some("无法确定配置目录，发行版集成未修改".into());
            cx.notify();
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            self.runtime_error = Some("WSL2 后台运行时不可用".into());
            cx.notify();
            return;
        };
        let mut integrations = self.settings.wsl.integrations.clone();
        if enabled {
            integrations.insert(distribution.clone(), true);
        } else {
            integrations.remove(&distribution);
        }
        self.runtime_busy = Some(format!(
            "{} {distribution} 集成…",
            if enabled { "启用" } else { "停用" }
        ));
        self.runtime_error = None;
        self.runtime_notice = None;
        cx.notify();
        let handle = host.set_integration(distribution.clone(), enabled, integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                match result {
                    Ok(snapshot) => {
                        this.runtime_snapshot = snapshot;
                        this.settings.wsl.integrations = integrations;
                        let action = if enabled { "启用" } else { "停用" };
                        this.runtime_notice = Some(format!("已{action} {distribution} 集成"));
                        if let Some(store) = &this.settings_store {
                            if let Err(error) = store.save(&this.settings) {
                                this.runtime_error =
                                    Some(format!("集成已生效，但设置保存失败：{error}"));
                            }
                        }
                    }
                    Err(error) => this.runtime_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn nvidia_gpu_available(&self) -> bool {
        #[cfg(windows)]
        if std::env::var_os("DOCKER_HOST").is_none() {
            return self.runtime_snapshot.gpu.available;
        }
        true
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.loading = false;
            self.error = Some("后台运行时不可用，请重启应用".into());
            cx.notify();
            return;
        };

        let revision = self.refresh_revision.advance();
        self.loading = true;
        self.error = None;
        self.notice = None;
        cx.notify();
        let handle = engine.snapshot();
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(snapshot) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot);
                        this.after_volume_snapshot(cx);
                    }
                    Err(error) => {
                        this.connected = false;
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply_snapshot(&mut self, snapshot: EngineSnapshot) {
        if let Some(selected) = self.selected_container.as_ref() {
            if !snapshot
                .containers
                .iter()
                .any(|container| &container.id == selected)
            {
                self.selected_container = None;
                self.logs_revision.advance();
                self.logs_loading = false;
                self.logs.clear();
                self.logs_error = None;
            }
        }
        self.snapshot = snapshot;
        self.prune_volume_selection();
    }

    fn select_container(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected_container = Some(id.clone());
        self.load_logs(id, cx);
    }

    fn load_logs(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let revision = self.logs_revision.advance();
        self.logs_loading = true;
        self.logs_error = None;
        self.logs.clear();
        cx.notify();
        let handle = engine.container_logs(id);
        self._logs_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.logs_revision.is_current(revision) {
                    return;
                }
                this.logs_loading = false;
                match result {
                    Ok(logs) => this.logs = logs,
                    Err(error) => this.logs_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn start_container_action(
        &mut self,
        id: String,
        action: ContainerAction,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let handle = engine.container_action(id, action);
        self.run_snapshot_operation(action.label().to_string(), handle, cx);
    }

    fn remove_image(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let handle = engine.remove_image(id);
        self.run_snapshot_operation("删除镜像".into(), handle, cx);
    }

    fn pull_image(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let reference = self.pull_input.read(cx).value().trim().to_string();
        if reference.is_empty() {
            self.error = Some("请输入要拉取的镜像引用".into());
            cx.notify();
            return;
        }
        if reference.chars().any(char::is_whitespace) {
            self.error = Some("镜像引用不能包含空白字符".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let handle = engine.pull_image(reference.clone());
        self.run_snapshot_operation(format!("拉取镜像 {reference}"), handle, cx);
    }

    fn open_create_container_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.create_error = None;
        self.create_nvidia_gpu = false;
        for input in [
            &self.create_image_input,
            &self.create_name_input,
            &self.create_command_input,
            &self.create_ports_input,
        ] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }

        let image = self.create_image_input.clone();
        let name = self.create_name_input.clone();
        let command = self.create_command_input.clone();
        let ports = self.create_ports_input.clone();
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let error = weak
                .upgrade()
                .and_then(|this| this.read(cx).create_error.clone());
            let (gpu_available, gpu_selected) = weak
                .upgrade()
                .map(|this| {
                    let this = this.read(cx);
                    (this.nvidia_gpu_available(), this.create_nvidia_gpu)
                })
                .unwrap_or((false, false));
            let gpu_weak = weak.clone();
            let submit_weak = weak.clone();
            let mut form = div()
                .flex()
                .flex_col()
                .gap_4()
                .child(form_field(
                    "镜像引用 *",
                    &image,
                    "仅使用本地已有镜像；本步骤不会自动拉取。",
                    cx,
                ))
                .child(form_field(
                    "容器名称",
                    &name,
                    "可包含字母、数字、下划线、点和连字符。",
                    cx,
                ))
                .child(form_field(
                    "启动命令",
                    &command,
                    "覆盖镜像 CMD，支持引号；不会自动经过 shell 执行。",
                    cx,
                ))
                .child(form_field(
                    "TCP 端口映射",
                    &ports,
                    "多个映射用逗号分隔，仅绑定到 127.0.0.1。",
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            Switch::new("create-nvidia-gpu")
                                .label("使用全部 NVIDIA GPU")
                                .checked(gpu_selected)
                                .disabled(!gpu_available)
                                .on_change(move |enabled, _, cx| {
                                    _ = gpu_weak.update(cx, |this, cx| {
                                        this.create_nvidia_gpu = *enabled;
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if gpu_available {
                                    "映射为 --gpus all；需要受管环境的 NVIDIA runtime。"
                                } else {
                                    "未检测到可用的 NVIDIA GPU/runtime，请先在设置中检查 WSL2 环境。"
                                }),
                        ),
                );
            if let Some(error) = error {
                form = form.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().danger)
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                );
            }

            dialog
                .title("创建并运行容器")
                .w(px(560.))
                .child(form)
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-create")
                                .outline()
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("submit-create")
                                .primary()
                                .icon(IconName::Play)
                                .label("创建并运行")
                                .on_click(move |_, window, cx| {
                                    _ = submit_weak.update(cx, |this, cx| {
                                        this.submit_create_container(window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn submit_create_container(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let draft = ContainerDraft {
            image: self.create_image_input.read(cx).value().to_string(),
            name: self.create_name_input.read(cx).value().to_string(),
            command: self.create_command_input.read(cx).value().to_string(),
            ports: self.create_ports_input.read(cx).value().to_string(),
            nvidia_gpu: self.create_nvidia_gpu,
        };
        let request = match draft.parse() {
            Ok(request) => request,
            Err(error) => {
                self.create_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(engine) = self.engine.clone() else {
            self.create_error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };

        window.close_dialog(cx);
        let handle = engine.create_container(request);
        self.run_snapshot_operation("创建并运行容器".into(), handle, cx);
    }

    fn proxy_draft(&self, cx: &App) -> DockerProxySettings {
        DockerProxySettings {
            enabled: self.settings_draft.docker_proxy.enabled,
            http_proxy: self.http_proxy_input.read(cx).value().to_string(),
            https_proxy: self.https_proxy_input.read(cx).value().to_string(),
            no_proxy: self.no_proxy_input.read(cx).value().to_string(),
        }
    }

    /// The settings as currently edited, normalized but not validated.
    fn collect_settings(&self, cx: &App) -> Result<AppSettings, String> {
        let threshold = self
            .saver_threshold_input
            .read(cx)
            .value()
            .trim()
            .parse::<u64>()
            .map_err(|_| "资源节省阈值必须是整数秒".to_string())?;
        let mut candidate = self.settings_draft.clone();
        candidate.docker_proxy = self.proxy_draft(cx);
        candidate.resource_saver.threshold_secs = threshold;
        // Integrations are applied and saved immediately, never drafted.
        candidate.wsl = self.settings.wsl.clone();
        Ok(candidate.normalized())
    }

    fn settings_dirty(&self, cx: &App) -> bool {
        self.collect_settings(cx)
            .map_or(true, |candidate| candidate != self.settings)
    }

    /// Target of a disk move the candidate settings ask for, if any.
    fn pending_disk_move(&self, candidate: &AppSettings) -> Option<PathBuf> {
        if !self.manages_engine()
            || !self.runtime_snapshot.installed
            || candidate.vm == self.settings.vm
        {
            return None;
        }
        let target = disk_location_setting(candidate)?;
        match &self.runtime_snapshot.disk_location {
            Some(current) if same_path(current, &target) => None,
            _ => Some(target),
        }
    }

    fn disk_location_display(&self) -> String {
        if let Some(location) = disk_location_setting(&self.settings_draft) {
            return location.display().to_string();
        }
        if let Some(location) = &self.runtime_snapshot.disk_location {
            return location.display().to_string();
        }
        self.runtime_host
            .as_ref()
            .map(|host| host.default_disk_location().display().to_string())
            .unwrap_or_else(|| "不可用".into())
    }

    fn browse_disk_location(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            _ = this.update(cx, |this, cx| {
                this.settings_draft.vm.disk_image_location = path.display().to_string();
                this.settings_notice = None;
                this.settings_error = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn discard_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.settings.clone();
        self.settings_draft = saved.clone();
        for (input, value) in [
            (
                &self.http_proxy_input,
                saved.docker_proxy.http_proxy.clone(),
            ),
            (
                &self.https_proxy_input,
                saved.docker_proxy.https_proxy.clone(),
            ),
            (&self.no_proxy_input, saved.docker_proxy.no_proxy.clone()),
            (
                &self.saver_threshold_input,
                saved.resource_saver.threshold_secs.to_string(),
            ),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.saver_slider.update(cx, |slider, cx| {
            slider.set_value(saved.resource_saver.threshold_secs as f32, window, cx);
        });
        apply_theme(saved.theme, cx);
        self.settings_error = None;
        self.settings_notice = None;
        cx.notify();
    }

    /// "Apply": validates the draft, moves the virtual disk first when its
    /// location changed (settings are only saved if that succeeds), then
    /// saves and applies everything else.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let candidate = match self
            .collect_settings(cx)
            .and_then(|candidate| candidate.validate().map(|()| candidate))
        {
            Ok(candidate) => candidate,
            Err(error) => {
                self.settings_error = Some(error);
                self.settings_notice = None;
                cx.notify();
                return;
            }
        };
        if self.settings_store.is_none() {
            self.settings_error = Some("无法确定配置目录，设置未保存".into());
            cx.notify();
            return;
        }
        let Some(target) = self.pending_disk_move(&candidate) else {
            self.commit_settings(candidate, cx);
            cx.notify();
            return;
        };
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        if self.runtime_busy.is_some() {
            self.settings_error = Some("运行环境正忙，请稍后再应用".into());
            cx.notify();
            return;
        }
        self.runtime_busy = Some("正在移动磁盘镜像…".into());
        self.runtime_error = None;
        self.settings_error = None;
        self.settings_notice = Some("正在移动磁盘镜像，Engine 会暂时停止…".into());
        self.connected = false;
        cx.notify();
        let handle = host.move_disk(target, self.settings.wsl.integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                match result {
                    Ok(snapshot) => {
                        let ready = snapshot.engine_ready;
                        this.runtime_snapshot = snapshot;
                        this.commit_settings(candidate, cx);
                        if ready {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => {
                        this.settings_notice = None;
                        this.settings_error =
                            Some(format!("移动磁盘镜像失败，设置未保存：{error}"));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn commit_settings(&mut self, candidate: AppSettings, cx: &mut Context<Self>) {
        let Some(store) = &self.settings_store else {
            return;
        };
        match store.save(&candidate) {
            Ok(()) => {
                if let Some(host) = &self.runtime_host {
                    host.set_resource_saver(&candidate.resource_saver);
                    host.set_data_location(disk_location_setting(&candidate));
                }
                self.settings = candidate.clone();
                self.settings_draft = candidate;
                self.settings_warning = None;
                self.settings_error = None;
                self.settings_notice = Some("设置已保存".into());
                // Idempotent: dockerd is only restarted if daemon.json changes.
                self.apply_engine_proxy(cx);
            }
            Err(error) => {
                self.settings_error = Some(error.to_string());
                self.settings_notice = None;
            }
        }
    }

    /// True when the app manages the engine itself (Windows, no
    /// `DOCKER_HOST`), so settings can be applied directly.
    fn manages_engine(&self) -> bool {
        cfg!(windows) && self.runtime_host.is_some() && std::env::var_os("DOCKER_HOST").is_none()
    }

    fn apply_engine_proxy(&mut self, cx: &mut Context<Self>) {
        if !self.manages_engine() {
            self.settings_notice = Some(
                "设置已保存。当前 Engine 不由 Rusker 管理（非 Windows 或设置了 DOCKER_HOST），请导出 Engine JSON 并在该 Engine 上应用"
                    .into(),
            );
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.runtime_busy = Some("正在应用 Engine 代理…".into());
        self.runtime_error = None;
        self.settings_notice = Some("设置已保存，正在应用 Engine 代理…".into());
        let handle = host.apply_proxy(
            self.settings.docker_proxy.clone(),
            self.settings.wsl.integrations.clone(),
        );
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                match result {
                    Ok((outcome, snapshot)) => {
                        let ready = snapshot.engine_ready;
                        this.runtime_snapshot = snapshot;
                        this.settings_notice = Some(
                            match outcome {
                                ProxyApplyOutcome::Restarted => "代理已生效：Engine 已按新配置重启",
                                ProxyApplyOutcome::Unchanged => "代理已生效（Engine 配置无变化）",
                                ProxyApplyOutcome::Deferred => {
                                    "设置已保存，代理将在 Engine 启动时自动生效"
                                }
                            }
                            .into(),
                        );
                        if outcome == ProxyApplyOutcome::Restarted && ready {
                            this.refresh(cx);
                        }
                    }
                    Err(error) => {
                        this.settings_notice = None;
                        this.settings_error = Some(format!("设置已保存，但应用代理失败：{error}"));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn export_proxy_settings(&mut self, cx: &mut Context<Self>) {
        let proxy = self.proxy_draft(cx);
        if let Err(error) = proxy.validate() {
            self.settings_error = Some(error);
            self.settings_notice = None;
            cx.notify();
            return;
        }
        let Some(store) = &self.settings_store else {
            self.settings_error = Some("无法确定配置目录，代理配置未导出".into());
            cx.notify();
            return;
        };

        match store.export_proxy(&proxy) {
            Ok(path) => {
                self.settings_error = None;
                self.settings_notice = Some(format!(
                    "已导出 {}。请手动合并到 daemon.json 并重启 Engine",
                    path.display()
                ));
            }
            Err(error) => {
                self.settings_error = Some(error.to_string());
                self.settings_notice = None;
            }
        }
        cx.notify();
    }

    fn theme_button(&self, preference: ThemePreference, cx: &mut Context<Self>) -> Button {
        let selected = self.settings_draft.theme == preference;
        let button = Button::new(preference.label())
            .label(preference.label())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings_draft.theme = preference;
                this.settings_notice = None;
                apply_theme(preference, cx);
                cx.notify();
            }));
        if selected {
            button.primary()
        } else {
            button.outline()
        }
    }

    fn run_snapshot_operation(
        &mut self,
        label: String,
        handle: JoinHandle<Result<EngineSnapshot, EngineError>>,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let revision = self.refresh_revision.advance();
        self.busy = Some(format!("{label}…"));
        self.loading = false;
        self.error = None;
        self.notice = None;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(snapshot) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot);
                        this.notice = Some(format!("{label}完成"));
                        this.after_volume_snapshot(cx);
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn confirm_remove_container(
        &mut self,
        container: &ContainerSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = container.id.clone();
        let title = format!("删除容器 {}？", container.name);
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let id = id.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description("此操作不可撤销。关联卷不会被删除；运行中的容器需先停止。")
                .confirm()
                .ok_text("删除")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    _ = weak.update(cx, |this, cx| {
                        this.start_container_action(id, ContainerAction::Remove, cx);
                    });
                    true
                })
        });
    }

    fn confirm_remove_image(
        &mut self,
        image: &ImageSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = image.id.clone();
        let title = format!("删除镜像 {}？", image_tags(image));
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let id = id.clone();
            let weak = weak.clone();
            dialog
                .title(title.clone())
                .description("此操作不可撤销。仍被容器使用的镜像不会被强制删除。")
                .confirm()
                .ok_text("删除")
                .cancel_text("取消")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    _ = weak.update(cx, |this, cx| this.remove_image(id, cx));
                    true
                })
        });
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        // While the engine starts or stops, the whole menu is placeholders
        // (Docker Desktop-style); the app bar gear still opens settings.
        let placeholders = self.engine_phase().is_some();
        let nav = div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .py_3()
            .flex()
            .flex_col()
            .gap_1();
        let skeleton = || {
            div()
                .mx_2()
                .h(px(36.))
                .flex()
                .items_center()
                .child(Skeleton::new().h(px(28.)).rounded_md())
        };
        let separator = div().mx_3().my_2().h(px(1.)).bg(cx.theme().border);
        if placeholders {
            return nav
                .children((0..4).map(|_| skeleton()))
                .child(separator)
                .child(skeleton());
        }
        nav.child(self.nav_item(Page::Overview, IconName::LayoutDashboard, cx))
            .child(self.nav_item(Page::Containers, AssetIcon::Container, cx))
            .child(self.nav_item(Page::Images, AssetIcon::Layers2, cx))
            .child(self.nav_item(Page::Volumes, AssetIcon::Database, cx))
            .child(separator)
            .child(self.nav_item(Page::Settings, IconName::Settings, cx))
    }

    /// The engine phase that replaces engine-backed pages with the
    /// starting/stopping animation, if any.
    fn engine_phase(&self) -> Option<EngineTransition> {
        if self.quitting {
            return Some(EngineTransition::Stopping);
        }
        if let Some(transition) = self.engine_transition {
            return Some(transition);
        }
        (self.loading && !self.connected).then_some(EngineTransition::Connecting)
    }

    /// Spinning ring driven by `motion_epoch`; `render` keeps frames coming
    /// while one is on screen (see `wants_motion`).
    fn loading_ring(&self, size: Pixels, color: Option<Hsla>) -> Icon {
        let turn = self.motion_epoch.elapsed().as_secs_f32() / SPINNER_PERIOD_SECS;
        Icon::new(IconName::LoaderCircle)
            .with_size(Size::Size(size))
            .when_some(color, |this, color| this.text_color(color))
            .transform(Transformation::rotate(percentage(turn.fract())))
    }

    /// Whether a loading indicator is visible and needs animation frames.
    fn wants_motion(&self) -> bool {
        self.engine_phase().is_some() || self.runtime_busy.is_some()
    }

    /// Centered spinner with a title, faded and slid in when the phase
    /// changes (the animation id is per phase, so it replays).
    fn render_engine_transition(&self, phase: EngineTransition, cx: &mut Context<Self>) -> Div {
        let accent = brand_color(cx);
        div().flex_1().min_h_0().flex().justify_center().child(
            div()
                .id(SharedString::from(format!("engine-{}", phase.key())))
                .pt(px(140.))
                .px_6()
                .flex()
                .flex_col()
                .items_center()
                .gap_4()
                .child(self.loading_ring(px(56.), Some(accent)))
                .child(
                    div()
                        .mt_2()
                        .text_lg()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(if self.quitting {
                            "正在停止 Docker Engine 并退出…"
                        } else {
                            phase.title()
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(phase.description()),
                )
                .with_animation(
                    SharedString::from(format!("engine-{}-enter", phase.key())),
                    Animation::new(Duration::from_millis(450)).with_easing(ease_out_quint()),
                    |this, delta| this.opacity(delta).mt(px(16. * (1. - delta))),
                ),
        )
    }

    /// Docker Desktop-style navigation row: full-width, icon + label, the
    /// selected page highlighted with a tinted background.
    fn nav_item(
        &self,
        page: Page,
        icon: impl Into<Icon>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.page == page;
        let accent = brand_color(cx);
        div()
            .id(SharedString::from(format!("nav-{}", page.title())))
            .mx_2()
            .px_3()
            .h(px(36.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_3()
            .text_sm()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(accent.opacity(0.16)).font_semibold()
            })
            .when(!selected, |this| {
                this.hover(|style| style.bg(cx.theme().sidebar_accent))
            })
            .child(Icon::new(icon).small())
            .child(page.title())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.navigate(page, cx);
            }))
    }

    fn render_header(&self, show_refresh: bool, cx: &mut Context<Self>) -> Div {
        div()
            .h(px(60.))
            .w_full()
            .flex_shrink_0()
            .px_6()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_lg().font_semibold().child(self.page.title()))
            .when(show_refresh, |this| {
                this.child(
                    Button::new("refresh")
                        .outline()
                        .small()
                        .icon(IconName::RefreshCw)
                        .label(if self.loading { "刷新中" } else { "刷新" })
                        .disabled(self.loading || self.busy.is_some())
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
            })
    }

    fn render_overview(&self, cx: &mut Context<Self>) -> Div {
        let engine = &self.snapshot.engine;
        div().flex_1().min_h_0().child(
            div()
                .id("overview-page")
                .size_full()
                .overflow_y_scroll()
            .p_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(page_header(
                        "容器运行概览",
                        "直接读取本地 Docker/Podman 兼容引擎，不使用模拟数据。",
                        cx,
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .gap_4()
                            .child(stat_card(
                                "全部容器",
                                self.snapshot.containers.len().to_string(),
                                "包括运行中和已停止",
                                cx,
                            ))
                            .child(stat_card(
                                "运行中",
                                self.snapshot.running_containers().to_string(),
                                format!("{} 个未运行", self.snapshot.stopped_containers()),
                                cx,
                            ))
                            .child(stat_card(
                                "本地镜像",
                                self.snapshot.images.len().to_string(),
                                "包含无标签镜像",
                                cx,
                            ))
                            .child(stat_card(
                                "卷",
                                self.snapshot.volumes.len().to_string(),
                                format!(
                                    "{} 个正被容器使用",
                                    self.snapshot
                                        .volumes
                                        .iter()
                                        .filter(|volume| self.snapshot.volume_in_use(&volume.name))
                                        .count()
                                ),
                                cx,
                            )),
                    )
                    .child(
                        section_panel(cx)
                            .p_5()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(div().text_lg().font_semibold().child("引擎信息"))
                            .child(detail_row("名称", engine.name.clone(), cx))
                            .child(detail_row("Engine", engine.version.clone(), cx))
                            .child(detail_row("API", engine.api_version.clone(), cx))
                            .child(detail_row("操作系统", engine.operating_system.clone(), cx))
                            .child(detail_row("架构", engine.architecture.clone(), cx)),
                    )
                    .child(muted(
                        "连接已有本地或 WSL2 引擎；发行版/虚拟机安装、Compose 和 Networks 管理见路线图。",
                        cx,
                    )),
            ),
        )
    }

    fn render_containers(&self, cx: &mut Context<Self>) -> Div {
        let query = self.search_input.read(cx).value().to_string();
        let containers: Vec<ContainerSummary> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container_matches(container, &query))
            .cloned()
            .collect();
        let selected = self
            .selected_container
            .as_ref()
            .and_then(|id| {
                self.snapshot
                    .containers
                    .iter()
                    .find(|container| &container.id == id)
            })
            .cloned();

        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(page_header(
                "容器",
                format!(
                    "共 {} 个，当前显示 {} 个",
                    self.snapshot.containers.len(),
                    containers.len()
                ),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div().w(px(420.)).child(
                            Input::new(&self.search_input)
                                .prefix(Icon::new(IconName::Search).small())
                                .cleanable(true),
                        ),
                    )
                    .child(
                        Button::new("open-create-container")
                            .primary()
                            .icon(IconName::Plus)
                            .label("创建容器")
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_create_container_dialog(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_4()
                    .child(self.render_container_list(&containers, cx))
                    .child(self.render_container_details(selected.as_ref(), cx)),
            )
    }

    fn render_container_list(
        &self,
        containers: &[ContainerSummary],
        cx: &mut Context<Self>,
    ) -> Div {
        let list = section_panel(cx)
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .flex()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().w(px(190.)).child("名称"))
                    .child(div().flex_1().child("镜像"))
                    .child(div().w(px(90.)).child("状态")),
            );

        if containers.is_empty() {
            return list.child(empty_state(
                "没有匹配的容器",
                "调整搜索条件，或确认引擎中存在容器。",
                cx,
            ));
        }

        let rows = containers.iter().enumerate().map(|(index, container)| {
            let id = container.id.clone();
            let selected = self.selected_container.as_ref() == Some(&container.id);
            let state_color = if container.is_running() {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            };
            div()
                .id(("container-row", index))
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .cursor_pointer()
                .when(selected, |row| row.bg(cx.theme().accent))
                .hover(|row| row.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_container(id.clone(), cx);
                }))
                .child(
                    div()
                        .w(px(190.))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_ellipsis()
                                .child(container.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(short_id(&container.id)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pr_3()
                        .text_sm()
                        .text_ellipsis()
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

        list.child(
            div()
                .id("container-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows),
        )
    }

    fn render_container_details(
        &self,
        container: Option<&ContainerSummary>,
        cx: &mut Context<Self>,
    ) -> Div {
        let panel = section_panel(cx)
            .w(px(430.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col();
        let Some(container) = container else {
            return panel.child(empty_state(
                "选择一个容器",
                "查看详情、日志并执行操作。",
                cx,
            ));
        };

        let busy = self.busy.is_some();
        let start_id = container.id.clone();
        let stop_id = container.id.clone();
        let restart_id = container.id.clone();
        let remove_container = container.clone();
        let logs = if self.logs_loading {
            "正在读取日志…".to_string()
        } else if let Some(error) = &self.logs_error {
            error.clone()
        } else if self.logs.trim().is_empty() {
            "该容器最近没有 stdout/stderr 日志。".to_string()
        } else {
            self.logs.clone()
        };

        panel.child(
            div()
                .id("container-details")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_5()
                .flex()
                .flex_col()
                .gap_5()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .child(container.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(container.id.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            Button::new("start-container")
                                .outline()
                                .small()
                                .icon(IconName::Play)
                                .label("启动")
                                .disabled(busy || container.is_running())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_container_action(
                                        start_id.clone(),
                                        ContainerAction::Start,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("stop-container")
                                .outline()
                                .small()
                                .icon(IconName::Square)
                                .label("停止")
                                .disabled(busy || !container.is_running())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_container_action(
                                        stop_id.clone(),
                                        ContainerAction::Stop,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("restart-container")
                                .outline()
                                .small()
                                .icon(IconName::RotateCw)
                                .label("重启")
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_container_action(
                                        restart_id.clone(),
                                        ContainerAction::Restart,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("remove-container")
                                .danger()
                                .small()
                                .icon(IconName::Delete)
                                .label("删除")
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_remove_container(&remove_container, window, cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(detail_row("状态", container.status.clone(), cx))
                        .child(detail_row("镜像", container.image.clone(), cx))
                        .child(detail_row("端口", format_ports(&container.ports), cx))
                        .child(detail_row(
                            "创建于",
                            format_timestamp(container.created),
                            cx,
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().font_semibold().child("最近日志（200 行）"))
                        .child(
                            div()
                                .p_3()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().background)
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_xs()
                                .child(logs),
                        ),
                ),
        )
    }

    fn render_images(&self, cx: &mut Context<Self>) -> Div {
        let query = self.search_input.read(cx).value().to_string();
        let images: Vec<ImageSummary> = self
            .snapshot
            .images
            .iter()
            .filter(|image| image_matches(image, &query))
            .cloned()
            .collect();
        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(page_header(
                "镜像",
                format!(
                    "共 {} 个，当前显示 {} 个",
                    self.snapshot.images.len(),
                    images.len()
                ),
                cx,
            ))
            .child(
                section_panel(cx)
                    .p_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(Input::new(&self.pull_input)))
                    .child(
                        Button::new("pull-image")
                            .primary()
                            .icon(IconName::ArrowDown)
                            .label("拉取镜像")
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.pull_image(cx))),
                    ),
            )
            .child(
                div().w(px(420.)).child(
                    Input::new(&self.search_input)
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true),
                ),
            )
            .child(self.render_image_list(&images, cx))
    }

    /// Engine runtime controls shown at the top of "VM 资源": state of the
    /// dedicated distribution plus install / start / stop.
    fn render_engine_runtime(&self, cx: &mut Context<Self>) -> Div {
        let snapshot = &self.runtime_snapshot;
        let status = if !snapshot.installed {
            "未安装"
        } else if snapshot.engine_ready {
            "Engine 运行中"
        } else if self.resource_saver_active() {
            "资源节省模式（Engine 已暂停，有客户端连接时自动恢复）"
        } else if snapshot.running {
            "发行版运行中，Engine 未就绪"
        } else {
            "已停止"
        };
        let busy = self.runtime_busy.is_some();
        let gpu_detail = if snapshot.gpu.detail.is_empty() {
            "正在检测 NVIDIA GPU 与 runtime"
        } else {
            &snapshot.gpu.detail
        };
        let warning = snapshot.warnings.join("\n");

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(settings_label("WSL2 运行环境", cx))
            .child(detail_row("专用发行版", "rusker (WSL2)", cx))
            .child(detail_row("运行状态", status, cx))
            .child(detail_row("Windows 端点", MANAGED_DOCKER_HOST, cx))
            .child(detail_row("WSL2 端点", WSL_DOCKER_HOST, cx))
            .child(detail_row("NVIDIA GPU", gpu_detail, cx))
            .when_some(self.runtime_busy.clone(), |this, label| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.loading_ring(px(16.), None))
                        .child(muted(label, cx)),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        Button::new("install-runtime")
                            .primary()
                            .icon(IconName::HardDrive)
                            .label(if snapshot.installed { "修复环境" } else { "安装环境" })
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Install, cx);
                            })),
                    )
                    .child(
                        Button::new("start-runtime")
                            .outline()
                            .icon(IconName::Play)
                            .label("启动")
                            .disabled(busy || !snapshot.installed || snapshot.engine_ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Start, cx);
                            })),
                    )
                    .child(
                        Button::new("stop-runtime")
                            .outline()
                            .icon(IconName::Square)
                            .label("停止")
                            .disabled(busy || !snapshot.running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Stop, cx);
                            })),
                    )
                    .child(
                        Button::new("refresh-runtime")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label("刷新状态")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_runtime_host(cx);
                            })),
                    ),
            )
            .when(!warning.is_empty(), |this| {
                this.child(div().text_sm().text_color(cx.theme().warning).child(warning))
            })
            .when_some(self.runtime_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .when_some(self.runtime_notice.clone(), |this, notice| {
                this.child(div().text_sm().text_color(cx.theme().success).child(notice))
            })
            .child(muted(
                "Engine 不监听 TCP 端口。Windows 工具可执行：docker context create rusker --docker host=npipe:////./pipe/rusker_engine。退出 Rusker 时会停止 Engine。安装或修复可能需要数分钟，Engine 日志位于专用发行版的 /var/log/rusker-dockerd.log。",
                cx,
            ))
    }

    fn render_wsl_integration(&self, cx: &mut Context<Self>) -> Div {
        let snapshot = &self.runtime_snapshot;
        let busy = self.runtime_busy.is_some();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(muted(
                "开关安装或移除 Rusker 标记的 Docker CLI 包装器及 Compose/Buildx 插件，默认连接共享 socket；它不是防止手动访问的安全边界。",
                cx,
            ))
            .when(snapshot.distributions.is_empty(), |this| {
                this.child(muted("没有发现可集成的其他 WSL2 发行版。", cx))
            })
            .children(snapshot.distributions.iter().map(|distribution| {
                let name = distribution.name.clone();
                let title = if distribution.is_default {
                    format!("{}（默认）", distribution.name)
                } else {
                    distribution.name.clone()
                };
                div()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().font_medium().child(title))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(distribution.state.clone()),
                    )
                    .child(
                        Switch::new(SharedString::from(format!(
                            "wsl-integration-{}",
                            distribution.name
                        )))
                        .checked(distribution.integration_enabled)
                        .disabled(
                            busy
                                || self.settings_store.is_none()
                                || (!distribution.integration_enabled && !snapshot.engine_ready),
                        )
                        .on_change(cx.listener(move |this, enabled, _, cx| {
                            this.set_runtime_integration(name.clone(), *enabled, cx);
                        })),
                    )
            }))
            .child(muted(
                "集成会立即生效，无需点击“应用”。资源节省模式下 WSL 发行版内的 docker 命令无法唤醒 Engine，请通过 Windows 端点或 Rusker 恢复。",
                cx,
            ))
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let sections = SettingsSection::visible();
        let active = self.active_settings_section(&sections);
        let content = sections
            .iter()
            .enumerate()
            .map(|(index, section)| {
                let body = match section {
                    SettingsSection::General => self.render_general_settings(cx),
                    SettingsSection::VmResources => self.render_vm_settings(cx),
                    SettingsSection::ResourceSaver => self.render_resource_saver_settings(cx),
                    SettingsSection::Proxies => self.render_proxy_settings(cx),
                    SettingsSection::WslIntegration => self.render_wsl_integration(cx),
                };
                div()
                    .py_6()
                    .when(index + 1 < sections.len(), |this| {
                        this.border_b_1().border_color(cx.theme().border)
                    })
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(div().text_2xl().font_semibold().child(section.title()))
                    .child(body)
            })
            .collect::<Vec<_>>();

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .when_some(self.settings_warning.clone(), |this, warning| {
                this.child(div().px_8().pt_6().child(callout(
                    CalloutKind::Warning,
                    "设置警告",
                    div().child(warning),
                    cx,
                )))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("settings-scroll")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.settings_scroll)
                            .px_8()
                            .pb_10()
                            .children(content),
                    )
                    .child(
                        div()
                            .w(px(200.))
                            .flex_shrink_0()
                            .my_6()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .flex()
                            .flex_col()
                            .children(sections.iter().enumerate().map(|(index, section)| {
                                self.settings_nav_item(index, *section, index == active, cx)
                            })),
                    ),
            )
            .child(self.render_settings_footer(cx))
    }

    /// The section whose heading is at the top of the scroll view, or the
    /// one last clicked when the view cannot scroll that far.
    fn active_settings_section(&self, sections: &[SettingsSection]) -> usize {
        let top = self
            .settings_scroll
            .top_item()
            .min(sections.len().saturating_sub(1));
        let at_bottom = self.settings_scroll.offset().y <= -self.settings_scroll.max_offset().y
            && self.settings_scroll.max_offset().y > px(0.);
        match self.settings_nav_target {
            Some(target) if at_bottom && target > top => target,
            _ => top,
        }
    }

    fn settings_nav_item(
        &self,
        index: usize,
        section: SettingsSection,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let accent = if cx.theme().mode.is_dark() {
            rgb(0x60A5FA).into()
        } else {
            brand_color(cx)
        };
        div()
            .id(("settings-nav", index))
            .ml(px(-1.))
            .pl_5()
            .py_2()
            .border_l_2()
            .border_color(if selected {
                accent
            } else {
                gpui_kit::transparent_black()
            })
            .text_sm()
            .cursor_pointer()
            .when(selected, |this| this.font_semibold().text_color(accent))
            .when(!selected, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|style| style.text_color(cx.theme().foreground))
            })
            .child(section.title())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings_nav_target = Some(index);
                this.settings_scroll.scroll_to_top_of_item(index);
                cx.notify();
            }))
    }

    fn render_settings_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.settings_dirty(cx);
        let (message, color) = if let Some(error) = self.settings_error.clone() {
            (error, cx.theme().danger)
        } else if dirty {
            ("有未保存的更改".to_string(), cx.theme().warning)
        } else if let Some(notice) = self.settings_notice.clone() {
            (notice, cx.theme().success)
        } else {
            ("没有未保存的更改".to_string(), cx.theme().muted_foreground)
        };
        let manages_engine = self.manages_engine();
        let apply_blocked =
            self.settings_store.is_none() || (manages_engine && self.runtime_busy.is_some());
        div()
            .flex_shrink_0()
            .px_8()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(color)
                    .child(message),
            )
            .child(
                Button::new("discard-settings")
                    .outline()
                    .label("放弃更改")
                    .disabled(!dirty)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.discard_settings(window, cx);
                    })),
            )
            .child(
                Button::new("apply-settings")
                    .primary()
                    .label("应用")
                    .disabled(!dirty || apply_blocked)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.apply_settings(cx);
                    })),
            )
    }

    fn render_general_settings(&self, cx: &mut Context<Self>) -> Div {
        let config_path = self
            .settings_store
            .as_ref()
            .map(|store| store.config_path().display().to_string())
            .unwrap_or_else(|| "不可用".into());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(settings_label("外观", cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.theme_button(ThemePreference::System, cx))
                    .child(self.theme_button(ThemePreference::Light, cx))
                    .child(self.theme_button(ThemePreference::Dark, cx)),
            )
            .child(muted("主题立即预览，点击“应用”后保存。", cx))
            .child(muted(format!("Rusker 配置文件：{config_path}"), cx))
    }

    fn render_vm_settings(&self, cx: &mut Context<Self>) -> Div {
        if !self.manages_engine() {
            return div().child(callout(
                CalloutKind::Info,
                "外部 Engine",
                muted(
                    "当前 Engine 不由 Rusker 管理（非 Windows 或设置了 DOCKER_HOST），CPU、内存和磁盘由该 Engine 自行配置。",
                    cx,
                ),
                cx,
            ));
        }
        let location = self.disk_location_display();
        let move_pending = self.pending_disk_move(&self.settings_draft).is_some();
        let busy = self.runtime_busy.is_some();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(callout(
                CalloutKind::Info,
                "WSL 2 后端",
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("你正在使用 WSL 2 后端，资源限制由 Windows 管理。")
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .child("可以在")
                            .child(
                                div()
                                    .id("open-wslconfig-docs")
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().link)
                                    .underline()
                                    .cursor_pointer()
                                    .child(".wslconfig 文件")
                                    .child(Icon::new(AssetIcon::ExternalLink).xsmall())
                                    .on_click(|_, _, cx| cx.open_url(WSLCONFIG_DOCS_URL)),
                            )
                            .child("中配置 WSL 2 可用的内存、CPU 和交换空间上限。"),
                    ),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_label("磁盘镜像位置", cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(36.))
                                    .px_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .bg(cx.theme().muted)
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Icon::new(AssetIcon::Folder)
                                            .small()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .text_sm()
                                            .child(location),
                                    ),
                            )
                            .child(
                                Button::new("browse-disk-location")
                                    .outline()
                                    .label("浏览…")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.browse_disk_location(cx);
                                    })),
                            ),
                    )
                    .child(muted(
                        if self.runtime_snapshot.installed {
                            "Docker 数据磁盘（docker-data.vhdx，存放镜像、容器和卷）所在目录。选择新目录并应用即可移动，不影响其他 WSL 发行版。"
                        } else {
                            "Docker 数据磁盘（docker-data.vhdx，存放镜像、容器和卷）的创建目录。"
                        },
                        cx,
                    ))
                    .when(move_pending, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child("应用后会停止 Engine 并移动数据磁盘，运行中的容器会被停止；跨磁盘移动较大的数据时可能需要数分钟。"),
                        )
                    }),
            )
            .child(self.render_engine_runtime(cx))
    }

    fn render_resource_saver_settings(&self, cx: &mut Context<Self>) -> Div {
        let enabled = self.settings_draft.resource_saver.enabled;
        let managed = self.manages_engine();
        let threshold_labels = [
            "30 秒",
            "10 分钟",
            "20 分钟",
            "30 分钟",
            "40 分钟",
            "50 分钟",
            "60 分钟",
        ];
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(div().flex_1().child(settings_label("启用资源节省", cx)))
                    .child(
                        Switch::new("resource-saver-enabled")
                            .checked(enabled)
                            .disabled(!managed)
                            .on_change(cx.listener(|this, enabled, _, cx| {
                                this.settings_draft.resource_saver.enabled = *enabled;
                                this.settings_notice = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(muted(
                if managed {
                    "没有运行中的容器时停止 Engine 和专用发行版，释放 CPU 与内存。Windows 客户端（docker.exe、Rusker）再次连接时自动恢复。"
                } else {
                    "资源节省只作用于 Rusker 管理的 WSL2 Engine，当前 Engine 不由 Rusker 管理。"
                },
                cx,
            ))
            .when(self.resource_saver_active(), |this| {
                this.child(callout(
                    CalloutKind::Info,
                    "正处于资源节省模式",
                    muted("Engine 已暂停。运行 docker 命令或刷新 Rusker 即可恢复。", cx),
                    cx,
                ))
            })
            .child(
                div()
                    .ml_6()
                    .pl_6()
                    .border_l_2()
                    .border_color(cx.theme().border)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(!enabled || !managed, |this| this.opacity(0.5))
                    .child(settings_label("资源节省阈值", cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().w(px(96.)).child(
                                            Input::new(&self.saver_threshold_input)
                                                .disabled(!enabled || !managed),
                                        ),
                                    )
                                    .child(muted("秒", cx)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(240.))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        Slider::new(&self.saver_slider)
                                            .disabled(!enabled || !managed),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .justify_between()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .children(threshold_labels.map(|label| div().child(label))),
                                    ),
                            ),
                    )
                    .child(muted(
                        "拖动滑块设置从没有运行中的容器到进入资源节省模式之间的等待时间（30 秒到 60 分钟）。",
                        cx,
                    )),
            )
    }

    fn render_proxy_settings(&self, cx: &mut Context<Self>) -> Div {
        let manages_engine = self.manages_engine();
        let export_path = self
            .settings_store
            .as_ref()
            .map(|store| store.proxy_export_path().display().to_string())
            .unwrap_or_else(|| "不可用".into());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(div().flex_1().child(settings_label("Docker Engine 拉取代理", cx)))
                    .child(
                        Switch::new("docker-proxy-enabled")
                            .checked(self.settings_draft.docker_proxy.enabled)
                            .on_change(cx.listener(|this, enabled, _, cx| {
                                this.settings_draft.docker_proxy.enabled = *enabled;
                                this.settings_notice = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(muted(
                "Engine 拉取镜像等出站请求使用的代理。应用后直接写入受管 Engine 的 daemon.json 并生效。",
                cx,
            ))
            .child(form_field(
                "HTTP_PROXY",
                &self.http_proxy_input,
                "支持 http:// 或 https://，不保存内嵌用户名或密码。",
                cx,
            ))
            .child(form_field(
                "HTTPS_PROXY",
                &self.https_proxy_input,
                "HTTPS 请求使用的代理地址，也可以填写 http:// 代理。",
                cx,
            ))
            .child(form_field(
                "NO_PROXY",
                &self.no_proxy_input,
                "逗号分隔的主机、域名、IP 或 CIDR，例如 localhost,127.0.0.1,::1。",
                cx,
            ))
            .child(callout(
                CalloutKind::Warning,
                if manages_engine { "会重启 Engine" } else { "需要手动应用" },
                muted(
                    if manages_engine {
                        "代理配置变化时会重启 Engine，运行中的容器会被停止（设置了重启策略的容器会自动恢复）。WSL2 使用 NAT 网络时，127.0.0.1 指向 WSL 自身而非 Windows，请填写 Windows 主机地址。"
                    } else {
                        "当前 Engine 不由 Rusker 管理，无法直接修改其配置。请导出后手动合并 proxies 到 daemon.json 并重启 Engine；不要覆盖 daemon.json 中的其他配置。"
                    },
                    cx,
                ),
                cx,
            ))
            .when(!manages_engine, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            Button::new("export-proxy-settings")
                                .outline()
                                .icon(IconName::FileText)
                                .label("导出 Engine JSON")
                                .disabled(self.settings_store.is_none())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_proxy_settings(cx);
                                })),
                        )
                        .child(muted(format!("导出到：{export_path}"), cx)),
                )
            })
    }

    fn render_image_list(&self, images: &[ImageSummary], cx: &mut Context<Self>) -> Div {
        let list = section_panel(cx)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .flex()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().flex_1().child("标签"))
                    .child(div().w(px(150.)).child("镜像 ID"))
                    .child(div().w(px(100.)).child("大小"))
                    .child(div().w(px(180.)).child("创建时间"))
                    .child(div().w(px(76.)).child("操作")),
            );
        if images.is_empty() {
            return list.child(empty_state(
                "没有匹配的镜像",
                "调整搜索条件，或在上方拉取一个镜像。",
                cx,
            ));
        }

        let rows = images.iter().enumerate().map(|(index, image)| {
            let remove_image = image.clone();
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pr_3()
                        .text_sm()
                        .child(image_tags(image)),
                )
                .child(
                    div()
                        .w(px(150.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .child(short_id(&image.id)),
                )
                .child(div().w(px(100.)).text_sm().child(format_bytes(image.size)))
                .child(
                    div()
                        .w(px(180.))
                        .text_xs()
                        .child(format_timestamp(image.created)),
                )
                .child(
                    div().w(px(76.)).child(
                        Button::new(("remove-image", index))
                            .danger()
                            .small()
                            .icon(IconName::Delete)
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_remove_image(&remove_image, window, cx);
                            })),
                    ),
                )
        });
        list.child(
            div()
                .id("image-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows),
        )
    }
}

impl Render for RuskerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.wants_motion() {
            // Keep redrawing while a loading indicator is visible.
            window.request_animation_frame();
        }
        let phase = self.engine_phase();
        // Settings stays usable mid-transition (reached via the app bar
        // gear); everything else shows the start/stop animation alone.
        let transition = phase.filter(|_| self.quitting || self.page != Page::Settings);
        let page = match (self.page, transition) {
            (_, Some(phase)) => self.render_engine_transition(phase, cx),
            (Page::Overview, None) => self.render_overview(cx),
            (Page::Containers, None) => self.render_containers(cx),
            (Page::Images, None) => self.render_images(cx),
            (Page::Volumes, None) => self.render_volumes(cx),
            (Page::Settings, None) => self.render_settings(cx),
        };
        let mut content = div().flex_1().min_w_0().h_full().flex().flex_col();
        if transition.is_none() {
            content = content.child(self.render_header(phase.is_none(), cx));
            if let Some(error) = self.error.clone() {
                content = content.child(message_banner(&error, true, cx));
            }
            if let Some(busy) = self.busy.clone() {
                content = content.child(message_banner(&busy, false, cx));
            } else if let Some(notice) = self.notice.clone() {
                content = content.child(message_banner(&notice, false, cx));
            }
        }

        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(self.render_sidebar(cx))
            .child(content.child(page));

        window_border().child(
            div()
                .size_full()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .flex()
                .flex_col()
                .child(self.render_app_bar(window, cx))
                .child(body)
                .child(self.render_status_bar(cx)),
        )
    }
}

const SIDEBAR_WIDTH: f32 = 216.;
/// Seconds per spinner revolution.
const SPINNER_PERIOD_SECS: f32 = 0.9;
/// Seconds per status-dot breath.
const STATUS_PULSE_PERIOD_SECS: f32 = 1.6;
const APP_BAR_HEIGHT: f32 = 48.;
const WINDOW_BUTTON_WIDTH: f32 = 46.;

/// Brand blue used for the app bar and selection accents (Docker
/// Desktop-like). A deeper navy keeps contrast in dark mode.
fn brand_color(cx: &App) -> Hsla {
    if cx.theme().mode.is_dark() {
        rgb(0x1E3A8A).into()
    } else {
        rgb(0x1D63ED).into()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowButton {
    Minimize,
    Maximize,
    Close,
}

impl RuskerApp {
    /// Top app bar replacing the native caption: brand on the left, empty
    /// space as the drag region, and fixed minimize / maximize / close
    /// buttons in the top-right corner.
    ///
    /// Only the brand and spacer are drag regions, so buttons inside the bar
    /// never get swallowed by the caption hit test. On Windows the buttons
    /// map to native hit-test areas (Snap Layouts on hover, WM_CLOSE on
    /// close, so "minimize to tray" still applies); on Linux they act on
    /// click; on macOS the native traffic lights are kept.
    fn render_app_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let maximized = window.is_maximized();
        let leading_inset = if cfg!(target_os = "macos") { 80. } else { 16. };
        div()
            .id("app-bar")
            .h(px(APP_BAR_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(brand_color(cx))
            .text_color(gpui_kit::white())
            .child(
                drag_region(div().id("app-bar-brand"))
                    .h_full()
                    .pl(px(leading_inset))
                    .pr_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(AssetIcon::Container).size(px(22.)))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(div().text_lg().font_bold().child("rusker"))
                            .child(div().text_lg().child("desktop")),
                    ),
            )
            .child(drag_region(div().id("app-bar-drag")).flex_1().h_full())
            .child(
                div()
                    .id("app-bar-settings")
                    .size(px(32.))
                    .mr_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(gpui_kit::white().opacity(0.16)))
                    .child(Icon::new(IconName::Settings).small())
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Page::Settings, cx))),
            )
            .when(!cfg!(target_os = "macos"), |this| {
                this.child(self.window_button(WindowButton::Minimize, maximized))
                    .child(self.window_button(WindowButton::Maximize, maximized))
                    .child(self.window_button(WindowButton::Close, maximized))
            })
    }

    fn window_button(&self, button: WindowButton, maximized: bool) -> impl IntoElement {
        let (id, icon, area) = match button {
            WindowButton::Minimize => (
                "window-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
            ),
            WindowButton::Maximize if maximized => (
                "window-restore",
                IconName::WindowRestore,
                WindowControlArea::Max,
            ),
            WindowButton::Maximize => (
                "window-maximize",
                IconName::WindowMaximize,
                WindowControlArea::Max,
            ),
            WindowButton::Close => (
                "window-close",
                IconName::WindowClose,
                WindowControlArea::Close,
            ),
        };
        let has_tray = self.tray.is_some();
        let is_close = button == WindowButton::Close;
        div()
            .id(id)
            .w(px(WINDOW_BUTTON_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .when(is_close, |this| {
                this.hover(|style| style.bg(rgb(0xE81123)))
                    .active(|style| style.bg(rgb(0xC50F1F)))
            })
            .when(!is_close, |this| {
                this.hover(|style| style.bg(gpui_kit::white().opacity(0.16)))
                    .active(|style| style.bg(gpui_kit::white().opacity(0.24)))
            })
            .child(Icon::new(icon).small())
            .when(cfg!(windows), |this| this.window_control_area(area))
            .when(cfg!(target_os = "linux"), |this| {
                this.on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    match button {
                        WindowButton::Minimize => window.minimize_window(),
                        WindowButton::Maximize => window.zoom_window(),
                        // Same behaviour as `on_window_should_close`.
                        WindowButton::Close if has_tray => window.minimize_window(),
                        WindowButton::Close => window.remove_window(),
                    }
                })
            })
    }

    /// Bottom status bar: engine state on the left (aligned with the
    /// sidebar), engine resources and counts in the middle, versions right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, color) = self.engine_status(cx);
        let engine = &self.snapshot.engine;
        let mut facts: Vec<String> = Vec::new();
        if self.connected {
            if engine.cpus > 0 {
                facts.push(format!("CPU {} 核", engine.cpus));
            }
            if engine.memory_bytes > 0 {
                facts.push(format!("内存 {}", format_bytes(engine.memory_bytes)));
            }
            facts.push(format!(
                "容器 {}/{} 运行中",
                self.snapshot.running_containers(),
                self.snapshot.containers.len()
            ));
            facts.push(format!("镜像 {}", self.snapshot.images.len()));
            facts.push(format!("卷 {}", self.snapshot.volumes.len()));
            if let Some(gpu) = self
                .runtime_snapshot
                .gpu
                .available
                .then(|| self.runtime_snapshot.gpu.name.clone())
                .flatten()
            {
                facts.push(format!("GPU {gpu}"));
            }
        }
        let engine_version = (self.connected && !engine.version.is_empty())
            .then(|| format!("Docker Engine {} · {}", engine.version, engine.architecture));

        div()
            .h(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                div()
                    .w(px(SIDEBAR_WIDTH))
                    .h_full()
                    .flex_shrink_0()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(self.render_status_dot(color))
                    .child(div().font_medium().text_color(color).child(label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .overflow_hidden()
                    .children(
                        facts
                            .into_iter()
                            .map(|fact| div().flex_shrink_0().child(fact)),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .flex_shrink_0()
                    .when_some(engine_version, |this, version| this.child(version))
                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
            )
    }

    /// Status bar dot; breathes while the engine starts or stops.
    fn render_status_dot(&self, color: Hsla) -> Div {
        let dot = div().size(px(8.)).rounded_full().bg(color);
        if !self.wants_motion() {
            return dot;
        }
        let t = self.motion_epoch.elapsed().as_secs_f32() / STATUS_PULSE_PERIOD_SECS;
        dot.opacity(pulsating_between(0.25, 1.)(t.fract()))
    }

    /// Engine state shown in the status bar, combining the Docker API
    /// connection with the managed runtime's lifecycle.
    fn engine_status(&self, cx: &App) -> (String, Hsla) {
        if self.quitting {
            return ("Engine 停止中…".into(), cx.theme().warning);
        }
        match self.engine_transition {
            Some(EngineTransition::Stopping) => {
                return ("Engine 停止中…".into(), cx.theme().warning)
            }
            Some(EngineTransition::Starting) => {
                return ("Engine 启动中…".into(), cx.theme().warning)
            }
            _ => {}
        }
        if self.connected {
            return ("Engine 运行中".into(), cx.theme().success);
        }
        if self.resource_saver_active() {
            return ("资源节省模式".into(), cx.theme().info);
        }
        if let Some(busy) = &self.runtime_busy {
            return (
                busy.trim_end_matches('…').to_string() + "…",
                cx.theme().warning,
            );
        }
        if self.loading {
            return ("正在连接 Engine…".into(), cx.theme().warning);
        }
        let label = if cfg!(windows) && !self.runtime_snapshot.installed && self.manages_engine() {
            "Engine 未安装"
        } else {
            "Engine 已停止"
        };
        (label.into(), cx.theme().danger)
    }
}

/// Marks an element as a window drag region: native caption hit-testing on
/// Windows, `start_window_move` elsewhere. Double-click maximizes.
fn drag_region(element: Stateful<Div>) -> Stateful<Div> {
    if cfg!(windows) {
        return element.window_control_area(WindowControlArea::Drag);
    }
    element.on_mouse_down(MouseButton::Left, |event, window, _| {
        if event.click_count >= 2 {
            if cfg!(target_os = "macos") {
                window.titlebar_double_click();
            } else {
                window.zoom_window();
            }
        } else {
            window.start_window_move();
        }
    })
}

pub fn apply_theme(preference: ThemePreference, cx: &mut App) {
    match preference {
        ThemePreference::System => {
            cx.set_window_appearance(None);
            let mode = ThemeMode::from(cx.window_appearance());
            Theme::change(mode, None, cx);
        }
        ThemePreference::Light => {
            cx.set_window_appearance(Some(WindowAppearance::Light));
            Theme::change(ThemeMode::Light, None, cx);
        }
        ThemePreference::Dark => {
            cx.set_window_appearance(Some(WindowAppearance::Dark));
            Theme::change(ThemeMode::Dark, None, cx);
        }
    }
}

/// The saved disk location, `None` for the default.
fn disk_location_setting(settings: &AppSettings) -> Option<PathBuf> {
    let location = settings.vm.disk_image_location.trim();
    (!location.is_empty()).then(|| PathBuf::from(location))
}

fn settings_label(text: &'static str, cx: &App) -> Div {
    div()
        .text_base()
        .font_semibold()
        .text_color(cx.theme().foreground)
        .child(text)
}

#[derive(Clone, Copy)]
enum CalloutKind {
    Info,
    Warning,
}

/// Tinted notice box with an icon and a title, like Docker Desktop's
/// "WSL2 backend" panel.
fn callout(kind: CalloutKind, title: &'static str, body: impl IntoElement, cx: &App) -> Div {
    let (color, icon) = match kind {
        CalloutKind::Info => (cx.theme().info, AssetIcon::Info),
        CalloutKind::Warning => (cx.theme().warning, AssetIcon::TriangleAlert),
    };
    let body: AnyElement = body.into_any_element();
    div()
        .p_5()
        .rounded_lg()
        .bg(color.opacity(0.1))
        .border_1()
        .border_color(color.opacity(0.25))
        .flex()
        .gap_3()
        .child(Icon::new(icon).text_color(color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .text_sm()
                .child(div().text_base().font_semibold().child(title))
                .child(body),
        )
}

fn form_field(label: &str, input: &Entity<InputState>, hint: &str, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_sm().font_medium().child(label.to_string()))
        .child(Input::new(input))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint.to_string()),
        )
}

async fn await_engine<T>(handle: JoinHandle<Result<T, EngineError>>) -> Result<T, EngineError> {
    handle
        .await
        .map_err(|error| EngineError::Task(error.to_string()))?
}

async fn await_runtime_host<T>(
    handle: JoinHandle<Result<T, RuntimeHostError>>,
) -> Result<T, RuntimeHostError> {
    handle
        .await
        .map_err(|error| RuntimeHostError::Task(error.to_string()))?
}
