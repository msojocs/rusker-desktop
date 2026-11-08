use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    skeleton::Skeleton,
    slider::{Slider, SliderEvent, SliderState},
    switch::Switch,
    tab::{Tab, TabBar},
    tooltip::Tooltip,
    window_border, ActiveTheme as _, Icon, IconName, Sizable as _, Size, ThemeMode, WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div, ease_out_quint, percentage,
    prelude::FluentBuilder as _,
    pulsating_between, px, rgb, Anchor, Animation, AnimationExt as _, AnyElement, AnyWindowHandle,
    App, AppContext as _, ClipboardItem, Context, Div, ElementId, Entity, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, PathPromptOptions,
    Pixels, Render, ScrollHandle, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Transformation, Window, WindowAppearance, WindowControlArea,
};
use rusker_desktop::{
    engine::{EngineError, EngineService},
    format::{
        container_matches, format_bytes, format_port_bindings, format_ports, format_relative,
        format_timestamp, short_id,
    },
    model::{ContainerAction, ContainerSummary, EngineSnapshot, Revision},
    runtime_host::{
        same_path, validate_daemon_config, EngineRecovery, LanForwardState, ProxyApplyOutcome,
        RuntimeHostError, RuntimeHostService, RuntimeHostSnapshot, MANAGED_DOCKER_HOST,
        WSL_DOCKER_HOST,
    },
    settings::{
        apply_client_proxy, docker_client_config_path, AppSettings, DockerProxySettings,
        NetworkingMode, PortBinding, ProxyMode, ResolvedProxy, SettingsStore, ThemePreference,
        DEFAULT_DOCKER_SUBNET, RESOURCE_SAVER_MAX_SECS, RESOURCE_SAVER_MIN_SECS,
    },
    tray::{TrayCommand, TrayMenu},
};
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::ui::theme::change_theme;
use crate::ui::{button, detail_row, empty_state, muted, section_panel, stat_card};

mod images;
mod notifications;
mod run;
mod volumes;

use images::ImageState;
use run::RunForm;
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
    Network,
    WslIntegration,
    DockerEngine,
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
            sections.push(Self::Network);
            sections.push(Self::WslIntegration);
            sections.push(Self::DockerEngine);
        }
        sections
    }

    fn title(self) -> &'static str {
        match self {
            Self::General => "常规",
            Self::VmResources => "VM 资源",
            Self::ResourceSaver => "资源节省",
            Self::Proxies => "代理",
            Self::Network => "网络",
            Self::WslIntegration => "WSL 集成",
            Self::DockerEngine => "Docker Engine",
        }
    }
}

const WSLCONFIG_DOCS_URL: &str =
    "https://learn.microsoft.com/windows/wsl/wsl-config#configuration-settings-for-wslconfig";
const DAEMON_CONFIG_DOCS_URL: &str =
    "https://docs.docker.com/reference/cli/dockerd/#daemon-configuration-file";
const NETWORKING_DOCS_URL: &str = "https://docs.docker.com/engine/network/";

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
    /// The engine went away without a deliberate stop (`wsl --shutdown`,
    /// a crash) and the backend is restarting it.
    Recovering,
}

impl EngineTransition {
    fn key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Connecting => "connecting",
            Self::Stopping => "stopping",
            Self::Recovering => "recovering",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Starting => "正在启动 Docker Engine…",
            Self::Connecting => "正在连接 Docker Engine…",
            Self::Stopping => "正在停止 Docker Engine…",
            Self::Recovering => "Engine 意外退出，正在自动重启…",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Starting | Self::Connecting => "Docker Engine 是运行容器的底层技术",
            Self::Stopping => "运行中的容器将被停止，Engine 关闭后可在设置中重新启动",
            Self::Recovering => {
                "WSL 被关闭或 Engine 崩溃。设置了重启策略的容器会随 Engine 一起恢复"
            }
        }
    }
}

#[derive(Default)]
struct ContainerRemoval {
    pending_ids: Vec<String>,
    submitting: bool,
    error: Option<String>,
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
    /// Sidebar shows icons only (labels move into tooltips).
    sidebar_collapsed: bool,
    snapshot: EngineSnapshot,
    connected: bool,
    loading: bool,
    busy: Option<String>,
    selected_container: Option<String>,
    /// Container list shows only running containers.
    containers_running_only: bool,
    /// IDs ticked for batch removal in the container list.
    containers_checked: HashSet<String>,
    container_removal: ContainerRemoval,
    /// Container detail tab: 0 logs, 1 details, 2 mounts.
    container_tab: usize,
    /// Scroll state of the container log viewer.
    container_logs_scroll: ScrollHandle,
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
    images: ImageState,
    run: RunForm,
    volumes: VolumeState,
    volume_name_input: Entity<InputState>,
    settings: AppSettings,
    /// Edits not yet applied. Proxy addresses and the Resource Saver
    /// threshold live in their input states instead.
    settings_draft: AppSettings,
    settings_store: Option<SettingsStore>,
    settings_warning: Option<String>,
    settings_notice: Option<String>,
    /// "Apply" is still running one of its background steps (WSL
    /// integrations, disk move, Engine config).
    settings_applying: bool,
    settings_error: Option<String>,
    settings_scroll: ScrollHandle,
    settings_nav_target: Option<usize>,
    http_proxy_input: Entity<InputState>,
    https_proxy_input: Entity<InputState>,
    no_proxy_input: Entity<InputState>,
    container_http_proxy_input: Entity<InputState>,
    container_https_proxy_input: Entity<InputState>,
    container_no_proxy_input: Entity<InputState>,
    /// Network section: IPv4 subnet of the default bridge.
    network_subnet_input: Entity<InputState>,
    /// Docker Engine section: `daemon.json` without `proxies`.
    daemon_editor: Entity<EditorState>,
    /// The text last read from the engine (None: not loaded).
    daemon_loaded: Option<String>,
    daemon_loading: bool,
    daemon_error: Option<String>,
    _daemon_task: Option<Task<()>>,
    /// For updating the editor from async tasks (setting text needs a window).
    window_handle: AnyWindowHandle,
    saver_threshold_input: Entity<InputState>,
    saver_slider: Entity<SliderState>,
    /// Last Resource Saver state the UI reacted to.
    saver_seen: bool,
    /// Last NAT LAN forwarding state shown in the Network section.
    lan_forward: LanForwardState,
    /// Last automatic engine restart state seen from the backend, and the
    /// number of completed restarts (a change means: reconnect).
    recovery: EngineRecovery,
    recovery_generation: u64,
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
        let container_http_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.container_proxy.http_proxy.clone())
                .placeholder("例如：http://192.168.1.10:7890")
        });
        let container_https_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.container_proxy.https_proxy.clone())
                .placeholder("通常与 HTTP 代理地址相同")
        });
        let container_no_proxy_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.container_proxy.no_proxy.clone())
                .placeholder("localhost,127.0.0.1,::1")
        });
        let network_subnet_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.network.subnet.clone())
                .placeholder(format!("{DEFAULT_DOCKER_SUBNET}（Docker 默认）"))
        });
        let daemon_editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
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
        for input in [
            &http_proxy_input,
            &https_proxy_input,
            &no_proxy_input,
            &container_http_proxy_input,
            &container_https_proxy_input,
            &container_no_proxy_input,
            &network_subnet_input,
        ] {
            subscriptions.push(cx.subscribe_in(input, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        subscriptions.push(
            cx.subscribe_in(&daemon_editor, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
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
                .update(cx, |this, cx| {
                    this.sync_resource_saver(cx);
                    this.sync_lan_forward(cx);
                    this.sync_engine_recovery(cx);
                })
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
                change_theme(mode, app_cx);
            }
        });
        // The system / Engine proxy may have changed since the last run.
        if follows_other_proxy(settings.container_proxy.mode) {
            if let Err(error) = sync_container_proxy(&settings) {
                settings_warning.get_or_insert(error);
            }
        }
        let (engine, error) = match EngineService::new() {
            Ok(engine) => (Some(engine), None),
            Err(error) => (None, Some(error.to_string())),
        };
        if let Some(error) = error {
            cx.defer_in(window, move |this, _, cx| this.notify_error(error, cx));
        }
        let (runtime_host, runtime_error) = match RuntimeHostService::new() {
            Ok(host) => {
                host.set_engine_proxy(&settings.docker_proxy);
                host.set_engine_network(&settings.network);
                host.set_host_networking(settings.network.host_networking);
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
            sidebar_collapsed: false,
            snapshot: EngineSnapshot::default(),
            connected: false,
            loading: false,
            busy: None,
            selected_container: None,
            containers_running_only: false,
            containers_checked: HashSet::new(),
            container_removal: ContainerRemoval::default(),
            container_tab: 0,
            container_logs_scroll: ScrollHandle::new(),
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
            images: ImageState::default(),
            run: RunForm::default(),
            volumes: VolumeState::default(),
            volume_name_input,
            settings_draft: settings.clone(),
            settings,
            settings_store,
            settings_warning,
            settings_notice: None,
            settings_applying: false,
            settings_error: None,
            settings_scroll: ScrollHandle::new(),
            settings_nav_target: None,
            http_proxy_input,
            https_proxy_input,
            no_proxy_input,
            container_http_proxy_input,
            container_https_proxy_input,
            container_no_proxy_input,
            network_subnet_input,
            daemon_editor,
            daemon_loaded: None,
            daemon_loading: false,
            daemon_error: None,
            _daemon_task: None,
            window_handle: window.window_handle(),
            saver_threshold_input,
            saver_slider,
            saver_seen: false,
            lan_forward: LanForwardState::default(),
            recovery: EngineRecovery::default(),
            recovery_generation: 0,
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
        let entering_containers = page == Page::Containers && self.page != Page::Containers;
        if page != Page::Images || self.page != Page::Images {
            self.images.selected = None;
            self.reset_image_detail();
        }
        self.page = page;
        if entering_containers {
            // The sidebar entry always opens the container list, not the
            // detail page of the last selected container.
            self.selected_container = None;
            self.logs_revision.advance();
            self.logs_loading = false;
            self.logs.clear();
            self.logs_error = None;
        }
        if page == Page::Settings {
            self.refresh_runtime_host(cx);
            // Keep unsaved edits; otherwise pick up the current file.
            if self.daemon_pending(cx).is_none() {
                self.load_daemon_config(cx);
            }
        }
        if entering_volumes && self.connected {
            self.after_volume_snapshot(cx);
        }
        cx.notify();
    }

    /// Reads the managed `daemon.json` into the Docker Engine editor.
    fn load_daemon_config(&mut self, cx: &mut Context<Self>) {
        if !self.manages_engine() || self.daemon_loading {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.daemon_loading = true;
        self.daemon_error = None;
        let handle = host.read_daemon_config();
        let window = self.window_handle;
        self._daemon_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.daemon_loading = false;
                match result {
                    Ok(text) => {
                        let editor = this.daemon_editor.clone();
                        let value = text.clone();
                        _ = cx.update_window(window, |_, window, cx| {
                            editor.update(cx, |state, cx| state.set_value(value, window, cx));
                        });
                        this.daemon_loaded = Some(text);
                    }
                    Err(error) => {
                        this.daemon_loaded = None;
                        this.daemon_error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
    }

    /// The edited `daemon.json` text when it differs from the loaded file.
    fn daemon_pending(&self, cx: &App) -> Option<String> {
        let loaded = self.daemon_loaded.as_ref()?;
        let value = self.daemon_editor.read(cx).value().to_string();
        (value.trim() != loaded.trim()).then_some(value)
    }

    /// Launch-time hook: starts the managed engine before connecting to its
    /// API. External engines can be connected to immediately.
    pub fn startup_runtime_host(&mut self, cx: &mut Context<Self>) {
        if !self.manages_engine() {
            self.refresh(cx);
            self.refresh_runtime_host(cx);
            return;
        }
        if self.runtime_busy.is_some() {
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
            // Resource Saver retains the WSL session and mounted data disk.
            self.runtime_snapshot.running = true;
            self.runtime_snapshot.resource_saver_active = true;
            cx.notify();
        } else {
            self.runtime_snapshot.resource_saver_active = false;
            self.runtime_snapshot.engine_ready = self
                .runtime_host
                .as_ref()
                .is_some_and(RuntimeHostService::engine_session_running);
            // The original operation owns the wake-up and applies its own
            // snapshot. Don't run GPU/environment probes or replace the page
            // with a reconnect animation while that operation is completing.
            if self.runtime_snapshot.engine_ready
                && !self.connected
                && self.busy.is_none()
                && !self.loading
            {
                self.refresh(cx);
            }
            cx.notify();
        }
    }

    /// Polled with Resource Saver: re-renders when the forwarded ports change.
    fn sync_lan_forward(&mut self, cx: &mut Context<Self>) {
        let state = self
            .runtime_host
            .as_ref()
            .filter(|_| self.manages_engine())
            .map(RuntimeHostService::lan_forward_state)
            .unwrap_or_default();
        if state != self.lan_forward {
            self.lan_forward = state;
            cx.notify();
        }
    }

    /// Polled with Resource Saver: follows the backend's automatic restart
    /// of a lost engine, and reconnects once it is back.
    fn sync_engine_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.runtime_host.as_ref().filter(|_| self.manages_engine()) else {
            return;
        };
        let (recovery, generation) = host.engine_recovery();
        if recovery != self.recovery {
            match &recovery {
                EngineRecovery::Restarting { .. } => {
                    self.connected = false;
                    self.runtime_snapshot.engine_ready = false;
                    self.runtime_snapshot.running = false;
                }
                EngineRecovery::Failed(message) => {
                    self.connected = false;
                    self.runtime_error = Some(message.clone());
                }
                EngineRecovery::Idle => {}
            }
            self.recovery = recovery;
            cx.notify();
        }
        // A restart may also finish between two polls; either way the old
        // connection is gone.
        if generation != self.recovery_generation && self.runtime_busy.is_none() {
            self.recovery_generation = generation;
            self.connected = false;
            self.runtime_error = None;
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
        // Only drafted; takes effect when the settings are applied.
        let integrations = &mut self.settings_draft.wsl.integrations;
        if enabled {
            integrations.insert(distribution, true);
        } else {
            integrations.remove(&distribution);
        }
        self.settings_notice = None;
        self.settings_error = None;
        cx.notify();
    }

    fn nvidia_gpu_available(&self) -> bool {
        #[cfg(windows)]
        if std::env::var_os("DOCKER_HOST").is_none() {
            return self.runtime_snapshot.gpu.available;
        }
        true
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // A managed engine cannot accept API requests until its lifecycle
        // operation finishes. Startup refreshes automatically once it is ready.
        if self.busy.is_some()
            || self.quitting
            || (self.manages_engine() && self.engine_transition.is_some())
        {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.loading = false;
            self.notify_error("后台运行时不可用，请重启应用", cx);
            cx.notify();
            return;
        };

        let revision = self.refresh_revision.advance();
        self.loading = true;
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
                        this.notify_error(error.to_string(), cx);
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
        let ids: HashSet<&str> = self
            .snapshot
            .containers
            .iter()
            .map(|container| container.id.as_str())
            .collect();
        self.containers_checked
            .retain(|id| ids.contains(id.as_str()));
        self.prune_volume_selection();
        self.after_image_snapshot();
    }

    fn select_container(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_container.as_deref() != Some(id.as_str()) {
            self.container_tab = 0;
        }
        self.selected_container = Some(id.clone());
        self.load_logs(id, cx);
    }

    /// Returns from the container detail page to the container list.
    fn close_container_detail(&mut self, cx: &mut Context<Self>) {
        self.selected_container = None;
        self.logs_revision.advance();
        self.logs_loading = false;
        self.logs.clear();
        self.logs_error = None;
        cx.notify();
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
                    Ok(logs) => {
                        this.logs = logs;
                        this.container_logs_scroll.scroll_to_bottom();
                    }
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
            self.notify_error("容器引擎不可用", cx);
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
            self.notify_error("容器引擎不可用", cx);
            cx.notify();
            return;
        };
        let handle = engine.remove_image(id);
        self.run_snapshot_operation("删除镜像".into(), handle, cx);
    }

    fn proxy_draft(&self, cx: &App) -> DockerProxySettings {
        DockerProxySettings {
            mode: self.settings_draft.docker_proxy.mode,
            http_proxy: self.http_proxy_input.read(cx).value().to_string(),
            https_proxy: self.https_proxy_input.read(cx).value().to_string(),
            no_proxy: self.no_proxy_input.read(cx).value().to_string(),
        }
    }

    fn container_proxy_draft(&self, cx: &App) -> DockerProxySettings {
        DockerProxySettings {
            mode: self.settings_draft.container_proxy.mode,
            http_proxy: self.container_http_proxy_input.read(cx).value().to_string(),
            https_proxy: self
                .container_https_proxy_input
                .read(cx)
                .value()
                .to_string(),
            no_proxy: self.container_no_proxy_input.read(cx).value().to_string(),
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
        candidate.container_proxy = self.container_proxy_draft(cx);
        candidate.resource_saver.threshold_secs = threshold;
        candidate.network.subnet = self.network_subnet_input.read(cx).value().to_string();
        Ok(candidate.normalized())
    }

    fn settings_dirty(&self, cx: &App) -> bool {
        self.daemon_pending(cx).is_some()
            || self
                .collect_settings(cx)
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
                &self.container_http_proxy_input,
                saved.container_proxy.http_proxy.clone(),
            ),
            (
                &self.container_https_proxy_input,
                saved.container_proxy.https_proxy.clone(),
            ),
            (
                &self.container_no_proxy_input,
                saved.container_proxy.no_proxy.clone(),
            ),
            (
                &self.saver_threshold_input,
                saved.resource_saver.threshold_secs.to_string(),
            ),
            (&self.network_subnet_input, saved.network.subnet.clone()),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.saver_slider.update(cx, |slider, cx| {
            slider.set_value(saved.resource_saver.threshold_secs as f32, window, cx);
        });
        if let Some(loaded) = self.daemon_loaded.clone() {
            self.daemon_editor
                .update(cx, |state, cx| state.set_value(loaded, window, cx));
        }
        apply_theme(saved.theme, cx);
        self.settings_error = None;
        self.settings_notice = None;
        cx.notify();
    }

    /// "Apply": validates the draft, moves the virtual disk first when its
    /// location changed (settings are only saved if that succeeds), then
    /// saves and applies everything else.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_applying {
            return;
        }
        let was_busy = self.runtime_busy.is_some();
        self.start_apply_settings(cx);
        // Every background step of "Apply" holds `runtime_busy` until it
        // hands over to the next one, so that marks the whole chain.
        self.settings_applying = !was_busy && self.runtime_busy.is_some();
        cx.notify();
    }

    /// Ends the apply animation once a step finished without starting
    /// another one.
    fn settle_settings_applying(&mut self) {
        self.settings_applying &= self.runtime_busy.is_some();
    }

    fn start_apply_settings(&mut self, cx: &mut Context<Self>) {
        let candidate = match self
            .collect_settings(cx)
            .and_then(|candidate| candidate.validate().map(|()| candidate))
            .and_then(|candidate| match self.daemon_pending(cx) {
                Some(text) => validate_daemon_config(&text).map(|()| candidate),
                None => Ok(candidate),
            }) {
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
        let changes =
            integration_changes(&self.settings.wsl.integrations, &candidate.wsl.integrations);
        if changes.is_empty() {
            self.apply_validated_settings(candidate, None, cx);
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            self.settings_error = Some("WSL2 后台运行时不可用，设置未保存".into());
            cx.notify();
            return;
        };
        if self.runtime_busy.is_some() {
            self.settings_error = Some("运行环境正忙，请稍后再应用".into());
            cx.notify();
            return;
        }
        self.runtime_busy = Some("正在应用 WSL 集成…".into());
        self.runtime_error = None;
        self.settings_error = None;
        self.settings_notice = Some("正在应用 WSL 集成…".into());
        cx.notify();
        let handle = host.apply_integrations(changes, candidate.wsl.integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
                match result {
                    Ok((snapshot, effective, failures)) => {
                        this.runtime_snapshot = snapshot;
                        let mut candidate = candidate;
                        candidate.wsl.integrations = effective;
                        let warning = (!failures.is_empty())
                            .then(|| format!("部分 WSL 集成未生效：{}", failures.join("；")));
                        this.apply_validated_settings(candidate, warning, cx);
                    }
                    Err(error) => {
                        this.settings_notice = None;
                        this.settings_error =
                            Some(format!("应用 WSL 集成失败，设置未保存：{error}"));
                    }
                }
                this.settle_settings_applying();
                cx.notify();
            });
        }));
    }

    /// Second half of "Apply", once WSL integrations are in effect: moves the
    /// virtual disk when needed, then saves and applies the rest.
    fn apply_validated_settings(
        &mut self,
        candidate: AppSettings,
        warning: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.pending_disk_move(&candidate) else {
            self.commit_settings(candidate, warning, cx);
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
                        this.commit_settings(candidate, warning, cx);
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
                this.settle_settings_applying();
                cx.notify();
            });
        }));
    }

    fn commit_settings(
        &mut self,
        candidate: AppSettings,
        warning: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = &self.settings_store else {
            return;
        };
        match store.save(&candidate) {
            Ok(()) => {
                if let Some(host) = &self.runtime_host {
                    host.set_resource_saver(&candidate.resource_saver);
                    host.set_host_networking(candidate.network.host_networking);
                    host.set_data_location(disk_location_setting(&candidate));
                }
                // Modes that follow other sources are re-resolved on every
                // apply; the write is skipped when nothing changed.
                let container_proxy_changed = candidate.container_proxy
                    != self.settings.container_proxy
                    || follows_other_proxy(candidate.container_proxy.mode);
                self.settings = candidate.clone();
                self.settings_draft = candidate;
                self.settings_warning = warning;
                self.settings_error = None;
                self.settings_notice = Some("设置已保存".into());
                if container_proxy_changed {
                    if let Err(error) = sync_container_proxy(&self.settings) {
                        self.settings_error = Some(format!("设置已保存，但{error}"));
                    }
                }
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
        let daemon_config = self.daemon_pending(cx);
        let with_daemon = daemon_config.is_some();
        self.runtime_busy = Some("正在应用 Engine 配置…".into());
        self.runtime_error = None;
        self.settings_notice = Some("设置已保存，正在应用 Engine 配置…".into());
        let handle = host.apply_engine_config(
            self.settings.docker_proxy.clone(),
            self.settings.network.clone(),
            daemon_config,
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
                                ProxyApplyOutcome::Restarted => "已生效：Engine 已按新配置重启",
                                ProxyApplyOutcome::Unchanged => "已生效（Engine 配置无变化）",
                                ProxyApplyOutcome::Deferred => {
                                    "设置已保存，Engine 配置将在 Engine 启动时自动生效"
                                }
                            }
                            .into(),
                        );
                        if outcome == ProxyApplyOutcome::Restarted && ready {
                            this.refresh(cx);
                        }
                        if with_daemon {
                            // Show the file as written (formatting, key order).
                            this.load_daemon_config(cx);
                        }
                    }
                    Err(error) => {
                        this.settings_notice = None;
                        this.settings_error =
                            Some(format!("设置已保存，但应用 Engine 配置失败：{error}"));
                    }
                }
                this.settle_settings_applying();
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
        let button = button(preference.label())
            .label(preference.label())
            .pointer(true)
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
                        this.notify_success(format!("{label}完成"), cx);
                        this.after_volume_snapshot(cx);
                    }
                    Err(error) => this.notify_error(error.to_string(), cx),
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
        self.open_remove_containers_dialog(
            vec![container.id.clone()],
            format!("删除容器 {}？", container.name),
            "此操作不可撤销。关联卷不会被删除；运行中的容器需先停止。".into(),
            window,
            cx,
        );
    }

    fn confirm_remove_containers(
        &mut self,
        containers: Vec<ContainerSummary>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if containers.is_empty() || self.busy.is_some() {
            return;
        }
        let mut listed: Vec<String> = containers
            .iter()
            .take(8)
            .map(|container| format!("{}（{}）", container.name, short_id(&container.id)))
            .collect();
        if containers.len() > listed.len() {
            listed.push("…".into());
        }
        let title = format!("删除 {} 个容器？", containers.len());
        let description = format!(
            "此操作不可撤销。关联卷不会被删除；运行中的容器会被 Engine 拒绝。\n{}",
            listed.join("\n")
        );
        let ids = containers
            .into_iter()
            .map(|container| container.id)
            .collect();
        self.open_remove_containers_dialog(ids, title, description, window, cx);
    }

    fn open_remove_containers_dialog(
        &mut self,
        ids: Vec<String>,
        title: String,
        description: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() || self.busy.is_some() {
            return;
        }
        self.container_removal = ContainerRemoval {
            pending_ids: ids,
            ..ContainerRemoval::default()
        };
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let Some(this) = weak.upgrade() else {
                return dialog;
            };
            let removal = &this.read(cx).container_removal;
            let submitting = removal.submitting;
            let error = removal.error.clone();
            let submit = weak.clone();
            let confirm = weak.clone();
            let cancel = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .keyboard(!submitting)
                .on_ok(move |_, _, cx| {
                    _ = confirm.update(cx, |this, cx| this.submit_remove_containers(cx));
                    // Close only after the engine reports success.
                    false
                })
                .on_cancel(move |_, _, cx| {
                    cancel
                        .upgrade()
                        .is_none_or(|this| !this.read(cx).container_removal.submitting)
                })
                .when_some(error, |dialog, error| {
                    dialog.child(div().text_sm().text_color(cx.theme().danger).child(error))
                })
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            button("cancel-remove-containers")
                                .outline()
                                .label("取消")
                                .disabled(submitting)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            button("submit-remove-containers")
                                .danger()
                                .label(if submitting { "删除中..." } else { "删除" })
                                .loading(submitting)
                                .disabled(submitting)
                                .on_click(move |_, _, cx| {
                                    _ = submit.update(cx, |this, cx| {
                                        this.submit_remove_containers(cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn submit_remove_containers(&mut self, cx: &mut Context<Self>) {
        if self.container_removal.submitting
            || self.container_removal.pending_ids.is_empty()
            || self.busy.is_some()
        {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.container_removal.error = Some("容器引擎不可用".into());
            cx.notify();
            return;
        };
        let ids = self.container_removal.pending_ids.clone();
        let total = ids.len();
        let handle = engine.remove_containers(ids);
        let revision = self.refresh_revision.advance();
        self.container_removal.submitting = true;
        self.container_removal.error = None;
        self.busy = Some("删除中...".into());
        self.loading = false;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.busy = None;
                this.container_removal.submitting = false;
                match result {
                    Ok((snapshot, failures)) => {
                        this.connected = true;
                        // On retry, do not submit containers already deleted by a partial success.
                        this.container_removal.pending_ids.retain(|id| {
                            snapshot
                                .containers
                                .iter()
                                .any(|container| &container.id == id)
                        });
                        this.apply_snapshot(snapshot);
                        this.after_volume_snapshot(cx);
                        if failures.is_empty() {
                            let window = this.window_handle;
                            _ = cx.update_window(window, |_, window, cx| window.close_dialog(cx));
                            this.notify_success(format!("已删除 {total} 个容器"), cx);
                        } else {
                            this.container_removal.error = Some(format!(
                                "已删除 {} 个容器，{} 个失败：\n{}",
                                total - failures.len(),
                                failures.len(),
                                failures.join("\n")
                            ));
                        }
                    }
                    Err(error) => this.container_removal.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        // While the engine starts or stops, the whole menu is placeholders
        // (Docker Desktop-style); the app bar gear still opens settings.
        let placeholders = self.engine_phase().is_some();
        let nav = div()
            .w(px(self.sidebar_width()))
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
            .child(div().flex_1())
            .child(self.sidebar_toggle(cx))
    }

    fn sidebar_width(&self) -> f32 {
        if self.sidebar_collapsed {
            SIDEBAR_COLLAPSED_WIDTH
        } else {
            SIDEBAR_WIDTH
        }
    }

    /// Bottom row of the sidebar that collapses it to icons or expands it.
    fn sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.sidebar_collapsed;
        let (icon, label) = if collapsed {
            (IconName::PanelLeftOpen, "展开菜单")
        } else {
            (IconName::PanelLeftClose, "收起菜单")
        };
        div()
            .id("sidebar-toggle")
            .mx_2()
            .px_3()
            .h(px(36.))
            .rounded_md()
            .flex()
            .items_center()
            .when(collapsed, |this| this.justify_center())
            .gap_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().sidebar_accent))
            .child(Icon::new(icon).small())
            .when(!collapsed, |this| this.child(label))
            .when(collapsed, |this| {
                this.tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
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
        if self.recovering() {
            return Some(EngineTransition::Recovering);
        }
        (self.loading && !self.connected).then_some(EngineTransition::Connecting)
    }

    /// The backend is restarting an engine that went away unexpectedly.
    fn recovering(&self) -> bool {
        matches!(self.recovery, EngineRecovery::Restarting { .. })
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
        self.engine_phase().is_some() || self.runtime_busy.is_some() || self.run.submitting
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
        let collapsed = self.sidebar_collapsed;
        let accent = brand_color(cx);
        div()
            .id(SharedString::from(format!("nav-{}", page.title())))
            .mx_2()
            .px_3()
            .h(px(36.))
            .rounded_md()
            .flex()
            .items_center()
            .when(collapsed, |this| this.justify_center())
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
            .when(!collapsed, |this| this.child(page.title()))
            .when(collapsed, |this| {
                this.tooltip(move |window, cx| Tooltip::new(page.title()).build(window, cx))
            })
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
                    button("refresh")
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
                        .child(div().text_2xl().font_semibold().child("容器运行概览"))
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
                                            .filter(|volume| self
                                                .snapshot
                                                .volume_in_use(&volume.name))
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
                        ),
                ),
        )
    }

    fn render_containers(&self, cx: &mut Context<Self>) -> Div {
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
        match selected {
            Some(container) => self.render_container_detail(&container, cx),
            None => self.render_container_list(cx),
        }
    }

    /// Docker Desktop-style container list: usage summary, search and the
    /// running-only switch above a table with selection and row actions.
    fn render_container_list(&self, cx: &mut Context<Self>) -> Div {
        let query = self.search_input.read(cx).value().to_string();
        let all_containers = &self.snapshot.containers;
        let containers: Vec<ContainerSummary> = all_containers
            .iter()
            .filter(|container| {
                container_matches(container, &query)
                    && (!self.containers_running_only || container.is_running())
            })
            .cloned()
            .collect();
        let shown = containers.len();
        let running = all_containers
            .iter()
            .filter(|container| container.is_running())
            .count();
        let busy = self.busy.is_some();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut checked: Vec<String> = self.containers_checked.iter().cloned().collect();
        checked.sort();
        let selected_containers: Vec<ContainerSummary> = all_containers
            .iter()
            .filter(|container| self.containers_checked.contains(&container.id))
            .cloned()
            .collect();

        let usage = if running == 0 {
            "暂无运行中的容器。".to_string()
        } else {
            format!("{running} 个容器运行中，实时统计尚未支持。")
        };

        let title = div()
            .flex()
            .items_center()
            .gap_3()
            .child(div().text_2xl().font_semibold().child("容器"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{running} 个运行中 · 共 {} 个",
                        all_containers.len()
                    )),
            )
            .child(div().flex_1())
            .child(
                list_action_button("refresh-containers", cx.theme().primary, cx)
                    .small()
                    .icon(IconName::RotateCw)
                    .tooltip("刷新")
                    .disabled(self.loading || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                button("open-create-container")
                    .pointer(true)
                    .primary()
                    .icon(IconName::Plus)
                    .label("创建")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_create_container_dialog(None, window, cx);
                    })),
            );

        let usage_row = div()
            .flex()
            .items_start()
            .gap_8()
            .child(container_usage(
                "容器 CPU 使用率",
                "需要 Engine 实时统计支持，当前版本暂未提供。",
                &usage,
                cx,
            ))
            .child(container_usage(
                "容器内存使用率",
                "需要 Engine 实时统计支持，当前版本暂未提供。",
                &usage,
                cx,
            ));

        let toolbar = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div().w(px(340.)).child(
                    Input::new(&self.search_input)
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true),
                ),
            )
            .child(
                Switch::new("container-running-only")
                    .checked(self.containers_running_only)
                    .label("仅显示运行中的容器")
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.containers_running_only = *checked;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .when(!checked.is_empty(), |this| {
                let count = checked.len();
                let selected = selected_containers.clone();
                this.child(
                    list_action_button("clear-container-selection", cx.theme().primary, cx)
                        .label("取消选择")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.containers_checked.clear();
                            cx.notify();
                        })),
                )
                .child(
                    button("remove-checked-containers")
                        .pointer(true)
                        .danger()
                        .icon(AssetIcon::Trash)
                        .label(format!("删除所选（{count}）"))
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove_containers(selected.clone(), window, cx);
                        })),
                )
            });

        let all_checked = shown > 0
            && containers
                .iter()
                .all(|container| self.containers_checked.contains(&container.id));
        let visible_ids: Vec<String> = containers
            .iter()
            .map(|container| container.id.clone())
            .collect();

        let header = div()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .text_xs()
            .font_semibold()
            .text_color(cx.theme().muted_foreground)
            .child(
                div().w(px(CONTAINER_CHECK_COLUMN)).flex_shrink_0().child(
                    Checkbox::new("check-all-containers")
                        .checked(all_checked)
                        .tooltip("全选当前列表")
                        .disabled(visible_ids.is_empty())
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            for id in &visible_ids {
                                if *checked {
                                    this.containers_checked.insert(id.clone());
                                } else {
                                    this.containers_checked.remove(id);
                                }
                            }
                            cx.stop_propagation();
                            cx.notify();
                        })),
                ),
            )
            .child(div().w(px(CONTAINER_STATUS_COLUMN)).flex_shrink_0())
            .child(div().flex_1().min_w(px(112.)).pr_3().child("名称"))
            .child(
                div()
                    .w(px(CONTAINER_ID_COLUMN))
                    .flex_shrink_0()
                    .child("容器 ID"),
            )
            .child(div().flex_1().min_w(px(112.)).pr_3().child("镜像"))
            .child(
                div()
                    .w(px(CONTAINER_PORTS_COLUMN))
                    .flex_shrink_0()
                    .child("端口"),
            )
            .child(
                div()
                    .w(px(CONTAINER_CPU_COLUMN))
                    .flex_shrink_0()
                    .child("CPU (%)"),
            )
            .child(
                div()
                    .w(px(CONTAINER_CREATED_COLUMN))
                    .flex_shrink_0()
                    .child("创建时间"),
            )
            .child(
                div()
                    .w(px(CONTAINER_ACTIONS_COLUMN))
                    .flex_shrink_0()
                    .child("操作"),
            );

        let panel = div()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header);

        let footer = div()
            .flex()
            .justify_end()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(format!("显示 {shown} 个"));

        if containers.is_empty() {
            return div()
                .flex_1()
                .min_h_0()
                .p_6()
                .flex()
                .flex_col()
                .gap_4()
                .child(title)
                .child(usage_row)
                .child(toolbar)
                .child(panel.child(if all_containers.is_empty() {
                    empty_state("还没有容器", "点击右上角“创建”，从镜像运行第一个容器。", cx)
                } else if self.containers_running_only && running == 0 {
                    empty_state(
                        "没有运行中的容器",
                        "关闭“仅显示运行中的容器”可查看全部容器。",
                        cx,
                    )
                } else {
                    empty_state("没有匹配的容器", "调整搜索条件或筛选方式。", cx)
                }))
                .child(footer);
        }

        let rows = containers
            .iter()
            .enumerate()
            .map(|(index, container)| self.render_container_row(index, container, now, busy, cx));

        div()
            .flex_1()
            .min_h_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(title)
            .child(usage_row)
            .child(toolbar)
            .child(
                panel.child(
                    div()
                        .id("container-rows")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(rows),
                ),
            )
            .child(footer)
    }

    fn render_container_row(
        &self,
        index: usize,
        container: &ContainerSummary,
        now: i64,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let detail_id = container.id.clone();
        let check_id = container.id.clone();
        let toggle_id = container.id.clone();
        let copy_id = container.id.clone();
        let image_container = container.clone();
        let is_checked = self.containers_checked.contains(&container.id);
        let running = container.is_running();
        let paused = container.state.eq_ignore_ascii_case("paused");
        let state_color = if running {
            cx.theme().success
        } else if paused {
            cx.theme().warning
        } else {
            cx.theme().muted_foreground
        };
        let state_dot = if running || paused {
            div().size(px(8.)).rounded_full().bg(state_color)
        } else {
            div()
                .size(px(8.))
                .rounded_full()
                .border_1()
                .border_color(state_color)
        };
        let status = if container.status.trim().is_empty() {
            container.state.clone()
        } else {
            container.status.clone()
        };
        let state_tooltip = status.clone();
        let menu_container = container.clone();
        let remove_container = container.clone();
        let weak = cx.weak_entity();

        div()
            .id(("container-row", index))
            .px_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .text_sm()
            .cursor_pointer()
            .when(is_checked, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_container(detail_id.clone(), cx);
            }))
            .child(
                div().w(px(CONTAINER_CHECK_COLUMN)).flex_shrink_0().child(
                    Checkbox::new(("check-container", index))
                        .checked(is_checked)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if *checked {
                                this.containers_checked.insert(check_id.clone());
                            } else {
                                this.containers_checked.remove(&check_id);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        })),
                ),
            )
            .child(
                div()
                    .id(("container-state", index))
                    .w(px(CONTAINER_STATUS_COLUMN))
                    .flex_shrink_0()
                    .child(state_dot)
                    .tooltip(move |window, cx| {
                        Tooltip::new(state_tooltip.clone()).build(window, cx)
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(112.))
                    .py_3()
                    .pr_3()
                    .truncate()
                    .child(container.name.clone()),
            )
            .child(
                div()
                    .w(px(CONTAINER_ID_COLUMN))
                    .flex_shrink_0()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .child(
                        div()
                            .id(("copy-container-id", index))
                            .cursor_pointer()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|style| style.underline())
                            .tooltip(|window, cx| Tooltip::new("复制容器 ID").build(window, cx))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            })
                            .child(short_id(&container.id)),
                    ),
            )
            .child(
                div().flex_1().min_w(px(112.)).pr_3().truncate().child(
                    div()
                        .id(("open-container-image", index))
                        .min_w_0()
                        .truncate()
                        .cursor_pointer()
                        .text_color(cx.theme().primary)
                        .hover(|style| style.underline())
                        .tooltip(|window, cx| Tooltip::new("查看镜像详情").build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_container_image(&image_container, cx);
                        }))
                        .child(container.image.clone()),
                ),
            )
            .child(
                div()
                    .w(px(CONTAINER_PORTS_COLUMN))
                    .flex_shrink_0()
                    .pr_2()
                    .truncate()
                    .child(format_port_bindings(&container.ports)),
            )
            .child(
                div()
                    .w(px(CONTAINER_CPU_COLUMN))
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground)
                    .child("N/A"),
            )
            .child(
                div()
                    .w(px(CONTAINER_CREATED_COLUMN))
                    .flex_shrink_0()
                    .child(format_relative(container.created, now)),
            )
            .child(
                div()
                    .w(px(CONTAINER_ACTIONS_COLUMN))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        list_action_button(("toggle-container", index), cx.theme().primary, cx)
                            .small()
                            .icon(if running {
                                IconName::Square
                            } else {
                                IconName::Play
                            })
                            .tooltip(if running { "停止" } else { "启动" })
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.start_container_action(
                                    toggle_id.clone(),
                                    if running {
                                        ContainerAction::Stop
                                    } else {
                                        ContainerAction::Start
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        list_action_button(("container-more", index), cx.theme().primary, cx)
                            .small()
                            .icon(IconName::EllipsisVertical)
                            .disabled(busy)
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                container_menu(menu, &menu_container, weak.clone())
                            }),
                    )
                    .child(
                        list_action_button(("remove-container-row", index), cx.theme().danger, cx)
                            .small()
                            .icon(Icon::new(AssetIcon::Trash).text_color(cx.theme().danger))
                            .tooltip("删除")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.confirm_remove_container(&remove_container, window, cx);
                            })),
                    ),
            )
    }

    /// Container detail page: breadcrumb, identity header with the Docker
    /// Desktop action cluster, and logs / details / mounts tabs.
    fn render_container_detail(&self, container: &ContainerSummary, cx: &mut Context<Self>) -> Div {
        let busy = self.busy.is_some();
        let running = container.is_running();
        let status = if container.status.trim().is_empty() {
            container.state.clone()
        } else {
            container.status.clone()
        };
        let tab = self.container_tab.min(2);
        let weak = cx.weak_entity();
        let back_weak = weak.clone();
        let start_id = container.id.clone();
        let stop_id = container.id.clone();
        let restart_id = container.id.clone();
        let remove_container = container.clone();
        let logs_id = container.id.clone();
        let logs_for_copy = self.logs.clone();

        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                button("stop-container")
                    .secondary()
                    .icon(IconName::Square)
                    .tooltip("停止")
                    .disabled(busy || !running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_container_action(stop_id.clone(), ContainerAction::Stop, cx);
                    })),
            )
            .child(
                button("start-container")
                    .primary()
                    .icon(IconName::Play)
                    .tooltip("启动")
                    .disabled(busy || running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_container_action(start_id.clone(), ContainerAction::Start, cx);
                    })),
            )
            .child(
                button("restart-container")
                    .outline()
                    .icon(IconName::RotateCw)
                    .tooltip("重启")
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
                button("remove-container")
                    .danger()
                    .icon(AssetIcon::Trash)
                    .tooltip("删除")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove_container(&remove_container, window, cx);
                    })),
            );

        let tabs = div()
            .flex()
            .items_center()
            .gap_2()
            .px_6()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("container-detail-tabs")
                    .underline()
                    .flex_1()
                    .selected_index(tab)
                    .child(Tab::new().label("日志"))
                    .child(Tab::new().label("详情"))
                    .child(Tab::new().label("挂载"))
                    .on_click({
                        let weak = weak.clone();
                        move |index, _, cx| {
                            _ = weak.update(cx, |this, cx| {
                                this.container_tab = *index;
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(
                button("copy-container-logs")
                    .ghost()
                    .small()
                    .icon(IconName::Copy)
                    .tooltip("复制日志")
                    .disabled(self.logs.is_empty())
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(logs_for_copy.clone()));
                    }),
            )
            .child(
                button("refresh-container-logs")
                    .ghost()
                    .small()
                    .icon(IconName::RotateCw)
                    .tooltip("刷新日志")
                    .disabled(self.logs_loading)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.load_logs(logs_id.clone(), cx);
                    })),
            );

        let content = match tab {
            1 => self
                .render_container_details_tab(container, cx)
                .into_any_element(),
            2 => self
                .render_container_mounts_tab(container, cx)
                .into_any_element(),
            _ => self.render_container_logs_tab(cx).into_any_element(),
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div().px_6().pt_4().child(
                    Breadcrumb::new()
                        .child(
                            BreadcrumbItem::new("容器")
                                .text_color(cx.theme().primary)
                                .on_click(move |_, _, cx| {
                                    _ = back_weak
                                        .update(cx, |this, cx| this.close_container_detail(cx));
                                }),
                        )
                        .child(BreadcrumbItem::new(container.name.clone())),
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
                        button("container-back")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .tooltip("返回容器列表")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.close_container_detail(cx)),
                            ),
                    )
                    .child(
                        Icon::new(AssetIcon::Container)
                            .size(px(40.))
                            .text_color(if running {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            }),
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
                                    .truncate()
                                    .child(container.name.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .flex_wrap()
                                    .text_sm()
                                    .child(copyable_value(
                                        "copy-detail-id",
                                        AssetIcon::Link,
                                        short_id(&container.id),
                                        "复制容器 ID",
                                        cx.theme().foreground,
                                        cx.theme().muted_foreground,
                                    ))
                                    .child(copyable_value(
                                        "copy-detail-image",
                                        AssetIcon::Link,
                                        container.image.clone(),
                                        "复制镜像引用",
                                        cx.theme().primary,
                                        cx.theme().muted_foreground,
                                    ))
                                    .child(
                                        div()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format_port_bindings(&container.ports)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_6()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_end()
                                    .gap_1()
                                    .child(div().text_xs().font_semibold().child("状态"))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(status),
                                    ),
                            )
                            .child(actions),
                    ),
            )
            .child(tabs)
            .child(content)
    }

    fn render_container_logs_tab(&self, cx: &mut Context<Self>) -> Div {
        let is_placeholder =
            self.logs_loading || self.logs_error.is_some() || self.logs.trim().is_empty();
        let text = if self.logs_loading {
            "正在读取日志…".to_string()
        } else if let Some(error) = &self.logs_error {
            error.clone()
        } else if self.logs.trim().is_empty() {
            "该容器最近没有 stdout/stderr 日志。".to_string()
        } else {
            self.logs.clone()
        };
        let color = if self.logs_error.is_some() {
            cx.theme().danger
        } else if is_placeholder {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                div()
                    .id("container-log-scroll")
                    .size_full()
                    .overflow_scroll()
                    .track_scroll(&self.container_logs_scroll)
                    .child(
                        div()
                            .p_4()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(color)
                            .when(!is_placeholder, |this| this.whitespace_nowrap())
                            .child(text),
                    ),
            )
            .when(!is_placeholder, |this| {
                this.child(
                    div().absolute().bottom_4().right_6().child(
                        button("container-logs-bottom")
                            .outline()
                            .icon(IconName::ArrowDown)
                            .tooltip("滚动到底部")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.container_logs_scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                )
            })
    }

    fn render_container_details_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = if container.status.trim().is_empty() {
            container.state.clone()
        } else {
            format!("{}（{}）", container.state, container.status)
        };
        let volumes = if container.volumes.is_empty() {
            "—".to_string()
        } else {
            container.volumes.join(", ")
        };

        div()
            .id("container-details-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .child(
                section_panel(cx)
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(detail_row("容器 ID", container.id.clone(), cx))
                    .child(detail_row("镜像", container.image.clone(), cx))
                    .child(detail_row("镜像 ID", short_id(&container.image_id), cx))
                    .child(detail_row("状态", state, cx))
                    .child(detail_row(
                        "创建时间",
                        format_timestamp(container.created),
                        cx,
                    ))
                    .child(detail_row("端口", format_ports(&container.ports), cx))
                    .child(detail_row("命名卷", volumes, cx)),
            )
    }

    fn render_container_mounts_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = if container.volumes.is_empty() {
            empty_state("没有命名卷挂载", "该容器没有挂载命名卷。", cx)
        } else {
            let mut list = section_panel(cx).flex().flex_col();
            for (index, volume) in container.volumes.iter().enumerate() {
                list = list.child(
                    div()
                        .id(("container-volume", index))
                        .px_4()
                        .py_3()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            Icon::new(AssetIcon::Database)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_sm().child(volume.clone())),
                );
            }
            list
        };

        div()
            .id("container-mounts-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .child(content)
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
                        button("install-runtime")
                            .primary()
                            .icon(IconName::HardDrive)
                            .label(if snapshot.installed { "修复环境" } else { "安装环境" })
                            .disabled(busy)
                            .pointer(!busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Install, cx);
                            })),
                    )
                    .child(
                        button("start-runtime")
                            .outline()
                            .icon(IconName::Play)
                            .label("启动")
                            .disabled(busy || !snapshot.installed || snapshot.engine_ready)
                            .pointer(!(busy || !snapshot.installed || snapshot.engine_ready))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Start, cx);
                            })),
                    )
                    .child(
                        button("stop-runtime")
                            .outline()
                            .icon(IconName::Square)
                            .label("停止")
                            .disabled(busy || !snapshot.running)
                            .pointer(!(busy || !snapshot.running))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_runtime_operation(RuntimeOperation::Stop, cx);
                            })),
                    )
                    .child(
                        button("refresh-runtime")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label("刷新状态")
                            .disabled(busy)
                            .pointer(!busy)
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

    /// Docker Desktop's "Docker engine" page: edit `daemon.json` directly.
    fn render_docker_engine_settings(&self, cx: &mut Context<Self>) -> Div {
        let managed = self.manages_engine();
        let loaded = self.daemon_loaded.is_some();
        let busy = self.runtime_busy.is_some();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("通过编辑 JSON 格式的 Docker daemon")
                    .child(
                        div()
                            .id("open-daemon-docs")
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(cx.theme().link)
                            .underline()
                            .cursor_pointer()
                            .child("配置文件")
                            .child(Icon::new(AssetIcon::ExternalLink).xsmall())
                            .on_click(|_, _, cx| cx.open_url(DAEMON_CONFIG_DOCS_URL)),
                    )
                    .child("来配置 Engine。"),
            )
            .child(callout(
                CalloutKind::Warning,
                "高级设置",
                muted(
                    "错误的配置可能导致 Docker 无法启动，请谨慎修改。应用前会用 dockerd --validate 校验，应用后 Engine 会重启，运行中的容器会被停止。proxies 由“代理”设置管理，bip、ip、ipv6 等网络配置由“网络”设置管理，不在此显示。",
                    cx,
                ),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(settings_label("Daemon 配置文件", cx)))
                    .child(
                        button("reload-daemon-config")
                            .ghost()
                            .small()
                            .icon(IconName::Redo)
                            .label(if self.daemon_loading { "读取中…" } else { "重新读取" })
                            .disabled(!managed || self.daemon_loading || busy)
                            .pointer(!(!managed || self.daemon_loading || busy))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_daemon_config(cx);
                                cx.notify();
                            })),
                    ),
            )
            .when(!managed, |this| {
                this.child(muted(
                    "当前 Engine 不由 Rusker 管理（设置了 DOCKER_HOST），请直接编辑该 Engine 的 daemon.json。",
                    cx,
                ))
            })
            .when_some(self.daemon_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(managed, |this| {
                this.child(
                    Editor::new(&self.daemon_editor)
                        .h(px(320.))
                        .disabled(!loaded || busy),
                )
            })
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
                let saved = self
                    .settings
                    .wsl
                    .integrations
                    .contains_key(&distribution.name);
                let drafted = self
                    .settings_draft
                    .wsl
                    .integrations
                    .contains_key(&distribution.name);
                let switch_disabled = busy
                    || self.settings_store.is_none()
                    || (!drafted && !saved && !snapshot.engine_ready);
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
                        .checked(drafted)
                        .disabled(switch_disabled)
                        .pointer(!switch_disabled)
                        .on_change(cx.listener(move |this, enabled, _, cx| {
                            this.set_runtime_integration(name.clone(), *enabled, cx);
                        })),
                    )
            }))
            .child(muted(
                "点击“应用”后集成才会生效。资源节省模式下 WSL 发行版内的 docker 命令无法唤醒 Engine，请通过 Windows 端点或 Rusker 恢复。",
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
                    SettingsSection::Network => self.render_network_settings(cx),
                    SettingsSection::WslIntegration => self.render_wsl_integration(cx),
                    SettingsSection::DockerEngine => self.render_docker_engine_settings(cx),
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
                    .relative()
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
                    )
                    .when(self.settings_applying, |this| {
                        this.child(self.render_settings_applying(cx))
                    }),
            )
            .child(self.render_settings_footer(cx))
    }

    /// Docker Desktop-style "Applying settings" veil over the settings
    /// content; it also swallows clicks so nothing is edited mid-apply.
    fn render_settings_applying(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-applying")
            .absolute()
            .inset_0()
            .occlude()
            .bg(cx.theme().background.opacity(0.85))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(self.loading_ring(px(40.), Some(brand_color(cx))))
            .child(
                div()
                    .text_base()
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child("正在应用设置"),
            )
            .when_some(self.settings_notice.clone(), |this, notice| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(notice),
                )
            })
            .with_animation(
                "settings-applying-enter",
                Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
                |this, delta| this.opacity(delta),
            )
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
        let applying = self.settings_applying;
        let (message, color) = if let Some(error) = self.settings_error.clone() {
            (error, cx.theme().danger)
        } else if applying {
            (
                self.settings_notice
                    .clone()
                    .unwrap_or_else(|| "正在应用设置…".into()),
                cx.theme().muted_foreground,
            )
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
                button("discard-settings")
                    .outline()
                    .label("放弃更改")
                    .disabled(!dirty || applying)
                    .pointer(dirty && !applying)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.discard_settings(window, cx);
                    })),
            )
            .child(
                button("apply-settings")
                    .primary()
                    .label("应用")
                    .disabled(applying || !dirty || apply_blocked)
                    .pointer(!applying && dirty && !apply_blocked)
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
                                button("browse-disk-location")
                                    .outline()
                                    .label("浏览…")
                                    .disabled(busy)
                                    .pointer(!busy)
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
                            .pointer(managed)
                            .on_change(cx.listener(|this, enabled, _, cx| {
                                this.settings_draft.resource_saver.enabled = *enabled;
                                this.settings_notice = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(muted(
                if managed {
                    "没有运行中的容器且未启用 WSL 集成时暂停 Engine，保留 WSL 发行版和数据盘以便快速恢复。Windows 客户端再次连接时自动继续；内存回收由 WSL 管理。"
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
                                            .disabled(!enabled || !managed)
                                            .pointer(enabled && managed),
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
        let mode = self.settings_draft.docker_proxy.mode;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(proxy_heading(
                "Docker Engine 代理",
                self.proxy_mode_select(
                    "engine-proxy-mode",
                    mode,
                    &ProxyMode::ENGINE,
                    |settings| &mut settings.docker_proxy.mode,
                    cx,
                ),
                cx,
            ))
            .child(muted(
                "Engine 拉取或推送镜像等出站请求使用的代理。应用后直接写入受管 Engine 的 daemon.json 并生效。",
                cx,
            ))
            .when(mode == ProxyMode::Manual, |this| {
                this.child(proxy_fields(
                    [
                        &self.http_proxy_input,
                        &self.https_proxy_input,
                        &self.no_proxy_input,
                    ],
                    cx,
                ))
            })
            .when(mode == ProxyMode::System, |this| {
                this.child(proxy_summary(&self.settings_draft.docker_proxy.resolve(None), cx))
            })
            .when(mode != ProxyMode::None || self.settings.docker_proxy.mode != ProxyMode::None, |this| {
                this.child(callout(
                    if manages_engine { CalloutKind::Info } else { CalloutKind::Warning },
                    if manages_engine { "不会重启 Engine" } else { "需要手动应用" },
                    muted(
                        if manages_engine {
                            "与 Docker Desktop 相同，Engine 始终经 Rusker 内置代理出网，代理设置在 Windows 上逐个请求生效：127.0.0.1 指 Windows 本机，与 WSL 网络模式（NAT/mirrored）无关；修改后无需重启 Engine，系统代理的变化也会自动跟随。"
                        } else {
                            "当前 Engine 不由 Rusker 管理，无法直接修改其配置。请导出后手动合并 proxies 到 daemon.json 并重启 Engine；不要覆盖 daemon.json 中的其他配置。"
                        },
                        cx,
                    ),
                    cx,
                ))
            })
            .when(!manages_engine, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            button("export-proxy-settings")
                                .outline()
                                .icon(IconName::FileText)
                                .label("导出 Engine JSON")
                                .disabled(self.settings_store.is_none())
                                .pointer(self.settings_store.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_proxy_settings(cx);
                                })),
                        )
                        .child(muted(format!("导出到：{export_path}"), cx)),
                )
            })
            .child(div().my_2().h(px(1.)).bg(cx.theme().border))
            .child(self.render_container_proxy_settings(cx))
    }

    /// Docker Desktop's "Containers proxy": outbound traffic from containers.
    fn render_container_proxy_settings(&self, cx: &mut Context<Self>) -> Div {
        let config_path = docker_client_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "不可用".into());
        let mode = self.settings_draft.container_proxy.mode;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(proxy_heading(
                "容器代理",
                self.proxy_mode_select(
                    "container-proxy-mode",
                    mode,
                    &ProxyMode::CONTAINER,
                    |settings| &mut settings.container_proxy.mode,
                    cx,
                ),
                cx,
            ))
            .child(muted(
                "运行中容器的出站流量使用的代理，新建容器时以 HTTP_PROXY / HTTPS_PROXY / NO_PROXY 环境变量传入。",
                cx,
            ))
            .when(mode == ProxyMode::Manual, |this| {
                this.child(proxy_fields(
                    [
                        &self.container_http_proxy_input,
                        &self.container_https_proxy_input,
                        &self.container_no_proxy_input,
                    ],
                    cx,
                ))
            })
            .when(follows_other_proxy(mode), |this| {
                // Resolved against the edited Engine proxy, so "same as host"
                // previews what applying would give.
                let engine = DockerProxySettings {
                    mode: self.settings_draft.docker_proxy.mode,
                    ..self.proxy_draft(cx)
                };
                let resolved = self.settings_draft.container_proxy.resolve(Some(&engine));
                this.child(proxy_summary(&resolved, cx))
            })
            .child(callout(
                CalloutKind::Info,
                "仅对新建容器生效",
                muted(
                    format!(
                        "应用后写入 Docker 客户端配置 {config_path} 的 proxies.default（保留其他配置），docker run / docker build 会自动传入代理；已有容器需重新创建。容器内的 127.0.0.1 指向容器自身，请使用宿主机的局域网地址。"
                    ),
                    cx,
                ),
                cx,
            ))
    }

    /// Docker Desktop-style proxy mode drop-down.
    fn proxy_mode_select(
        &self,
        id: &'static str,
        current: ProxyMode,
        options: &'static [ProxyMode],
        field: fn(&mut AppSettings) -> &mut ProxyMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.settings_select(id, current, options, ProxyMode::label, field, true, cx)
    }

    /// Drop-down editing one drafted setting.
    #[allow(clippy::too_many_arguments)]
    fn settings_select<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        current: T,
        options: &'static [T],
        label: fn(T) -> &'static str,
        field: fn(&mut AppSettings) -> &mut T,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.weak_entity();
        button(id)
            .outline()
            .label(label(current))
            .dropdown_caret(true)
            .disabled(!enabled)
            .pointer(enabled)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                for &option in options {
                    let weak = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label(option))
                            .checked(option == current)
                            .on_click(move |_, _, cx| {
                                _ = weak.update(cx, |this, cx| {
                                    *field(&mut this.settings_draft) = option;
                                    this.settings_notice = None;
                                    this.settings_error = None;
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu.min_w(px(180.))
            })
    }

    /// How published ports reach the LAN under the current WSL networking
    /// mode, plus listen failures. None while no engine session runs.
    fn lan_forward_summary(&self) -> Option<(String, Vec<String>)> {
        let state = &self.lan_forward;
        let mode = state.mode.as_deref()?;
        let text = match mode {
            "mirrored" => {
                "WSL 使用镜像网络：已发布端口直接出现在 Windows 网络上，局域网设备可通过本机 IP 访问。".to_string()
            }
            "nat" | "unknown" if self.settings.network.port_binding == PortBinding::LocalhostOnly => {
                "WSL 使用 NAT 网络：端口仅绑定本机，不转发到局域网。".to_string()
            }
            "nat" | "unknown" => {
                let ports = if state.forwarded.is_empty() {
                    "当前没有需要转发的端口".to_string()
                } else {
                    let mut list = state
                        .forwarded
                        .iter()
                        .take(12)
                        .map(|(protocol, port)| format!("{} {port}", protocol.label()))
                        .collect::<Vec<_>>()
                        .join("、");
                    if state.forwarded.len() > 12 {
                        list.push_str(&format!(" 等 {} 个", state.forwarded.len()));
                    }
                    format!("已转发 {list}")
                };
                format!(
                    "WSL 使用 NAT 网络：与 Docker Desktop 相同，Rusker 在 Windows 上监听发布到所有接口的端口（TCP/UDP，IPv4 与 IPv6）并转发到 VM，局域网设备可通过本机 IP 访问（{ports}）。首次监听时请在 Windows 防火墙提示中允许 Rusker 访问网络。"
                )
            }
            other => format!("WSL 网络模式为 {other}，Rusker 不转发端口。"),
        };
        Some((text, state.failures.clone()))
    }

    /// Docker Desktop's "Network" section, applied to the managed Engine's
    /// `daemon.json`.
    fn render_network_settings(&self, cx: &mut Context<Self>) -> Div {
        let managed = self.manages_engine();
        let network = &self.settings_draft.network;
        let indented = |cx: &App| {
            div()
                .ml_2()
                .pl_4()
                .border_l_2()
                .border_color(cx.theme().border)
                .flex()
                .flex_col()
                .gap_2()
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(muted("配置 Docker 容器与网络交互的方式。", cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_label("Docker 子网", cx))
                    .child(Input::new(&self.network_subnet_input).disabled(!managed))
                    .child(muted(
                        "默认 bridge 网络（docker0）使用的 IPv4 网段，留空则使用 Docker 默认值。与局域网或 VPN 网段冲突时可修改；已有容器需重新创建才会使用新网段。",
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(div().flex_1().child(settings_label("启用主机网络", cx)))
                    .child(
                        Switch::new("network-host-networking")
                            .checked(network.host_networking)
                            .disabled(!managed)
                            .pointer(managed)
                            .on_change(cx.listener(|this, enabled, _, cx| {
                                this.settings_draft.network.host_networking = *enabled;
                                this.settings_notice = None;
                                this.settings_error = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(muted(
                "以 --net=host 启动的容器监听的 TCP 和 UDP 端口也会在 Windows 上转发：NAT 网络下局域网设备可访问，UDP 也可通过 localhost 访问。localhost 上的 TCP 由 WSL 自身转发，不受此开关影响；镜像网络下无需转发。应用后立即生效，不会重启 Engine。",
                cx,
            ))
            .child(proxy_heading(
                "端口绑定行为",
                self.settings_select(
                    "network-port-binding",
                    network.port_binding,
                    &PortBinding::ALL,
                    PortBinding::label,
                    |settings| &mut settings.network.port_binding,
                    managed,
                    cx,
                ),
                cx,
            ))
            .child(muted(
                "控制发布的容器端口绑定到所有网络接口（默认），还是只绑定到 127.0.0.1。端口映射中显式指定的地址（如 0.0.0.0:8080:80）不受影响。",
                cx,
            ))
            .when_some(self.lan_forward_summary(), |this, (text, failures)| {
                this.child(
                    indented(cx)
                        .child(muted(text, cx))
                        .children(failures.into_iter().map(|failure| {
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child(failure)
                        })),
                )
            })
            .child(proxy_heading(
                "默认网络模式",
                self.settings_select(
                    "network-mode",
                    network.mode,
                    &NetworkingMode::ALL,
                    NetworkingMode::label,
                    |settings| &mut settings.network.mode,
                    managed,
                    cx,
                ),
                cx,
            ))
            .child(muted(
                "作用于默认 bridge 网络和之后新建的 bridge 网络，创建网络时仍可单独指定（docker network create --ipv6）。",
                cx,
            ))
            .when(network.mode == NetworkingMode::DualStack, |this| {
                this.child(indented(cx).child(muted(
                    "默认 bridge 网络使用 IPv6 子网 fd00:7275:736b::/64，新建网络从 Docker 的 IPv6 地址池分配。",
                    cx,
                )))
            })
            .child(callout(
                CalloutKind::Info,
                if managed { "会重启 Engine" } else { "需要手动应用" },
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_color(cx.theme().muted_foreground)
                    .child(if managed {
                        "网络配置写入受管 Engine 的 daemon.json，变化时 Engine 会重启，运行中的容器会被停止。"
                    } else {
                        "当前 Engine 不由 Rusker 管理（设置了 DOCKER_HOST），请直接编辑该 Engine 的 daemon.json。"
                    })
                    .child(
                        div()
                            .id("open-network-docs")
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(cx.theme().link)
                            .underline()
                            .cursor_pointer()
                            .child("了解更多")
                            .child(Icon::new(AssetIcon::ExternalLink).xsmall())
                            .on_click(|_, _, cx| cx.open_url(NETWORKING_DOCS_URL)),
                    ),
                cx,
            ))
    }
}

/// Transparent list action using the shared theme's hover and pressed colors.
fn list_action_button(id: impl Into<ElementId>, color: Hsla, cx: &App) -> Button {
    button(id).pointer(true).custom(
        ButtonCustomVariant::new(cx)
            .foreground(color)
            .hover(cx.theme().button_hover)
            .active(cx.theme().button_active),
    )
}

/// Usage summary block shown above the container table.
fn container_usage(
    title: &'static str,
    tooltip: &'static str,
    value: &str,
    cx: &App,
) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_sm().font_medium().child(title))
                .child(
                    div()
                        .id(SharedString::from(format!("container-usage-{title}")))
                        .child(Icon::new(IconName::Info).xsmall().text_color(muted))
                        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx)),
                ),
        )
        .child(div().text_sm().text_color(muted).child(value.to_string()))
}

/// A link-coloured value that copies `text` when clicked.
fn copyable_value(
    id: impl Into<ElementId>,
    icon: impl Into<AssetIcon>,
    text: impl Into<SharedString>,
    tooltip: &'static str,
    text_color: Hsla,
    icon_color: Hsla,
) -> impl IntoElement {
    let text: SharedString = text.into();
    let copy_text = text.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .min_w_0()
        .cursor_pointer()
        .hover(|style| style.underline())
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
        })
        .child(Icon::new(icon.into()).xsmall().text_color(icon_color))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(text_color)
                .child(text),
        )
}

/// Custom content spans the menu row, including its horizontal padding,
/// so both the icon and label (and the space around them) use a pointer.
/// Keep the handler on PopupMenuItem to preserve dismissal and keyboard activation.
fn container_menu_item(label: &'static str, icon: impl Into<Icon>) -> PopupMenuItem {
    let icon: Icon = icon.into();
    PopupMenuItem::element(move |_, _| {
        div()
            .flex_1()
            .min_h(px(26.))
            .mx_neg_2()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .child(icon.clone().xsmall())
            .child(label)
    })
}

fn container_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    container: &ContainerSummary,
    weak: gpui_kit::WeakEntity<RuskerApp>,
) -> gpui_kit::component::menu::PopupMenu {
    let detail_container = container.clone();
    let detail_weak = weak.clone();
    let restart_id = container.id.clone();
    let restart_weak = weak;
    let id = container.id.clone();
    let image = container.image.clone();
    menu.item(
        container_menu_item("查看详情", AssetIcon::Container).on_click(move |_, _, cx| {
            _ = detail_weak.update(cx, |this, cx| {
                this.select_container(detail_container.id.clone(), cx);
            });
        }),
    )
    .item(
        container_menu_item("重启", IconName::RotateCw).on_click(move |_, _, cx| {
            _ = restart_weak.update(cx, |this, cx| {
                this.start_container_action(restart_id.clone(), ContainerAction::Restart, cx);
            });
        }),
    )
    .separator()
    .item(
        container_menu_item("复制容器 ID", IconName::Copy).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
        }),
    )
    .item(
        container_menu_item("复制镜像引用", IconName::Copy).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(image.clone()));
        }),
    )
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
            // The container pages carry their own Docker Desktop-style title
            // row (list) or breadcrumb (detail).
            if self.page != Page::Containers {
                content = content.child(self.render_header(phase.is_none(), cx));
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
const SIDEBAR_COLLAPSED_WIDTH: f32 = 56.;
/// Seconds per spinner revolution.
const SPINNER_PERIOD_SECS: f32 = 0.9;
/// Seconds per status-dot breath.
const STATUS_PULSE_PERIOD_SECS: f32 = 1.6;
const APP_BAR_HEIGHT: f32 = 48.;
const WINDOW_BUTTON_WIDTH: f32 = 46.;
/// Container table columns; the name and image columns share the rest.
const CONTAINER_CHECK_COLUMN: f32 = 34.;
const CONTAINER_STATUS_COLUMN: f32 = 26.;
const CONTAINER_ID_COLUMN: f32 = 108.;
const CONTAINER_PORTS_COLUMN: f32 = 88.;
const CONTAINER_CPU_COLUMN: f32 = 56.;
const CONTAINER_CREATED_COLUMN: f32 = 88.;
const CONTAINER_ACTIONS_COLUMN: f32 = 108.;

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
            .cursor_pointer()
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
                    .w(px(self.sidebar_width()))
                    .h_full()
                    .flex_shrink_0()
                    .px_4()
                    .flex()
                    .items_center()
                    .when(self.sidebar_collapsed, |this| this.justify_center())
                    .gap_2()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(self.render_status_dot(color))
                    .when(!self.sidebar_collapsed, |this| {
                        this.child(div().font_medium().text_color(color).child(label))
                    }),
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
        match self
            .engine_transition
            .or(self.recovering().then_some(EngineTransition::Recovering))
        {
            Some(EngineTransition::Stopping) => {
                return ("Engine 停止中…".into(), cx.theme().warning)
            }
            Some(EngineTransition::Starting) => {
                return ("Engine 启动中…".into(), cx.theme().warning)
            }
            Some(EngineTransition::Recovering) => {
                return ("Engine 重启中…".into(), cx.theme().warning)
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
        // Backend readiness precedes the original action's fresh snapshot.
        // Read the live session rather than the periodically polled snapshot:
        // during that gap connected/loading can both be false, even though
        // the engine is already handling the request that woke it.
        let session_running = self.manages_engine()
            && self
                .runtime_host
                .as_ref()
                .is_some_and(RuntimeHostService::engine_session_running);
        if let Some(label) = engine_reconnect_label(
            session_running,
            self.connected,
            self.busy.is_some(),
            self.loading,
        ) {
            return (label.into(), cx.theme().warning);
        }
        let label = if cfg!(windows) && !self.runtime_snapshot.installed && self.manages_engine() {
            "Engine 未安装"
        } else {
            "Engine 已停止"
        };
        (label.into(), cx.theme().danger)
    }
}

/// A live managed session must not be labelled stopped merely because the
/// action/snapshot that establishes the UI connection has not completed yet.
fn engine_reconnect_label(
    session_running: bool,
    connected: bool,
    busy: bool,
    loading: bool,
) -> Option<&'static str> {
    if connected {
        None
    } else if session_running && busy {
        Some("Engine 恢复中…")
    } else if session_running || loading {
        Some("正在连接 Engine…")
    } else {
        None
    }
}

#[cfg(test)]
mod engine_status_tests {
    use super::engine_reconnect_label;

    #[test]
    fn wake_up_action_is_not_reported_as_stopped_before_snapshot() {
        // Saver has cleared, but action deliberately sets loading=false and
        // connected remains false until its full snapshot has arrived.
        assert_eq!(
            engine_reconnect_label(true, false, true, false),
            Some("Engine 恢复中…")
        );
        // Also covers an external CLI wake-up before the next UI poll.
        assert_eq!(
            engine_reconnect_label(true, false, false, false),
            Some("正在连接 Engine…")
        );
    }

    #[test]
    fn actual_stop_and_success_do_not_get_a_reconnect_label() {
        assert_eq!(engine_reconnect_label(false, false, false, false), None);
        assert_eq!(engine_reconnect_label(false, false, true, false), None);
        assert_eq!(engine_reconnect_label(true, true, true, false), None);
        assert_eq!(
            engine_reconnect_label(false, false, false, true),
            Some("正在连接 Engine…")
        );
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
            change_theme(mode, cx);
        }
        ThemePreference::Light => {
            cx.set_window_appearance(Some(WindowAppearance::Light));
            change_theme(ThemeMode::Light, cx);
        }
        ThemePreference::Dark => {
            cx.set_window_appearance(Some(WindowAppearance::Dark));
            change_theme(ThemeMode::Dark, cx);
        }
    }
}

/// The saved disk location, `None` for the default.
/// Pointing-hand cursor for enabled controls; gpui-component buttons and
/// switches otherwise show the arrow.
trait PointerCursor: gpui_kit::Styled + Sized {
    fn pointer(self, enabled: bool) -> Self {
        if enabled {
            self.cursor_pointer()
        } else {
            self.cursor_default()
        }
    }
}

impl<T: gpui_kit::Styled> PointerCursor for T {}

/// `(distribution, enabled)` changes needed to go from `saved` to `draft`.
fn integration_changes(
    saved: &std::collections::BTreeMap<String, bool>,
    draft: &std::collections::BTreeMap<String, bool>,
) -> Vec<(String, bool)> {
    let enabled = |map: &std::collections::BTreeMap<String, bool>, name: &String| {
        map.get(name).copied().unwrap_or(false)
    };
    saved
        .keys()
        .chain(draft.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|name| enabled(saved, name) != enabled(draft, name))
        .map(|name| (name.clone(), enabled(draft, name)))
        .collect()
}

fn disk_location_setting(settings: &AppSettings) -> Option<PathBuf> {
    let location = settings.vm.disk_image_location.trim();
    (!location.is_empty()).then(|| PathBuf::from(location))
}

/// Section title with its mode drop-down on the right.
fn proxy_heading(title: &'static str, select: impl IntoElement, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap_4()
        .child(div().flex_1().child(settings_label(title, cx)))
        .child(select)
}

/// Manual configuration fields, indented under the heading like Docker
/// Desktop.
fn proxy_fields(inputs: [&Entity<InputState>; 3], cx: &App) -> Div {
    let [http, https, no_proxy] = inputs;
    div()
        .ml_2()
        .pl_4()
        .border_l_2()
        .border_color(cx.theme().border)
        .flex()
        .flex_col()
        .gap_4()
        .child(form_field(
            "Web 服务器（HTTP）",
            http,
            "支持 http:// 或 https://，不保存内嵌用户名或密码。",
            cx,
        ))
        .child(form_field(
            "安全 Web 服务器（HTTPS）",
            https,
            "HTTPS 请求使用的代理地址，也可以填写 http:// 代理。",
            cx,
        ))
        .child(form_field(
            "对以下主机和域名不使用代理",
            no_proxy,
            "逗号分隔，例如 registry-1.docker.com,*.docker.com,10.0.0.0/8",
            cx,
        ))
}

/// The addresses a non-manual mode currently resolves to.
fn proxy_summary(proxy: &ResolvedProxy, cx: &App) -> Div {
    let text = if proxy.is_empty() {
        "当前未检测到可用的 HTTP/HTTPS 代理，将直接连接。".to_string()
    } else {
        let or_none = |value: &str| {
            if value.is_empty() {
                "（无）".to_string()
            } else {
                value.to_string()
            }
        };
        format!(
            "当前生效：HTTP {} · HTTPS {} · 不使用代理 {}",
            or_none(&proxy.http_proxy),
            or_none(&proxy.https_proxy),
            or_none(&proxy.no_proxy)
        )
    };
    div()
        .ml_2()
        .pl_4()
        .border_l_2()
        .border_color(cx.theme().border)
        .child(muted(text, cx))
}

/// Proxy modes whose addresses come from somewhere else and can change
/// without the setting itself changing.
fn follows_other_proxy(mode: ProxyMode) -> bool {
    matches!(mode, ProxyMode::System | ProxyMode::SameAsHost)
}

/// Writes the container proxy into the Docker CLI's `config.json`
/// (`proxies.default`), so `docker run` / `docker build` pass it on.
/// Containers created from Rusker get the variables directly.
fn sync_container_proxy(settings: &AppSettings) -> Result<(), String> {
    let path = docker_client_config_path()
        .ok_or("无法确定 Docker 客户端配置目录，容器代理仅对 Rusker 创建的容器生效")?;
    apply_client_proxy(&path, &settings.container_proxy_resolved())
        .map(|_| ())
        .map_err(|error| format!("写入容器代理到 {} 失败：{error}", path.display()))
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
