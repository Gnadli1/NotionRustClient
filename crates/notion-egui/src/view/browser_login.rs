//! The browser-session login dialog.

use eframe::egui::{self, Align2, RichText, TextEdit, Vec2};

use crate::app::Action;
use crate::browser_login::Flow;
use crate::theme::Colors;

pub fn dialog(ctx: &egui::Context, c: &Colors, flow: &mut Flow) -> Option<Action> {
    let mut act = None;
    egui::Window::new("Browser session")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(
                RichText::new(
                    "This logs in with the same session as Notion's web app. It sees every \
                     workspace your account can see, including pages other people shared with \
                     you, and it talks to Notion's own API in Rust — no WebView.",
                )
                .color(c.secondary),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("How to get token_v2").strong());
            ui.label(
                RichText::new(
                    "1. Open notion.so in your browser and make sure you are logged in.\n\
                     2. Open DevTools (F12) → Application (Chrome/Edge) or Storage (Firefox) → \
                     Cookies → https://www.notion.so.\n\
                     3. Copy the token_v2 value, or paste the whole Cookie header.",
                )
                .small()
                .color(c.secondary),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("token_v2").small().color(c.secondary));
            let edit = ui.add(
                TextEdit::singleline(&mut flow.token)
                    .password(true)
                    .hint_text("token_v2 value or a Cookie header")
                    .desired_width(f32::INFINITY),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                act = Some(Action::BrowserLoginConnect);
            }
            ui.label(
                RichText::new(
                    "Images, covers and files are downloaded with the same session; no other \
                     cookie is needed.\n\
                     The token is a password: it is stored only in profiles.json on this \
                     computer. Logging out of Notion or changing the password invalidates it.",
                )
                .small()
                .color(c.secondary),
            );
            if let Some(e) = &flow.error {
                ui.add_space(6.0);
                ui.colored_label(c.error, e);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let b = ui.add_enabled(!flow.busy, egui::Button::new("Connect"));
                if b.clicked() {
                    act = Some(Action::BrowserLoginConnect);
                }
                if ui.button("Cancel").clicked() {
                    act = Some(Action::BrowserLoginCancel);
                }
                if flow.busy {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(
                        RichText::new("finding workspaces…")
                            .small()
                            .color(c.secondary),
                    );
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Each workspace becomes a native profile with its own cache. Switch with the \
                     picker in the top bar.",
                )
                .small()
                .color(c.secondary),
            );
        });
    act
}
