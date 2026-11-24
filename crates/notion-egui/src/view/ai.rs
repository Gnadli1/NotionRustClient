//! The AI panel (Ctrl+Shift+H): ask about the open page, stream the answer,
//! insert it into the page.

use std::sync::mpsc;

use eframe::egui::{
    self, Align, Align2, FontId, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText,
    Sense, Vec2,
};

use crate::ai::{AiEvent, Backend, Secrets};
use crate::app::App;
use crate::icons::{self, Icon};
use crate::theme;

const SYSTEM: &str = "You are an assistant inside a Notion client. Answer in GitHub-flavoured \
Markdown. The user's open page follows.";

/// The panel's state while it's open.
#[derive(Default)]
pub struct AiPanel {
    pub prompt: String,
    /// The question the answer belongs to.
    pub asked: String,
    pub answer: String,
    pub job: Option<crate::ai::Job>,
    pub error: Option<String>,
}

/// The empty chat's one-click prompts: (icon, label, prompt).
const SUGGESTIONS: [(Icon, &str, &str); 4] = [
    (Icon::Page, "Summarize this page", "Summarize this page."),
    (
        Icon::Comment,
        "Translate this page",
        "Translate this page into English (into German if it is already English).",
    ),
    (
        Icon::Search,
        "Analyze for insights",
        "Analyze this page and list the key insights.",
    ),
    (
        Icon::Check,
        "Create a task list",
        "Turn this page into a Markdown to-do list (- [ ] items).",
    ),
];

/// Moves everything waiting in `rx` into the panel; true once the job is over.
fn drain(p: &mut AiPanel, rx: &mpsc::Receiver<AiEvent>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(AiEvent::Text(t)) => p.answer.push_str(&t),
            Ok(AiEvent::Done) => return true,
            Ok(AiEvent::Error(e)) => {
                p.error = Some(e);
                return true;
            }
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                p.error = Some("the AI request stopped unexpectedly".into());
                return true;
            }
        }
    }
}

/// A round avatar with the sparkle, like Notion AI's face.
fn avatar(ui: &mut egui::Ui, c: &theme::Colors, size: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().circle(
        r.center(),
        size / 2.0,
        c.text.gamma_multiply(0.85),
        (1.0, c.border),
    );
    icons::paint(ui, r.shrink(size * 0.24), Icon::Sparkle, c.bg);
}

/// The model the answer will come from, for the input box's corner.
fn model_label(cfg: &crate::ai::AiConfig) -> String {
    match (cfg.backend, cfg.model.trim()) {
        (Backend::ClaudeCli, "") => "Claude".into(),
        (Backend::Anthropic, "") => "claude-opus-5-5".into(),
        (Backend::OpenAiCompatible, "") => "no model set".into(),
        (_, m) => m.into(),
    }
}

/// The right-hand panel, inside `ui` (the frame's root), when `app.ai` is Some.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let c = theme::colors(ui);
    let (mut send, mut stop, mut insert, mut close, mut fresh, mut setup) =
        (false, false, false, false, false, false);
    let model = model_label(&app.settings.ai);
    let Some(p) = app.ai.as_mut() else { return };
    if let Some(j) = p.job.take() {
        if !drain(p, &j.rx) {
            p.job = Some(j);
        }
    }
    let side = Frame::new()
        .fill(c.bg)
        .stroke((1.0, c.border))
        .inner_margin(Margin::symmetric(14, 10));
    egui::Panel::right("ai")
        .frame(side)
        .resizable(true)
        .size_range(320.0..=640.0)
        .default_size(400.0)
        .show(ui, |ui| {
            // header: avatar, title; new chat, AI settings, close
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                avatar(ui, c, 26.0);
                let title = if p.asked.is_empty() {
                    "New AI chat"
                } else {
                    p.asked.as_str()
                };
                let w = (ui.available_width() - 96.0).max(40.0);
                ui.allocate_ui(Vec2::new(w, 28.0), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(title).size(15.0).color(c.text)).truncate(),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    close = icons::button(
                        ui,
                        28.0,
                        Icon::ChevronRight,
                        c.secondary,
                        c.hover,
                        Some("Close"),
                    )
                    .clicked();
                    setup = icons::button(
                        ui,
                        28.0,
                        Icon::Dots,
                        c.secondary,
                        c.hover,
                        Some("AI settings"),
                    )
                    .clicked();
                    fresh = icons::button(
                        ui,
                        28.0,
                        Icon::Compose,
                        c.secondary,
                        c.hover,
                        Some("New chat"),
                    )
                    .clicked();
                });
            });
            ui.add_space(6.0);
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                // the input card
                Frame::new()
                    .fill(c.sidebar)
                    .stroke((1.0, c.border))
                    .corner_radius(14)
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // the panel is bottom-up here: the button row first, then the text
                        let busy = p.job.is_some();
                        let mut clicked = false;
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Enter sends · Shift+Enter new line")
                                    .size(11.5)
                                    .color(c.secondary),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ready = busy || !p.prompt.trim().is_empty();
                                let (b, resp) =
                                    ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                                let fill = if ready {
                                    c.accent
                                } else {
                                    c.hover.gamma_multiply(2.0)
                                };
                                ui.painter().circle_filled(b.center(), 14.0, fill);
                                if busy {
                                    ui.painter().rect_filled(
                                        egui::Rect::from_center_size(b.center(), Vec2::splat(9.0)),
                                        2.0,
                                        egui::Color32::WHITE,
                                    );
                                    stop = resp.on_hover_text("Stop").clicked();
                                } else {
                                    icons::paint(ui, b.shrink(7.0), Icon::ArrowUp, egui::Color32::WHITE);
                                    clicked = resp.on_hover_text("Send").clicked();
                                }
                                ui.label(RichText::new(&model).size(12.5).color(c.secondary));
                            });
                        });
                        let r = ui.add(
                            egui::TextEdit::multiline(&mut p.prompt)
                                .frame(Frame::NONE)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .font(FontId::proportional(14.5))
                                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                                .hint_text("Ask AI about this page…"),
                        );
                        let enter = (r.has_focus() || r.lost_focus())
                            && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
                        if enter {
                            r.request_focus();
                        }
                        send = !busy && (clicked || enter) && !p.prompt.trim().is_empty();
                    });
                ui.add_space(8.0);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    if p.asked.is_empty() && p.answer.is_empty() && p.error.is_none() {
                        // empty chat: greeting and suggestions, sitting on the input
                        ui.add_space((ui.available_height() - 260.0).max(0.0));
                        avatar(ui, c, 52.0);
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("What can I do for you today?")
                                .size(19.0)
                                .strong()
                                .color(c.text),
                        );
                        ui.add_space(8.0);
                        for (icon, label, prompt) in SUGGESTIONS {
                            let (r, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 34.0),
                                Sense::click(),
                            );
                            if resp.hovered() {
                                ui.painter().rect_filled(r, 6.0, c.hover);
                            }
                            let ir = egui::Rect::from_center_size(
                                egui::pos2(r.left() + 14.0, r.center().y),
                                Vec2::splat(18.0),
                            );
                            icons::paint(ui, ir, icon, c.text);
                            ui.painter().text(
                                egui::pos2(r.left() + 34.0, r.center().y),
                                Align2::LEFT_CENTER,
                                label,
                                FontId::proportional(14.5),
                                c.text,
                            );
                            if resp.clicked() && p.job.is_none() {
                                p.prompt = prompt.into();
                                send = true;
                            }
                        }
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if !p.asked.is_empty() {
                                // the question: a bubble on the right
                                ui.with_layout(Layout::top_down(Align::Max), |ui| {
                                    Frame::new()
                                        .fill(c.hover.gamma_multiply(1.6))
                                        .corner_radius(12)
                                        .inner_margin(Margin::symmetric(12, 8))
                                        .show(ui, |ui| {
                                            ui.set_max_width(ui.available_width() * 0.85);
                                            ui.label(
                                                RichText::new(&p.asked).size(14.0).color(c.text),
                                            );
                                        });
                                });
                                ui.add_space(12.0);
                            }
                            ui.horizontal(|ui| {
                                avatar(ui, c, 22.0);
                                let what = if p.job.is_some() { "Thinking…" } else { "AI" };
                                ui.label(RichText::new(what).size(13.0).color(c.secondary));
                            });
                            ui.add_space(4.0);
                            // ponytail: plain selectable text, not rendered Markdown
                            ui.add(
                                egui::Label::new(RichText::new(&p.answer).size(14.0).color(c.text))
                                    .selectable(true),
                            );
                            if let Some(e) = &p.error {
                                ui.add_space(6.0);
                                ui.colored_label(c.error, e);
                            }
                            if !p.answer.is_empty() && p.job.is_none() {
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    insert = ui.button("Insert into page").clicked();
                                    if ui.button("Copy").clicked() {
                                        ui.copy_text(p.answer.clone());
                                    }
                                });
                            }
                        });
                });
            });
        });
    if close {
        app.ai = None;
    } else if stop {
        if let Some(j) = p.job.take() {
            j.cancel();
        }
    } else if fresh {
        app.ai = Some(AiPanel::default()); // dropping the old job cancels it
    } else if setup {
        let mut st = crate::view::settings::SettingsUi::default();
        st.tab = 2;
        app.settings_ui = Some(st);
    } else if insert {
        let md = p.answer.clone();
        app.insert_markdown(&md);
    } else if send {
        start(app, ui.ctx());
    }
}

fn start(app: &mut App, ctx: &egui::Context) {
    let cfg = app.settings.ai.clone();
    let key = Secrets::slot(&cfg).and_then(|s| app.ai_secrets.keys.get(&s).cloned());
    let doc = format!("Page: {}\n\n{}", app.title, app.page_markdown());
    let Some(p) = app.ai.as_mut() else { return };
    p.error = None;
    p.answer.clear();
    if cfg.backend == Backend::Anthropic && key.is_none() {
        p.error = Some("add an API key in Settings → AI Agent".into());
        return;
    }
    p.asked = p.prompt.trim().to_string();
    p.prompt.clear();
    let prompt = format!("{doc}\n\n---\n\n{}", p.asked);
    p.job = Some(crate::ai::start(&cfg, key, SYSTEM, &prompt, ctx.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_appends_and_ends() {
        let (tx, rx) = mpsc::channel();
        let mut p = AiPanel::default();
        tx.send(AiEvent::Text("a".into())).unwrap();
        assert!(!drain(&mut p, &rx));
        tx.send(AiEvent::Text("b".into())).unwrap();
        tx.send(AiEvent::Done).unwrap();
        assert!(drain(&mut p, &rx));
        assert_eq!(p.answer, "ab");
        tx.send(AiEvent::Error("x".into())).unwrap();
        assert!(drain(&mut p, &rx));
        assert_eq!(p.error.as_deref(), Some("x"));
    }

    #[test]
    fn page_markdown_and_insert() {
        let mut w = crate::testkit::Win::with_texts(&["one", "two"]);
        assert_eq!(w.app.page_markdown(), "one\ntwo");
        w.app.insert_markdown("# A\n- b");
        let md = w.app.page_markdown();
        assert!(md.ends_with("# A\n- b"), "{md}");
        assert_eq!(w.app.info.as_deref(), Some("inserted"));
    }

    #[test]
    fn drain_text_after_done_is_left_queued() {
        let (tx, rx) = mpsc::channel();
        let mut p = AiPanel::default();
        tx.send(AiEvent::Text("a".into())).unwrap();
        tx.send(AiEvent::Done).unwrap();
        tx.send(AiEvent::Text("late".into())).unwrap();
        assert!(drain(&mut p, &rx));
        assert_eq!(p.answer, "a");
        assert!(p.error.is_none());
    }

    #[test]
    fn anthropic_without_key_errors_and_starts_nothing() {
        let mut w = crate::testkit::Win::with_texts(&["one"]);
        w.app.settings.ai.backend = Backend::Anthropic;
        w.app.ai = Some(AiPanel {
            prompt: "q".into(),
            answer: "old".into(),
            ..Default::default()
        });
        start(&mut w.app, &egui::Context::default());
        let p = w.app.ai.as_ref().unwrap();
        assert!(p.job.is_none());
        assert!(p.answer.is_empty());
        assert!(p.error.as_deref().unwrap().contains("add an API key"));
    }
}
