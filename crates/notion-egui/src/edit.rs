//! The keys the row editor takes over from the text edit, as pure functions
//! (ported from notion-gui's edit.rs): the key names `slash.rs` routes, and
//! what each does to the focused row.

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

/// Where the cursor is: logical line (not wrapped visual line) and count.
#[derive(Debug, Clone, Copy)]
pub struct Caret {
    pub line: usize,
    pub lines: usize,
    pub empty: bool,
}

impl Caret {
    pub fn new(text: &str, line: usize) -> Caret {
        Caret {
            line,
            lines: text.split('\n').count(),
            empty: text.is_empty(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Let the text edit do it.
    Pass,
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
        EditKey::Down if c.line + 1 >= c.lines => KeyAction::FocusNext,
        EditKey::Up | EditKey::Down => KeyAction::Pass,
        EditKey::Enter => KeyAction::NewBlockAfter,
        EditKey::Backspace if c.empty => KeyAction::DeleteRow,
        EditKey::Backspace => KeyAction::Pass,
        EditKey::Escape => KeyAction::Leave,
        EditKey::Indent => KeyAction::Indent,
        EditKey::Outdent => KeyAction::Outdent,
        EditKey::ToggleCheck => KeyAction::ToggleCheck,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caret(line: usize, lines: usize, empty: bool) -> Caret {
        Caret { line, lines, empty }
    }

    #[test]
    fn key_actions() {
        use KeyAction::*;
        assert_eq!(
            key_action(EditKey::Enter, caret(0, 1, false)),
            NewBlockAfter
        );
        assert_eq!(key_action(EditKey::Up, caret(0, 3, false)), FocusPrev);
        assert_eq!(key_action(EditKey::Up, caret(1, 3, false)), Pass);
        assert_eq!(key_action(EditKey::Down, caret(2, 3, false)), FocusNext);
        assert_eq!(key_action(EditKey::Down, caret(1, 3, false)), Pass);
        assert_eq!(key_action(EditKey::Backspace, caret(0, 1, true)), DeleteRow);
        assert_eq!(key_action(EditKey::Backspace, caret(0, 1, false)), Pass);
        assert_eq!(key_action(EditKey::Escape, caret(0, 1, false)), Leave);
        assert_eq!(key_action(EditKey::Indent, caret(0, 1, false)), Indent);
        assert_eq!(key_action(EditKey::Outdent, caret(0, 1, false)), Outdent);
        assert_eq!(
            key_action(EditKey::ToggleCheck, caret(0, 1, false)),
            ToggleCheck
        );
    }

    #[test]
    fn caret_counts_logical_lines() {
        let c = Caret::new("a\nb\n", 1);
        assert_eq!((c.line, c.lines, c.empty), (1, 3, false));
        assert!(Caret::new("", 0).empty);
    }
}
