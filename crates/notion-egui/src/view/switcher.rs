//! The Ctrl+K switcher: a box near the top over a dimmed page. The keys are
//! taken in `Switcher::keys`; this draws the field and the list.

use eframe::egui::{
    self, Align2, Context, Id, Label, Modal, RichText, ScrollArea, TextEdit, Vec2,
};

use crate::switcher::{Item, Out, Switcher};
use crate::theme::Colors;

pub fn show(ctx: &Context, c: &Colors, sw: &mut Switcher, items: &[Item]) -> Out {
    let id = Id::unique("switcher");
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 80.0));
    let mut out = Out::None;
    let modal = Modal::new(id).area(area).show(ctx, |ui| {
        ui.set_width(560.0);
        let field = ui.add(
            TextEdit::singleline(&mut sw.query)
                .hint_text("Search pages…")
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Heading),
        );
        // typing goes to the field, whatever was clicked
        if !field.has_focus() {
            field.request_focus();
        }
        if field.changed() {
            sw.query_changed();
        }
        ui.add_space(4.0);
        if sw.query.trim().is_empty() && !items.is_empty() {
            ui.label(RichText::new("Recent").small().color(c.secondary));
        }
        if items.is_empty() {
            let text = if sw.query.trim().is_empty() {
                "No recent pages. Type to search."
            } else {
                "No matches yet…"
            };
            ui.label(RichText::new(text).color(c.secondary));
        }
        let sel = sw.sel.min(items.len().saturating_sub(1));
        ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, it) in items.iter().enumerate() {
                    let r = super::item(ui, c, i == sel, 28.0, |ui| {
                        ui.add(Label::new(&it.icon));
                        ui.add(Label::new(super::untitled(&it.title)).truncate());
                    });
                    if i == sel && sw.moved {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        let new_tab = ui.input(|i| i.modifiers.ctrl);
                        out = Out::Pick {
                            id: it.id.clone(),
                            new_tab,
                        };
                    }
                }
            });
        sw.moved = false;
    });
    if out == Out::None && modal.backdrop_response.clicked() {
        out = Out::Close;
    }
    out
}
