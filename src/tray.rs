use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const ICON_SIZE: u32 = 64;

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
        let icon = Icon::from_rgba(icon_rgba(), ICON_SIZE, ICON_SIZE)
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

fn icon_rgba() -> Vec<u8> {
    let mut pixels = vec![0; (ICON_SIZE * ICON_SIZE * 4) as usize];
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let background = inside_rounded_square(x, y);
            let foreground = (16..30).contains(&x) && (19..30).contains(&y)
                || (34..48).contains(&x) && (19..30).contains(&y)
                || (16..30).contains(&x) && (34..45).contains(&y)
                || (34..48).contains(&x) && (34..45).contains(&y);
            let color = if foreground {
                [236, 253, 245, 255]
            } else if background {
                [13, 148, 136, 255]
            } else {
                [0, 0, 0, 0]
            };
            let offset = ((y * ICON_SIZE + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }
    pixels
}

fn inside_rounded_square(x: u32, y: u32) -> bool {
    const MIN: u32 = 4;
    const MAX: u32 = 59;
    const RADIUS: u32 = 12;
    if !(MIN..=MAX).contains(&x) || !(MIN..=MAX).contains(&y) {
        return false;
    }
    let nearest_x = x.clamp(MIN + RADIUS, MAX - RADIUS);
    let nearest_y = y.clamp(MIN + RADIUS, MAX - RADIUS);
    let dx = x.abs_diff(nearest_x);
    let dy = y.abs_diff(nearest_y);
    dx * dx + dy * dy <= RADIUS * RADIUS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icon_has_expected_dimensions_transparency_and_shapes() {
        let pixels = icon_rgba();
        assert_eq!(pixels.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);

        let foreground = ((22 * ICON_SIZE + 22) * 4) as usize;
        assert_eq!(&pixels[foreground..foreground + 4], &[236, 253, 245, 255]);
        let background = ((50 * ICON_SIZE + 10) * 4) as usize;
        assert_eq!(&pixels[background..background + 4], &[13, 148, 136, 255]);
    }

    #[test]
    fn rounded_icon_does_not_paint_outside_its_bounds() {
        assert!(!inside_rounded_square(0, 0));
        assert!(!inside_rounded_square(4, 4));
        assert!(inside_rounded_square(32, 32));
        assert!(inside_rounded_square(4, 32));
    }
}
