//! The right-click menu on the text being edited, like Notion's: turn into,
//! inline marks, comment, and AI actions on the selection.

use eframe::egui::{self, vec2, Button, RichText, Ui};

use crate::icons::Icon;
use crate::keys::{Command, Mark};
use crate::theme::Colors;

pub enum Pick {
    /// Runs as if its key was pressed (keeps the editor and its selection).
    Cmd(Command),
    Comment,
    /// An instruction for the AI panel; `true`: send it at once.
    Ai(&'static str, bool),
}

/// One-click AI actions on the selection: (label, instruction).
const SKILLS: [(&str, &str); 4] = [
    ("Improve writing", "Improve the writing of this text, keep its language and meaning"),
    ("Proofread", "Fix spelling and grammar in this text, change nothing else"),
    ("Explain", "Explain this text simply"),
    ("Reformat", "Reformat this text as clean, well-structured Markdown"),
];

/// `row`: a block, not a table cell; `code`: raw text (no marks).
pub fn show(ui: &mut Ui, c: &Colors, row: bool, code: bool) -> Option<Pick> {
    let mut pick = None;
    ui.set_width(240.0);
    let item = |ui: &mut Ui, icon: Icon, label: &str| {
        let r = ui.add(
            Button::new(RichText::new(format!("      {label}")).size(14.0))
                .frame_when_inactive(false)
                .min_size(vec2(ui.available_width(), 28.0)),
        );
        let g = egui::Rect::from_center_size(
            egui::pos2(r.rect.left() + 16.0, r.rect.center().y),
            vec2(16.0, 16.0),
        );
        crate::icons::paint(ui, g, icon, c.secondary);
        r.clicked()
    };
    if row && !code && item(ui, Icon::Table, "Turn into") {
        pick = Some(Pick::Cmd(Command::TurnInto));
    }
    if !code {
        ui.separator();
        ui.horizontal(|ui| {
            for (text, mark, tip) in [
                (RichText::new("B").strong(), Mark::Bold, "Bold"),
                (RichText::new("I").italics(), Mark::Italic, "Italic"),
                (RichText::new("S").strikethrough(), Mark::Strike, "Strikethrough"),
                (RichText::new("</>").monospace(), Mark::Code, "Code"),
                (RichText::new("√x"), Mark::Equation, "Equation"),
                (RichText::new("🔗"), Mark::Link, "Link"),
            ] {
                let b = Button::new(text.size(15.0))
                    .frame_when_inactive(false)
                    .min_size(vec2(32.0, 30.0));
                if ui.add(b).on_hover_text(tip).clicked() {
                    pick = Some(Pick::Cmd(Command::Mark(mark)));
                }
            }
        });
    }
    if row {
        ui.separator();
        if item(ui, Icon::Comment, "Comment") {
            pick = Some(Pick::Comment);
        }
    }
    ui.separator();
    ui.label(RichText::new("AI").size(12.0).color(c.secondary));
    for (label, instr) in SKILLS {
        if item(ui, Icon::Sparkle, label) {
            pick = Some(Pick::Ai(instr, true));
        }
    }
    if item(ui, Icon::Sparkle, "Edit with AI…") {
        pick = Some(Pick::Ai("", false));
    }
    if pick.is_some() {
        ui.close();
    }
    pick
}
