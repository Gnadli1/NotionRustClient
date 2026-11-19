//! Light and dark palettes (Notion's layout colours, not its branding),
//! picked from the system's light/dark mode.

use std::sync::LazyLock;

use iced::theme::{Mode, Palette};
use iced::widget::{button, container, rule, text_editor, text_input};
use iced::{Background, Border, Color, Theme};

/// The colours iced's [`Palette`] has no slot for; spans need them as plain
/// [`Color`]s, so the view passes this around.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub bg: Color,
    pub text: Color,
    pub secondary: Color,
    pub sidebar: Color,
    pub hover: Color,
    pub border: Color,
    pub accent: Color,
    pub error: Color,
}

pub const LIGHT: Colors = Colors {
    bg: Color::from_rgb8(0xFF, 0xFF, 0xFF),
    text: Color::from_rgb8(0x37, 0x35, 0x2F),
    secondary: Color::from_rgb8(0x78, 0x77, 0x74),
    sidebar: Color::from_rgb8(0xF7, 0xF7, 0xF5),
    hover: Color::from_rgba8(55, 53, 47, 0.06),
    border: Color::from_rgb8(0xE9, 0xE9, 0xE7),
    accent: Color::from_rgb8(0x23, 0x83, 0xE2),
    error: Color::from_rgb8(0xE0, 0x3E, 0x3E),
};

pub const DARK: Colors = Colors {
    bg: Color::from_rgb8(0x19, 0x19, 0x19),
    text: Color::from_rgb8(0xD4, 0xD4, 0xD4),
    secondary: Color::from_rgb8(0x9B, 0x9B, 0x9B),
    sidebar: Color::from_rgb8(0x20, 0x20, 0x20),
    hover: Color::from_rgba8(255, 255, 255, 0.055),
    border: Color::from_rgb8(0x2F, 0x2F, 0x2F),
    accent: Color::from_rgb8(0x23, 0x83, 0xE2),
    error: Color::from_rgb8(0xE6, 0x6A, 0x6A),
};

/// No system preference (`Mode::None`) means light.
pub fn colors(mode: Mode) -> &'static Colors {
    if mode == Mode::Dark {
        &DARK
    } else {
        &LIGHT
    }
}

fn build(name: &'static str, c: &Colors) -> Theme {
    let base = if c == &DARK {
        Palette::DARK
    } else {
        Palette::LIGHT
    };
    Theme::custom(
        name,
        Palette {
            background: c.bg,
            text: c.text,
            primary: c.accent,
            danger: c.error,
            ..base
        },
    )
}

static LIGHT_THEME: LazyLock<Theme> = LazyLock::new(|| build("Light", &LIGHT));
static DARK_THEME: LazyLock<Theme> = LazyLock::new(|| build("Dark", &DARK));

/// The app theme for a system mode (built once; this is an Arc clone).
pub fn theme(mode: Mode) -> Theme {
    if mode == Mode::Dark {
        DARK_THEME.clone()
    } else {
        LIGHT_THEME.clone()
    }
}

/// The colours of the theme a style closure is handed.
fn of(t: &Theme) -> &'static Colors {
    if t.extended_palette().is_dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// Text-like button: no fill, hover colour on hover; `selected` keeps it.
pub fn item(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |t, status| {
        let c = of(t);
        let lit = selected || matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: lit.then_some(Background::Color(c.hover)),
            text_color: c.text,
            border: Border::default().rounded(4),
            ..button::Style::default()
        }
    }
}

pub fn sidebar(t: &Theme) -> container::Style {
    container::Style {
        background: Some(of(t).sidebar.into()),
        ..container::Style::default()
    }
}

/// Table cells and the `/` menu.
pub fn bordered(t: &Theme) -> container::Style {
    let c = of(t);
    container::Style {
        background: Some(c.bg.into()),
        border: Border::default().width(1).color(c.border).rounded(4),
        ..container::Style::default()
    }
}

/// Callout and code blocks: a light tint, rounded.
pub fn tinted(t: &Theme) -> container::Style {
    container::Style {
        background: Some(of(t).hover.into()),
        border: Border::default().rounded(4),
        ..container::Style::default()
    }
}

/// The bar left of a quote.
pub fn quote_bar(t: &Theme) -> rule::Style {
    rule::Style {
        color: of(t).text,
        ..rule::default(t)
    }
}

pub fn divider(t: &Theme) -> rule::Style {
    rule::Style {
        color: of(t).border,
        ..rule::default(t)
    }
}

/// The row editor blends into the page: no box, only the caret.
pub fn editor(t: &Theme, _: text_editor::Status) -> text_editor::Style {
    let c = of(t);
    text_editor::Style {
        background: Color::TRANSPARENT.into(),
        border: Border::default(),
        placeholder: c.secondary,
        value: c.text,
        selection: c.accent.scale_alpha(0.3),
    }
}

pub fn search(t: &Theme, status: text_input::Status) -> text_input::Style {
    let c = of(t);
    text_input::Style {
        background: c.hover.into(),
        border: Border::default().rounded(4).width(1).color(
            if matches!(status, text_input::Status::Focused { .. }) {
                c.accent
            } else {
                Color::TRANSPARENT
            },
        ),
        icon: c.secondary,
        placeholder: c.secondary,
        value: c.text,
        selection: c.accent.scale_alpha(0.3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_mode_picks_the_theme() {
        assert_eq!(colors(Mode::Dark), &DARK);
        assert_eq!(colors(Mode::Light), &LIGHT);
        // no preference: light
        assert_eq!(colors(Mode::None), &LIGHT);
        assert!(theme(Mode::Dark).extended_palette().is_dark);
        assert!(!theme(Mode::Light).extended_palette().is_dark);
        assert!(!theme(Mode::None).extended_palette().is_dark);
        // style closures map a theme back to the same colours
        assert_eq!(of(&theme(Mode::Dark)), &DARK);
        assert_eq!(of(&theme(Mode::None)), &LIGHT);
    }
}
