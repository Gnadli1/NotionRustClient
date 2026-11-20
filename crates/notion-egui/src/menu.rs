//! The `/` menu's key handling as a pure function, so it is testable without a
//! window. `slash::menu_key` decides what each key means; this applies it.

use notion_core::model::TargetKind;

use crate::edit::EditKey;
use crate::slash::{self, Menu, MenuKey};

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Eaten by the menu (highlight moved, or the key is ignored).
    Handled,
    /// Let the text edit have the key; the menu re-checks its query after.
    Pass,
    Pick(TargetKind),
    Close,
}

/// `query` is the text after the `/`. Up/Down wrap around.
pub fn menu_step(menu: &mut Menu, query: &str, key: EditKey) -> Outcome {
    let items = slash::filter(query);
    let n = items.len();
    let sel = menu.selected.min(n.saturating_sub(1));
    match slash::menu_key(key) {
        MenuKey::Prev if n > 0 => {
            menu.selected = (sel + n - 1) % n;
            Outcome::Handled
        }
        MenuKey::Next if n > 0 => {
            menu.selected = (sel + 1) % n;
            Outcome::Handled
        }
        MenuKey::Prev | MenuKey::Next | MenuKey::Ignore => Outcome::Handled,
        MenuKey::Pick => items
            .get(sel)
            .map_or(Outcome::Close, |i| Outcome::Pick(i.kind)),
        MenuKey::Close => Outcome::Close,
        MenuKey::Editor => Outcome::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        Menu {
            line: 0,
            slash: 0,
            selected: 0,
        }
    }

    #[test]
    fn enter_picks_the_filtered_item() {
        let mut m = menu();
        assert_eq!(
            menu_step(&mut m, "h2", EditKey::Enter),
            Outcome::Pick(TargetKind::Heading2)
        );
        assert_eq!(
            menu_step(&mut m, "", EditKey::Enter),
            Outcome::Pick(TargetKind::Paragraph),
            "empty query: first item"
        );
    }

    #[test]
    fn arrows_move_and_wrap() {
        let mut m = menu();
        // "head" matches Heading 1..3
        assert_eq!(menu_step(&mut m, "head", EditKey::Down), Outcome::Handled);
        assert_eq!(m.selected, 1);
        menu_step(&mut m, "head", EditKey::Down);
        menu_step(&mut m, "head", EditKey::Down);
        assert_eq!(m.selected, 0, "wraps past the last item");
        menu_step(&mut m, "head", EditKey::Up);
        assert_eq!(m.selected, 2, "wraps before the first item");
        assert_eq!(
            menu_step(&mut m, "head", EditKey::Enter),
            Outcome::Pick(TargetKind::Heading3)
        );
    }

    #[test]
    fn stale_selection_is_clamped_to_the_filtered_list() {
        let mut m = Menu {
            selected: 9,
            ..menu()
        };
        assert_eq!(
            menu_step(&mut m, "h1", EditKey::Enter),
            Outcome::Pick(TargetKind::Heading1)
        );
    }

    #[test]
    fn esc_closes_tab_is_ignored_backspace_goes_to_the_editor() {
        let mut m = menu();
        assert_eq!(menu_step(&mut m, "x", EditKey::Escape), Outcome::Close);
        assert_eq!(menu_step(&mut m, "x", EditKey::Indent), Outcome::Handled);
        assert_eq!(menu_step(&mut m, "x", EditKey::Backspace), Outcome::Pass);
    }

    #[test]
    fn no_match_closes_on_enter_and_arrows_do_nothing() {
        let mut m = menu();
        assert_eq!(menu_step(&mut m, "zzz", EditKey::Down), Outcome::Handled);
        assert_eq!(menu_step(&mut m, "zzz", EditKey::Enter), Outcome::Close);
    }

    #[test]
    fn backspace_past_the_slash_closes() {
        // The menu's query goes None once the cursor is at or before the `/`;
        // app.rs closes the menu then (checked here at the slash.rs level).
        let m = menu();
        assert_eq!(m.query("/h", 0, 2), Some("h"));
        assert_eq!(m.query("h", 0, 0), None);
        assert_eq!(m.query("", 0, 0), None);
    }
}
