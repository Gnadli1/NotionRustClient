//! Drawing of the shell: tabs, sidebar, status bar, switcher. Each function
//! returns the `Action` the user triggered, if any; `App::apply` runs it.

pub mod browser_login;
pub mod sidebar;
pub mod status;
pub mod switcher;
pub mod tabbar;
pub mod workspace;

use eframe::egui::{
    self, pos2, vec2, Align, Id, Key, Label, Layout, Modifiers, Rect, RichText, Sense, Stroke,
    TextEdit, Ui, UiBuilder,
};

use crate::app::Action;
use crate::keys;
use crate::nav::Find;
use crate::theme::Colors;

pub fn untitled(t: &str) -> &str {
    if t.is_empty() {
        "Untitled"
    } else {
        t
    }
}

/// A full-width clickable row: highlighted when `selected` or hovered, with
/// `content` laid out left to right, vertically centred, inside it.
pub fn item(
    ui: &mut Ui,
    c: &Colors,
    selected: bool,
    height: f32,
    content: impl FnOnce(&mut Ui),
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, 4.0, c.hover.gamma_multiply(2.0));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, c.hover);
    }
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // labels must not take the click from the row
    child.style_mut().interaction.selectable_labels = false;
    child.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
    content(&mut child);
    resp
}

/// Icon and title of each page above the open one (clickable), then the open
/// page's own title.
pub fn breadcrumb(
    ui: &mut Ui,
    c: &Colors,
    crumbs: &[(String, String, String)],
    current: &str,
) -> Option<Action> {
    let mut act = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (id, icon, title) in crumbs {
            let text = RichText::new(format!("{icon} {title}"))
                .size(13.0)
                .color(c.secondary);
            let r = ui.add(Label::new(text).sense(Sense::click()).selectable(false));
            if r.hovered() {
                ui.painter().rect_filled(r.rect.expand(2.0), 3.0, c.hover);
            }
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                act = Some(Action::Open(id.clone()));
            }
            ui.label(RichText::new("›").size(13.0).color(c.secondary));
        }
        ui.label(RichText::new(current).size(13.0).color(c.secondary));
    });
    act
}

/// The centre of a tab with no page. Offers the browser-session login when no page
/// is open, which is also the first thing a new user sees.
pub fn empty_tab(ui: &mut Ui, c: &Colors) -> Option<Action> {
    let mut act = None;
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("New tab. Press Ctrl+K to open a page.").color(c.secondary));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("🌐 Browser session").clicked() {
                    act = Some(Action::BrowserLoginStart);
                }
            });
            ui.label(
                RichText::new(
                    "Browser session sees every workspace shared with your account. \
                     Tokens use the public API.",
                )
                .small()
                .color(c.secondary),
            );
        });
    });
    act
}

/// What the find bar did this frame.
pub enum FindOut {
    Typed,
    /// Enter (1) or Shift+Enter (-1).
    Step(isize),
    Close,
}

/// Ctrl+F's bar: the text box (Enter / Shift+Enter step, Esc closes while it
/// has the keyboard), "n/m", a close button.
pub fn find_bar(ui: &mut Ui, c: &Colors, f: &mut Find) -> Option<FindOut> {
    let id = Id::unique("find-box");
    let mut out = None;
    if ui.memory(|m| m.has_focus(id)) {
        let (next, prev) = ui.input_mut(|i| {
            (
                keys::take_exact(&mut i.events, Modifiers::NONE, Key::Enter).0,
                keys::take_exact(&mut i.events, Modifiers::SHIFT, Key::Enter).0,
            )
        });
        out = if prev > 0 {
            Some(FindOut::Step(-1))
        } else if next > 0 {
            Some(FindOut::Step(1))
        } else {
            None
        };
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            TextEdit::singleline(&mut f.query)
                .id(id)
                .hint_text("Find in page")
                .desired_width(240.0),
        );
        if std::mem::take(&mut f.focus) {
            r.request_focus();
        }
        if r.changed() {
            out = Some(FindOut::Typed);
        }
        // egui drops the focus on Esc before we see it
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            out = Some(FindOut::Close);
        }
        let n = f.hits.len();
        let count = if n == 0 {
            "0/0".to_string()
        } else {
            format!("{}/{n}", f.cur + 1)
        };
        ui.label(RichText::new(count).color(c.secondary));
        if ui.small_button("✕").clicked() {
            out = Some(FindOut::Close);
        }
    });
    out
}

/// The outline: a tick per heading, indented by level, vertically centred.
/// Hover shows the heading, a click goes to it.
pub fn outline(ui: &mut Ui, c: &Colors, heads: &[(usize, u8, String)]) -> Option<Action> {
    let area = ui.max_rect();
    let step = (area.height() / heads.len().max(1) as f32).min(14.0);
    let top = area.center().y - step * heads.len() as f32 / 2.0;
    let mut act = None;
    for (k, (row, level, text)) in heads.iter().enumerate() {
        let band = Rect::from_min_size(
            pos2(area.left(), top + k as f32 * step),
            vec2(area.width(), step),
        );
        let r = ui.interact(band, Id::unique(("outline", k)), Sense::click());
        let color = if r.hovered() { c.accent } else { c.secondary };
        let y = band.center().y;
        let x = band.left() + 4.0 + 4.0 * f32::from(level.saturating_sub(1));
        ui.painter().line_segment(
            [pos2(x, y), pos2(band.right() - 4.0, y)],
            Stroke::new(2.0, color),
        );
        if r.on_hover_text(untitled(text))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            act = Some(Action::Goto(*row));
        }
    }
    act
}
