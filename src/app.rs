use std::time::Duration;

use gpui_kit::component::{
    button::{Button, ButtonVariant, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    switch::Switch,
    window_border, ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::{
    assets::IconName as AssetIcon,
    base::{Disableable as _, StyledExt as _},
    div,
    prelude::FluentBuilder as _,
    px, rgb, App, AppContext as _, Context, Div, Entity, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, WindowAppearance,
    WindowControlArea,
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
        ProxyApplyOutcome, RuntimeHostError, RuntimeHostService, RuntimeHostSnapshot,
        MANAGED_DOCKER_HOST, WSL_DOCKER_HOST,
    },
    settings::{AppSettings, DockerProxySettings, SettingsStore, ThemePreference},
    tray::{TrayCommand, TrayMenu},
};
use tokio::task::JoinHandle;

use crate::ui::{
    detail_row, empty_state, message_banner, muted, page_header, section_panel, stat_card,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Containers,
    Images,
    Settings,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Containers => "容器",
            Self::Images => "镜像",
            Self::Settings => "设置",
        }
    }
}

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

pub struct RuskerApp {
    engine: Option<EngineService>,
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
    settings: AppSettings,
    settings_store: Option<SettingsStore>,
    settings_warning: Option<String>,
    settings_notice: Option<String>,
    settings_error: Option<String>,
    http_proxy_input: Entity<InputState>,
    https_proxy_input: Entity<InputState>,
    no_proxy_input: Entity<InputState>,
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
        let subscription = cx.subscribe_in(&search_input, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let weak = cx.weak_entity();
        let appearance_subscription = window.observe_window_appearance(move |_, app_cx| {
            let follow_system = weak
                .update(app_cx, |this, _| {
                    this.settings.theme == ThemePreference::System
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
                (Some(host), None)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        // Like Docker Desktop, quitting the app stops the engine it owns.
        // Runs synchronously: GPUI only waits ~200 ms for quit futures.
        let quit_subscription = cx.on_app_quit(|this: &mut Self, _| {
            if let Some(host) = this.runtime_host.clone() {
                host.shutdown_blocking(Duration::from_secs(50));
            }
            async {}
        });

        let (tray, tray_error) = match TrayMenu::new() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(error)),
        };
        if tray.is_some() {
            window.on_window_should_close(cx, |window, _| {
                window.minimize_window();
                false
            });
        }
        if let Some(error) = tray_error {
            let warning = match settings_warning.take() {
                Some(existing) => format!("{existing}\n{error}"),
                None => error,
            };
            settings_warning = Some(warning);
        }

        Self {
            engine,
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
            settings,
            settings_store,
            settings_warning,
            settings_notice: None,
            settings_error: None,
            http_proxy_input,
            https_proxy_input,
            no_proxy_input,
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
            _subscriptions: vec![subscription, appearance_subscription, quit_subscription],
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
            TrayCommand::Quit => cx.quit(),
        }
    }

    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Settings {
            self.refresh_runtime_host(cx);
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
        self.runtime_error = None;
        cx.notify();
        let handle = host.startup(self.settings.wsl.integrations.clone());
        self._runtime_task = Some(cx.spawn(async move |this, cx| {
            let result = await_runtime_host(handle).await;
            _ = this.update(cx, |this, cx| {
                this.runtime_busy = None;
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
                    Ok(snapshot) => this.runtime_snapshot = snapshot,
                    Err(error) => this.runtime_error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
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

    fn proxy_draft(&self, cx: &Context<Self>) -> DockerProxySettings {
        DockerProxySettings {
            enabled: self.settings.docker_proxy.enabled,
            http_proxy: self.http_proxy_input.read(cx).value().to_string(),
            https_proxy: self.https_proxy_input.read(cx).value().to_string(),
            no_proxy: self.no_proxy_input.read(cx).value().to_string(),
        }
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let candidate = AppSettings {
            theme: self.settings.theme,
            docker_proxy: self.proxy_draft(cx),
            wsl: self.settings.wsl.clone(),
        }
        .normalized();
        if let Err(error) = candidate.validate() {
            self.settings_error = Some(error);
            self.settings_notice = None;
            cx.notify();
            return;
        }
        let Some(store) = &self.settings_store else {
            self.settings_error = Some("无法确定配置目录，设置未保存".into());
            cx.notify();
            return;
        };

        match store.save(&candidate) {
            Ok(()) => {
                self.settings = candidate;
                self.settings_warning = None;
                self.settings_error = None;
                self.settings_notice = Some("设置已保存".into());
                // Idempotent: dockerd is only restarted if daemon.json changes.
                // (The enable switch edits `self.settings` before saving, so a
                // before/after comparison here would miss toggles.)
                self.apply_engine_proxy(cx);
            }
            Err(error) => {
                self.settings_error = Some(error.to_string());
                self.settings_notice = None;
            }
        }
        cx.notify();
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
        let selected = self.settings.theme == preference;
        let button = Button::new(preference.label())
            .label(preference.label())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings.theme = preference;
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
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .py_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.nav_item(Page::Overview, IconName::LayoutDashboard, cx))
            .child(self.nav_item(Page::Containers, AssetIcon::Container, cx))
            .child(self.nav_item(Page::Images, AssetIcon::Layers2, cx))
            .child(div().mx_3().my_2().h(px(1.)).bg(cx.theme().border))
            .child(self.nav_item(Page::Settings, IconName::Settings, cx))
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

    fn render_header(&self, cx: &mut Context<Self>) -> Div {
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
            .child(
                Button::new("refresh")
                    .outline()
                    .small()
                    .icon(IconName::RefreshCw)
                    .label(if self.loading { "刷新中" } else { "刷新" })
                    .disabled(self.loading || self.busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
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
                        "连接已有本地或 WSL2 引擎；发行版/虚拟机安装、Compose、Volumes 和 Networks 管理见路线图。",
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

    fn render_runtime_host(&self, cx: &mut Context<Self>) -> Div {
        let snapshot = &self.runtime_snapshot;
        let status = if !snapshot.installed {
            "未安装"
        } else if snapshot.engine_ready {
            "Engine 运行中"
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
        let runtime_busy_label = self.runtime_busy.clone();

        section_panel(cx)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_lg().font_semibold().child("WSL2 运行环境"))
                    .child(muted(
                        "使用 Rusker 创建的专用 Ubuntu 发行版，不复用现有 Docker Engine。",
                        cx,
                    )),
            )
            .child(detail_row("专用发行版", "rusker (WSL2)", cx))
            .child(detail_row("运行状态", status, cx))
            .child(detail_row("Windows 端点", MANAGED_DOCKER_HOST, cx))
            .child(detail_row("WSL2 端点", WSL_DOCKER_HOST, cx))
            .child(detail_row("NVIDIA GPU", gpu_detail, cx))
            .child(muted(
                "Engine 不监听 TCP 端口。Windows 工具可执行：docker context create rusker --docker host=npipe:////./pipe/rusker_engine。退出 Rusker 时会停止 Engine，与 Docker Desktop 一致。",
                cx,
            ))
            .child(muted(
                "Windows 宿主机必须已安装支持 WSL2 GPU-PV 的 NVIDIA 驱动。这里只安装容器工具链，不在 WSL 内安装显卡驱动。",
                cx,
            ))
            .when_some(runtime_busy_label, |this, label| {
                this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(label))
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_semibold().child("发行版集成"))
                    .child(muted(
                        "开关安装或移除 Rusker 标记的 Docker CLI 包装器及 Compose/Buildx 插件，默认连接共享 socket；它不是防止手动访问的安全边界。",
                        cx,
                    )),
            )
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
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().child(title))
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
            .child(muted(
                "安装或修复可能需要数分钟。Engine 日志位于专用发行版的 /var/log/rusker-dockerd.log。",
                cx,
            ))
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let config_path = self
            .settings_store
            .as_ref()
            .map(|store| store.config_path().display().to_string())
            .unwrap_or_else(|| "不可用".into());
        let export_path = self
            .settings_store
            .as_ref()
            .map(|store| store.proxy_export_path().display().to_string())
            .unwrap_or_else(|| "不可用".into());
        let manages_engine = self.manages_engine();

        div().flex_1().min_h_0().child(
            div()
                .id("settings-page")
                .size_full()
                .overflow_y_scroll()
                .p_6()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_5()
                        .child(page_header(
                            "设置",
                            "WSL2 运行环境、外观与 Docker Engine 拉取代理配置。",
                            cx,
                        ))
                        .when(cfg!(windows), |this| {
                            this.child(self.render_runtime_host(cx))
                        })
                        .when_some(self.settings_warning.clone(), |this, warning| {
                            this.child(
                                div()
                                    .p_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().warning)
                                    .text_sm()
                                    .text_color(cx.theme().warning)
                                    .child(warning),
                            )
                        })
                        .child(
                            section_panel(cx)
                                .p_5()
                                .flex()
                                .flex_col()
                                .gap_4()
                                .child(div().text_lg().font_semibold().child("外观"))
                                .child(muted("主题立即生效，点击“保存设置”后持久化。", cx))
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(self.theme_button(ThemePreference::System, cx))
                                        .child(self.theme_button(ThemePreference::Light, cx))
                                        .child(self.theme_button(ThemePreference::Dark, cx)),
                                ),
                        )
                        .child(
                            section_panel(cx)
                                .p_5()
                                .flex()
                                .flex_col()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_lg()
                                                .font_semibold()
                                                .child("Docker Engine 拉取代理"),
                                        )
                                        .child(muted(
                                            "Engine 拉取镜像等出站请求使用的代理。保存后直接写入受管 Engine 的 daemon.json 并生效。",
                                            cx,
                                        )),
                                )
                                .child(
                                    Switch::new("docker-proxy-enabled")
                                        .label("启用代理配置")
                                        .checked(self.settings.docker_proxy.enabled)
                                        .on_change(cx.listener(|this, enabled, _, cx| {
                                            this.settings.docker_proxy.enabled = *enabled;
                                            this.settings_notice = None;
                                            cx.notify();
                                        })),
                                )
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
                                .child(
                                    div()
                                        .p_3()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(cx.theme().warning)
                                        .text_sm()
                                        .text_color(cx.theme().warning)
                                        .child(if manages_engine {
                                            "代理配置变化时会重启 Engine，运行中的容器会被停止（设置了重启策略的容器会自动恢复）。WSL2 使用 NAT 网络时，127.0.0.1 指向 WSL 自身而非 Windows，请填写 Windows 主机地址。"
                                        } else {
                                            "当前 Engine 不由 Rusker 管理，无法直接修改其配置。请导出后手动合并 proxies 到 daemon.json 并重启 Engine；不要覆盖 daemon.json 中的其他配置。"
                                        }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .gap_3()
                                        .child(
                                            Button::new("save-settings")
                                                .primary()
                                                .icon(IconName::Check)
                                                .label(if manages_engine {
                                                    "保存并应用"
                                                } else {
                                                    "保存设置"
                                                })
                                                .disabled(
                                                    self.settings_store.is_none()
                                                        || (manages_engine
                                                            && self.runtime_busy.is_some()),
                                                )
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.save_settings(cx);
                                                })),
                                        )
                                        .when(!manages_engine, |this| {
                                            this.child(
                                                Button::new("export-proxy-settings")
                                                    .outline()
                                                    .icon(IconName::FileText)
                                                    .label("导出 Engine JSON")
                                                    .disabled(self.settings_store.is_none())
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.export_proxy_settings(cx);
                                                    })),
                                            )
                                        }),
                                )
                                .child(muted(format!("Rusker 配置：{config_path}"), cx))
                                .when(!manages_engine, |this| {
                                    this.child(muted(format!("代理导出：{export_path}"), cx))
                                }),
                        )
                        .when_some(self.settings_error.clone(), |this, error| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                        .when_some(self.settings_notice.clone(), |this, notice| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().success)
                                    .child(notice),
                            )
                        }),
                ),
        )
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
        let page = match self.page {
            Page::Overview => self.render_overview(cx),
            Page::Containers => self.render_containers(cx),
            Page::Images => self.render_images(cx),
            Page::Settings => self.render_settings(cx),
        };
        let mut content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_header(cx));
        if let Some(error) = self.error.clone() {
            content = content.child(message_banner(&error, true, cx));
        }
        if let Some(busy) = self.busy.clone() {
            content = content.child(message_banner(&busy, false, cx));
        } else if let Some(notice) = self.notice.clone() {
            content = content.child(message_banner(&notice, false, cx));
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
                    .child(div().size(px(8.)).rounded_full().bg(color))
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

    /// Engine state shown in the status bar, combining the Docker API
    /// connection with the managed runtime's lifecycle.
    fn engine_status(&self, cx: &App) -> (String, Hsla) {
        if self.connected {
            return ("Engine 运行中".into(), cx.theme().success);
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
