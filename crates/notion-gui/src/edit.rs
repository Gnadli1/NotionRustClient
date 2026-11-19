//! Editing the focused row: which keys the app takes over from the
//! text_editor, and what they do. Plain functions, testable without a window.

use iced::keyboard::{key::Named, Key, Modifiers};
use iced::widget::text_editor::{self, Action, Binding, Edit, KeyPress, Motion, Status};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, RichText};

use crate::app::Message;
use crate::keys::{Chord, Command, Keymap};
use crate::outline::{Marker, Row};
use crate::slash::{filter, Menu};

/// Widget id of the (single) row editor.
pub const EDITOR_ID: &str = "row-editor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Up,
    Down,
    Enter,
    Backspace,
    Escape,
    /// keymap `indent` (Tab)
    Indent,
    /// keymap `outdent` (Shift+Tab)
    Outdent,
    /// keymap `toggle_check` (Ctrl+Enter)
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

/// The editor's own keys, after the keymap had its say: Tab and modified
/// Enter never type a character.
pub fn route(key: &Key, m: Modifiers) -> Route {
    let Key::Named(named) = key.as_ref() else {
        return Route::Default;
    };
    let plain = m.is_empty();
    match named {
        Named::Tab => Route::Swallow,
        Named::Enter if m.alt() || m.command() => Route::Swallow,
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

/// `text_editor::key_binding` for the row editor: keymap commands first
/// (global ones are left to the window listener), then [`route`].
pub fn binding(kp: KeyPress, keys: &Keymap) -> Option<Binding<Message>> {
    if !matches!(kp.status, Status::Focused { .. }) {
        return None;
    }
    let cmd = Chord::of(&kp.key, kp.physical_key, kp.modifiers).and_then(|c| keys.get(&c));
    if let Some(cmd) = cmd {
        let msg = match cmd {
            c if c.is_global() => return None,
            Command::Indent => Message::EditKey(EditKey::Indent),
            Command::Outdent => Message::EditKey(EditKey::Outdent),
            Command::ToggleCheck => Message::EditKey(EditKey::ToggleCheck),
            c => Message::Command(c),
        };
        return Some(Binding::Custom(msg));
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
    /// Code and equation rows are edited as raw text, not Markdown.
    pub code: bool,
    /// An equation block: the buffer is its TeX expression.
    pub equation: bool,
    pub original: String,
    pub content: text_editor::Content,
    /// The open `/` menu.
    pub menu: Option<Menu>,
    /// A table cell (table_row id, column); `block_id` is then the table's.
    pub cell: Option<(String, usize)>,
}

impl Editing {
    pub fn new(row: &Row) -> Self {
        let equation = row.marker == Marker::Equation;
        let code = equation || matches!(row.marker, Marker::Code(_));
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
            equation,
            content,
            original,
            menu: None,
            cell: None,
        }
    }

    /// Cell `col` of table row `row_id` in table `table_id`, as Markdown.
    pub fn cell(table_id: &str, row_id: &str, col: usize, rich_text: &[RichText]) -> Self {
        let original = to_markdown(rich_text);
        let mut content = text_editor::Content::with_text(&original);
        content.perform(Action::Move(Motion::DocumentEnd));
        Editing {
            block_id: table_id.into(),
            code: false,
            equation: false,
            content,
            original,
            menu: None,
            cell: Some((row_id.into(), col)),
        }
    }

    /// Replaces the buffer and puts the cursor at `line`/`col` (bytes).
    pub fn set_buffer(&mut self, text: &str, line: usize, col: usize) {
        self.content = text_editor::Content::with_text(text);
        self.content.move_to(text_editor::Cursor {
            position: text_editor::Position { line, column: col },
            selection: None,
        });
    }

    /// Cursor (line, byte column).
    pub fn pos(&self) -> (usize, usize) {
        let p = self.content.cursor().position;
        (p.line, p.column)
    }

    /// The `/` menu's query, closing the menu when the cursor left it or
    /// nothing matches.
    pub fn menu_query(&mut self) -> Option<String> {
        let m = self.menu.as_mut()?;
        let (line, col) = (
            self.content.cursor().position.line,
            self.content.cursor().position.column,
        );
        let q = m.query(&self.content.text(), line, col).map(str::to_string);
        let n = q.as_deref().map_or(0, |q| filter(q).len());
        if n == 0 {
            self.menu = None;
            return None;
        }
        m.selected = m.selected.min(n - 1);
        q
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
        // Ctrl+Enter / Tab / Shift+Tab come from the keymap; unbound, they're eaten
        assert_eq!(route(&k(Named::Enter), Modifiers::CTRL), Route::Swallow);
        assert_eq!(route(&k(Named::Enter), Modifiers::ALT), Route::Swallow);
        assert_eq!(route(&k(Named::Tab), none), Route::Swallow);
        assert_eq!(route(&k(Named::Tab), Modifiers::SHIFT), Route::Swallow);
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
