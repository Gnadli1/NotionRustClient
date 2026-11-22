//! The "Sign in with Notion" dialogs: one-time OAuth app setup, then the
//! waiting/manual-code window.

use eframe::egui::{self, Align2, RichText, TextEdit, Vec2};

use crate::app::Action;
use crate::oauth::Flow;
use crate::profiles::OAuthApp;
use crate::theme::Colors;

pub fn setup(
    ctx: &egui::Context,
    c: &Colors,
    app: &mut OAuthApp,
    error: &mut Option<String>,
) -> Option<Action> {
    let mut act = None;
    egui::Window::new("Sign in with Notion")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(
                RichText::new(
                    "Notion does not give third-party apps your password. This uses Notion's \
                     official OAuth page: you log in there, choose the pages, and the app gets \
                     a token for that workspace.",
                )
                .color(c.secondary),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("1. Create a public OAuth connection (once)").strong());
            ui.label(
                RichText::new(
                    "Developer portal → Connections → New connection → Authentication: OAuth → \
                     Installation: Any workspace. Add the redirect URI below and enable read/\
                     update/insert/comments/user information.",
                )
                .small()
                .color(c.secondary),
            );
            if ui.button("Open Notion Developer portal").clicked() {
                act = Some(Action::OAuthDevPortal);
            }
            ui.add_space(10.0);
            ui.label(RichText::new("2. Paste its client credentials").strong());
            ui.label(RichText::new("Client ID").small().color(c.secondary));
            ui.add(
                TextEdit::singleline(&mut app.client_id)
                    .hint_text("OAuth client ID")
                    .desired_width(f32::INFINITY),
            );
            ui.label(RichText::new("Client secret").small().color(c.secondary));
            ui.add(
                TextEdit::singleline(&mut app.client_secret)
                    .password(true)
                    .hint_text("OAuth client secret")
                    .desired_width(f32::INFINITY),
            );
            ui.label(RichText::new("Redirect URI").small().color(c.secondary));
            ui.add(TextEdit::singleline(&mut app.redirect_uri).desired_width(f32::INFINITY));
            ui.label(
                RichText::new(
                    "Use http://localhost:8765/callback for automatic sign-in. Use the hostname \
                     localhost, not an IP address (Notion rejects IPs). If Notion still refuses \
                     it, use any https:// URL you control and paste the code in the next window.",
                )
                .small()
                .color(c.secondary),
            );
            if let Some(e) = error {
                ui.add_space(6.0);
                ui.colored_label(c.error, e);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save and sign in").clicked() {
                    act = Some(Action::OAuthSetupSave);
                }
                if ui.button("Cancel").clicked() {
                    act = Some(Action::OAuthSetupCancel);
                }
            });
            ui.label(
                RichText::new(
                    "The client secret stays in profiles.json on this computer, like the tokens.",
                )
                .small()
                .color(c.secondary),
            );
        });
    act
}

pub fn waiting(ctx: &egui::Context, c: &Colors, flow: &mut Flow) -> Option<Action> {
    let mut act = None;
    egui::Window::new("Sign in with Notion")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Finish the sign-in in your browser.").strong());
            });
            ui.label(
                RichText::new(
                    "Choose the workspace (top right), select the pages you want in this client, \
                     then press Allow access. This window will pick up the token.",
                )
                .small()
                .color(c.secondary),
            );
            ui.label(
                RichText::new(
                    "Only pages you have full access to are listed. If a shared page is missing, \
                     its owner must grant your account access or create an internal-connection token.",
                )
                .small()
                .color(c.secondary),
            );
            if let Some(e) = &flow.error {
                ui.add_space(6.0);
                ui.colored_label(c.error, e);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Open browser again").clicked() {
                    act = Some(Action::OAuthOpenAgain);
                }
                if ui.button("Cancel").clicked() {
                    act = Some(Action::OAuthCancel);
                }
            });
            ui.separator();
            ui.label(RichText::new("Browser could not return to the app?").strong());
            ui.label(
                RichText::new(
                    "After Allow access, copy the URL from the browser (it contains code=…) or just \
                     the code, and paste it here.",
                )
                .small()
                .color(c.secondary),
            );
            let edit = ui.add(
                TextEdit::singleline(&mut flow.manual)
                    .hint_text("http://…/callback?code=… or the code")
                    .desired_width(f32::INFINITY),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                act = Some(Action::OAuthManual);
            }
            ui.horizontal(|ui| {
                let b = ui.add_enabled(!flow.busy, egui::Button::new("Complete sign-in"));
                if b.clicked() {
                    act = Some(Action::OAuthManual);
                }
                if flow.busy {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
            ui.label(
                RichText::new(format!("Redirect URI: {}", flow.app.redirect_uri))
                    .small()
                    .color(c.secondary),
            );
        });
    act
}
