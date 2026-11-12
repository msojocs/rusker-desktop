use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode},
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
    container::{valid_image_tag, ImageTagReference},
    docker_context::CliContext,
    engine::{EngineError, EngineService},
    format::{
        container_matches, format_bytes, format_port_bindings, format_relative, format_timestamp,
        short_id,
    },
    model::{ContainerAction, ContainerSummary, EngineSnapshot, Revision},
    runtime_host::{
        same_path, validate_daemon_config, EngineRecovery, LanForwardState, ProxyApplyOutcome,
        RuntimeHostError, RuntimeHostService, RuntimeHostSnapshot, MANAGED_DOCKER_HOST,
        WSL_DOCKER_HOST,
    },
    settings::{
        apply_client_proxy, docker_client_config_path, AppSettings, DockerProxySettings,
        LanguagePreference, NetworkingMode, PortBinding, ProxyMode, ResolvedProxy, SettingsStore,
        ThemePreference, DEFAULT_DOCKER_SUBNET, RESOURCE_SAVER_MAX_SECS, RESOURCE_SAVER_MIN_SECS,
    },
    tray::{TrayCommand, TrayMenu},
};
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::ui::theme::change_theme;
use crate::ui::{button, detail_row, empty_state, muted, section_panel, stat_card};

mod compose_config;
mod compose_detail;
mod container_actions;
mod container_detail;
mod containers;
mod desktop_import;
mod docker_context;
mod images;
mod navigation;
mod notifications;
mod run;
mod volumes;

use container_detail::{ContainerStatsState, ContainerTools};
use containers::{ContainerEmpty, ContainerTables};
use desktop_import::DesktopImportState;
use images::{ImageState, ImageTables};
use navigation::{ContainerNavigation, ContainerPage};
use run::RunForm;
use volumes::{VolumeState, VolumeTables};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Containers,
    Images,
    Volumes,
    Settings,
}

impl Page {
    /// Stable slug used for element ids (not translated).
    fn key(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Containers => "containers",
            Self::Images => "images",
            Self::Volumes => "volumes",
            Self::Settings => "settings",
        }
    }

    fn title(self) -> String {
        match self {
            Self::Overview => t!("app.page.overview").into(),
            Self::Containers => t!("app.page.containers").into(),
            Self::Images => t!("app.page.images").into(),
            Self::Volumes => t!("app.page.volumes").into(),
            Self::Settings => t!("app.page.settings").into(),
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
    DesktopImport,
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
            sections.push(Self::DesktopImport);
        }
        sections
    }

    fn title(self) -> String {
        match self {
            Self::General => t!("app.settings_section.general").into(),
            Self::VmResources => t!("app.settings_section.vm_resources").into(),
            Self::ResourceSaver => t!("app.settings_section.resource_saver").into(),
            Self::Proxies => t!("app.settings_section.proxies").into(),
            Self::Network => t!("app.settings_section.network").into(),
            Self::WslIntegration => t!("app.settings_section.wsl_integration").into(),
            Self::DockerEngine => "Docker Engine".into(),
            Self::DesktopImport => t!("app.settings_section.desktop_import").into(),
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
    fn label(self) -> String {
        match self {
            Self::Install => t!("app.runtime_op.install").into(),
            Self::Start => t!("app.runtime_op.start").into(),
            Self::Stop => t!("app.runtime_op.stop").into(),
        }
    }

    fn busy_label(self) -> String {
        t!("app.operation_progress", operation = self.label()).into()
    }

    fn done_label(self) -> String {
        t!("app.operation_done", operation = self.label()).into()
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

    fn title(self) -> String {
        match self {
            Self::Starting => t!("app.transition.starting").into(),
            Self::Connecting => t!("app.transition.connecting").into(),
            Self::Stopping => t!("app.transition.stopping").into(),
            Self::Recovering => t!("app.transition.recovering").into(),
        }
    }

    fn description(self) -> String {
        match self {
            Self::Starting | Self::Connecting => t!("app.transition.description.starting").into(),
            Self::Stopping => t!("app.transition.description.stopping").into(),
            Self::Recovering => t!("app.transition.description.recovering").into(),
        }
    }
}

#[derive(Default)]
struct ContainerRemoval {
    pending_ids: Vec<String>,
    compose_project: Option<(String, Vec<ContainerSummary>)>,
    submitting: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ContainerReplacement {
    pending: Option<ContainerSummary>,
    submitting: bool,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ContainerListItem {
    Container(Box<ContainerSummary>),
    ComposeProject {
        project: String,
        containers: Vec<ContainerSummary>,
    },
}

/// Keep the Engine's order, placing all services of a Compose project at the
/// project's first occurrence while leaving standalone containers independent.
fn group_container_list(containers: Vec<ContainerSummary>) -> Vec<ContainerListItem> {
    let mut items = Vec::new();
    let mut project_indices: HashMap<String, usize> = HashMap::new();
    for container in containers {
        let Some(project) = container.compose_project.clone() else {
            items.push(ContainerListItem::Container(Box::new(container)));
            continue;
        };
        if let Some(index) = project_indices.get(&project).copied() {
            let Some(ContainerListItem::ComposeProject { containers, .. }) = items.get_mut(index)
            else {
                unreachable!("Compose project index must reference a project row");
            };
            containers.push(container);
        } else {
            project_indices.insert(project.clone(), items.len());
            items.push(ContainerListItem::ComposeProject {
                project,
                containers: vec![container],
            });
        }
    }
    items
}

/// Remove the first `${project}-` occurrence from nested Compose container names.
fn container_list_name(container: &ContainerSummary, nested: bool) -> String {
    if nested {
        if let Some(project) = container.compose_project.as_deref() {
            return container.name.replacen(&format!("{project}-"), "", 1);
        }
    }
    container.name.clone()
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
    container_actions: container_actions::ContainerActions,
    container_navigation: ContainerNavigation,
    compose_config_view: Option<Entity<compose_config::ComposeConfigView>>,
    /// Container list shows only running containers.
    containers_running_only: bool,
    /// IDs ticked for batch removal in the container list.
    containers_checked: HashSet<String>,
    /// Compose projects collapsed in the container list. Projects default to expanded.
    compose_projects_collapsed: HashSet<String>,
    container_tables: ContainerTables,
    images_table: ImageTables,
    volumes_table: VolumeTables,
    container_removal: ContainerRemoval,
    container_replacement: ContainerReplacement,
    /// Container detail tab: logs, details, mounts, exec, files, stats.
    container_tab: usize,
    container_tools: ContainerTools,
    container_stats: ContainerStatsState,
    container_terminal: Option<Entity<crate::ui::terminal::TerminalView>>,
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
    replace_image_input: Entity<InputState>,
    replace_tag_input: Entity<InputState>,
    create_error: Option<String>,
    images: ImageState,
    run: RunForm,
    volumes: VolumeState,
    /// Session-only source selection, progress and per-item import results.
    desktop_import: DesktopImportState,
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
    /// Docker CLI's current context, as last read (None: not read yet).
    docker_context: Option<Result<CliContext, String>>,
    /// The switch-context prompt was already considered this session.
    docker_context_prompted: bool,
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
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let pull_input = cx.new(|cx| InputState::new(window, cx));
        let create_image_input = cx.new(|cx| InputState::new(window, cx));
        let create_name_input = cx.new(|cx| InputState::new(window, cx));
        let create_command_input = cx.new(|cx| InputState::new(window, cx));
        let create_ports_input = cx.new(|cx| InputState::new(window, cx));
        let replace_image_input = cx.new(|cx| InputState::new(window, cx));
        let replace_tag_input = cx.new(|cx| InputState::new(window, cx));
        let volume_name_input = cx.new(|cx| InputState::new(window, cx));
        let http_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.docker_proxy.http_proxy.clone())
        });
        let https_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.docker_proxy.https_proxy.clone())
        });
        let no_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.docker_proxy.no_proxy.clone())
        });
        let container_http_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.container_proxy.http_proxy.clone())
        });
        let container_https_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.container_proxy.https_proxy.clone())
        });
        let container_no_proxy_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.container_proxy.no_proxy.clone())
        });
        let network_subnet_input =
            cx.new(|cx| InputState::new(window, cx).default_value(settings.network.subnet.clone()));
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

        let app_weak = cx.weak_entity();
        let container_tables = ContainerTables::new(app_weak.clone(), window, cx);
        let images_table = ImageTables::new(app_weak.clone(), window, cx);
        let volumes_table = VolumeTables::new(app_weak, window, cx);

        let mut this = Self {
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
            container_actions: container_actions::ContainerActions::default(),
            container_navigation: ContainerNavigation::default(),
            compose_config_view: None,
            containers_running_only: false,
            containers_checked: HashSet::new(),
            compose_projects_collapsed: HashSet::new(),
            container_tables,
            images_table,
            volumes_table,
            container_removal: ContainerRemoval::default(),
            container_replacement: ContainerReplacement::default(),
            container_tab: 0,
            container_tools: ContainerTools::default(),
            container_stats: ContainerStatsState::default(),
            container_terminal: None,
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
            replace_image_input,
            replace_tag_input,
            create_error: None,
            images: ImageState::default(),
            run: RunForm::default(),
            volumes: VolumeState::default(),
            desktop_import: DesktopImportState::default(),
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
            docker_context: None,
            docker_context_prompted: false,
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
        };
        this.apply_placeholders(window, cx);
        this
    }

    /// Switches the UI language (preview or applied setting). Rendered text
    /// follows on the next frame; text held outside of render is reset here.
    fn apply_language(
        &mut self,
        preference: LanguagePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if rusker_desktop::i18n::resolve(preference) == &*rust_i18n::locale() {
            return;
        }
        rusker_desktop::i18n::apply(preference);
        if let Some(tray) = &self.tray {
            tray.retranslate();
        }
        self.apply_placeholders(window, cx);
        cx.refresh_windows();
    }

    /// Input placeholders in the current language.
    fn apply_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let subnet = t!("app.placeholder.subnet", subnet = DEFAULT_DOCKER_SUBNET);
        for (input, placeholder) in [
            (&self.search_input, t!("app.placeholder.search")),
            (&self.pull_input, t!("app.placeholder.pull_image")),
            (&self.create_image_input, t!("app.placeholder.create_image")),
            (&self.create_name_input, t!("app.placeholder.create_name")),
            (
                &self.create_command_input,
                t!("app.placeholder.create_command"),
            ),
            (&self.create_ports_input, t!("app.placeholder.create_ports")),
            (&self.volume_name_input, t!("app.placeholder.volume_name")),
            (&self.http_proxy_input, t!("app.placeholder.http_proxy")),
            (&self.https_proxy_input, t!("app.placeholder.https_proxy")),
            (&self.no_proxy_input, "localhost,127.0.0.1,::1".into()),
            (
                &self.container_http_proxy_input,
                t!("app.placeholder.container_http_proxy"),
            ),
            (
                &self.container_https_proxy_input,
                t!("app.placeholder.https_proxy"),
            ),
            (
                &self.container_no_proxy_input,
                "localhost,127.0.0.1,::1".into(),
            ),
            (&self.network_subnet_input, subnet),
        ] {
            let placeholder = SharedString::from(placeholder.into_owned());
            input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        self.apply_run_placeholders(window, cx);
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
                    let starting = matches!(
                        this.engine_phase(),
                        Some(
                            EngineTransition::Starting
                                | EngineTransition::Connecting
                                | EngineTransition::Recovering
                        )
                    );
                    if let Some(tray) = this.tray.as_mut() {
                        tray.update_animation(starting);
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
        if self.desktop_import_running() {
            self.notify_error(t!("app.error.quit_importing"), cx);
            cx.notify();
            return;
        }
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
        if page != Page::Containers {
            self.reset_container_tools();
            self.container_navigation.reset();
            self.compose_config_view = None;
        }
        self.page = page;
        if entering_containers {
            self.reset_container_tools();
            // The sidebar entry always opens the container list, not the
            // detail page of the last selected container.
            self.container_navigation.reset();
            self.logs_revision.advance();
            self.logs_loading = false;
            self.logs.clear();
            self.logs_error = None;
        }
        if page == Page::Settings {
            self.refresh_runtime_host(cx);
            if self.manages_engine() {
                self.load_docker_context();
            }
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
        if !self.manages_engine() || self.daemon_loading || self.desktop_import_running() {
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

    /// Launch-time hook: installs a missing runtime and starts the managed
    /// engine before connecting. External engines can connect immediately.
    pub fn startup_runtime_host(&mut self, cx: &mut Context<Self>) {
        if !self.manages_engine() {
            self.refresh(cx);
            self.refresh_runtime_host(cx);
            return;
        }
        if self.runtime_busy.is_some() || self.desktop_import_running() {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.runtime_busy = Some(t!("app.busy.preparing_wsl").into());
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
        if !cfg!(windows) || self.runtime_busy.is_some() || self.desktop_import_running() {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        self.runtime_busy = Some(t!("app.busy.reading_wsl").into());
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
        if self.runtime_busy.is_some() || self.desktop_import_running() {
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            self.runtime_error = Some(t!("app.error.runtime_unavailable").into());
            cx.notify();
            return;
        };
        self.runtime_busy = Some(operation.busy_label());
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
                        this.runtime_notice = Some(operation.done_label());
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
            self.notify_error(t!("app.error.backend_unavailable"), cx);
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
                        this.apply_snapshot(snapshot, cx);
                        this.after_volume_snapshot(cx);
                        this.sync_container_tools(cx);
                        this.maybe_prompt_docker_context(cx);
                    }
                    Err(error) => {
                        this.connected = false;
                        this.stop_container_live_tools();
                        this.notify_error(error.to_string(), cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply_snapshot(&mut self, snapshot: EngineSnapshot, cx: &mut Context<Self>) {
        self.reconcile_container_stats(&snapshot.containers);
        let page_changed = self.container_navigation.reconcile(&snapshot.containers);
        self.snapshot = snapshot;
        if page_changed {
            self.restore_container_page(cx);
        }
        if self
            .container_navigation
            .container()
            .is_some_and(|selected| {
                self.snapshot
                    .containers
                    .iter()
                    .any(|container| &container.id == selected && !container.is_running())
            })
        {
            self.stop_container_live_tools();
        }
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
        if self.container_navigation.container() != Some(&id) {
            self.reset_container_tools();
            self.container_tab = 0;
        }
        self.container_navigation
            .push(ContainerPage::Container(id.clone()));
        self.compose_config_view = None;
        self.load_logs(id, cx);
    }

    /// Returns to the page from which the container detail was opened.
    fn close_container_detail(&mut self, cx: &mut Context<Self>) {
        self.container_navigation.back();
        self.restore_container_page(cx);
    }

    fn show_container_list(&mut self, cx: &mut Context<Self>) {
        self.container_navigation.reset();
        self.restore_container_page(cx);
    }

    fn restore_container_page(&mut self, cx: &mut Context<Self>) {
        self.reset_container_tools();
        self.compose_config_view = None;
        self.logs_revision.advance();
        self._logs_task = None;
        self.logs_loading = false;
        self.logs.clear();
        self.logs_error = None;
        match self.container_navigation.current().clone() {
            ContainerPage::Container(id) => {
                self.container_tab = 0;
                self.load_logs(id, cx);
            }
            ContainerPage::ComposeProject(project) => self.load_compose_logs(&project, cx),
            ContainerPage::ComposeConfig(project) => self.open_compose_config(&project, cx),
            ContainerPage::List => {}
        }
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
        if self.container_action_busy(&id) {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.notify_error(t!("app.error.engine_unavailable"), cx);
            cx.notify();
            return;
        };
        let handle = engine.container_actions(vec![id.clone()], action);
        self.run_container_action(
            container_actions::ActionTarget::Container(id.clone()),
            HashSet::from([id]),
            action,
            handle,
            cx,
        );
    }

    fn remove_image(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.notify_error(t!("app.error.engine_unavailable"), cx);
            cx.notify();
            return;
        };
        let handle = engine.remove_image(id);
        self.run_snapshot_operation(t!("app.op.remove_image").into(), handle, cx);
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
            .map_err(|_| t!("app.error.threshold_integer").to_string())?;
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
            .unwrap_or_else(|| t!("app.not_available").into())
    }

    fn browse_disk_location(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("app.select").into()),
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
        self.apply_language(saved.language, window, cx);
        self.settings_error = None;
        self.settings_notice = None;
        cx.notify();
    }

    /// "Apply": validates the draft, moves the virtual disk first when its
    /// location changed (settings are only saved if that succeeds), then
    /// saves and applies everything else.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        if self.desktop_import_running() {
            self.settings_error = Some(t!("app.error.settings_importing").into());
            cx.notify();
            return;
        }
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
            self.settings_error = Some(t!("app.error.config_dir_settings").into());
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
            self.settings_error = Some(t!("app.error.runtime_unavailable_settings").into());
            cx.notify();
            return;
        };
        if self.runtime_busy.is_some() {
            self.settings_error = Some(t!("app.error.runtime_busy").into());
            cx.notify();
            return;
        }
        self.runtime_busy = Some(t!("app.busy.applying_wsl").into());
        self.runtime_error = None;
        self.settings_error = None;
        self.settings_notice = Some(t!("app.busy.applying_wsl").into());
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
                        let warning = (!failures.is_empty()).then(|| {
                            t!(
                                "app.warning.wsl_partial",
                                errors = failures.join(t!("app.list_separator").as_ref())
                            )
                            .into()
                        });
                        this.apply_validated_settings(candidate, warning, cx);
                    }
                    Err(error) => {
                        this.settings_notice = None;
                        this.settings_error =
                            Some(t!("app.error.apply_wsl_failed", error = error).into());
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
            self.settings_error = Some(t!("app.error.runtime_busy").into());
            cx.notify();
            return;
        }
        self.runtime_busy = Some(t!("app.busy.moving_disk").into());
        self.runtime_error = None;
        self.settings_error = None;
        self.settings_notice = Some(t!("app.notice.moving_disk").into());
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
                            Some(t!("app.error.move_disk_failed", error = error).into());
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
                self.settings_notice = Some(t!("app.notice.settings_saved").into());
                if container_proxy_changed {
                    if let Err(error) = sync_container_proxy(&self.settings) {
                        self.settings_error = Some(t!("app.error.saved_but", error = error).into());
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
            self.settings_notice = Some(t!("app.notice.settings_saved_external").into());
            return;
        }
        let Some(host) = self.runtime_host.clone() else {
            return;
        };
        let daemon_config = self.daemon_pending(cx);
        let with_daemon = daemon_config.is_some();
        self.runtime_busy = Some(t!("app.busy.applying_engine").into());
        self.runtime_error = None;
        self.settings_notice = Some(t!("app.notice.settings_saved_applying").into());
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
                                ProxyApplyOutcome::Restarted => {
                                    t!("app.notice.applied_restarted")
                                }
                                ProxyApplyOutcome::Unchanged => t!("app.notice.applied_unchanged"),
                                ProxyApplyOutcome::Deferred => t!("app.notice.applied_deferred"),
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
                            Some(t!("app.error.apply_engine_failed", error = error).into());
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
            self.settings_error = Some(t!("app.error.config_dir_proxy").into());
            cx.notify();
            return;
        };

        match store.export_proxy(&proxy) {
            Ok(path) => {
                self.settings_error = None;
                self.settings_notice = Some(
                    t!(
                        "app.notice.proxy_exported",
                        path = path.display().to_string()
                    )
                    .into(),
                );
            }
            Err(error) => {
                self.settings_error = Some(error.to_string());
                self.settings_notice = None;
            }
        }
        cx.notify();
    }

    fn language_button(&self, preference: LanguagePreference, cx: &mut Context<Self>) -> Button {
        let selected = self.settings_draft.language == preference;
        let button = button(SharedString::from(format!("language-{preference:?}")))
            .label(preference.label())
            .pointer(true)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.settings_draft.language = preference;
                this.settings_notice = None;
                this.apply_language(preference, window, cx);
                cx.notify();
            }));
        if selected {
            button.primary()
        } else {
            button.outline()
        }
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
        self.busy = Some(t!("app.operation_progress", operation = label).into());
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
                        this.apply_snapshot(snapshot, cx);
                        this.notify_success(t!("app.operation_done", operation = label), cx);
                        this.after_volume_snapshot(cx);
                        this.sync_container_tools(cx);
                    }
                    Err(error) => this.notify_error(error.to_string(), cx),
                }
                cx.notify();
            });
        }));
    }

    fn confirm_replace_container(
        &mut self,
        container: &ContainerSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if container.is_compose() || self.busy.is_some() {
            return;
        }
        let image = ImageTagReference::parse(&container.image);
        self.replace_image_input.update(cx, |input, cx| {
            input.set_value(image.repository.clone(), window, cx)
        });
        self.replace_tag_input.update(cx, |input, cx| {
            input.set_value(image.default_tag().unwrap_or(""), window, cx)
        });
        self.container_replacement = ContainerReplacement {
            pending: Some(container.clone()),
            ..ContainerReplacement::default()
        };
        let title = t!("app.dialog.replace_container.title", name = container.name).into_owned();
        let description = t!("app.dialog.replace_container.description").into_owned();
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let Some(this) = weak.upgrade() else {
                return dialog;
            };
            let replacement = &this.read(cx).container_replacement;
            let submitting = replacement.submitting;
            let error = replacement.error.clone();
            let image_input = this.read(cx).replace_image_input.clone();
            let tag_input = this.read(cx).replace_tag_input.clone();
            let submit = weak.clone();
            let confirm = weak.clone();
            let cancel = weak.clone();
            dialog
                .title(title.clone())
                .description(description.clone())
                .child(form_field(
                    t!("app.image"),
                    &image_input,
                    t!("app.replace_image_hint"),
                    cx,
                ))
                .child(form_field(
                    t!("app.image_tag"),
                    &tag_input,
                    t!("app.replace_tag_hint"),
                    cx,
                ))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(t!("app.replace_image_warning")),
                )
                .keyboard(!submitting)
                .on_ok(move |_, _, cx| {
                    _ = confirm.update(cx, |this, cx| this.submit_replace_container(cx));
                    false
                })
                .on_cancel(move |_, _, cx| {
                    cancel
                        .upgrade()
                        .is_none_or(|this| !this.read(cx).container_replacement.submitting)
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
                            button("cancel-replace-container")
                                .outline()
                                .label(t!("app.cancel"))
                                .disabled(submitting)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            button("submit-replace-container")
                                .danger()
                                .label(if submitting {
                                    t!("app.replacing")
                                } else {
                                    t!("app.replace")
                                })
                                .loading(submitting)
                                .disabled(submitting)
                                .on_click(move |_, _, cx| {
                                    _ = submit.update(cx, |this, cx| {
                                        this.submit_replace_container(cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn submit_replace_container(&mut self, cx: &mut Context<Self>) {
        if self.container_replacement.submitting || self.busy.is_some() {
            return;
        }
        let Some(container) = self.container_replacement.pending.clone() else {
            return;
        };
        if container.is_compose() {
            return;
        }
        let repository = self.replace_image_input.read(cx).value().trim().to_string();
        if repository.is_empty()
            || repository.chars().any(char::is_whitespace)
            || repository.contains('@')
            || ImageTagReference::parse(&repository).has_tag()
        {
            self.container_replacement.error =
                Some(t!("app.error.invalid_image_name", image = repository).into_owned());
            cx.notify();
            return;
        }
        let tag = self.replace_tag_input.read(cx).value().trim().to_string();
        if !tag.is_empty() && !valid_image_tag(&tag) {
            self.container_replacement.error =
                Some(t!("app.error.invalid_image_tag", tag = tag).into_owned());
            cx.notify();
            return;
        }
        let original = ImageTagReference::parse(&container.image);
        let image = if tag.is_empty() && repository == original.repository {
            None
        } else if tag.is_empty() {
            Some(repository)
        } else {
            Some(format!("{repository}:{tag}"))
        };
        let Some(engine) = self.engine.clone() else {
            self.container_replacement.error =
                Some(t!("app.error.engine_unavailable").into_owned());
            cx.notify();
            return;
        };

        let handle = engine.replace_container(container.id.clone(), image);
        let revision = self.refresh_revision.advance();
        self.container_replacement.submitting = true;
        self.container_replacement.error = None;
        self.busy = Some(t!("app.replacing").into_owned());
        self.loading = false;
        cx.notify();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = await_engine(handle).await;
            _ = this.update(cx, |this, cx| {
                if !this.refresh_revision.is_current(revision) {
                    return;
                }
                this.busy = None;
                this.container_replacement.submitting = false;
                match result {
                    Ok((_new_id, snapshot)) => {
                        this.connected = true;
                        this.apply_snapshot(snapshot, cx);
                        this.after_volume_snapshot(cx);
                        this.sync_container_tools(cx);
                        let window = this.window_handle;
                        _ = cx.update_window(window, |_, window, cx| window.close_dialog(cx));
                        this.notify_success(
                            t!("app.notice.container_replaced", name = container.name),
                            cx,
                        );
                    }
                    Err(error) => {
                        this.container_replacement.error = Some(error.to_string());
                        // A failure after the new container started can leave a
                        // partially completed replacement; show its actual state.
                        this.refresh(cx);
                    }
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
            t!("app.dialog.remove_container.title", name = container.name).into(),
            t!("app.dialog.remove_container.description").into(),
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
            .map(|container| format!("{} ({})", container.name, short_id(&container.id)))
            .collect();
        if containers.len() > listed.len() {
            listed.push("…".into());
        }
        let title = t!(
            "app.dialog.remove_containers.title",
            count = containers.len()
        )
        .into();
        let description = t!(
            "app.dialog.remove_containers.description",
            containers = listed.join("\n")
        )
        .into();
        let ids = containers
            .into_iter()
            .map(|container| container.id)
            .collect();
        self.open_remove_containers_dialog(ids, title, description, window, cx);
    }

    fn confirm_remove_compose_project(
        &mut self,
        project: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let containers: Vec<_> = self
            .snapshot
            .containers
            .iter()
            .filter(|container| container.compose_project.as_deref() == Some(project.as_str()))
            .cloned()
            .collect();
        if containers.is_empty() {
            return;
        }
        let ids = containers
            .iter()
            .map(|container| container.id.clone())
            .collect();
        self.open_remove_containers_dialog(
            ids,
            t!("app.dialog.remove_compose.title", project = project).into(),
            t!("app.dialog.remove_compose.description").into(),
            window,
            cx,
        );
        self.container_removal.compose_project = Some((project, containers));
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
                                .label(t!("app.cancel"))
                                .disabled(submitting)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            button("submit-remove-containers")
                                .danger()
                                .label(if submitting {
                                    t!("app.deleting")
                                } else {
                                    t!("app.delete")
                                })
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
            || (self.container_removal.pending_ids.is_empty()
                && self.container_removal.compose_project.is_none())
            || self.busy.is_some()
        {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.container_removal.error = Some(t!("app.error.engine_unavailable").into());
            cx.notify();
            return;
        };
        let ids = self.container_removal.pending_ids.clone();
        let total = ids.len();
        let compose_project = self.container_removal.compose_project.clone();
        let handle = if let Some((project, containers)) = &compose_project {
            engine.compose_down(project.clone(), containers.clone())
        } else {
            engine.remove_containers(ids)
        };
        let revision = self.refresh_revision.advance();
        self.container_removal.submitting = true;
        self.container_removal.error = None;
        self.busy = Some(t!("app.deleting").into());
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
                        this.apply_snapshot(snapshot, cx);
                        this.after_volume_snapshot(cx);
                        this.sync_container_tools(cx);
                        if failures.is_empty() {
                            let window = this.window_handle;
                            _ = cx.update_window(window, |_, window, cx| window.close_dialog(cx));
                            this.notify_success(
                                if let Some((project, _)) = &compose_project {
                                    t!("app.notice.compose_removed", project = project)
                                } else {
                                    t!("app.notice.containers_removed", count = total)
                                },
                                cx,
                            );
                        } else if compose_project.is_some() {
                            this.container_removal.error = Some(failures.join("\n"));
                        } else {
                            this.container_removal.error = Some(
                                t!(
                                    "app.error.containers_partial",
                                    removed = total - failures.len(),
                                    failed = failures.len(),
                                    errors = failures.join("\n")
                                )
                                .into(),
                            );
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
            (
                IconName::PanelLeftOpen,
                t!("app.sidebar.expand").into_owned(),
            )
        } else {
            (
                IconName::PanelLeftClose,
                t!("app.sidebar.collapse").into_owned(),
            )
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
            .when(!collapsed, |this| this.child(label.clone()))
            .when(collapsed, |this| {
                this.tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
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
        self.engine_phase().is_some()
            || self.runtime_busy.is_some()
            || self.run.submitting
            || self.desktop_import_running()
            || self.desktop_import_scanning()
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
                            t!("app.transition.quitting")
                        } else {
                            phase.title().into()
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
            .id(SharedString::from(format!("nav-{}", page.key())))
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
                        .label(if self.loading {
                            t!("app.refreshing")
                        } else {
                            t!("app.refresh")
                        })
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
                        .child(
                            div()
                                .text_2xl()
                                .font_semibold()
                                .child(t!("app.overview.heading")),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .gap_4()
                                .child(stat_card(
                                    t!("app.overview.all_containers"),
                                    self.snapshot.containers.len().to_string(),
                                    t!("app.overview.all_containers_detail"),
                                    cx,
                                ))
                                .child(stat_card(
                                    t!("app.overview.running"),
                                    self.snapshot.running_containers().to_string(),
                                    t!(
                                        "app.overview.not_running",
                                        count = self.snapshot.stopped_containers()
                                    ),
                                    cx,
                                ))
                                .child(stat_card(
                                    t!("app.overview.images"),
                                    self.snapshot.images.len().to_string(),
                                    t!("app.overview.images_detail"),
                                    cx,
                                ))
                                .child(stat_card(
                                    t!("app.overview.volumes"),
                                    self.snapshot.volumes.len().to_string(),
                                    t!(
                                        "app.overview.volumes_in_use",
                                        count = self
                                            .snapshot
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
                                .child(
                                    div()
                                        .text_lg()
                                        .font_semibold()
                                        .child(t!("app.overview.engine_info")),
                                )
                                .child(detail_row(t!("app.name"), engine.name.clone(), cx))
                                .child(detail_row("Engine", engine.version.clone(), cx))
                                .child(detail_row("API", engine.api_version.clone(), cx))
                                .child(detail_row(
                                    t!("app.os"),
                                    engine.operating_system.clone(),
                                    cx,
                                ))
                                .child(detail_row(t!("app.arch"), engine.architecture.clone(), cx)),
                        ),
                ),
        )
    }

    fn render_containers(&self, cx: &mut Context<Self>) -> Div {
        if let Some(project) = self.container_navigation.compose_project() {
            let containers: Vec<ContainerSummary> = self
                .snapshot
                .containers
                .iter()
                .filter(|container| container.compose_project.as_deref() == Some(project.as_str()))
                .cloned()
                .collect();
            return self.render_compose_project_detail(project, &containers, cx);
        }
        let selected = self
            .container_navigation
            .container()
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
        let mut checked: Vec<String> = self.containers_checked.iter().cloned().collect();
        checked.sort();
        let selected_containers: Vec<ContainerSummary> = all_containers
            .iter()
            .filter(|container| self.containers_checked.contains(&container.id))
            .cloned()
            .collect();

        let latest_stats: Vec<_> = all_containers
            .iter()
            .filter(|container| container.is_running())
            .filter_map(|container| self.container_stats.latest(&container.id))
            .collect();
        let cpu_total = latest_stats.iter().try_fold(0.0, |total, stats| {
            stats.cpu_percent.map(|value| total + value)
        });
        let cpu_usage = if running == 0 {
            format_cpu_usage(0.0, self.snapshot.engine.cpus)
        } else {
            cpu_total.map_or_else(
                || t!("app.not_available").into_owned(),
                |total| format_cpu_usage(total, self.snapshot.engine.cpus),
            )
        };
        let memory_usage = if running > 0 && latest_stats.is_empty() {
            t!("app.not_available").into_owned()
        } else {
            let used = latest_stats.iter().fold(0u64, |total, stats| {
                total.saturating_add(stats.memory_bytes)
            });
            format_memory_usage(used, self.snapshot.engine.memory_bytes)
        };

        let title = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_2xl()
                    .font_semibold()
                    .child(t!("app.page.containers")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!(
                        "app.containers.summary",
                        running = running,
                        total = all_containers.len()
                    )),
            )
            .child(div().flex_1())
            .child(
                list_action_button("refresh-containers", cx.theme().primary, cx)
                    .small()
                    .icon(IconName::RotateCw)
                    .tooltip(t!("app.refresh"))
                    .disabled(self.loading || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                button("open-create-container")
                    .pointer(true)
                    .primary()
                    .icon(IconName::Plus)
                    .label(t!("app.create"))
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
                "cpu",
                t!("app.containers.cpu_usage"),
                t!("app.containers.cpu_usage_tooltip"),
                &cpu_usage,
                cx,
            ))
            .child(container_usage(
                "memory",
                t!("app.containers.memory_usage"),
                t!("app.containers.memory_usage_tooltip"),
                &memory_usage,
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
                    .label(t!("app.containers.running_only").into_owned())
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
                        .label(t!("app.clear_selection"))
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
                        .label(t!("app.remove_selected", count = count))
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_remove_containers(selected.clone(), window, cx);
                        })),
                )
            });

        let empty = if all_containers.is_empty() {
            ContainerEmpty::All
        } else if self.containers_running_only && running == 0 {
            ContainerEmpty::Running
        } else {
            ContainerEmpty::Filtered
        };
        let footer = div()
            .flex()
            .justify_end()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(t!("app.containers.shown", count = shown));

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
            .child(self.container_tables.render(
                group_container_list(containers),
                empty,
                &self.compose_projects_collapsed,
                cx,
            ))
            .child(footer)
    }

    /// Container detail page: breadcrumb, identity header with the Docker
    /// Desktop action cluster and logs / details / mounts / exec / files / stats tabs.
    fn render_container_detail(&self, container: &ContainerSummary, cx: &mut Context<Self>) -> Div {
        let busy = self.busy.is_some();
        let action_busy = self.container_action_busy(&container.id);
        let running = container.is_running();
        let start_action = if container.is_paused() {
            ContainerAction::Unpause
        } else {
            ContainerAction::Start
        };
        let status = if container.status.trim().is_empty() {
            container.state.clone()
        } else {
            container.status.clone()
        };
        let tab = self.container_tab.min(5);
        let weak = cx.weak_entity();
        let back_weak = weak.clone();
        let parent_project = self.container_navigation.parent_project().cloned();
        let back_tooltip = if parent_project.is_some() {
            t!("app.compose.back_to_project")
        } else {
            t!("app.back_to_containers")
        };
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
                    .tooltip(t!("app.stop"))
                    .disabled(action_busy || !running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_container_action(stop_id.clone(), ContainerAction::Stop, cx);
                    })),
            )
            .child(
                button("start-container")
                    .primary()
                    .icon(IconName::Play)
                    .tooltip(start_action.label())
                    .disabled(action_busy || running)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_container_action(start_id.clone(), start_action, cx);
                    })),
            )
            .child(
                button("restart-container")
                    .outline()
                    .icon(IconName::RotateCw)
                    .tooltip(t!("app.restart"))
                    .disabled(action_busy)
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
                    .tooltip(t!("app.delete"))
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
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.logs")))
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.inspect")))
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.mounts")))
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.terminal")))
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.files")))
                    .child(Tab::new().cursor_pointer().label(t!("app.tab.stats")))
                    .on_click({
                        let weak = weak.clone();
                        move |index, window, cx| {
                            _ = weak.update(cx, |this, cx| {
                                this.select_container_tab(*index, cx);
                                if *index == 3 {
                                    if let Some(terminal) = &this.container_terminal {
                                        terminal
                                            .update(cx, |terminal, cx| terminal.focus(window, cx));
                                    }
                                }
                            });
                        }
                    }),
            )
            .when(tab == 1, |tabs| {
                tabs.child(
                    Switch::new("container-inspect-json")
                        .checked(!self.container_tools.inspect_summary)
                        .label("Raw JSON")
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.container_tools.inspect_summary = !*checked;
                            cx.notify();
                        })),
                )
                .child(
                    button("refresh-container-inspect")
                        .ghost()
                        .small()
                        .icon(IconName::RotateCw)
                        .tooltip(t!("app.refresh_inspect"))
                        .on_click(cx.listener(|this, _, _, cx| this.load_container_inspect(cx))),
                )
            })
            .when(tab == 0, |tabs| {
                tabs.child(
                    button("copy-container-logs")
                        .ghost()
                        .small()
                        .icon(IconName::Copy)
                        .tooltip(t!("app.copy_logs"))
                        .disabled(self.logs.is_empty())
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(logs_for_copy.clone()));
                        }),
                )
            })
            .when(tab == 0, |tabs| {
                tabs.child(
                    button("refresh-container-logs")
                        .ghost()
                        .small()
                        .icon(IconName::RotateCw)
                        .tooltip(t!("app.refresh_logs"))
                        .disabled(self.logs_loading)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load_logs(logs_id.clone(), cx);
                        })),
                )
            });

        let content = match tab {
            1 if !self.container_tools.inspect_summary => {
                self.render_container_inspect_tab(cx).into_any_element()
            }
            1 => self.render_container_details_tab(cx).into_any_element(),
            2 => self
                .render_container_mounts_tab(container, cx)
                .into_any_element(),
            3 => self
                .render_container_exec_tab(container, cx)
                .into_any_element(),
            4 => self
                .render_container_files_tab(container, cx)
                .into_any_element(),
            5 => self
                .render_container_stats_tab(container, cx)
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
                            BreadcrumbItem::new(t!("app.page.containers").into_owned())
                                .text_color(cx.theme().primary)
                                .on_click(move |_, _, cx| {
                                    _ = back_weak
                                        .update(cx, |this, cx| this.show_container_list(cx));
                                }),
                        )
                        .when_some(parent_project, |breadcrumb, project| {
                            breadcrumb.child(
                                BreadcrumbItem::new(project)
                                    .text_color(cx.theme().primary)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.close_container_detail(cx);
                                    })),
                            )
                        })
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
                            .tooltip(back_tooltip)
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
                                        t!("app.copy_container_id"),
                                        cx.theme().foreground,
                                        cx.theme().muted_foreground,
                                    ))
                                    .child(copyable_value(
                                        "copy-detail-image",
                                        AssetIcon::Link,
                                        container.image.clone(),
                                        t!("app.copy_image_ref"),
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
                                    .child(div().text_xs().font_semibold().child(t!("app.status")))
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
            t!("app.logs.loading").to_string()
        } else if let Some(error) = &self.logs_error {
            error.clone()
        } else if self.logs.trim().is_empty() {
            t!("app.logs.empty").to_string()
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
                            .tooltip(t!("app.scroll_to_bottom"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.container_logs_scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                )
            })
    }

    fn render_container_details_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("container-details-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .child(self.render_container_inspect_fields(cx))
    }

    fn render_container_mounts_tab(
        &self,
        container: &ContainerSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = if container.volumes.is_empty() {
            empty_state(
                t!("app.empty.no_mounts"),
                t!("app.empty.no_mounts_hint"),
                cx,
            )
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
            t!("app.runtime.not_installed")
        } else if snapshot.engine_ready {
            t!("app.status.engine_running")
        } else if self.resource_saver_active() {
            t!("app.runtime.saver_active_status")
        } else if snapshot.running {
            t!("app.runtime.distro_running")
        } else {
            t!("app.runtime.stopped")
        };
        let busy = self.runtime_busy.is_some() || self.desktop_import_running();
        let gpu_detail = if snapshot.gpu.detail.is_empty() {
            t!("app.runtime.detecting_gpu")
        } else {
            snapshot.gpu.detail.as_str().into()
        };
        let warning = snapshot.warnings.join("\n");

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(settings_label(t!("app.runtime.wsl_environment"), cx))
            .child(detail_row(t!("app.runtime.distro"), "rusker (WSL2)", cx))
            .child(detail_row(t!("app.runtime.state"), status, cx))
            .child(detail_row(
                t!("app.runtime.windows_endpoint"),
                MANAGED_DOCKER_HOST,
                cx,
            ))
            .child(detail_row(
                t!("app.runtime.wsl_endpoint"),
                WSL_DOCKER_HOST,
                cx,
            ))
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
                            .label(if snapshot.installed {
                                t!("app.runtime.repair")
                            } else {
                                t!("app.runtime.install")
                            })
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
                            .label(t!("app.start"))
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
                            .label(t!("app.stop"))
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
                            .label(t!("app.refresh_status"))
                            .disabled(busy)
                            .pointer(!busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_runtime_host(cx);
                            })),
                    ),
            )
            .when(!warning.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(warning),
                )
            })
            .when_some(self.runtime_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .when_some(self.runtime_notice.clone(), |this, notice| {
                this.child(div().text_sm().text_color(cx.theme().success).child(notice))
            })
            .child(muted(t!("app.runtime.engine_hint"), cx))
    }

    /// Docker Desktop's "Docker engine" page: edit `daemon.json` directly.
    fn render_docker_engine_settings(&self, cx: &mut Context<Self>) -> Div {
        let managed = self.manages_engine();
        let loaded = self.daemon_loaded.is_some();
        let busy = self.runtime_busy.is_some() || self.desktop_import_running();
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
                    .child(t!("app.daemon.intro_prefix"))
                    .child(
                        div()
                            .id("open-daemon-docs")
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(cx.theme().link)
                            .underline()
                            .cursor_pointer()
                            .child(t!("app.daemon.config_file_link"))
                            .child(Icon::new(AssetIcon::ExternalLink).xsmall())
                            .on_click(|_, _, cx| cx.open_url(DAEMON_CONFIG_DOCS_URL)),
                    )
                    .child(t!("app.daemon.intro_suffix")),
            )
            .child(callout(
                CalloutKind::Warning,
                t!("app.daemon.advanced_title"),
                muted(t!("app.daemon.advanced_body"), cx),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(settings_label(t!("app.daemon.file_label"), cx)),
                    )
                    .child(
                        button("reload-daemon-config")
                            .ghost()
                            .small()
                            .icon(IconName::Redo)
                            .label(if self.daemon_loading {
                                t!("app.daemon.reading")
                            } else {
                                t!("app.daemon.reload")
                            })
                            .disabled(!managed || self.daemon_loading || busy)
                            .pointer(!(!managed || self.daemon_loading || busy))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_daemon_config(cx);
                                cx.notify();
                            })),
                    ),
            )
            .when(!managed, |this| {
                this.child(muted(t!("app.daemon.unmanaged_hint"), cx))
            })
            .when_some(self.daemon_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .when(managed, |this| {
                this.child(
                    Editor::new(&self.daemon_editor)
                        .h(px(320.))
                        .disabled(!loaded || busy),
                )
                .child(div().my_2().h(px(1.)).bg(cx.theme().border))
                .child(self.render_docker_context_setting(cx))
            })
    }

    fn render_wsl_integration(&self, cx: &mut Context<Self>) -> Div {
        let snapshot = &self.runtime_snapshot;
        let busy = self.runtime_busy.is_some() || self.desktop_import_running();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(muted(t!("app.wsl.integration_hint"), cx))
            .when(snapshot.distributions.is_empty(), |this| {
                this.child(muted(t!("app.wsl.no_distros"), cx))
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
                    t!("app.wsl.default_distro", name = distribution.name).to_string()
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
                        .on_change(cx.listener(
                            move |this, enabled, _, cx| {
                                this.set_runtime_integration(name.clone(), *enabled, cx);
                            },
                        )),
                    )
            }))
            .child(muted(t!("app.wsl.apply_hint"), cx))
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
                    SettingsSection::DesktopImport => self.render_desktop_import(cx),
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
                    t!("app.settings_warning"),
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
                    .child(t!("app.applying_settings")),
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
                    .unwrap_or_else(|| t!("app.applying_settings_progress").into()),
                cx.theme().muted_foreground,
            )
        } else if dirty {
            (t!("app.unsaved_changes").to_string(), cx.theme().warning)
        } else if let Some(notice) = self.settings_notice.clone() {
            (notice, cx.theme().success)
        } else {
            (
                t!("app.no_unsaved_changes").to_string(),
                cx.theme().muted_foreground,
            )
        };
        let manages_engine = self.manages_engine();
        let apply_blocked = self.settings_store.is_none()
            || self.desktop_import_running()
            || (manages_engine && self.runtime_busy.is_some());
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
                    .label(t!("app.discard_changes"))
                    .disabled(!dirty || applying)
                    .pointer(dirty && !applying)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.discard_settings(window, cx);
                    })),
            )
            .child(
                button("apply-settings")
                    .primary()
                    .label(t!("app.apply"))
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
            .unwrap_or_else(|| t!("app.not_available").into());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(settings_label(t!("app.general.appearance"), cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.theme_button(ThemePreference::System, cx))
                    .child(self.theme_button(ThemePreference::Light, cx))
                    .child(self.theme_button(ThemePreference::Dark, cx)),
            )
            .child(settings_label(t!("app.settings.language"), cx))
            .child(div().flex().gap_2().children(
                LanguagePreference::ALL.map(|preference| self.language_button(preference, cx)),
            ))
            .child(muted(t!("app.general.theme_hint"), cx))
            .child(muted(t!("app.general.config_path", path = config_path), cx))
    }

    fn render_vm_settings(&self, cx: &mut Context<Self>) -> Div {
        if !self.manages_engine() {
            return div().child(callout(
                CalloutKind::Info,
                t!("app.vm.external_title"),
                muted(t!("app.vm.external_body"), cx),
                cx,
            ));
        }
        let location = self.disk_location_display();
        let move_pending = self.pending_disk_move(&self.settings_draft).is_some();
        let busy = self.runtime_busy.is_some() || self.desktop_import_running();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(callout(
                CalloutKind::Info,
                t!("app.vm.wsl_backend_title"),
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(t!("app.vm.wsl_backend_body"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .child(t!("app.vm.wslconfig_prefix"))
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
                                    .child(t!("app.vm.wslconfig_file"))
                                    .child(Icon::new(AssetIcon::ExternalLink).xsmall())
                                    .on_click(|_, _, cx| cx.open_url(WSLCONFIG_DOCS_URL)),
                            )
                            .child(t!("app.vm.wslconfig_suffix")),
                    ),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_label(t!("app.vm.disk_location"), cx))
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
                                    .label(t!("app.browse"))
                                    .disabled(busy)
                                    .pointer(!busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.browse_disk_location(cx);
                                    })),
                            ),
                    )
                    .child(muted(
                        if self.runtime_snapshot.installed {
                            t!("app.vm.disk_hint_installed")
                        } else {
                            t!("app.vm.disk_hint_new")
                        },
                        cx,
                    ))
                    .when(move_pending, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child(t!("app.vm.disk_move_warning")),
                        )
                    }),
            )
            .child(self.render_engine_runtime(cx))
    }

    fn render_resource_saver_settings(&self, cx: &mut Context<Self>) -> Div {
        let enabled = self.settings_draft.resource_saver.enabled;
        let managed = self.manages_engine();
        let threshold_labels = [
            t!("app.saver.threshold_seconds"),
            t!("app.saver.threshold_minutes", minutes = 10),
            t!("app.saver.threshold_minutes", minutes = 20),
            t!("app.saver.threshold_minutes", minutes = 30),
            t!("app.saver.threshold_minutes", minutes = 40),
            t!("app.saver.threshold_minutes", minutes = 50),
            t!("app.saver.threshold_minutes", minutes = 60),
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
                    .child(
                        div()
                            .flex_1()
                            .child(settings_label(t!("app.saver.enable"), cx)),
                    )
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
                    t!("app.saver.hint_managed")
                } else {
                    t!("app.saver.hint_unmanaged")
                },
                cx,
            ))
            .when(self.resource_saver_active(), |this| {
                this.child(callout(
                    CalloutKind::Info,
                    t!("app.saver.active_title"),
                    muted(t!("app.saver.active_body"), cx),
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
                    .child(settings_label(t!("app.saver.threshold"), cx))
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
                                    .child(muted(t!("app.saver.seconds_unit"), cx)),
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
                                            .children(
                                                threshold_labels.map(|label| div().child(label)),
                                            ),
                                    ),
                            ),
                    )
                    .child(muted(t!("app.saver.threshold_hint"), cx)),
            )
    }

    fn render_proxy_settings(&self, cx: &mut Context<Self>) -> Div {
        let manages_engine = self.manages_engine();
        let export_path = self
            .settings_store
            .as_ref()
            .map(|store| store.proxy_export_path().display().to_string())
            .unwrap_or_else(|| t!("app.not_available").into());
        let mode = self.settings_draft.docker_proxy.mode;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(proxy_heading(
                t!("app.proxy.engine_title"),
                self.proxy_mode_select(
                    "engine-proxy-mode",
                    mode,
                    &ProxyMode::ENGINE,
                    |settings| &mut settings.docker_proxy.mode,
                    cx,
                ),
                cx,
            ))
            .child(muted(t!("app.proxy.engine_hint"), cx))
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
                this.child(proxy_summary(
                    &self.settings_draft.docker_proxy.resolve(None),
                    cx,
                ))
            })
            .when(
                mode != ProxyMode::None || self.settings.docker_proxy.mode != ProxyMode::None,
                |this| {
                    this.child(callout(
                        if manages_engine {
                            CalloutKind::Info
                        } else {
                            CalloutKind::Warning
                        },
                        if manages_engine {
                            t!("app.proxy.no_restart")
                        } else {
                            t!("app.proxy.manual_apply")
                        },
                        muted(
                            if manages_engine {
                                t!("app.proxy.managed_hint")
                            } else {
                                t!("app.proxy.unmanaged_hint")
                            },
                            cx,
                        ),
                        cx,
                    ))
                },
            )
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
                                .label(t!("app.proxy.export"))
                                .disabled(self.settings_store.is_none())
                                .pointer(self.settings_store.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_proxy_settings(cx);
                                })),
                        )
                        .child(muted(t!("app.proxy.export_path", path = export_path), cx)),
                )
            })
            .child(div().my_2().h(px(1.)).bg(cx.theme().border))
            .child(self.render_container_proxy_settings(cx))
    }

    /// Docker Desktop's "Containers proxy": outbound traffic from containers.
    fn render_container_proxy_settings(&self, cx: &mut Context<Self>) -> Div {
        let config_path = docker_client_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| t!("app.not_available").into());
        let mode = self.settings_draft.container_proxy.mode;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(proxy_heading(
                t!("app.proxy.container_title"),
                self.proxy_mode_select(
                    "container-proxy-mode",
                    mode,
                    &ProxyMode::CONTAINER,
                    |settings| &mut settings.container_proxy.mode,
                    cx,
                ),
                cx,
            ))
            .child(muted(t!("app.proxy.container_hint"), cx))
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
                t!("app.proxy.new_containers_only"),
                muted(t!("app.proxy.new_containers_body", path = config_path), cx),
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
        label: fn(T) -> String,
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
            "mirrored" => t!("app.lan.mirrored").to_string(),
            "nat" | "unknown"
                if self.settings.network.port_binding == PortBinding::LocalhostOnly =>
            {
                t!("app.lan.nat_localhost").to_string()
            }
            "nat" | "unknown" => {
                let ports = if state.forwarded.is_empty() {
                    t!("app.lan.no_ports").to_string()
                } else {
                    let mut list = state
                        .forwarded
                        .iter()
                        .take(12)
                        .map(|(protocol, port)| format!("{} {port}", protocol.label()))
                        .collect::<Vec<_>>()
                        .join(t!("app.list_separator").as_ref());
                    if state.forwarded.len() > 12 {
                        list.push_str(
                            t!("app.lan.more_ports", count = state.forwarded.len()).as_ref(),
                        );
                    }
                    t!("app.lan.forwarded", ports = list).to_string()
                };
                t!("app.lan.nat", ports = ports).to_string()
            }
            other => t!("app.lan.other", mode = other).to_string(),
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
            .child(muted(t!("app.network.hint"), cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_label(t!("app.network.subnet"), cx))
                    .child(Input::new(&self.network_subnet_input).disabled(!managed))
                    .child(muted(t!("app.network.subnet_hint"), cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .child(settings_label(t!("app.network.host_networking"), cx)),
                    )
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
            .child(muted(t!("app.network.host_networking_hint"), cx))
            .child(proxy_heading(
                t!("app.network.port_binding"),
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
            .child(muted(t!("app.network.port_binding_hint"), cx))
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
                t!("app.network.default_mode"),
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
            .child(muted(t!("app.network.default_mode_hint"), cx))
            .when(network.mode == NetworkingMode::DualStack, |this| {
                this.child(indented(cx).child(muted(t!("app.network.ipv6_hint"), cx)))
            })
            .child(callout(
                CalloutKind::Info,
                if managed {
                    t!("app.network.restarts_engine")
                } else {
                    t!("app.proxy.manual_apply")
                },
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_color(cx.theme().muted_foreground)
                    .child(if managed {
                        t!("app.network.managed_hint")
                    } else {
                        t!("app.daemon.unmanaged_hint")
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
                            .child(t!("app.learn_more"))
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

/// Formats CPU as used / available, where 100% is one logical CPU.
fn format_cpu_usage(used: f64, available_cpus: u32) -> String {
    format!(
        "{} / {}",
        container_detail::format_cpu(Some(used)),
        container_detail::format_cpu(Some(f64::from(available_cpus) * 100.0))
    )
}

/// Formats memory as used / available.
fn format_memory_usage(used: u64, available: u64) -> String {
    format!("{} / {}", format_bytes(used), format_bytes(available))
}

/// Usage summary block shown above the container table.
fn container_usage(
    key: &'static str,
    title: impl Into<SharedString>,
    tooltip: impl Into<SharedString>,
    value: &str,
    cx: &App,
) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    let title = title.into();
    let tooltip = tooltip.into();
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
                        .id(SharedString::from(format!("container-usage-{key}")))
                        .child(Icon::new(IconName::Info).xsmall().text_color(muted))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
                ),
        )
        .child(div().text_sm().text_color(muted).child(value.to_string()))
}

/// A link-coloured value that copies `text` when clicked.
fn copyable_value(
    id: impl Into<ElementId>,
    icon: impl Into<AssetIcon>,
    text: impl Into<SharedString>,
    tooltip: impl Into<SharedString>,
    text_color: Hsla,
    icon_color: Hsla,
) -> impl IntoElement {
    let text: SharedString = text.into();
    let copy_text = text.clone();
    let tooltip = tooltip.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .min_w_0()
        .cursor_pointer()
        .hover(|style| style.underline())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
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
fn container_menu_item(label: impl Into<SharedString>, icon: impl Into<Icon>) -> PopupMenuItem {
    container_menu_item_enabled(label, icon, true)
}

fn container_menu_item_enabled(
    label: impl Into<SharedString>,
    icon: impl Into<Icon>,
    enabled: bool,
) -> PopupMenuItem {
    let label = label.into();
    let icon: Icon = icon.into();
    PopupMenuItem::element(move |_, cx| {
        div()
            .flex_1()
            .min_h(px(26.))
            .mx_neg_2()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .when(enabled, |this| this.cursor_pointer())
            .when(!enabled, |this| {
                this.cursor_default()
                    .text_color(cx.theme().muted_foreground)
            })
            .child(icon.clone().xsmall())
            .child(label.clone())
    })
    .disabled(!enabled)
}

fn container_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    container: &ContainerSummary,
    weak: gpui_kit::WeakEntity<RuskerApp>,
) -> gpui_kit::component::menu::PopupMenu {
    let detail_container = container.clone();
    let detail_weak = weak.clone();
    let restart_id = container.id.clone();
    let restart_weak = weak.clone();
    let replace_container = container.clone();
    let replace_weak = weak;
    let id = container.id.clone();
    let image = container.image.clone();
    let mut menu = menu
        .item(
            container_menu_item(t!("app.view_details"), AssetIcon::Container).on_click(
                move |_, _, cx| {
                    _ = detail_weak.update(cx, |this, cx| {
                        this.select_container(detail_container.id.clone(), cx);
                    });
                },
            ),
        )
        .item(
            container_menu_item(t!("app.restart"), IconName::RotateCw).on_click(move |_, _, cx| {
                _ = restart_weak.update(cx, |this, cx| {
                    this.start_container_action(restart_id.clone(), ContainerAction::Restart, cx);
                });
            }),
        );
    if !container.is_compose() {
        menu = menu.item(
            container_menu_item(t!("app.replace"), IconName::RefreshCw).on_click(
                move |_, window, cx| {
                    _ = replace_weak.update(cx, |this, cx| {
                        this.confirm_replace_container(&replace_container, window, cx);
                    });
                },
            ),
        );
    }
    menu.separator()
        .item(
            container_menu_item(t!("app.copy_container_id"), IconName::Copy).on_click(
                move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(id.clone()));
                },
            ),
        )
        .item(
            container_menu_item(t!("app.copy_image_ref"), IconName::Copy).on_click(
                move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(image.clone()));
                },
            ),
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
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    // Focusable controls prevent the default mouse-down behavior.
                    // Any other press is on passive UI and should clear focus.
                    if !window.default_prevented() {
                        window.blur(cx);
                    }
                })
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
const APP_BAR_HEIGHT: f32 = 58.;
const WINDOW_BUTTON_WIDTH: f32 = 56.;
/// Shared widths for the separate selection and status columns.
const TABLE_CHECK_COLUMN: f32 = 34.;
const TABLE_STATUS_COLUMN: f32 = 51.;
/// Container table column defaults; DataTable owns subsequent width changes.
const CONTAINER_EXPAND_COLUMN: f32 = 25.;
const CONTAINER_NAME_COLUMN: f32 = 160.;
const CONTAINER_ID_COLUMN: f32 = 108.;
const CONTAINER_IMAGE_COLUMN: f32 = 160.;
const CONTAINER_PORTS_COLUMN: f32 = 88.;
const CONTAINER_CPU_COLUMN: f32 = 152.;
const CONTAINER_MEMORY_COLUMN: f32 = 152.;
const CONTAINER_CREATED_COLUMN: f32 = 88.;
const CONTAINER_ACTIONS_COLUMN: f32 = 108.;

fn table_control_cell() -> Div {
    // Definite height keeps controls centered even on the first layout pass.
    div()
        .w_full()
        .h(Size::Large.table_row_height())
        .flex()
        .items_center()
        .justify_center()
}

fn table_control_header_cell(width: Pixels, cx: &App) -> Div {
    // DataTable reserves space for sorting even on non-sortable columns.
    // Keep narrow headers at their full width and separators out of layout.
    table_control_cell()
        .w(width)
        .flex_shrink_0()
        .relative()
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(1.))
                .bg(cx.theme().border),
        )
}

fn table_horizontal_scrollbar<H: ScrollbarHandle + Clone>(
    handle: &H,
    content_width: Option<Pixels>,
) -> Div {
    let mut scrollbar = Scrollbar::horizontal(handle)
        .mode(ScrollbarMode::Always)
        .viewport_from_layout();
    if let Some(width) = content_width {
        scrollbar = scrollbar.scroll_size(gpui_kit::size(width, px(0.)));
    }
    div()
        .relative()
        .w_full()
        .h(Scrollbar::width())
        .flex_shrink_0()
        .overflow_hidden()
        .child(scrollbar)
}

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
                    .gap_3()
                    .child(
                        gpui_kit::img(crate::APP_LOGO_PATH)
                            .size(px(36.))
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(div().text_xl().font_bold().child("rusker"))
                            .child(div().text_xl().child("desktop")),
                    ),
            )
            .child(drag_region(div().id("app-bar-drag")).flex_1().h_full())
            .child(
                div()
                    .id("app-bar-settings")
                    .size(px(40.))
                    .mr_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(gpui_kit::white().opacity(0.16)))
                    .child(Icon::new(IconName::Settings).size(px(20.)))
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
            .child(Icon::new(icon).size(px(20.)))
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
                facts.push(t!("app.statusbar.cpus", count = engine.cpus).to_string());
            }
            if engine.memory_bytes > 0 {
                facts.push(
                    t!(
                        "app.statusbar.memory",
                        size = format_bytes(engine.memory_bytes)
                    )
                    .to_string(),
                );
            }
            facts.push(
                t!(
                    "app.statusbar.containers",
                    running = self.snapshot.running_containers(),
                    total = self.snapshot.containers.len()
                )
                .to_string(),
            );
            facts.push(t!("app.statusbar.images", count = self.snapshot.images.len()).to_string());
            facts
                .push(t!("app.statusbar.volumes", count = self.snapshot.volumes.len()).to_string());
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
            return (t!("app.status.engine_stopping").into(), cx.theme().warning);
        }
        match self
            .engine_transition
            .or(self.recovering().then_some(EngineTransition::Recovering))
        {
            Some(EngineTransition::Stopping) => {
                return (t!("app.status.engine_stopping").into(), cx.theme().warning)
            }
            Some(EngineTransition::Starting) => {
                return (t!("app.status.engine_starting").into(), cx.theme().warning)
            }
            Some(EngineTransition::Recovering) => {
                return (
                    t!("app.status.engine_restarting").into(),
                    cx.theme().warning,
                )
            }
            _ => {}
        }
        if self.connected {
            return (t!("app.status.engine_running").into(), cx.theme().success);
        }
        if self.resource_saver_active() {
            return (t!("app.status.saver").into(), cx.theme().info);
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
            return (label, cx.theme().warning);
        }
        let label = if cfg!(windows) && !self.runtime_snapshot.installed && self.manages_engine() {
            t!("app.status.not_installed")
        } else {
            t!("app.status.stopped")
        };
        (label.into_owned(), cx.theme().danger)
    }
}

/// A live managed session must not be labelled stopped merely because the
/// action/snapshot that establishes the UI connection has not completed yet.
fn engine_reconnect_label(
    session_running: bool,
    connected: bool,
    busy: bool,
    loading: bool,
) -> Option<String> {
    if connected {
        None
    } else if session_running && busy {
        Some(t!("app.status.recovering").into())
    } else if session_running || loading {
        Some(t!("app.status.connecting").into())
    } else {
        None
    }
}

#[cfg(test)]
mod container_list_tests {
    use super::{container_list_name, group_container_list, ContainerListItem};
    use rusker_desktop::model::ContainerSummary;

    fn container(id: &str, compose_project: Option<&str>) -> ContainerSummary {
        ContainerSummary {
            id: id.into(),
            name: id.into(),
            compose_project: compose_project.map(str::to_owned),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn nested_names_remove_the_first_project_name_occurrence() {
        for (name, expected) in [
            ("shop-web-1", "web-1"),
            ("shop-shop-web-1", "shop-web-1"),
            ("custom-shop-web-1", "custom-web-1"),
            ("shop_database_1", "shop_database_1"),
            ("other-web-1", "other-web-1"),
        ] {
            let mut member = container(name, Some("shop"));
            member.compose_replace = Some("frontend".into());
            assert_eq!(container_list_name(&member, true), expected);
            assert_eq!(container_list_name(&member, false), name);
        }
        assert_eq!(
            container_list_name(&container("shop-web-1", None), true),
            "shop-web-1"
        );
    }

    #[test]
    fn compose_services_group_at_the_projects_first_occurrence() {
        let items = group_container_list(vec![
            container("web", Some("shop")),
            container("standalone", None),
            container("database", Some("shop")),
            container("proxy", Some("edge")),
        ]);

        assert_eq!(items.len(), 3);
        match &items[0] {
            ContainerListItem::ComposeProject {
                project,
                containers,
            } => {
                assert_eq!(project, "shop");
                assert_eq!(
                    containers
                        .iter()
                        .map(|container| container.id.as_str())
                        .collect::<Vec<_>>(),
                    ["web", "database"]
                );
            }
            other => panic!("expected shop project, got {other:?}"),
        }
        assert!(matches!(
            &items[1],
            ContainerListItem::Container(container) if container.id == "standalone"
        ));
        assert!(matches!(
            &items[2],
            ContainerListItem::ComposeProject { project, .. } if project == "edge"
        ));
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
            engine_reconnect_label(true, false, true, false).as_deref(),
            Some("Engine recovering…")
        );
        // Also covers an external CLI wake-up before the next UI poll.
        assert_eq!(
            engine_reconnect_label(true, false, false, false).as_deref(),
            Some("Connecting to Engine…")
        );
    }

    #[test]
    fn actual_stop_and_success_do_not_get_a_reconnect_label() {
        assert_eq!(engine_reconnect_label(false, false, false, false), None);
        assert_eq!(engine_reconnect_label(false, false, true, false), None);
        assert_eq!(engine_reconnect_label(true, true, true, false), None);
        assert_eq!(
            engine_reconnect_label(false, false, false, true).as_deref(),
            Some("Connecting to Engine…")
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
fn proxy_heading(title: impl Into<SharedString>, select: impl IntoElement, cx: &App) -> Div {
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
            t!("app.proxy.http_label"),
            http,
            t!("app.proxy.http_hint"),
            cx,
        ))
        .child(form_field(
            t!("app.proxy.https_label"),
            https,
            t!("app.proxy.https_hint"),
            cx,
        ))
        .child(form_field(
            t!("app.proxy.no_proxy_label"),
            no_proxy,
            t!("app.proxy.no_proxy_hint"),
            cx,
        ))
}

/// The addresses a non-manual mode currently resolves to.
fn proxy_summary(proxy: &ResolvedProxy, cx: &App) -> Div {
    let text = if proxy.is_empty() {
        t!("app.proxy.no_proxy_detected").to_string()
    } else {
        let or_none = |value: &str| {
            if value.is_empty() {
                t!("app.proxy.none").to_string()
            } else {
                value.to_string()
            }
        };
        t!(
            "app.proxy.current",
            http = or_none(&proxy.http_proxy),
            https = or_none(&proxy.https_proxy),
            no_proxy = or_none(&proxy.no_proxy)
        )
        .to_string()
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
    let path = docker_client_config_path().ok_or_else(|| t!("app.error.client_config_dir"))?;
    apply_client_proxy(&path, &settings.container_proxy_resolved())
        .map(|_| ())
        .map_err(|error| {
            t!(
                "app.error.write_client_proxy",
                path = path.display().to_string(),
                error = error
            )
            .to_string()
        })
}

fn settings_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_base()
        .font_semibold()
        .text_color(cx.theme().foreground)
        .child(text.into())
}

#[derive(Clone, Copy)]
enum CalloutKind {
    Info,
    Warning,
}

/// Tinted notice box with an icon and a title, like Docker Desktop's
/// "WSL2 backend" panel.
fn callout(
    kind: CalloutKind,
    title: impl Into<SharedString>,
    body: impl IntoElement,
    cx: &App,
) -> Div {
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
                .child(div().text_base().font_semibold().child(title.into()))
                .child(body),
        )
}

fn form_field(
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
    hint: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_sm().font_medium().child(label.into()))
        .child(Input::new(input))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint.into()),
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
