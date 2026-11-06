// GUI application: use the Windows GUI subsystem so launching the exe does not
// open a console window. All child processes (wsl.exe, curl.exe) are spawned
// with CREATE_NO_WINDOW as well.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod ui;

use std::borrow::Cow;

use app::{apply_theme, RuskerApp};
use gpui_kit::{
    assets, component::TitleBar, open_window, px, size, AppContext as _, AssetSource, Bounds,
    Point, SharedString, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions,
};
use rusker_desktop::settings::{AppSettings, SettingsStore};

// The default `Assets` bundle only carries gpui-component's own icons; embed
// the extra Lucide icons the app uses on top of it.
gpui_kit::assets::icon_assets!(ExtraIcons, [Container, Layers2]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = ExtraIcons.load(path)? {
            return Ok(Some(data));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut names = assets::Assets.list(path)?;
        names.extend(ExtraIcons.list(path)?);
        Ok(names)
    }
}

fn main() {
    let (settings_store, settings, settings_warning) = match SettingsStore::discover() {
        Ok(store) => match store.load() {
            Ok(settings) => (Some(store), settings, None),
            Err(error) => (
                Some(store),
                AppSettings::default(),
                Some(format!("{error}；当前使用默认设置，保存后将覆盖损坏的配置")),
            ),
        },
        Err(error) => (None, AppSettings::default(), Some(error.to_string())),
    };

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(settings.theme, cx);
            // In-app title bar (gpui-component `TitleBar`): the native caption
            // is hidden, the app draws its own bar and window controls. On
            // Windows the controls map to native hit-test areas, so Snap
            // Layouts and the system menu keep working.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1280.), px(800.)),
                })),
                window_min_size: Some(size(px(1040.), px(680.))),
                titlebar: Some(TitlebarOptions {
                    // Still used for the taskbar / Alt+Tab entry.
                    title: Some("Rusker Desktop".into()),
                    ..TitleBar::title_bar_options()
                }),
                // Ask Linux compositors for client-side decorations so they
                // do not draw a second, native title bar.
                window_decorations: cfg!(target_os = "linux").then_some(WindowDecorations::Client),
                app_id: Some("dev.rusker.desktop".into()),
                ..TitleBar::window_options()
            };
            open_window(options, cx, move |window, cx| {
                cx.new(|cx| {
                    let mut app = RuskerApp::new(
                        window,
                        cx,
                        settings_store.clone(),
                        settings.clone(),
                        settings_warning.clone(),
                    );
                    app.refresh(cx);
                    app.startup_runtime_host(cx);
                    app.start_tray_polling(window, cx);
                    app
                })
            })
            .expect("无法创建 Rusker Desktop 主窗口");
        });
}
