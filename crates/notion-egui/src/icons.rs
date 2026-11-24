//! Monochrome vector icons for the shell, drawn with the painter so they look
//! like Notion's line icons at any DPI. Emoji stay where they are content (a
//! page's own icon); everything around the page is drawn here.

use eframe::egui::{
    self, Color32, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};

// Some glyphs are ahead of their call sites: the hover menus and page actions
// land in later passes.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    Bell,
    Sync,
    Trash,
    Panel,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    Plus,
    Close,
    Page,
    Database,
    Dots,
    Comment,
    Check,
    Back,
    Forward,
    Sidebar,
    Compose,
    Home,
    /// A line chart (the chart view's tab).
    Chart,
    /// A grid (the table view's tab).
    Table,
    /// Settings.
    Gear,
    /// The AI agent: a four-point star.
    Sparkle,
    /// Send: an arrow pointing up.
    ArrowUp,
}

/// Draws `icon` centred in `rect` in `color`. The stroke scales with the icon
/// but stays between 1.2 and 1.8 points, like Notion's 16 px line set.
pub fn paint(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()).min(22.0);
    let r = Rect::from_center_size(rect.center(), Vec2::splat(s));
    let p = |x: f32, y: f32| r.min + Vec2::new(x * s, y * s);
    let stroke = Stroke::new((s / 11.0).clamp(1.25, 1.8), color);
    let painter = ui.painter();
    let line = |pts: Vec<Pos2>| painter.add(Shape::line(pts, stroke));
    let closed = |pts: Vec<Pos2>| painter.add(Shape::closed_line(pts, stroke));
    let arc = |cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32| {
        let n = 24;
        let pts = (0..=n)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / n as f32;
                p(cx + rx * a.cos(), cy + ry * a.sin())
            })
            .collect::<Vec<_>>();
        line(pts);
    };
    match icon {
        Icon::Search => {
            let c = p(0.43, 0.43);
            let rad = 0.27 * s;
            painter.circle_stroke(c, rad, stroke);
            line(vec![p(0.62, 0.62), p(0.84, 0.84)]);
        }
        Icon::Bell => {
            closed(vec![
                p(0.26, 0.70),
                p(0.32, 0.63),
                p(0.32, 0.44),
                p(0.36, 0.32),
                p(0.44, 0.25),
                p(0.56, 0.25),
                p(0.64, 0.32),
                p(0.68, 0.44),
                p(0.68, 0.63),
                p(0.74, 0.70),
            ]);
            line(vec![p(0.43, 0.77), p(0.57, 0.77)]);
        }
        Icon::Sync => {
            arc(0.5, 0.5, 0.32, 0.32, -2.75, -0.35);
            line(vec![p(0.78, 0.29), p(0.82, 0.46), p(0.66, 0.42)]);
            arc(0.5, 0.5, 0.32, 0.32, 0.39, 2.79);
            line(vec![p(0.22, 0.71), p(0.18, 0.54), p(0.34, 0.58)]);
        }
        Icon::Trash => {
            line(vec![p(0.20, 0.28), p(0.80, 0.28)]);
            line(vec![
                p(0.38, 0.28),
                p(0.38, 0.18),
                p(0.62, 0.18),
                p(0.62, 0.28),
            ]);
            closed(vec![
                p(0.27, 0.33),
                p(0.30, 0.83),
                p(0.70, 0.83),
                p(0.73, 0.33),
            ]);
            line(vec![p(0.42, 0.42), p(0.43, 0.71)]);
            line(vec![p(0.58, 0.42), p(0.57, 0.71)]);
        }
        Icon::Panel => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.18, 0.22), p(0.82, 0.78)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            line(vec![p(0.40, 0.22), p(0.40, 0.78)]);
        }
        Icon::ChevronLeft => {
            line(vec![p(0.62, 0.24), p(0.38, 0.5), p(0.62, 0.76)]);
        }
        Icon::ChevronRight => {
            line(vec![p(0.38, 0.24), p(0.62, 0.5), p(0.38, 0.76)]);
        }
        Icon::ChevronDown => {
            line(vec![p(0.26, 0.40), p(0.5, 0.64), p(0.74, 0.40)]);
        }
        Icon::Plus => {
            line(vec![p(0.5, 0.22), p(0.5, 0.78)]);
            line(vec![p(0.22, 0.5), p(0.78, 0.5)]);
        }
        Icon::Close => {
            line(vec![p(0.26, 0.26), p(0.74, 0.74)]);
            line(vec![p(0.74, 0.26), p(0.26, 0.74)]);
        }
        Icon::Page => {
            closed(vec![
                p(0.30, 0.15),
                p(0.58, 0.15),
                p(0.74, 0.31),
                p(0.74, 0.85),
                p(0.30, 0.85),
            ]);
            line(vec![p(0.58, 0.15), p(0.58, 0.31), p(0.74, 0.31)]);
            line(vec![p(0.38, 0.50), p(0.66, 0.50)]);
            line(vec![p(0.38, 0.64), p(0.60, 0.64)]);
        }
        Icon::Database => {
            painter.add(Shape::ellipse_stroke(
                p(0.5, 0.27),
                Vec2::new(0.29 * s, 0.11 * s),
                stroke,
            ));
            line(vec![p(0.21, 0.27), p(0.21, 0.73)]);
            line(vec![p(0.79, 0.27), p(0.79, 0.73)]);
            arc(0.5, 0.73, 0.29, 0.11, 0.0, std::f32::consts::PI);
            line(vec![p(0.21, 0.47), p(0.79, 0.47)]);
        }
        Icon::Dots => {
            for x in [0.26, 0.5, 0.74] {
                painter.circle_filled(p(x, 0.5), (0.055 * s).max(1.1), color);
            }
        }
        Icon::Comment => {
            let body = Rect::from_min_max(p(0.18, 0.22), p(0.82, 0.70));
            painter.rect_stroke(body, 2.5, stroke, StrokeKind::Inside);
            line(vec![p(0.32, 0.70), p(0.32, 0.84), p(0.46, 0.70)]);
        }
        Icon::Check => {
            line(vec![p(0.22, 0.52), p(0.42, 0.72), p(0.78, 0.28)]);
        }
        Icon::Back => {
            line(vec![p(0.46, 0.24), p(0.24, 0.5), p(0.46, 0.76)]);
            line(vec![p(0.24, 0.5), p(0.78, 0.5)]);
        }
        Icon::Forward => {
            line(vec![p(0.54, 0.24), p(0.76, 0.5), p(0.54, 0.76)]);
            line(vec![p(0.22, 0.5), p(0.76, 0.5)]);
        }
        Icon::Sidebar => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.16, 0.22), p(0.84, 0.78)),
                2.0,
                stroke,
                StrokeKind::Inside,
            );
            line(vec![p(0.43, 0.22), p(0.43, 0.78)]);
            painter.rect_filled(
                Rect::from_min_max(p(0.18, 0.24), p(0.41, 0.76)),
                1.5,
                color.gamma_multiply(0.28),
            );
        }
        Icon::Compose => {
            closed(vec![
                p(0.24, 0.30),
                p(0.24, 0.80),
                p(0.74, 0.80),
                p(0.74, 0.52),
            ]);
            line(vec![p(0.50, 0.58), p(0.86, 0.22)]);
        }
        Icon::Gear => {
            painter.circle_stroke(p(0.5, 0.5), 0.12 * s, stroke);
            painter.circle_stroke(p(0.5, 0.5), 0.24 * s, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (x, y) = (a.cos(), a.sin());
                line(vec![
                    p(0.5 + 0.24 * x, 0.5 + 0.24 * y),
                    p(0.5 + 0.34 * x, 0.5 + 0.34 * y),
                ]);
            }
        }
        Icon::Sparkle => {
            closed(vec![
                p(0.50, 0.14),
                p(0.59, 0.41),
                p(0.86, 0.50),
                p(0.59, 0.59),
                p(0.50, 0.86),
                p(0.41, 0.59),
                p(0.14, 0.50),
                p(0.41, 0.41),
            ]);
        }
        Icon::ArrowUp => {
            line(vec![p(0.50, 0.82), p(0.50, 0.20)]);
            line(vec![p(0.26, 0.44), p(0.50, 0.20), p(0.74, 0.44)]);
        }
        Icon::Home => {
            line(vec![p(0.20, 0.50), p(0.50, 0.20), p(0.80, 0.50)]);
            closed(vec![
                p(0.28, 0.47),
                p(0.28, 0.82),
                p(0.72, 0.82),
                p(0.72, 0.47),
            ]);
        }
        Icon::Chart => {
            line(vec![p(0.20, 0.20), p(0.20, 0.80), p(0.82, 0.80)]);
            line(vec![
                p(0.29, 0.66),
                p(0.45, 0.44),
                p(0.60, 0.56),
                p(0.80, 0.30),
            ]);
            painter.circle_filled(p(0.45, 0.44), (0.045 * s).max(0.9), color);
            painter.circle_filled(p(0.60, 0.56), (0.045 * s).max(0.9), color);
        }
        Icon::Table => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.17, 0.22), p(0.83, 0.78)),
                1.5,
                stroke,
                StrokeKind::Inside,
            );
            line(vec![p(0.17, 0.40), p(0.83, 0.40)]);
            line(vec![p(0.44, 0.22), p(0.44, 0.78)]);
            line(vec![p(0.17, 0.59), p(0.83, 0.59)]);
        }
    }
}

/// A square hit target with a hover fill and an optional tooltip.
pub fn button(
    ui: &mut Ui,
    size: f32,
    icon: Icon,
    color: Color32,
    hover: Color32,
    tooltip: Option<&str>,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, hover);
    }
    paint(ui, rect.shrink(size * 0.22), icon, color);
    match tooltip {
        Some(t) => resp.on_hover_text(t),
        None => resp,
    }
}

/// A small rounded workspace avatar with its initial, like Notion's workspace
/// glyph. `seed` picks a stable tint.
pub fn workspace_avatar(ui: &mut Ui, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let hue = name
        .bytes()
        .fold(11u32, |h, b| h.wrapping_mul(37).wrapping_add(b as u32))
        % 360;
    let fill: Color32 = egui::ecolor::Hsva::new(
        hue as f32 / 360.0,
        if ui.visuals().dark_mode { 0.55 } else { 0.62 },
        if ui.visuals().dark_mode { 0.62 } else { 0.78 },
        1.0,
    )
    .into();
    ui.painter().rect_filled(rect, size * 0.22, fill);
    let initial = name
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(size * 0.58),
        Color32::WHITE,
    );
}

/// The fallback glyph of a page row: the page emoji when it has one, else a
/// line icon. `database` picks the cylinder.
pub fn page_glyph(ui: &mut Ui, rect: Rect, icon: Option<&str>, database: bool, color: Color32) {
    match icon.map(str::trim).filter(|i| !i.is_empty()) {
        Some(e) => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                e,
                egui::FontId::proportional(rect.height() * 0.74),
                color,
            );
        }
        None => paint(
            ui,
            rect.shrink(rect.height() * 0.16),
            if database { Icon::Database } else { Icon::Page },
            color,
        ),
    }
}
