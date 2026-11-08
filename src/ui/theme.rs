//! Application-wide brand and interaction colors, shared by every page.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{rgb, App};

/// Reload the component theme, then apply the app palette for the chosen mode.
/// Keep all mode changes on this path so following the OS cannot reset colors.
pub fn change_theme(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    Theme::update(cx, apply_palette);
}

fn apply_palette(theme: &mut Theme) {
    let (primary, hover, active, foreground) = if theme.mode.is_dark() {
        (0x60A5FA, 0x93C5FD, 0x3B82F6, 0x0F172A)
    } else {
        (0x1D63ED, 0x1A56D0, 0x1749B0, 0xFFFFFF)
    };
    let primary = rgb(primary).into();
    theme.primary = primary;
    theme.primary_hover = rgb(hover).into();
    theme.primary_active = rgb(active).into();
    theme.primary_foreground = rgb(foreground).into();
    theme.button_primary = theme.primary;
    theme.button_primary_hover = theme.primary_hover;
    theme.button_primary_active = theme.primary_active;
    theme.button_primary_foreground = theme.primary_foreground;
    theme.accent = primary.opacity(0.12);
    theme.accent_foreground = primary;
    theme.button_hover = theme.accent;
    theme.button_active = primary.opacity(0.24);
    theme.ring = primary;
    theme.link = primary;
    theme.link_hover = theme.primary_hover;
    theme.link_active = theme.primary_active;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_modes_share_primary_and_interaction_colors() {
        for (mode, expected) in [(ThemeMode::Light, 0x1D63ED), (ThemeMode::Dark, 0x60A5FA)] {
            let mut theme = Theme::default();
            theme.mode = mode;
            let danger = theme.danger;
            apply_palette(&mut theme);
            assert_eq!(theme.primary, rgb(expected).into());
            assert_eq!(theme.button_primary, theme.primary);
            assert_eq!(theme.button_hover, theme.primary.opacity(0.12));
            assert_eq!(theme.button_active, theme.primary.opacity(0.24));
            assert_eq!(theme.danger, danger);
        }
    }
}
