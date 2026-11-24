//! The AI panel (Ctrl+Shift+H): ask about the open page, stream the answer,
//! insert it into the page.

use std::sync::mpsc;

use eframe::egui::{self, Frame, Margin};

use crate::ai::{AiEvent, Backend, Secrets};
use crate::app::App;
use crate::theme;

const SYSTEM: &str = "You are an assistant inside a Notion client. Answer in GitHub-flavoured \
Markdown. The user's open page follows.";

/// The panel's state while it's open.
#[derive(Default)]
pub struct AiPanel {
    pub prompt: String,
    pub answer: String,
    pub job: Option<crate::ai::Job>,
    pub error: Option<String>,
}

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

/// The right-hand panel, inside `ui` (the frame's root), when `app.ai` is Some.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let c = theme::colors(ui);
    let (mut send, mut stop, mut insert, mut close) = (false, false, false, false);
    let backend = match app.settings.ai.backend {
        Backend::ClaudeCli => "Claude (subscription)",
        Backend::Anthropic => "Anthropic API",
        Backend::OpenAiCompatible => "OpenAI-compatible",
    };
    let Some(p) = app.ai.as_mut() else { return };
    if let Some(j) = p.job.take() {
        if !drain(p, &j.rx) {
            p.job = Some(j);
        }
    }
    let side = Frame::new().fill(c.sidebar).inner_margin(Margin::same(8));
    egui::Panel::right("ai")
        .frame(side)
        .resizable(true)
        .size_range(300.0..=600.0)
        .default_size(380.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(backend);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = ui.small_button("×").clicked();
                });
            });
            let r = ui.add(
                egui::TextEdit::multiline(&mut p.prompt)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("Ask about this page"),
            );
            let ctrl_enter = r.has_focus()
                && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if p.job.is_some() {
                    stop = ui.button("Stop").clicked();
                } else {
                    send = (ui.button("Ask").clicked() || ctrl_enter) && !p.prompt.trim().is_empty();
                }
                if !p.answer.is_empty() {
                    insert = ui.button("Insert into page").clicked();
                    if ui.button("Copy").clicked() {
                        ui.copy_text(p.answer.clone());
                    }
                }
            });
            if let Some(e) = &p.error {
                ui.colored_label(c.error, e);
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                // ponytail: plain selectable text, not rendered Markdown
                ui.add(egui::Label::new(&p.answer).selectable(true));
            });
        });
    if close {
        app.ai = None;
    } else if stop {
        if let Some(j) = p.job.take() {
            j.cancel();
        }
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
    let prompt = format!("{doc}\n\n---\n\n{}", p.prompt.trim());
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
        w.app.ai = Some(AiPanel { prompt: "q".into(), answer: "old".into(), ..Default::default() });
        start(&mut w.app, &egui::Context::default());
        let p = w.app.ai.as_ref().unwrap();
        assert!(p.job.is_none());
        assert!(p.answer.is_empty());
        assert!(p.error.as_deref().unwrap().contains("add an API key"));
    }
}
