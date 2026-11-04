mod app;
mod ui;

use app::{apply_theme, RuskerApp};
use gpui_kit::{
    assets, open_window, px, size, AppContext as _, Bounds, Point, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use rusker_desktop::settings::{AppSettings, SettingsStore};

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
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(settings.theme, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1280.), px(800.)),
                })),
                window_min_size: Some(size(px(1040.), px(680.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rusker Desktop".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.rusker.desktop".into()),
                ..Default::default()
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
                    app.start_tray_polling(window, cx);
                    app
                })
            })
            .expect("无法创建 Rusker Desktop 主窗口");
        });
}
