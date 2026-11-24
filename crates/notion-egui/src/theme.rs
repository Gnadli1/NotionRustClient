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
/// Vertical gap between blocks: Notion pads each block 3 px above and below
/// plus a 1–2 px margin.
pub const BLOCK_GAP: f32 = 9.0;
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

const fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

const fn pal(name: &'static str, dark: bool, c: [u32; 7]) -> Palette {
    let tint = if dark {
        Color32::from_rgba_premultiplied(14, 14, 14, 14)
    } else {
        Color32::from_rgba_premultiplied(3, 3, 3, 15)
    };
    Palette {
        name,
        dark,
        colors: Colors {
            bg: rgb(c[0]),
            text: rgb(c[1]),
            secondary: rgb(c[2]),
            sidebar: rgb(c[3]),
            hover: tint,
            border: rgb(c[4]),
            accent: rgb(c[5]),
            error: rgb(c[6]),
        },
    }
}

/// A selectable theme; "System" (follow the OS) is not in the list.
pub struct Palette {
    pub name: &'static str,
    pub colors: Colors,
    pub dark: bool,
}

// [bg, text, secondary, sidebar, border, accent, error]
pub const THEMES: &[Palette] = &[
    Palette {
        name: "Light",
        colors: LIGHT,
        dark: false,
    },
    Palette {
        name: "Dark",
        colors: DARK,
        dark: true,
    },
    pal(
        "Ocean",
        true,
        [
            0x0D2137, 0xDCEBF7, 0x8FB1CC, 0x112B47, 0x1F4468, 0x4DB3FF, 0xFF7B7B,
        ],
    ),
    pal(
        "Forest",
        true,
        [
            0x12261B, 0xDDEFE2, 0x8DB79B, 0x173222, 0x27503A, 0x4CC38A, 0xFF8A80,
        ],
    ),
    pal(
        "Sunset",
        false,
        [
            0xFFF4EA, 0x4A2C1A, 0x8A5A3C, 0xFFE8D3, 0xF5CBA7, 0xE8590C, 0xC92A2A,
        ],
    ),
    pal(
        "Lavender",
        false,
        [
            0xF6F1FF, 0x2E2150, 0x6B5B95, 0xEBE2FA, 0xD5C6F0, 0x7048E8, 0xC92A5A,
        ],
    ),
    pal(
        "Rose",
        true,
        [
            0x2A1620, 0xF8E1EA, 0xC596A9, 0x351C29, 0x5A2E42, 0xFF6FA5, 0xFF8A80,
        ],
    ),
    pal(
        "Mint",
        false,
        [
            0xECFAF6, 0x12403B, 0x3F7B73, 0xD9F3EC, 0xB3E3D8, 0x0B8F7E, 0xC92A2A,
        ],
    ),
    pal(
        "Sepia",
        false,
        [
            0xF4ECD8, 0x433422, 0x7A6648, 0xEADFC3, 0xD6C7A1, 0xA0522D, 0xB02A2A,
        ],
    ),
    pal(
        "Midnight",
        true,
        [
            0x0E0B26, 0xE6E3FF, 0x9E98D6, 0x15113A, 0x2B2562, 0x8B6BFF, 0xFF7A8A,
        ],
    ),
];

fn choice_id() -> egui::Id {
    egui::Id::unique("theme-choice")
}

/// Applies the theme `name`: "System" follows the OS, else a palette from
/// `THEMES` (unknown names count as "System").
pub fn select(ctx: &egui::Context, name: &str) {
    match THEMES.iter().position(|p| p.name == name) {
        Some(i) => {
            let p = &THEMES[i];
            let v = visuals(&p.colors, p.dark);
            ctx.set_visuals_of(Theme::Light, v.clone());
            ctx.set_visuals_of(Theme::Dark, v);
            ctx.set_theme(if p.dark { Theme::Dark } else { Theme::Light });
            ctx.data_mut(|d| d.insert_temp(choice_id(), i));
        }
        None => {
            install(ctx);
            ctx.set_theme(egui::ThemePreference::System);
            ctx.data_mut(|d| d.remove_temp::<usize>(choice_id()));
        }
    }
}

/// The palette of the theme `ui` is drawn in.
pub fn colors(ui: &egui::Ui) -> &'static Colors {
    let chosen = ui.ctx().data(|d| d.get_temp::<usize>(choice_id()));
    if let Some(p) = chosen.and_then(|i| THEMES.get(i)) {
        &p.colors
    } else if ui.visuals().dark_mode {
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

    fn lum(c: Color32) -> f32 {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    #[test]
    fn palettes_unique_and_readable() {
        assert!(THEMES.len() >= 8);
        for (i, p) in THEMES.iter().enumerate() {
            assert!(THEMES[..i].iter().all(|q| q.name != p.name), "{}", p.name);
            let (a, b) = (lum(p.colors.text), lum(p.colors.bg));
            let r = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(r >= 4.5, "{}: contrast {r}", p.name);
            let ratio = |x: Color32, y: Color32| {
                let (a, b) = (lum(x), lum(y));
                (a.max(b) + 0.05) / (a.min(b) + 0.05)
            };
            let c = &p.colors;
            let (acc, sec) = (ratio(c.accent, c.bg), ratio(c.secondary, c.sidebar));
            assert!(acc >= 3.0, "{}: accent/bg {acc}", p.name);
            assert!(sec >= 4.0, "{}: secondary/sidebar {sec}", p.name);
        }
    }

    #[test]
    fn select_picks_palette_colours() {
        let ctx = egui::Context::default();
        select(&ctx, "Ocean");
        let ocean = THEMES.iter().find(|p| p.name == "Ocean").unwrap();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            assert_eq!(*colors(ui), ocean.colors);
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn system_restores_the_system_palette() {
        let ctx = egui::Context::default();
        select(&ctx, "Ocean");
        assert!(ctx.data(|d| d.get_temp::<usize>(choice_id())).is_some());
        select(&ctx, "System");
        assert!(ctx.data(|d| d.get_temp::<usize>(choice_id())).is_none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        let fill = ctx.global_style().visuals.panel_fill;
        assert!(fill == LIGHT.bg || fill == DARK.bg);
        select(&ctx, "Nope");
        assert!(ctx.data(|d| d.get_temp::<usize>(choice_id())).is_none());
    }
}
