//! Keybinds and raw-event helpers. `keys.json` maps a chord ("ctrl+shift+1")
//! to a command name ("heading_1"); defaults follow Notion's Linux keys, the
//! file overrides them. Ported from notion-gui's keys.rs, with an egui
//! adapter in place of iced's `Chord::of`.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use eframe::egui::{Event, Key, Modifiers};
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

const NAMED: &[(Key, &str)] = &[
    (Key::Enter, "enter"),
    (Key::Tab, "tab"),
    (Key::ArrowUp, "up"),
    (Key::ArrowDown, "down"),
    (Key::ArrowLeft, "left"),
    (Key::ArrowRight, "right"),
    (Key::Backspace, "backspace"),
    (Key::Delete, "delete"),
    (Key::Escape, "escape"),
    (Key::Space, "space"),
    (Key::Home, "home"),
    (Key::End, "end"),
    (Key::PageUp, "pageup"),
    (Key::PageDown, "pagedown"),
    (Key::F1, "f1"),
    (Key::F2, "f2"),
    (Key::F3, "f3"),
    (Key::F4, "f4"),
    (Key::F5, "f5"),
    (Key::F6, "f6"),
    (Key::F7, "f7"),
    (Key::F8, "f8"),
    (Key::F9, "f9"),
    (Key::F10, "f10"),
    (Key::F11, "f11"),
    (Key::F12, "f12"),
];

/// Keys that name a character.
const CHARS: &[(Key, char)] = &[
    (Key::Num0, '0'),
    (Key::Num1, '1'),
    (Key::Num2, '2'),
    (Key::Num3, '3'),
    (Key::Num4, '4'),
    (Key::Num5, '5'),
    (Key::Num6, '6'),
    (Key::Num7, '7'),
    (Key::Num8, '8'),
    (Key::Num9, '9'),
    (Key::Colon, ':'),
    (Key::Comma, ','),
    (Key::Backslash, '\\'),
    (Key::Slash, '/'),
    (Key::Pipe, '|'),
    (Key::Questionmark, '?'),
    (Key::Exclamationmark, '!'),
    (Key::OpenBracket, '['),
    (Key::CloseBracket, ']'),
    (Key::OpenCurlyBracket, '{'),
    (Key::CloseCurlyBracket, '}'),
    (Key::Backtick, '`'),
    (Key::Minus, '-'),
    (Key::Period, '.'),
    (Key::Plus, '+'),
    (Key::Equals, '='),
    (Key::Semicolon, ';'),
    (Key::Quote, '\''),
];

/// "a".."z", a digit or symbol, or a named key; None for keys we don't name.
fn key_name(k: Key) -> Option<String> {
    if let Some((_, n)) = NAMED.iter().find(|(n, _)| *n == k) {
        return Some((*n).to_string());
    }
    if let Some((_, c)) = CHARS.iter().find(|(n, _)| *n == k) {
        return Some(c.to_string());
    }
    let n = k.name();
    (n.len() == 1 && n.chars().all(|c| c.is_ascii_alphabetic())).then(|| n.to_lowercase())
}

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

    fn with(m: Modifiers, key: String) -> Chord {
        Chord {
            ctrl: m.ctrl || m.command,
            alt: m.alt,
            shift: m.shift,
            logo: m.mac_cmd,
            key,
        }
    }
}

/// The chords a key press can mean, best first. Digits come from the physical
/// key (Shift+7 is "/" on a German layout, but Ctrl+Shift+7 is the 7), letters
/// from the logical one (QWERTZ swaps Y and Z). On the Austrian layout the
/// key left of Backspace is ß, which winit reports as the physical Minus, and
/// `\` is AltGr+ß: both stay reachable as "ctrl+ß" / "ctrl+\".
pub fn chords(key: Key, physical: Option<Key>, m: Modifiers) -> Vec<Chord> {
    let digit = physical.filter(|p| CHARS[..10].iter().any(|(k, _)| k == p));
    let mut out: Vec<Chord> = digit
        .or(Some(key))
        .and_then(key_name)
        .into_iter()
        .map(|n| Chord::with(m, n))
        .collect();
    match physical {
        Some(Key::Minus) => out.push(Chord::with(m, "ß".into())),
        Some(Key::Backslash) => out.push(Chord::with(m, "\\".into())),
        _ => {}
    }
    out
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

/// An inline mark wrapped around the selection. (No underline: Markdown has
/// none, and core drops it, as in notion-gui.)
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
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    Undo,
    Redo,
    /// Ctrl+/: the "Turn into" menu for the edited row.
    TurnInto,
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
                | Command::NewTab
                | Command::CloseTab
                | Command::NextTab
                | Command::PrevTab
                | Command::Undo
                | Command::Redo
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
    ("new_tab", Command::NewTab),
    ("close_tab", Command::CloseTab),
    ("next_tab", Command::NextTab),
    ("prev_tab", Command::PrevTab),
    ("undo", Command::Undo),
    ("redo", Command::Redo),
    ("turn_into", Command::TurnInto),
];

/// Notion's Linux keys (Ctrl+Shift+0-9 for block types; 9, "page", has no
/// command here), except link: Ctrl+K is search here, so Ctrl+Shift+K.
pub const DEFAULTS: &[(&str, &str)] = &[
    ("ctrl+shift+0", "paragraph"),
    ("ctrl+shift+1", "heading_1"),
    ("ctrl+shift+2", "heading_2"),
    ("ctrl+shift+3", "heading_3"),
    ("ctrl+shift+4", "to_do"),
    ("ctrl+shift+5", "bulleted"),
    ("ctrl+shift+6", "numbered"),
    ("ctrl+shift+7", "toggle"),
    ("ctrl+shift+8", "code"),
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
    ("ctrl+t", "new_tab"),
    ("ctrl+w", "close_tab"),
    ("ctrl+tab", "next_tab"),
    ("ctrl+shift+tab", "prev_tab"),
    ("ctrl+z", "undo"),
    ("ctrl+shift+z", "redo"),
    // On German/Austrian layouts `/` is Shift+7, i.e. Ctrl+Shift+7 = toggle:
    // rebind turn_into there
    ("ctrl+/", "turn_into"),
    // Ctrl+/ is Ctrl+Shift+7 on those layouts; Ctrl+. works anywhere
    ("ctrl+.", "turn_into"),
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

    /// The first bound chord of `cs` (see [`chords`]).
    pub fn get_any(&self, cs: &[Chord]) -> Option<Command> {
        cs.iter().find_map(|c| self.get(c))
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

/// Keys the editor hard-wires; bindable only with ctrl, alt or super.
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

/// Wraps the chars `sel` (start, end; either order) of `text` in the mark's
/// Markdown; with no selection, inserts the pair with the cursor between. A
/// link becomes `[sel]()`, cursor in the `()`. Returns the new text and the
/// new selection (a cursor: start == end), as char indices.
pub fn wrap(text: &str, sel: (usize, usize), m: Mark) -> (String, (usize, usize)) {
    let (l, r) = match m {
        Mark::Bold => ("**", "**"),
        Mark::Italic => ("*", "*"),
        Mark::Strike => ("~~", "~~"),
        Mark::Code => ("`", "`"),
        Mark::Equation => ("$", "$"),
        Mark::Link => ("[", "]()"),
    };
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    let byte = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    let (blo, bhi) = (byte(lo), byte(hi));
    let inner = &text[blo..bhi];
    let out = format!("{}{l}{inner}{r}{}", &text[..blo], &text[bhi..]);
    let after = lo + l.chars().count() + inner.chars().count() + r.chars().count();
    let back = if lo == hi {
        r.chars().count()
    } else {
        usize::from(m == Mark::Link)
    };
    (out, (after - back, after - back))
}

/// The keymap commands `want` in this frame's raw events, removed from
/// `events` so a focused text edit can't act on them too (Ctrl+B moves its
/// cursor, Ctrl+Z undoes in it). Repeats are removed but do not fire. On the
/// Austrian layout Ctrl+ß may arrive as Text "ß" (or `\` as Text "\\"), so
/// those count as chords while `ctrl_held`. When such a chord fires from the
/// key, a Text of its character in the same frame is dropped too.
pub fn take_commands(
    events: &mut Vec<Event>,
    ctrl_held: bool,
    map: &Keymap,
    want: impl Fn(Command) -> bool,
) -> Vec<Command> {
    let mut out = Vec::new();
    let mut drop_text = false;
    events.retain(|e| {
        let (cmd, repeat, modifiers) = match e {
            Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat,
                modifiers,
            } => (
                map.get_any(&chords(*key, *physical_key, *modifiers)),
                *repeat,
                *modifiers,
            ),
            Event::Text(t) if ctrl_held && matches!(t.as_str(), "\\" | "ß") => {
                let m = Modifiers::CTRL;
                (map.get(&Chord::with(m, t.clone())), false, m)
            }
            _ => return true,
        };
        let Some(cmd) = cmd.filter(|c| want(*c)) else {
            return true;
        };
        // Key and Text for one press: once
        if !repeat && !(matches!(e, Event::Text(_)) && out.contains(&cmd)) {
            out.push(cmd);
        }
        // the same press may also arrive as the character it types
        drop_text |= (modifiers.ctrl || modifiers.alt)
            && matches!(e, Event::Key { physical_key: Some(Key::Minus | Key::Backslash), .. });
        false
    });
    if drop_text {
        events.retain(|e| !matches!(e, Event::Text(t) if matches!(t.as_str(), "ß" | "\\" | "-")));
    }
    out
}

/// Removes the key presses of `key` with exactly `mods` from `events`:
/// (fresh presses, repeats). egui's own `consume_key` ignores extra Shift and
/// Alt, so Shift+Enter would go with Enter.
pub fn take_exact(events: &mut Vec<Event>, mods: Modifiers, key: Key) -> (usize, usize) {
    let (mut fresh, mut repeats) = (0, 0);
    events.retain(|e| match e {
        Event::Key {
            key: k,
            pressed: true,
            repeat,
            modifiers,
            ..
        } if *k == key && same_mods(*modifiers, mods) => {
            if *repeat {
                repeats += 1;
            } else {
                fresh += 1;
            }
            false
        }
        _ => true,
    });
    (fresh, repeats)
}

fn same_mods(a: Modifiers, b: Modifiers) -> bool {
    (a.ctrl || a.command, a.alt, a.shift, a.mac_cmd)
        == (b.ctrl || b.command, b.alt, b.shift, b.mac_cmd)
}

/// `NOTION_EGUI_KEYLOG=1`: print raw Key/Text/Ime events to stderr.
pub fn log(events: &[Event]) {
    for e in events {
        if matches!(e, Event::Key { .. } | Event::Text(_) | Event::Ime(_)) {
            eprintln!("{e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    fn press(key: Key, physical: Option<Key>, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: physical,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::NONE
    };

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
        let first = |k, p, m| chords(k, p, m).into_iter().next();
        assert_eq!(first(Key::K, None, CTRL_SHIFT), Some(ch("ctrl+shift+k")));
        assert_eq!(
            first(Key::Tab, None, Modifiers::SHIFT),
            Some(ch("shift+tab"))
        );
        assert_eq!(first(Key::Num2, None, CTRL_SHIFT), Some(ch("ctrl+shift+2")));
        assert_eq!(first(Key::Backslash, None, Modifiers::CTRL), Some(ch("ctrl+\\")));
        // a modifier alone has no name
        assert_eq!(first(Key::Copy, None, Modifiers::CTRL), None);
    }

    #[test]
    fn digits_follow_the_physical_key_letters_the_logical_one() {
        // German layout: Shift+7 is "/", Shift+2 is `"` (no egui key: the
        // physical one is reported)
        let slash = chords(Key::Slash, Some(Key::Num7), CTRL_SHIFT);
        assert_eq!(slash[0], ch("ctrl+shift+7"));
        // QWERTZ: the key labelled Z sits where US has Y
        let z = chords(Key::Z, Some(Key::Y), Modifiers::CTRL);
        assert_eq!(z[0], ch("ctrl+z"));
    }

    #[test]
    fn ctrl_sharp_s_and_backslash_variants() {
        let k = Keymap::default();
        let ctrl = Modifiers::CTRL;
        for ev in [
            press(Key::Backslash, None, ctrl),
            press(Key::Minus, Some(Key::Minus), ctrl), // Ctrl+ß, no logical key
            press(Key::A, Some(Key::Backslash), ctrl),
        ] {
            let mut v = vec![ev, Event::Text("ß".into())];
            let got = take_commands(&mut v, true, &k, Command::is_global);
            assert_eq!(got, [Command::ToggleSidebar]);
            assert!(v.is_empty(), "the shortcut is eaten, and its stray text");
        }
        // typing in the same frame is not lost
        let mut v = vec![Event::Text("x".into()), press(Key::S, Some(Key::S), ctrl)];
        assert_eq!(take_commands(&mut v, true, &k, |_| true), [Command::Push]);
        assert_eq!(v, [Event::Text("x".into())]);
        // Text only, and Key + Text for one press: once
        let mut v = vec![Event::Text("ß".into())];
        assert_eq!(take_commands(&mut v, true, &k, |_| true), [Command::ToggleSidebar]);
        assert!(v.is_empty());
        let mut v = vec![press(Key::Minus, Some(Key::Minus), ctrl), Event::Text("ß".into())];
        assert_eq!(take_commands(&mut v, true, &k, |_| true).len(), 1);
    }

    #[test]
    fn plain_typing_is_not_a_command() {
        let k = Keymap::default();
        let mut v = vec![
            press(Key::Backslash, None, Modifiers::NONE),
            Event::Text("ß".into()), // typing ß without Ctrl
            Event::Text("\\".into()),
        ];
        assert!(take_commands(&mut v, false, &k, |_| true).is_empty());
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn commands_are_filtered_by_scope_and_repeats_dont_fire() {
        let k = Keymap::default();
        let mut v = vec![
            press(Key::B, None, Modifiers::CTRL),
            press(Key::Z, None, Modifiers::CTRL),
            Event::Key {
                key: Key::Z,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: Modifiers::CTRL,
            },
        ];
        let g = take_commands(&mut v, true, &k, Command::is_global);
        assert_eq!(g, [Command::Undo], "the repeat is eaten but does not fire");
        assert_eq!(v.len(), 1, "Ctrl+B left for the editor");
        let e = take_commands(&mut v, true, &k, |c| !c.is_global());
        assert_eq!(e, [Command::Mark(Mark::Bold)]);
        assert!(v.is_empty());
    }

    #[test]
    fn exact_modifiers_keep_shift_enter() {
        let mut v = vec![
            press(Key::Enter, None, Modifiers::SHIFT),
            press(Key::Enter, None, Modifiers::NONE),
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: Modifiers::NONE,
            },
        ];
        assert_eq!(take_exact(&mut v, Modifiers::NONE, Key::Enter), (1, 1));
        assert_eq!(v, [press(Key::Enter, None, Modifiers::SHIFT)]);
    }

    #[test]
    fn default_map() {
        let k = Keymap::default();
        assert_eq!(k.0.len(), DEFAULTS.len(), "no duplicate defaults");
        assert_eq!(
            k.get(&ch("ctrl+shift+1")),
            Some(Command::Block(TargetKind::Heading1))
        );
        assert_eq!(
            k.get(&ch("ctrl+shift+0")),
            Some(Command::Block(TargetKind::Paragraph))
        );
        assert_eq!(
            k.get(&ch("ctrl+shift+8")),
            Some(Command::Block(TargetKind::Code))
        );
        assert_eq!(k.get(&ch("ctrl+shift+9")), None, "no page command");
        assert_eq!(k.get(&ch("ctrl+alt+1")), None, "iced's old block keys");
        assert_eq!(
            k.get(&ch("ctrl+alt+e")),
            Some(Command::Block(TargetKind::Equation))
        );
        assert_eq!(
            k.get(&ch("ctrl+shift+e")),
            Some(Command::Mark(Mark::Equation))
        );
        assert_eq!(k.get(&ch("ctrl+shift+s")), Some(Command::Mark(Mark::Strike)));
        assert_eq!(k.get(&ch("ctrl+k")), Some(Command::Search));
        assert_eq!(k.get(&ch("ctrl+\\")), Some(Command::ToggleSidebar));
        assert_eq!(k.get(&ch("ctrl+ß")), Some(Command::ToggleSidebar));
        assert_eq!(k.get(&ch("ctrl+shift+k")), Some(Command::Mark(Mark::Link)));
        assert_eq!(k.get(&ch("ctrl+enter")), Some(Command::ToggleCheck));
        assert_eq!(k.get(&ch("ctrl+tab")), Some(Command::NextTab));
        assert_eq!(k.get(&ch("ctrl+shift+tab")), Some(Command::PrevTab));
        assert_eq!(k.get(&ch("tab")), Some(Command::Indent));
        assert_eq!(k.get(&ch("ctrl+z")), Some(Command::Undo));
        assert_eq!(k.get(&ch("ctrl+shift+z")), Some(Command::Redo));
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
            written.contains(r#""ctrl+shift+1": "heading_1""#),
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
        // "say hi", hi selected
        let (t, sel) = wrap("say hi", (6, 4), Mark::Bold);
        assert_eq!((t.as_str(), sel), ("say **hi**", (10, 10)));

        // no selection: the pair, cursor between
        let (t, sel) = wrap("a", (1, 1), Mark::Equation);
        assert_eq!((t.as_str(), sel), ("a$$", (2, 2)));
        // ...and typing there
        assert_eq!(&format!("{}x{}", &t[..sel.0], &t[sel.0..]), "a$x$");

        // a link: the selection is the label, the cursor lands in the ()
        let (t, sel) = wrap("site", (0, 4), Mark::Link);
        assert_eq!((t.as_str(), sel), ("[site]()", (7, 7)));
        let (t, sel) = wrap("", (0, 0), Mark::Link);
        assert_eq!((t.as_str(), sel), ("[]()", (1, 1)));

        // multibyte text is indexed in chars
        let (t, sel) = wrap("äö ß", (3, 4), Mark::Code);
        assert_eq!((t.as_str(), sel), ("äö `ß`", (6, 6)));
        let (t, _) = wrap("x", (0, 1), Mark::Strike);
        assert_eq!(t, "~~x~~");
        let (t, _) = wrap("x", (0, 1), Mark::Italic);
        assert_eq!(t, "*x*");
    }
}
