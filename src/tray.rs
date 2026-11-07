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
    _icon: TrayIcon,
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
        let show = MenuItem::new("显示主窗口", true, None);
        let minimize = MenuItem::new("最小化到托盘", true, None);
        let refresh = MenuItem::new("刷新容器状态", true, None);
        let containers = MenuItem::new("打开容器", true, None);
        let images = MenuItem::new("打开镜像", true, None);
        let settings = MenuItem::new("打开设置", true, None);
        let quit = MenuItem::new("退出 Rusker", true, None);
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
        .map_err(|error| format!("创建托盘菜单失败：{error}"))?;
        let (rgba, width, height) = decode_icon(ICON_PNG)?;
        let icon = Icon::from_rgba(rgba, width, height)
            .map_err(|error| format!("创建托盘图标失败：{error}"))?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Rusker Desktop — 容器管理")
            .with_icon(icon)
            .build()
            .map_err(|error| format!("初始化系统托盘失败：{error}"))?;

        Ok(Self {
            _icon: tray,
            show_id: ids.0,
            minimize_id: ids.1,
            refresh_id: ids.2,
            containers_id: ids.3,
            images_id: ids.4,
            settings_id: ids.5,
            quit_id: ids.6,
        })
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
        .map_err(|error| format!("解析托盘图标失败：{error}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "解析托盘图标失败：图像过大".to_string())?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("解析托盘图标失败：{error}"))?;
    if info.color_type != png::ColorType::Rgba {
        return Err(format!("托盘图标颜色格式不受支持：{:?}", info.color_type));
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
