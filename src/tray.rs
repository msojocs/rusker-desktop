use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/rusker-64.png");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    Minimize,
    Refresh,
    Containers,
    Images,
    Settings,
    Quit,
}

pub struct TrayMenu {
    icon: TrayIcon,
    /// Items with their translation keys, relabeled on a language switch.
    items: Vec<(MenuItem, &'static str)>,
    show_id: MenuId,
    minimize_id: MenuId,
    refresh_id: MenuId,
    containers_id: MenuId,
    images_id: MenuId,
    settings_id: MenuId,
    quit_id: MenuId,
}

impl TrayMenu {
    pub fn new() -> Result<Self, String> {
        let item = |key: &'static str| MenuItem::new(t!(key), true, None);
        let show = item("tray.show");
        let minimize = item("tray.minimize");
        let refresh = item("tray.refresh");
        let containers = item("tray.containers");
        let images = item("tray.images");
        let settings = item("tray.settings");
        let quit = item("tray.quit");
        let ids = (
            show.id().clone(),
            minimize.id().clone(),
            refresh.id().clone(),
            containers.id().clone(),
            images.id().clone(),
            settings.id().clone(),
            quit.id().clone(),
        );

        let menu = Menu::new();
        menu.append_items(&[
            &show,
            &minimize,
            &PredefinedMenuItem::separator(),
            &refresh,
            &containers,
            &images,
            &settings,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|error| t!("tray.error.menu", error = error).into_owned())?;
        let (rgba, width, height) = decode_icon(ICON_PNG)?;
        let icon = Icon::from_rgba(rgba, width, height)
            .map_err(|error| t!("tray.error.icon", error = error).into_owned())?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(t!("tray.tooltip"))
            .with_icon(icon)
            .build()
            .map_err(|error| t!("tray.error.init", error = error).into_owned())?;

        Ok(Self {
            icon: tray,
            items: vec![
                (show, "tray.show"),
                (minimize, "tray.minimize"),
                (refresh, "tray.refresh"),
                (containers, "tray.containers"),
                (images, "tray.images"),
                (settings, "tray.settings"),
                (quit, "tray.quit"),
            ],
            show_id: ids.0,
            minimize_id: ids.1,
            refresh_id: ids.2,
            containers_id: ids.3,
            images_id: ids.4,
            settings_id: ids.5,
            quit_id: ids.6,
        })
    }

    /// Relabels the menu and tooltip in the current language.
    pub fn retranslate(&self) {
        for (item, key) in &self.items {
            item.set_text(t!(*key));
        }
        _ = self.icon.set_tooltip(Some(t!("tray.tooltip")));
    }

    pub fn drain_commands(&self) -> Vec<TrayCommand> {
        let mut commands = Vec::new();
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => commands.push(TrayCommand::Show),
                _ => {}
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let command = if event.id == self.show_id {
                Some(TrayCommand::Show)
            } else if event.id == self.minimize_id {
                Some(TrayCommand::Minimize)
            } else if event.id == self.refresh_id {
                Some(TrayCommand::Refresh)
            } else if event.id == self.containers_id {
                Some(TrayCommand::Containers)
            } else if event.id == self.images_id {
                Some(TrayCommand::Images)
            } else if event.id == self.settings_id {
                Some(TrayCommand::Settings)
            } else if event.id == self.quit_id {
                Some(TrayCommand::Quit)
            } else {
                None
            };
            if let Some(command) = command {
                commands.push(command);
            }
        }
        commands
    }
}

fn decode_icon(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|error| t!("tray.error.decode", error = error).into_owned())?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| t!("tray.error.decode", error = t!("tray.error.too_large")).into_owned())?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| t!("tray.error.decode", error = error).into_owned())?;
    if info.color_type != png::ColorType::Rgba {
        return Err(t!("tray.error.color", color = format!("{:?}", info.color_type)).into_owned());
    }
    buffer.truncate(info.buffer_size());
    Ok((buffer, info.width, info.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tray_icon_decodes_to_rgba() {
        let (pixels, width, height) = decode_icon(ICON_PNG).unwrap();
        assert_eq!((width, height), (64, 64));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
    }
}
