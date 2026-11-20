//! User keybinds: `keys.json` maps a chord ("ctrl+alt+1") to a command name
//! ("heading_1"). Defaults follow Notion's; the file overrides them.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use iced::keyboard::key::{Named, Physical};
use iced::keyboard::{Key, Modifiers};
use iced::widget::text_editor::{Action, Content, Edit, Motion};
use notion_core::model::TargetKind;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    /// Lower-case char ("a", "1", "/") or a named key ("enter", "f5").
    key: String,
}

const NAMED: &[(Named, &str)] = &[
    (Named::Enter, "enter"),
    (Named::Tab, "tab"),
    (Named::ArrowUp, "up"),
    (Named::ArrowDown, "down"),
    (Named::ArrowLeft, "left"),
    (Named::ArrowRight, "right"),
    (Named::Backspace, "backspace"),
    (Named::Delete, "delete"),
    (Named::Escape, "escape"),
    (Named::Space, "space"),
    (Named::Home, "home"),
    (Named::End, "end"),
    (Named::PageUp, "pageup"),
    (Named::PageDown, "pagedown"),
    (Named::F1, "f1"),
    (Named::F2, "f2"),
    (Named::F3, "f3"),
    (Named::F4, "f4"),
    (Named::F5, "f5"),
    (Named::F6, "f6"),
    (Named::F7, "f7"),
    (Named::F8, "f8"),
    (Named::F9, "f9"),
    (Named::F10, "f10"),
    (Named::F11, "f11"),
    (Named::F12, "f12"),
];

impl Chord {
    /// Parses "Ctrl+Shift+K", "alt+f4", "ctrl++" (case-insensitive).
    pub fn parse(s: &str) -> Option<Chord> {
        let s = s.trim().to_lowercase();
        let (mods, key) = match s.strip_suffix("++") {
            Some(m) => (m, "+"),
            None => s.rsplit_once('+').unwrap_or(("", &s)),
        };
        let key = match key {
            "esc" => "escape",
            " " => "space",
            k => k,
        };
        let one_char =
            key.chars().count() == 1 && key.chars().all(|c| !c.is_whitespace() && !c.is_control());
        if !one_char && !NAMED.iter().any(|(_, n)| *n == key) {
            return None;
        }
        let mut c = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            key: key.to_string(),
        };
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            match m {
                "ctrl" | "control" => c.ctrl = true,
                "alt" => c.alt = true,
                "shift" => c.shift = true,
                "super" | "meta" | "cmd" | "logo" => c.logo = true,
                _ => return None,
            }
        }
        Some(c)
    }

    /// The chord of a key press; None for keys we can't name (modifiers alone).
    pub fn of(key: &Key, physical: Physical, m: Modifiers) -> Option<Chord> {
        let name = match key.as_ref() {
            Key::Named(n) => NAMED.iter().find(|(k, _)| *k == n)?.1.to_string(),
            // layout-independent letters, like the editor's own Ctrl+C
            _ => match key.to_latin(physical)? {
                ' ' => "space".into(),
                c => c.to_lowercase().to_string(),
            },
        };
        Some(Chord {
            ctrl: m.control(),
            alt: m.alt(),
            shift: m.shift(),
            logo: m.logo(),
            key: name,
        })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, m) in [
            (self.ctrl, "ctrl+"),
            (self.alt, "alt+"),
            (self.shift, "shift+"),
            (self.logo, "super+"),
        ] {
            if on {
                f.write_str(m)?;
            }
        }
        f.write_str(&self.key)
    }
}

/// An inline mark wrapped around the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Strike,
    Code,
    Equation,
    Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Block(TargetKind),
    Mark(Mark),
    Push,
    Pull,
    ForcePull,
    Search,
    ToggleSidebar,
    Indent,
    Outdent,
    ToggleCheck,
    NewBlock,
}

impl Command {
    /// Works without a focused editor (handled by the window-wide listener).
    pub fn is_global(self) -> bool {
        matches!(
            self,
            Command::Push
                | Command::Pull
                | Command::ForcePull
                | Command::Search
                | Command::ToggleSidebar
        )
    }
}

const NAMES: &[(&str, Command)] = &[
    ("paragraph", Command::Block(TargetKind::Paragraph)),
    ("heading_1", Command::Block(TargetKind::Heading1)),
    ("heading_2", Command::Block(TargetKind::Heading2)),
    ("heading_3", Command::Block(TargetKind::Heading3)),
    ("bulleted", Command::Block(TargetKind::Bulleted)),
    ("numbered", Command::Block(TargetKind::Numbered)),
    ("to_do", Command::Block(TargetKind::ToDo)),
    ("toggle", Command::Block(TargetKind::Toggle)),
    ("quote", Command::Block(TargetKind::Quote)),
    ("callout", Command::Block(TargetKind::Callout)),
    ("code", Command::Block(TargetKind::Code)),
    ("divider", Command::Block(TargetKind::Divider)),
    ("equation", Command::Block(TargetKind::Equation)),
    ("bold", Command::Mark(Mark::Bold)),
    ("italic", Command::Mark(Mark::Italic)),
    ("strike", Command::Mark(Mark::Strike)),
    ("code_inline", Command::Mark(Mark::Code)),
    ("equation_inline", Command::Mark(Mark::Equation)),
    ("link", Command::Mark(Mark::Link)),
    ("push", Command::Push),
    ("pull", Command::Pull),
    ("force_pull", Command::ForcePull),
    ("search", Command::Search),
    ("toggle_sidebar", Command::ToggleSidebar),
    ("indent", Command::Indent),
    ("outdent", Command::Outdent),
    ("toggle_check", Command::ToggleCheck),
    ("new_block", Command::NewBlock),
];

/// Notion's defaults, except link: Ctrl+K is search here, so Ctrl+Shift+K.
pub const DEFAULTS: &[(&str, &str)] = &[
    ("ctrl+alt+0", "paragraph"),
    ("ctrl+alt+1", "heading_1"),
    ("ctrl+alt+2", "heading_2"),
    ("ctrl+alt+3", "heading_3"),
    ("ctrl+alt+4", "to_do"),
    ("ctrl+alt+5", "bulleted"),
    ("ctrl+alt+6", "numbered"),
    ("ctrl+alt+7", "toggle"),
    ("ctrl+alt+8", "code"),
    ("ctrl+alt+9", "quote"),
    ("ctrl+alt+e", "equation"),
    ("ctrl+b", "bold"),
    ("ctrl+i", "italic"),
    ("ctrl+shift+s", "strike"),
    ("ctrl+e", "code_inline"),
    ("ctrl+shift+e", "equation_inline"),
    ("ctrl+shift+k", "link"),
    ("ctrl+s", "push"),
    ("ctrl+r", "pull"),
    ("ctrl+shift+r", "force_pull"),
    ("ctrl+k", "search"),
    ("ctrl+\\", "toggle_sidebar"),
    // Ctrl+\ on German/Austrian layouts (the unshifted key is ß)
    ("ctrl+ß", "toggle_sidebar"),
    ("tab", "indent"),
    ("shift+tab", "outdent"),
    ("ctrl+enter", "toggle_check"),
];

fn command(name: &str) -> Option<Command> {
    NAMES.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Keymap(HashMap<Chord, Command>);

impl Default for Keymap {
    fn default() -> Self {
        Keymap(
            DEFAULTS
                .iter()
                .map(|(k, c)| (Chord::parse(k).unwrap(), command(c).unwrap()))
                .collect(),
        )
    }
}

impl Keymap {
    pub fn get(&self, c: &Chord) -> Option<Command> {
        self.0.get(c).copied()
    }

    /// Defaults overridden by `entries` (file order); `"none"` or `null`
    /// unbinds. Returns the problems found, for the status bar.
    fn merge(entries: Vec<(String, Value)>) -> (Keymap, Vec<String>) {
        let mut map = Keymap::default();
        let (mut bad, mut unknown, mut dup) = (Vec::new(), Vec::new(), Vec::new());
        let mut seen = std::collections::HashSet::new();
        for (k, v) in entries {
            // the editor's own keys can't be taken without ctrl/alt/super
            let Some(chord) = Chord::parse(&k)
                .filter(|c| c.ctrl || c.alt || c.logo || !EDITOR_KEYS.contains(&c.key.as_str()))
            else {
                bad.push(k);
                continue;
            };
            let v = match v {
                Value::String(s) => s.trim().to_lowercase(),
                Value::Null => "none".into(),
                other => {
                    unknown.push(format!("{k}: {other}"));
                    continue;
                }
            };
            let cmd = command(&v);
            if cmd.is_none() && v != "none" {
                unknown.push(v);
                continue;
            }
            if !seen.insert(chord.clone()) {
                dup.push(chord.to_string());
            }
            match cmd {
                Some(c) => map.0.insert(chord, c),
                None => map.0.remove(&chord),
            };
        }
        let mut problems = Vec::new();
        for (what, list) in [
            ("bad chords", bad),
            ("unknown actions", unknown),
            ("duplicate chords (last wins)", dup),
        ] {
            if !list.is_empty() {
                problems.push(format!("keys.json: {what}: {}", list.join(", ")));
            }
        }
        (map, problems)
    }
}

/// Keys `route` hard-wires; bindable only with ctrl, alt or super.
const EDITOR_KEYS: &[&str] = &[
    "enter",
    "backspace",
    "escape",
    "up",
    "down",
    "left",
    "right",
];

/// A JSON object read as (key, value) pairs in file order, duplicates kept.
struct Entries(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Entries;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#"an object of "chord": "action""#)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Entries, A::Error> {
                let mut v = Vec::new();
                while let Some(e) = m.next_entry()? {
                    v.push(e);
                }
                Ok(Entries(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// The keymap from `dir/keys.json`, writing the defaults there if it's
/// missing (never overwriting). Problems go to the status bar.
pub fn load(dir: &Path) -> (Keymap, Vec<String>) {
    let path = dir.join("keys.json");
    match std::fs::read_to_string(&path) {
        Ok(src) => match serde_json::from_str::<Entries>(&src) {
            Ok(Entries(e)) => Keymap::merge(e),
            Err(e) => (
                Keymap::default(),
                vec![format!("keys.json: {e}; using the default keys")],
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let problems = write_defaults(&path)
                .err()
                .map(|e| format!("cannot write {}: {e}", path.display()));
            (Keymap::default(), problems.into_iter().collect())
        }
        Err(e) => (
            Keymap::default(),
            vec![format!("cannot read {}: {e}", path.display())],
        ),
    }
}

fn write_defaults(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let body: Vec<String> = DEFAULTS
        .iter()
        .map(|(k, c)| format!("  {}: {}", serde_json::json!(k), serde_json::json!(c)))
        .collect();
    // create_new: a file that appeared meanwhile is the user's
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(format!("{{\n{}\n}}\n", body.join(",\n")).as_bytes())
}

/// Wraps the selection in the mark's Markdown; with no selection, inserts the
/// pair with the cursor between. A link becomes `[sel]()`, cursor in the `()`.
pub fn wrap(c: &mut Content, m: Mark) {
    let (l, r) = match m {
        Mark::Bold => ("**", "**"),
        Mark::Italic => ("*", "*"),
        Mark::Strike => ("~~", "~~"),
        Mark::Code => ("`", "`"),
        Mark::Equation => ("$", "$"),
        Mark::Link => ("[", "]()"),
    };
    let (text, back) = match c.selection() {
        Some(sel) => (format!("{l}{sel}{r}"), usize::from(m == Mark::Link)),
        None => (format!("{l}{r}"), r.chars().count()),
    };
    c.perform(Action::Edit(Edit::Paste(text.into())));
    for _ in 0..back {
        c.perform(Action::Move(Motion::Left));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::NativeCode;

    fn ch(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn parses_chords() {
        let c = ch("Ctrl+Alt+1");
        assert!(c.ctrl && c.alt && !c.shift && !c.logo && c.key == "1");
        assert_eq!(ch("CTRL+SHIFT+K"), ch("shift+ctrl+k"));
        assert_eq!(ch("super+Enter").to_string(), "super+enter");
        assert_eq!(ch("meta+f12"), ch("Super+F12"));
        assert_eq!(ch("ctrl+/").key, "/");
        assert_eq!(ch("ctrl+\\").key, "\\");
        assert_eq!(ch("ctrl++").key, "+");
        assert_eq!(ch("esc"), ch("escape"));
        assert_eq!(ch("tab").to_string(), "tab");
        for bad in ["", "ctrl+", "hyper+a", "ctrl+f13", "ctrl+ab", "ctrl+alt"] {
            assert_eq!(Chord::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn chords_from_key_presses() {
        let phys = Physical::Unidentified(NativeCode::Unidentified);
        let m = Modifiers::CTRL | Modifiers::SHIFT;
        assert_eq!(
            Chord::of(&Key::Character("K".into()), phys, m),
            Some(ch("ctrl+shift+k"))
        );
        assert_eq!(
            Chord::of(&Key::Named(Named::Tab), phys, Modifiers::SHIFT),
            Some(ch("shift+tab"))
        );
        assert_eq!(
            Chord::of(&Key::Named(Named::Shift), phys, Modifiers::SHIFT),
            None
        );
    }

    #[test]
    fn default_map() {
        let k = Keymap::default();
        assert_eq!(k.0.len(), DEFAULTS.len(), "no duplicate defaults");
        assert_eq!(
            k.get(&ch("ctrl+alt+1")),
            Some(Command::Block(TargetKind::Heading1))
        );
        assert_eq!(k.get(&ch("ctrl+k")), Some(Command::Search));
        assert_eq!(k.get(&ch("ctrl+\\")), Some(Command::ToggleSidebar));
        assert_eq!(k.get(&ch("ctrl+ß")), Some(Command::ToggleSidebar));
        assert_eq!(k.get(&ch("ctrl+shift+k")), Some(Command::Mark(Mark::Link)));
        assert_eq!(k.get(&ch("ctrl+enter")), Some(Command::ToggleCheck));
        assert_eq!(k.get(&ch("ctrl+x")), None);
    }

    fn entries(v: &[(&str, &str)]) -> Vec<(String, Value)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), Value::from(*b)))
            .collect()
    }

    #[test]
    fn editor_keys_need_a_modifier() {
        let (k, p) = Keymap::merge(entries(&[
            ("enter", "bold"),
            ("shift+enter", "bold"),
            ("up", "bold"),
            ("escape", "bold"),
            ("ctrl+backspace", "bold"),
            ("alt+left", "italic"),
            ("f5", "pull"),
        ]));
        assert_eq!(p, ["keys.json: bad chords: enter, shift+enter, up, escape"]);
        assert_eq!(
            k.get(&ch("ctrl+backspace")),
            Some(Command::Mark(Mark::Bold))
        );
        assert_eq!(k.get(&ch("alt+left")), Some(Command::Mark(Mark::Italic)));
        assert_eq!(k.get(&ch("f5")), Some(Command::Pull));
        assert_eq!(k.get(&ch("enter")), None);
    }

    #[test]
    fn json_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("keys.json"),
            r#"{"ctrl+b": null, "ctrl+i": 5, "ctrl+e": " Bold "}"#,
        )
        .unwrap();
        let (k, p) = load(dir.path());
        assert_eq!(p, ["keys.json: unknown actions: ctrl+i: 5"]);
        assert_eq!(k.get(&ch("ctrl+b")), None, "null unbinds");
        assert_eq!(
            k.get(&ch("ctrl+i")),
            Some(Command::Mark(Mark::Italic)),
            "default kept"
        );
        assert_eq!(k.get(&ch("ctrl+e")), Some(Command::Mark(Mark::Bold)));
    }

    #[test]
    fn user_file_overrides_and_reports() {
        let (k, p) = Keymap::merge(entries(&[
            ("ctrl+b", "italic"),
            ("ctrl+alt+b", "bold"),
            ("Ctrl+K", "none"),
        ]));
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(k.get(&ch("ctrl+b")), Some(Command::Mark(Mark::Italic)));
        assert_eq!(k.get(&ch("ctrl+alt+b")), Some(Command::Mark(Mark::Bold)));
        assert_eq!(k.get(&ch("ctrl+k")), None);
        assert_eq!(k.get(&ch("ctrl+s")), Some(Command::Push), "defaults kept");

        let (k, p) = Keymap::merge(entries(&[
            ("ctrl+q", "explode"),
            ("hyper+x", "bold"),
            ("ctrl+j", "bold"),
            ("CTRL+J", "italic"),
        ]));
        assert_eq!(
            p,
            [
                "keys.json: bad chords: hyper+x",
                "keys.json: unknown actions: explode",
                "keys.json: duplicate chords (last wins): ctrl+j",
            ]
        );
        assert_eq!(k.get(&ch("ctrl+q")), None);
        assert_eq!(k.get(&ch("ctrl+j")), Some(Command::Mark(Mark::Italic)));
    }

    #[test]
    fn load_writes_defaults_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("notion-client");
        let (k, p) = load(&sub);
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(k, Keymap::default());
        let written = std::fs::read_to_string(sub.join("keys.json")).unwrap();
        assert!(
            written.contains(r#""ctrl+alt+1": "heading_1""#),
            "{written}"
        );
        assert_eq!(load(&sub), (Keymap::default(), vec![]), "reads it back");

        // literal duplicate keys in the file: the last wins
        std::fs::write(
            sub.join("keys.json"),
            r#"{"ctrl+b": "italic", "ctrl+b": "strike"}"#,
        )
        .unwrap();
        let (k, p) = load(&sub);
        assert_eq!(k.get(&ch("ctrl+b")), Some(Command::Mark(Mark::Strike)));
        assert_eq!(p.len(), 1);

        std::fs::write(sub.join("keys.json"), "{ not json").unwrap();
        let (k, p) = load(&sub);
        assert_eq!(k, Keymap::default());
        assert!(p[0].contains("using the default keys"), "{p:?}");
        // never overwritten
        assert_eq!(
            std::fs::read_to_string(sub.join("keys.json")).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn marks_wrap_the_selection_or_insert_a_pair() {
        let mut c = Content::with_text("say hi");
        c.perform(Action::Move(Motion::DocumentEnd));
        c.perform(Action::Select(Motion::WordLeft));
        assert_eq!(c.selection().as_deref(), Some("hi"));
        wrap(&mut c, Mark::Bold);
        assert_eq!(c.text(), "say **hi**");

        let mut c = Content::with_text("a");
        c.perform(Action::Move(Motion::DocumentEnd));
        wrap(&mut c, Mark::Equation);
        c.perform(Action::Edit(Edit::Insert('x')));
        assert_eq!(c.text(), "a$x$");

        let mut c = Content::with_text("site");
        c.perform(Action::SelectAll);
        wrap(&mut c, Mark::Link);
        c.perform(Action::Edit(Edit::Insert('u')));
        assert_eq!(c.text(), "[site](u)");

        let mut c = Content::with_text("");
        wrap(&mut c, Mark::Link);
        c.perform(Action::Edit(Edit::Insert('t')));
        assert_eq!(c.text(), "[t]()");
    }
}
