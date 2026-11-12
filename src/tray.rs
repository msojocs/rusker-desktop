use std::time::Instant;

use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/rusker-64.png");
const ICON_SVG: &str = include_str!("../assets/icons/rusker.svg");
// The cog has twelve identical teeth: one tooth pitch is a seamless loop.
const ANIMATION_FRAMES: usize = 8;
const ANIMATION_FRAME_MILLIS: u128 = 100;
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
    icon: TrayIcon,
    static_icon: Icon,
    animation_frames: Vec<Icon>,
    animation: CogAnimation,
    current_frame: Option<usize>,
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
        let animation_frames = (0..ANIMATION_FRAMES)
            .map(|frame| {
                Icon::from_rgba(render_animation_frame(frame)?, ICON_SIZE, ICON_SIZE)
                    .map_err(|error| t!("tray.error.icon", error = error).into_owned())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(t!("tray.tooltip"))
            .with_icon(icon.clone())
            .build()
            .map_err(|error| t!("tray.error.init", error = error).into_owned())?;

        Ok(Self {
            icon: tray,
            static_icon: icon,
            animation_frames,
            animation: CogAnimation::default(),
            current_frame: None,
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

    /// Called by the tray timer, independently of window rendering/minimization.
    pub fn update_animation(&mut self, starting: bool) {
        let frame = self.animation.frame(starting, Instant::now());
        if frame == self.current_frame {
            return;
        }
        let icon = match frame {
            Some(index) => &self.animation_frames[index],
            None => &self.static_icon,
        };
        // Only remember successful updates, so the next tick can retry failures.
        if self.icon.set_icon(Some(icon.clone())).is_ok() {
            self.current_frame = frame;
        }
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

/// Pure animation clock: no native tray is needed to test lifecycle changes.
#[derive(Default)]
struct CogAnimation {
    started_at: Option<Instant>,
}

impl CogAnimation {
    fn frame(&mut self, starting: bool, now: Instant) -> Option<usize> {
        if !starting {
            self.started_at = None;
            return None;
        }
        let started_at = *self.started_at.get_or_insert(now);
        Some(
            ((now.duration_since(started_at).as_millis() / ANIMATION_FRAME_MILLIS)
                % ANIMATION_FRAMES as u128) as usize,
        )
    }
}

fn render_animation_frame(frame: usize) -> Result<Vec<u8>, String> {
    // Rotate only the mask geometry, keeping the gradient, glow and container
    // stationary. Reuse the logo source instead of maintaining a second cog.
    let angle = 30.0 * frame as f32 / ANIMATION_FRAMES as f32;
    let svg = ICON_SVG.replace(
        "<g id=\"cog-shape\">",
        &format!("<g id=\"cog-shape\" transform=\"rotate({angle} 512 512)\">"),
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        .map_err(|error| t!("tray.error.decode", error = error).into_owned())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)
        .ok_or_else(|| t!("tray.error.decode", error = t!("tray.error.too_large")).into_owned())?;
    let size = tree.size();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            ICON_SIZE as f32 / size.width(),
            ICON_SIZE as f32 / size.height(),
        ),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied pixels, but tray-icon expects straight RGBA.
    Ok(pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let pixel = pixel.demultiply();
            [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
        })
        .collect())
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
    use std::time::Duration;

    #[test]
    fn animation_clock_stops_and_restarts() {
        let now = Instant::now();
        let mut animation = CogAnimation::default();
        assert_eq!(animation.frame(false, now), None);
        assert_eq!(animation.frame(true, now), Some(0));
        assert_eq!(
            animation.frame(true, now + Duration::from_millis(100)),
            Some(1)
        );
        assert_eq!(
            animation.frame(true, now + Duration::from_millis(800)),
            Some(0)
        );
        assert_eq!(animation.frame(false, now + Duration::from_secs(1)), None);
        assert_eq!(animation.frame(true, now + Duration::from_secs(2)), Some(0));
    }

    #[test]
    fn animation_frames_rotate_only_the_cog_and_loop() {
        let first = render_animation_frame(0).unwrap();
        assert_eq!(first.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        for frame in 1..ANIMATION_FRAMES {
            let pixels = render_animation_frame(frame).unwrap();
            assert_ne!(pixels, first, "cog must move between frames");
            // The central container and glow must not rotate with the cog.
            for y in 18..46 {
                for x in 18..46 {
                    let offset = ((y * ICON_SIZE + x) * 4) as usize;
                    assert_eq!(pixels[offset..offset + 4], first[offset..offset + 4]);
                }
            }
        }
        let loop_frame = render_animation_frame(ANIMATION_FRAMES).unwrap();
        // A 30-degree turn returns the twelve-tooth cog to the same geometry;
        // allow small rasterizer rounding differences at antialiased edges.
        let difference: u64 = first
            .iter()
            .zip(loop_frame.iter())
            .map(|(a, b)| a.abs_diff(*b) as u64)
            .sum();
        assert!(
            difference < first.len() as u64,
            "animation loop differs by more than one level per channel on average: {difference}"
        );
    }

    #[test]
    fn embedded_tray_icon_decodes_to_rgba() {
        let (pixels, width, height) = decode_icon(ICON_PNG).unwrap();
        assert_eq!((width, height), (64, 64));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
    }
}
