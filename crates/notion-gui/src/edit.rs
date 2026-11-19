//! Editing the focused row: which keys the app takes over from the
//! text_editor, and what they do. Plain functions, testable without a window.

use iced::keyboard::{key::Named, Key, Modifiers};
use iced::widget::text_editor::{self, Action, Binding, Edit, KeyPress, Motion, Status};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, RichText};

use crate::app::Message;
use crate::outline::{Marker, Row};

/// Widget id of the (single) row editor.
pub const EDITOR_ID: &str = "row-editor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Up,
    Down,
    Enter,
    Backspace,
    Escape,
    /// Tab
    Indent,
    /// Shift+Tab
    Outdent,
    /// Ctrl+Enter
    ToggleCheck,
}

#[derive(Debug, PartialEq)]
pub enum Route {
    App(EditKey),
    /// Eaten (Alt+Enter, Ctrl+Tab): no stray newline or tab character.
    Swallow,
    /// text_editor's default binding.
    Default,
}

pub fn route(key: &Key, m: Modifiers) -> Route {
    let Key::Named(named) = key.as_ref() else {
        return Route::Default;
    };
    let plain = m.is_empty();
    match named {
        Named::Tab if m.command() || m.alt() => Route::Swallow,
        Named::Tab if m.shift() => Route::App(EditKey::Outdent),
        Named::Tab => Route::App(EditKey::Indent),
        Named::Enter if m.alt() => Route::Swallow,
        Named::Enter if m.command() => Route::App(EditKey::ToggleCheck),
        // Shift+Enter: the default binding inserts a newline
        Named::Enter if m.shift() => Route::Default,
        Named::Enter => Route::App(EditKey::Enter),
        Named::Escape => Route::App(EditKey::Escape),
        Named::ArrowUp if plain => Route::App(EditKey::Up),
        Named::ArrowDown if plain => Route::App(EditKey::Down),
        Named::Backspace if plain => Route::App(EditKey::Backspace),
        _ => Route::Default,
    }
}

/// `text_editor::key_binding` for the row editor.
pub fn binding(kp: KeyPress) -> Option<Binding<Message>> {
    if !matches!(kp.status, Status::Focused { .. }) {
        return None;
    }
    match route(&kp.key, kp.modifiers) {
        Route::App(k) => Some(Binding::Custom(Message::EditKey(k))),
        Route::Swallow => None,
        Route::Default => Binding::from_key_press(kp),
    }
}

/// Where the cursor is: logical line (not wrapped visual line) and count.
#[derive(Debug, Clone, Copy)]
pub struct Caret {
    pub line: usize,
    pub lines: usize,
    pub empty: bool,
}

#[derive(Debug, PartialEq)]
pub enum KeyAction {
    /// Let the editor do it.
    Pass(Action),
    FocusPrev,
    FocusNext,
    /// Commit, add an empty paragraph after the row, focus it.
    NewBlockAfter,
    DeleteRow,
    /// Commit and stop editing.
    Leave,
    Indent,
    Outdent,
    ToggleCheck,
}

pub fn key_action(k: EditKey, c: Caret) -> KeyAction {
    match k {
        EditKey::Up if c.line == 0 => KeyAction::FocusPrev,
        EditKey::Up => KeyAction::Pass(Action::Move(Motion::Up)),
        EditKey::Down if c.line + 1 >= c.lines => KeyAction::FocusNext,
        EditKey::Down => KeyAction::Pass(Action::Move(Motion::Down)),
        EditKey::Enter => KeyAction::NewBlockAfter,
        EditKey::Backspace if c.empty => KeyAction::DeleteRow,
        EditKey::Backspace => KeyAction::Pass(Action::Edit(Edit::Backspace)),
        EditKey::Escape => KeyAction::Leave,
        EditKey::Indent => KeyAction::Indent,
        EditKey::Outdent => KeyAction::Outdent,
        EditKey::ToggleCheck => KeyAction::ToggleCheck,
    }
}

/// The focused row: its editor buffer and the source it started from.
pub struct Editing {
    pub block_id: String,
    /// Code rows are edited as raw text, not Markdown.
    pub code: bool,
    pub original: String,
    pub content: text_editor::Content,
}

impl Editing {
    pub fn new(row: &Row) -> Self {
        let code = matches!(row.marker, Marker::Code(_));
        let original = if code {
            plain_text(&row.rich_text)
        } else {
            to_markdown(&row.rich_text)
        };
        let mut content = text_editor::Content::with_text(&original);
        content.perform(Action::Move(Motion::DocumentEnd));
        Editing {
            block_id: row.block_id.clone(),
            code,
            content,
            original,
        }
    }

    pub fn caret(&self) -> Caret {
        Caret {
            line: self.content.cursor().position.line,
            lines: self.content.line_count().max(1),
            empty: self.content.text().is_empty(),
        }
    }

    /// Rich text to store, or None if the text is unchanged.
    pub fn changed(&self) -> Option<Vec<RichText>> {
        let t = self.content.text();
        if t == self.original {
            None
        } else if self.code {
            Some(if t.is_empty() {
                Vec::new()
            } else {
                vec![RichText::plain(&t)]
            })
        } else {
            Some(from_markdown(&t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caret(line: usize, lines: usize, empty: bool) -> Caret {
        Caret { line, lines, empty }
    }

    #[test]
    fn routes_keys() {
        let none = Modifiers::empty();
        let k = Key::Named;
        assert_eq!(route(&k(Named::Enter), none), Route::App(EditKey::Enter));
        assert_eq!(route(&k(Named::Enter), Modifiers::SHIFT), Route::Default);
        assert_eq!(
            route(&k(Named::Enter), Modifiers::CTRL),
            Route::App(EditKey::ToggleCheck)
        );
        assert_eq!(route(&k(Named::Enter), Modifiers::ALT), Route::Swallow);
        assert_eq!(route(&k(Named::Tab), none), Route::App(EditKey::Indent));
        assert_eq!(
            route(&k(Named::Tab), Modifiers::SHIFT),
            Route::App(EditKey::Outdent)
        );
        assert_eq!(route(&k(Named::Tab), Modifiers::CTRL), Route::Swallow);
        assert_eq!(route(&k(Named::ArrowUp), none), Route::App(EditKey::Up));
        assert_eq!(
            route(&k(Named::ArrowUp), Modifiers::SHIFT),
            Route::Default,
            "select"
        );
        assert_eq!(route(&k(Named::Backspace), Modifiers::CTRL), Route::Default);
        assert_eq!(route(&Key::Character("a".into()), none), Route::Default);
    }

    #[test]
    fn key_actions() {
        use KeyAction::*;
        assert_eq!(
            key_action(EditKey::Enter, caret(0, 1, false)),
            NewBlockAfter
        );
        assert_eq!(key_action(EditKey::Up, caret(0, 3, false)), FocusPrev);
        assert_eq!(
            key_action(EditKey::Up, caret(1, 3, false)),
            Pass(Action::Move(Motion::Up))
        );
        assert_eq!(key_action(EditKey::Down, caret(2, 3, false)), FocusNext);
        assert_eq!(
            key_action(EditKey::Down, caret(1, 3, false)),
            Pass(Action::Move(Motion::Down))
        );
        assert_eq!(key_action(EditKey::Backspace, caret(0, 1, true)), DeleteRow);
        assert_eq!(
            key_action(EditKey::Backspace, caret(0, 1, false)),
            Pass(Action::Edit(Edit::Backspace))
        );
        assert_eq!(key_action(EditKey::Escape, caret(0, 1, false)), Leave);
    }
}
