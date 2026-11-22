//! The add/edit workspace dialog. The picker itself is in `view::sidebar`'s
//! workspace row; this file keeps its form.

use eframe::egui::{self, Align2, Key, RichText, TextEdit, Vec2};

use crate::app::Action;
use crate::profiles::Form;
use crate::theme::Colors;

/// The add/edit dialog. Save and Cancel are actions, applied after the frame.
pub fn form(ctx: &egui::Context, c: &Colors, f: &mut Form) -> Option<Action> {
    let mut act = None;
    let title = if f.original.is_some() {
        "Edit workspace"
    } else {
        "Add workspace"
    };
    let response = egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_width(440.0);
            ui.add_space(4.0);
            ui.label(RichText::new("Name").small().color(c.secondary));
            ui.add(
                TextEdit::singleline(&mut f.name)
                    .hint_text("e.g. Personal or Acme team")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("Token").small().color(c.secondary));
            ui.add(
                TextEdit::singleline(&mut f.token)
                    .password(true)
                    .hint_text("ntn_…")
                    .desired_width(f32::INFINITY),
            );
            ui.label(
                RichText::new(
                    "A personal access token (Developer portal → Personal access tokens) \
                     or an internal-connection token. It only sees its own workspace.",
                )
                .small()
                .color(c.secondary),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("User id (optional)")
                    .small()
                    .color(c.secondary),
            );
            ui.add(
                TextEdit::singleline(&mut f.user_id)
                    .hint_text("only needed for the Updates inbox with a bot token")
                    .desired_width(f32::INFINITY),
            );
            if let Some(e) = &f.error {
                ui.add_space(6.0);
                ui.colored_label(c.error, e);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    act = Some(Action::ProfileSave);
                }
                if ui.button("Cancel").clicked() {
                    act = Some(Action::ProfileCancel);
                }
            });
            ui.label(
                RichText::new(
                    "profiles.json lives in your config folder. Treat it like a password file.",
                )
                .small()
                .color(c.secondary),
            );
        });
    // Enter saves, Esc cancels, unless a button already did something.
    if response.is_some() && act.is_none() {
        let (enter, esc) = ctx.input(|i| {
            (
                i.key_pressed(Key::Enter) && !i.modifiers.shift,
                i.key_pressed(Key::Escape),
            )
        });
        if enter {
            act = Some(Action::ProfileSave);
        } else if esc {
            act = Some(Action::ProfileCancel);
        }
    }
    act
}
