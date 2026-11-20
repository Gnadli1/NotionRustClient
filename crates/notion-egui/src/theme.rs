//! Notion's light and dark palettes (layout colours, not its branding),
//! ported from notion-gui's `view/theme.rs`. eframe follows the system
//! light/dark setting; `install` gives each theme its own `Visuals`.

use eframe::egui::{self, Color32, Stroke, Theme, Visuals};

/// The colours `Visuals` has no slot for, plus the ones it has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub bg: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub sidebar: Color32,
    /// A translucent tint for hovered rows (current rows use twice it).
    pub hover: Color32,
    pub border: Color32,
    pub accent: Color32,
    pub error: Color32,
}

pub const LIGHT: Colors = Colors {
    bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    text: Color32::from_rgb(0x37, 0x35, 0x2F),
    secondary: Color32::from_rgb(0x78, 0x77, 0x74),
    sidebar: Color32::from_rgb(0xF7, 0xF7, 0xF5),
    hover: Color32::from_rgba_premultiplied(3, 3, 3, 15),
    border: Color32::from_rgb(0xE9, 0xE9, 0xE7),
    accent: Color32::from_rgb(0x23, 0x83, 0xE2),
    error: Color32::from_rgb(0xE0, 0x3E, 0x3E),
};

pub const DARK: Colors = Colors {
    bg: Color32::from_rgb(0x19, 0x19, 0x19),
    text: Color32::from_rgb(0xD4, 0xD4, 0xD4),
    secondary: Color32::from_rgb(0x9B, 0x9B, 0x9B),
    sidebar: Color32::from_rgb(0x20, 0x20, 0x20),
    hover: Color32::from_rgba_premultiplied(14, 14, 14, 14),
    border: Color32::from_rgb(0x2F, 0x2F, 0x2F),
    accent: Color32::from_rgb(0x23, 0x83, 0xE2),
    error: Color32::from_rgb(0xE6, 0x6A, 0x6A),
};

/// The palette of the theme `ui` is drawn in.
pub fn colors(ui: &egui::Ui) -> &'static Colors {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// `c` as egui visuals. The text colour goes into the widget strokes, not
/// `override_text_color`, so spans with their own colour keep it.
pub fn visuals(c: &Colors, dark: bool) -> Visuals {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = c.bg;
    v.window_fill = c.bg;
    v.extreme_bg_color = c.bg;
    v.faint_bg_color = c.sidebar;
    v.window_stroke = Stroke::new(1.0, c.border);
    v.hyperlink_color = c.accent;
    v.error_fg_color = c.error;
    v.weak_text_color = Some(c.secondary);
    v.selection.bg_fill = c.accent.gamma_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0, c.accent);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, c.border);
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
    ] {
        s.fg_stroke.color = c.text;
    }
    // buttons and rows: no fill until hovered, then the hover tint
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.hovered.weak_bg_fill = c.hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.weak_bg_fill = c.hover.gamma_multiply(2.0);
    v
}

/// Registers both palettes; eframe picks one from the system theme.
pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visuals_follow_the_palette() {
        let (l, d) = (visuals(&LIGHT, false), visuals(&DARK, true));
        assert!(!l.dark_mode && d.dark_mode);
        assert_eq!(l.panel_fill, LIGHT.bg);
        assert_eq!(d.panel_fill, DARK.bg);
        assert_eq!(d.widgets.noninteractive.fg_stroke.color, DARK.text);
        assert_eq!(l.override_text_color, None);
    }

    #[test]
    fn installed_themes_differ() {
        let ctx = egui::Context::default();
        install(&ctx);
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        assert_eq!(ctx.global_style().visuals.panel_fill, DARK.bg);
        ctx.set_theme(egui::ThemePreference::Light);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        assert_eq!(ctx.global_style().visuals.panel_fill, LIGHT.bg);
    }
}
