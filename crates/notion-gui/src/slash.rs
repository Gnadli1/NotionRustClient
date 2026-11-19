//! The `/` block-type menu and Markdown shortcuts. Plain functions over the
//! editor text; app.rs wires them to the editor.

use notion_core::markdown::markdown_shortcut;
use notion_core::model::TargetKind;

use crate::edit::EditKey;

pub struct Item {
    pub kind: TargetKind,
    pub name: &'static str,
    pub hint: &'static str,
    aliases: &'static [&'static str],
}

pub const ITEMS: &[Item] = &[
    Item {
        kind: TargetKind::Paragraph,
        name: "Text",
        hint: "Plain text",
        aliases: &["paragraph", "p", "plain"],
    },
    Item {
        kind: TargetKind::Heading1,
        name: "Heading 1",
        hint: "Big section heading",
        aliases: &["h1", "#"],
    },
    Item {
        kind: TargetKind::Heading2,
        name: "Heading 2",
        hint: "Medium section heading",
        aliases: &["h2", "##"],
    },
    Item {
        kind: TargetKind::Heading3,
        name: "Heading 3",
        hint: "Small section heading",
        aliases: &["h3", "###"],
    },
    Item {
        kind: TargetKind::Bulleted,
        name: "Bulleted list",
        hint: "Simple bulleted list",
        aliases: &["bullet", "ul", "-"],
    },
    Item {
        kind: TargetKind::Numbered,
        name: "Numbered list",
        hint: "List with numbering",
        aliases: &["number", "ol", "1."],
    },
    Item {
        kind: TargetKind::ToDo,
        name: "To-do list",
        hint: "Track tasks with a checkbox",
        aliases: &["todo", "checkbox", "[]"],
    },
    Item {
        kind: TargetKind::Toggle,
        name: "Toggle list",
        hint: "Hide content inside",
        aliases: &["toggle", ">"],
    },
    Item {
        kind: TargetKind::Quote,
        name: "Quote",
        hint: "Capture a quote",
        aliases: &["blockquote", "\""],
    },
    Item {
        kind: TargetKind::Callout,
        name: "Callout",
        hint: "Make writing stand out",
        aliases: &["note", "info"],
    },
    Item {
        kind: TargetKind::Code,
        name: "Code",
        hint: "Code snippet",
        aliases: &["```", "snippet"],
    },
    Item {
        kind: TargetKind::Divider,
        name: "Divider",
        hint: "Separate blocks",
        aliases: &["hr", "---", "line"],
    },
    Item {
        kind: TargetKind::Equation,
        name: "Equation",
        hint: "Display TeX formula",
        aliases: &["math", "tex", "katex", "$$"],
    },
];

/// Items whose name or an alias contains `query` (case-insensitive).
pub fn filter(query: &str) -> Vec<&'static Item> {
    let q = query.to_lowercase();
    ITEMS
        .iter()
        .filter(|i| i.name.to_lowercase().contains(&q) || i.aliases.iter().any(|a| a.contains(&q)))
        .collect()
}

/// An open menu: the `/` sits at byte `slash` of `line`.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub line: usize,
    pub slash: usize,
    pub selected: usize,
}

impl Menu {
    /// The text typed after `/`, or None if the cursor left the query
    /// (moved off the line, or backspaced past the `/`).
    pub fn query<'a>(&self, text: &'a str, line: usize, col: usize) -> Option<&'a str> {
        let l = text.split('\n').nth(self.line)?;
        if line != self.line || col <= self.slash || l.get(self.slash..self.slash + 1) != Some("/")
        {
            return None;
        }
        l.get(self.slash + 1..col)
    }

    /// `text` with `/query` (up to the cursor at `col`) removed.
    pub fn strip(&self, text: &str, col: usize) -> String {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if let Some(l) = lines.get_mut(self.line) {
            l.replace_range(self.slash..col.min(l.len()), "");
        }
        lines.join("\n")
    }
}

#[derive(Debug, PartialEq)]
pub enum MenuKey {
    Prev,
    Next,
    Pick,
    Close,
    /// The editor handles it (Backspace), then the menu re-checks its query.
    Editor,
    /// Eaten while the menu is open.
    Ignore,
}

pub fn menu_key(k: EditKey) -> MenuKey {
    match k {
        EditKey::Up => MenuKey::Prev,
        EditKey::Down => MenuKey::Next,
        EditKey::Enter => MenuKey::Pick,
        EditKey::Escape => MenuKey::Close,
        EditKey::Backspace => MenuKey::Editor,
        EditKey::Indent | EditKey::Outdent | EditKey::ToggleCheck => MenuKey::Ignore,
    }
}

/// After a keystroke left the cursor at byte `col` of `line`: the shortcut if
/// the text before the cursor is exactly one (`# `, `1. `, `` ``` ``, ...),
/// and the text that stays. `---` only fires as the whole text.
pub fn shortcut(text: &str, line: usize, col: usize) -> Option<(TargetKind, String)> {
    let head = text.get(..col).filter(|_| line == 0)?;
    let (kind, rest) = markdown_shortcut(head)?;
    if !rest.is_empty() || (kind == TargetKind::Divider && text != "---") {
        return None;
    }
    Some((kind, text[col..].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(q: &str) -> Vec<&str> {
        filter(q).iter().map(|i| i.name).collect()
    }

    #[test]
    fn equation_entry() {
        for q in ["equ", "math", "tex", "KaTeX", "$$"] {
            assert!(
                filter(q).iter().any(|i| i.kind == TargetKind::Equation),
                "{q}"
            );
        }
        assert_eq!(
            shortcut("$$ x", 0, 3),
            Some((TargetKind::Equation, "x".into()))
        );
    }

    #[test]
    fn filters_by_name_and_alias() {
        assert_eq!(names("").len(), ITEMS.len());
        assert_eq!(names("h1"), ["Heading 1"]);
        assert_eq!(names("HEAD"), ["Heading 1", "Heading 2", "Heading 3"]);
        assert_eq!(names("todo"), ["To-do list"]);
        assert_eq!(names("bullet"), ["Bulleted list"]);
        assert!(names("zzz").is_empty(), "no match: the menu closes");
        assert!(
            names("h1 ").is_empty(),
            "a space after a full word closes it too"
        );
    }

    #[test]
    fn query_tracks_the_cursor() {
        let m = Menu {
            line: 0,
            slash: 2,
            selected: 0,
        };
        assert_eq!(m.query("ab/h1", 0, 5), Some("h1"));
        assert_eq!(m.query("ab/", 0, 3), Some(""));
        assert_eq!(m.query("ab", 0, 2), None, "backspaced past /");
        assert_eq!(m.query("ab/h1\nx", 1, 1), None, "other line");
        assert_eq!(m.strip("ab/h1cd", 5), "abcd");
    }

    #[test]
    fn menu_keys() {
        assert_eq!(menu_key(EditKey::Up), MenuKey::Prev);
        assert_eq!(menu_key(EditKey::Down), MenuKey::Next);
        assert_eq!(menu_key(EditKey::Enter), MenuKey::Pick);
        assert_eq!(menu_key(EditKey::Escape), MenuKey::Close);
        assert_eq!(menu_key(EditKey::Backspace), MenuKey::Editor);
        assert_eq!(menu_key(EditKey::Indent), MenuKey::Ignore);
    }

    #[test]
    fn shortcuts_fire_only_at_the_start() {
        use TargetKind::*;
        assert_eq!(shortcut("# ", 0, 2), Some((Heading1, String::new())));
        assert_eq!(shortcut("# rest", 0, 2), Some((Heading1, "rest".into())));
        assert_eq!(shortcut("1. x", 0, 3), Some((Numbered, "x".into())));
        assert_eq!(shortcut("```", 0, 3), Some((Code, String::new())));
        assert_eq!(shortcut("---", 0, 3), Some((Divider, String::new())));
        assert_eq!(
            shortcut("---x", 0, 3),
            None,
            "divider only as the whole text"
        );
        assert_eq!(shortcut("a # ", 0, 4), None);
        assert_eq!(shortcut("#", 0, 1), None);
        assert_eq!(shortcut("# x", 0, 3), None, "cursor past the prefix");
        assert_eq!(shortcut("x\n# ", 1, 2), None);
    }
}
