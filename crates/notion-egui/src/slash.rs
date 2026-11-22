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
        aliases: &["paragraph", "p", "plain", "Text", "Absatz"],
    },
    Item {
        kind: TargetKind::Heading1,
        name: "Heading 1",
        hint: "Big section heading",
        aliases: &["h1", "#", "Überschrift 1", "Überschrift"],
    },
    Item {
        kind: TargetKind::Heading2,
        name: "Heading 2",
        hint: "Medium section heading",
        aliases: &["h2", "##", "Überschrift 2", "Überschrift"],
    },
    Item {
        kind: TargetKind::Heading3,
        name: "Heading 3",
        hint: "Small section heading",
        aliases: &["h3", "###", "Überschrift 3", "Überschrift"],
    },
    Item {
        kind: TargetKind::ToggleHeading1,
        name: "Toggle heading 1",
        hint: "Collapsible heading",
        aliases: &["th1"],
    },
    Item {
        kind: TargetKind::ToggleHeading2,
        name: "Toggle heading 2",
        hint: "Collapsible heading",
        aliases: &["th2"],
    },
    Item {
        kind: TargetKind::ToggleHeading3,
        name: "Toggle heading 3",
        hint: "Collapsible heading",
        aliases: &["th3"],
    },
    Item {
        kind: TargetKind::Bulleted,
        name: "Bulleted list",
        hint: "Simple bulleted list",
        aliases: &["bullet", "ul", "-", "Aufzählung", "Liste", "Stichpunkte"],
    },
    Item {
        kind: TargetKind::Numbered,
        name: "Numbered list",
        hint: "List with numbering",
        aliases: &["number", "ol", "1.", "Nummerierte Liste", "Nummerierung"],
    },
    Item {
        kind: TargetKind::ToDo,
        name: "To-do list",
        hint: "Track tasks with a checkbox",
        aliases: &["todo", "checkbox", "[]", "To-do", "Aufgabe", "Checkliste"],
    },
    Item {
        kind: TargetKind::Toggle,
        name: "Toggle list",
        hint: "Hide content inside",
        aliases: &["toggle", ">", "Aufklappen", "Aufklappbar"],
    },
    Item {
        kind: TargetKind::Quote,
        name: "Quote",
        hint: "Capture a quote",
        aliases: &["blockquote", "\"", "Zitat"],
    },
    Item {
        kind: TargetKind::Callout,
        name: "Callout",
        hint: "Make writing stand out",
        aliases: &["note", "info", "Hinweis", "Callout"],
    },
    Item {
        kind: TargetKind::Code,
        name: "Code",
        hint: "Code snippet",
        aliases: &["```", "snippet", "Code", "Programmcode"],
    },
    Item {
        kind: TargetKind::Divider,
        name: "Divider",
        hint: "Separate blocks",
        aliases: &["hr", "---", "line", "Trennlinie", "Trenner"],
    },
    Item {
        kind: TargetKind::Equation,
        name: "Equation",
        hint: "Display TeX formula",
        aliases: &[
            "math",
            "tex",
            "katex",
            "$$",
            "Block-Gleichung",
            "Gleichung",
            "Formel",
        ],
    },
];

/// Lower-case with umlauts folded (`ä`→`ae`, `ö`→`oe`, `ü`→`ue`, `ß`→`ss`), so
/// `ueberschrift` and `Überschrift` are the same word.
pub fn fold(s: &str) -> String {
    s.to_lowercase()
        .replace('ä', "ae")
        .replace('ö', "oe")
        .replace('ü', "ue")
        .replace('ß', "ss")
}

/// True if `query` is a prefix of `name` or of any word of it (a word starts
/// at the beginning, after a space, or after a `-`); both folded. So `glei`
/// finds `Block-Gleichung` and `liste` finds `Nummerierte Liste`, but not
/// `Checkliste`. An empty query matches everything.
pub fn word_prefix(query: &str, name: &str) -> bool {
    let (q, n) = (fold(query), fold(name));
    std::iter::once(0)
        .chain(n.match_indices([' ', '-']).map(|(i, _)| i + 1))
        .any(|i| n[i..].starts_with(&q))
}

/// Items whose name or an alias matches `query` ([`word_prefix`]).
pub fn filter(query: &str) -> Vec<&'static Item> {
    ITEMS
        .iter()
        .filter(|i| word_prefix(query, i.name) || i.aliases.iter().any(|a| word_prefix(query, a)))
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
        assert_eq!(names("HEAD").len(), 6, "headings and toggle headings");
        assert_eq!(names("th2"), ["Toggle heading 2"]);
        assert_eq!(names("todo"), ["To-do list"]);
        assert_eq!(names("bullet"), ["Bulleted list"]);
        assert!(names("zzz").is_empty(), "no match: the menu closes");
        assert!(
            names("h1 ").is_empty(),
            "a space after a full word closes it too"
        );
    }

    #[test]
    fn german_and_folded_queries_match() {
        let kinds = |q: &str| filter(q).iter().map(|i| i.kind).collect::<Vec<_>>();
        use TargetKind::*;
        assert_eq!(kinds("Glei"), [Equation]);
        assert_eq!(kinds("formel"), [Equation]);
        assert_eq!(kinds("ueberschrift"), [Heading1, Heading2, Heading3]);
        assert_eq!(kinds("Überschrift"), [Heading1, Heading2, Heading3]);
        assert_eq!(kinds("überschrift 2"), [Heading2]);
        assert_eq!(kinds("UEBERSCHRIFT 3"), [Heading3]);
        assert_eq!(kinds("liste"), [Bulleted, Numbered], "not Checkliste");
        assert_eq!(kinds("aufz"), [Bulleted]);
        assert_eq!(kinds("Aufzählung"), [Bulleted]);
        assert_eq!(kinds("aufzaehlung"), [Bulleted]);
        assert_eq!(kinds("nummer"), [Numbered]);
        assert_eq!(kinds("aufgabe"), [ToDo]);
        assert_eq!(kinds("check"), [ToDo]);
        assert_eq!(kinds("do"), [ToDo], "after the hyphen of To-do");
        assert_eq!(kinds("aufklapp"), [Toggle]);
        assert_eq!(kinds("zit"), [Quote]);
        assert_eq!(kinds("hinw"), [Callout]);
        assert_eq!(kinds("trenn"), [Divider]);
        assert_eq!(kinds("gleichung"), [Equation]);
        assert!(
            kinds("leichung").is_empty(),
            "word prefixes, not substrings"
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
