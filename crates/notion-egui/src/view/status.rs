//! The status bar (op counts, sync state, last error) and the problem list.

use eframe::egui::{
    self, text::LayoutJob, Align2, Area, Button, Context, Frame, Id, Label, Order, RichText,
    ScrollArea, TextFormat, Ui,
};

use crate::app::Action;
use crate::problems::Problem;
use crate::theme::Colors;

pub struct Status<'a> {
    /// queued, conflict, failed
    pub counts: [usize; 3],
    /// "pushing…" / "syncing…" while a command is in flight.
    pub busy: Option<&'static str>,
    pub error: Option<&'a str>,
    pub info: Option<&'a str>,
    pub problems_open: bool,
    pub has_problems: bool,
}

pub fn show(ui: &mut Ui, c: &Colors, s: &Status) -> Option<Action> {
    let mut act = None;
    let [queued, conflicts, failed] = s.counts;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("queued {queued} ·")).size(12.0).color(c.secondary));
        let counts = format!("conflicts {conflicts} · failed {failed}");
        if s.has_problems {
            // opens the list of them
            let b = Button::new(RichText::new(counts).size(12.0).color(c.error))
                .frame(false)
                .selected(s.problems_open);
            if ui.add(b).clicked() {
                act = Some(Action::ToggleProblems);
            }
        } else {
            ui.label(RichText::new(counts).size(12.0).color(c.secondary));
        }
        if let Some(b) = s.busy {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new(b).size(12.0).color(c.secondary));
        }
        if let Some(i) = s.info {
            ui.label(RichText::new(format!("· {i}")).size(12.0).color(c.secondary));
        }
        if let Some(e) = s.error {
            ui.add(
                Label::new(RichText::new(format!("· error: {e}")).size(12.0).color(c.error))
                    .truncate(),
            );
        }
    });
    act
}

/// The conflict/failed ops over the page's bottom-left corner: click one to
/// go to it, or resolve it: the panel for a conflict, or Keep theirs (the
/// remote version wins). Up/Down select, Enter opens the panel.
pub fn problems(ctx: &Context, c: &Colors, list: &[Problem], sel: usize) -> Option<Action> {
    let mut act = None;
    Area::new(Id::unique("problems"))
        .order(Order::Foreground)
        .anchor(Align2::LEFT_BOTTOM, egui::vec2(8.0, -34.0))
        .show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(480.0);
                ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for (i, p) in list.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let mut job = LayoutJob::default();
                            job.append(
                                &format!("{} · {}\n", p.kind, p.label),
                                0.0,
                                TextFormat {
                                    color: c.text,
                                    ..Default::default()
                                },
                            );
                            job.append(
                                &p.error,
                                0.0,
                                TextFormat {
                                    color: c.error,
                                    font_id: egui::FontId::proportional(12.0),
                                    ..Default::default()
                                },
                            );
                            let w = ui.available_width() - if p.conflict { 190.0 } else { 80.0 };
                            let open = Button::new(job)
                                .frame(false)
                                .selected(i == sel)
                                .min_size(egui::vec2(w, 0.0));
                            if ui.add(open).clicked() {
                                act = Some(Action::OpenProblem(p.op));
                            }
                            if p.conflict && ui.button("Resolve…").clicked() {
                                act = Some(Action::Resolve(p.op));
                            }
                            if ui.button(if p.conflict { "Keep theirs" } else { "Discard" }).clicked() {
                                act = Some(Action::Discard(p.op));
                            }
                        });
                    }
                });
            });
        });
    act
}
