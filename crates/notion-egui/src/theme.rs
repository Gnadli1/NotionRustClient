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

// Page layout, tuned to Notion's proportions (points).
pub const TITLE_SIZE: f32 = 40.0;
pub const TITLE_TOP: f32 = 80.0;
pub const PAGE_PAD: f32 = 48.0;
/// Where text starts inside the page column: the row gutter plus the item
/// spacing a text row gets. Icon, breadcrumb, title, blocks and tables share it.
pub const TEXT_INSET: f32 = 20.0;
pub const PAGE_MAX_WIDTH: f32 = 708.0;
/// Vertical gap between blocks.
pub const BLOCK_GAP: f32 = 5.0;
/// Gap between the columns of a `column_list` (Notion uses 46 points).
pub const COLUMN_GAP: f32 = 46.0;
pub const H_SIZE: [f32; 3] = [30.0, 24.0, 20.0];
/// Extra space above / below a heading of level 1, 2, 3.
pub const H_ABOVE: [f32; 3] = [28.0, 20.0, 14.0];
pub const H_BELOW: f32 = 2.0;
/// A table of contents line is this many times the font size tall.
pub const TOC_LINE: f32 = 1.6;
pub const TABLE_PAD_X: i8 = 9;
pub const TABLE_PAD_Y: i8 = 6;
/// Fill of the table's header row.
pub const TABLE_HEADER_TINT: f32 = 0.06;

/// The palette of the theme `ui` is drawn in.
pub fn colors(ui: &egui::Ui) -> &'static Colors {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// Notion's block colours as (text, background tint) for a colour name
/// (`red`, `blue_background`); `None` for `default` and anything unknown.
/// Hand-picked per theme, close to Notion's own.
pub fn block_color(name: &str, dark: bool) -> (Option<Color32>, Option<Color32>) {
    let (base, bg) = match name.strip_suffix("_background") {
        Some(b) => (b, true),
        None => (name, false),
    };
    // (text light, text dark, tint light, tint dark)
    let [tl, td, bl, bd]: [u32; 4] = match base {
        "gray" => [0x787774, 0x9B9B9B, 0xF1F1EF, 0x2F2F2F],
        "brown" => [0x9F6B53, 0xBA856F, 0xF4EEEE, 0x4A3228],
        "orange" => [0xD9730D, 0xC77D48, 0xFBECDD, 0x5C3B23],
        "yellow" => [0xCB912F, 0xCA9849, 0xFBF3DB, 0x564328],
        "green" => [0x448361, 0x529E72, 0xEDF3EC, 0x243D30],
        "blue" => [0x337EA9, 0x5E87C9, 0xE7F3F8, 0x143A4E],
        "purple" => [0x9065B0, 0x9D68D3, 0xF6F3F9, 0x3C2D49],
        "pink" => [0xC14C8A, 0xD15796, 0xFAF1F5, 0x4E2C3C],
        "red" => [0xD44C47, 0xDF5452, 0xFDEBEC, 0x5D2A28],
        _ => return (None, None),
    };
    let rgb = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
    let pick = |l, d| rgb(if dark { d } else { l });
    if bg {
        (None, Some(pick(bl, bd)))
    } else {
        (Some(pick(tl, td)), None)
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
    fn block_colours_per_theme() {
        use notion_core::model::COLORS;
        assert_eq!(block_color("default", false), (None, None));
        assert_eq!(block_color("nope", true), (None, None));
        for c in &COLORS[1..] {
            for dark in [false, true] {
                let (t, b) = block_color(c, dark);
                assert!(t.is_some() && b.is_none(), "{c}");
                let (t, b) = block_color(&format!("{c}_background"), dark);
                assert!(t.is_none() && b.is_some(), "{c}_background");
            }
            assert_ne!(
                block_color(c, false),
                block_color(c, true),
                "{c}: a palette per theme"
            );
        }
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
