//! The Ctrl+K switcher: a box near the top over a dimmed page. The keys are
//! taken in `Switcher::keys`; this draws the field and the list.

use eframe::egui::{
    self,
    text::{LayoutJob, TextFormat},
    Align2, Context, Id, Label, Modal, RichText, ScrollArea, TextEdit, Vec2,
};

use crate::switcher::{Item, Out, Switcher};
use crate::theme::Colors;

pub fn show(ctx: &Context, c: &Colors, sw: &mut Switcher, items: &[Item]) -> Out {
    let id = Id::unique("switcher");
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 80.0));
    let mut out = Out::None;
    let modal = Modal::new(id).area(area).show(ctx, |ui| {
        // the preview needs room: on a narrow window the list has it all
        let preview = ctx.content_rect().width() > 900.0;
        ui.set_width(if preview { 880.0 } else { 560.0 });
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(540.0_f32.min(ui.available_width()));
                let field = ui.add(
                    TextEdit::singleline(&mut sw.query)
                        .hint_text(if sw.moving.is_some() {
                            "Move to… (pick the new parent page)"
                        } else {
                            "Search pages…"
                        })
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
                            if it.group.is_some()
                                && it.group != i.checked_sub(1).and_then(|p| items[p].group)
                            {
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(it.group.unwrap_or_default())
                                        .small()
                                        .color(c.secondary),
                                );
                            }
                            let r = super::item(ui, c, i == sel, 28.0, |ui| {
                                ui.add(Label::new(&it.icon));
                                let font = egui::TextStyle::Body.resolve(ui.style());
                                let mut job = LayoutJob::default();
                                let fmt = |color, font: &egui::FontId| {
                                    TextFormat::simple(font.clone(), color)
                                };
                                job.append(
                                    super::untitled(&it.title),
                                    0.0,
                                    fmt(ui.visuals().text_color(), &font),
                                );
                                if !it.path.is_empty() {
                                    let small = egui::FontId::proportional(font.size - 2.0);
                                    job.append(
                                        &format!("  {}", it.path),
                                        0.0,
                                        fmt(c.secondary, &small),
                                    );
                                }
                                ui.add(Label::new(job).truncate());
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
            if preview {
                ui.separator();
                ui.vertical(|ui| {
                    let lines = sw.preview.as_ref().map_or(&[][..], |p| &p.1);
                    if lines.is_empty() {
                        ui.label(RichText::new("No preview").color(c.secondary));
                    }
                    for l in lines {
                        ui.add(Label::new(RichText::new(l).size(13.0)).truncate());
                    }
                });
            }
        });
    });
    if out == Out::None && modal.backdrop_response.clicked() {
        out = Out::Close;
    }
    out
}
