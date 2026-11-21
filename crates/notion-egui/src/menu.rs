//! The editor's popup menus, one implementation for all four: `/` (block
//! types), `@` (page and date mentions), `[[` (page links) and Ctrl+/ ("Turn
//! into"). Pure functions over the editor text, so they are testable without
//! a window; app.rs wires them to the editor.

use eframe::egui::{self, Frame, Id, Label, RichText as Text};
use notion_core::markdown::to_markdown;
use notion_core::model::{Annotations, RichText, RichTextKind, TargetKind, User, COLORS};
use notion_core::store::PageEntry;
use serde_json::json;

use crate::edit::EditKey;
use crate::slash::{self, MenuKey};

/// Pages listed by `@` and `[[`.
const MAX_PAGES: usize = 8;
/// Users listed by `@`.
const MAX_USERS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `/`: block types, the typed text after the `/` filters.
    Slash,
    /// `@`: pages and dates.
    Mention,
    /// `[[`: pages only.
    PageLink,
    /// Ctrl+/: block types for the row; the filter is typed into the menu.
    TurnInto,
}

impl Kind {
    /// The text that opens the menu (none for Ctrl+/).
    fn trigger(self) -> &'static str {
        match self {
            Kind::Slash => "/",
            Kind::Mention => "@",
            Kind::PageLink => "[[",
            Kind::TurnInto => "",
        }
    }
}

/// Today, tomorrow and yesterday as `YYYY-MM-DD`.
pub type Days = [String; 3];

/// Fills [`Days`] from a clock: `clock(n)` is the date `n` days from today.
pub fn days(clock: fn(i64) -> String) -> Days {
    [clock(0), clock(1), clock(-1)]
}

/// An open menu. Its trigger sits at byte `trigger` of line `line`; the text
/// typed after it is the filter (for Ctrl+/ the menu's own `filter`).
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub kind: Kind,
    pub line: usize,
    pub trigger: usize,
    pub selected: usize,
    /// Ctrl+/ only: what was typed into the menu.
    pub filter: String,
    /// `@` only; fixed when it opens, so a menu open over midnight stays consistent.
    pub days: Days,
}

impl Menu {
    pub fn new(kind: Kind, line: usize, trigger: usize, days: Days) -> Menu {
        Menu {
            kind,
            line,
            trigger,
            selected: 0,
            filter: String::new(),
            days,
        }
    }

    /// The filter text, or None if the cursor (at byte `col` of `line`) left
    /// it: moved off the line, or backspaced into the trigger.
    pub fn query(&self, text: &str, line: usize, col: usize) -> Option<String> {
        if self.kind == Kind::TurnInto {
            return Some(self.filter.clone());
        }
        let t = self.kind.trigger();
        let l = text.split('\n').nth(self.line)?;
        let from = self.trigger + t.len();
        if line != self.line || col < from || !l.get(self.trigger..)?.starts_with(t) {
            return None;
        }
        let q = l.get(from..col)?;
        // `@ ` and `[[ ` are text, not a search (`/` closes the same way: no match)
        if self.kind != Kind::Slash && q.starts_with(char::is_whitespace) {
            return None;
        }
        Some(q.to_string())
    }

    /// `text` with the trigger and the filter (up to the cursor at `col`)
    /// replaced by `with`.
    pub fn replace(&self, text: &str, col: usize, with: &str) -> String {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if let Some(l) = lines.get_mut(self.line) {
            l.replace_range(self.trigger..col.min(l.len()), with);
        }
        lines.join("\n")
    }
}

/// What a menu entry does.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// Convert the row to this block type.
    Block(TargetKind),
    /// Put this Markdown at the trigger (a mention token).
    Insert(String),
    /// Set the block's Notion colour (`red`, `blue_background`, ...).
    Color(String),
    /// `/page`: a new subpage at the cursor.
    NewPage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub label: String,
    pub hint: String,
    pub pick: Pick,
}

/// What the page entries are drawn from.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    /// All cached pages (the sidebar's).
    pub tree: &'a [PageEntry],
    /// Recently opened page ids, most recent first.
    pub recent: &'a [String],
    /// The cached workspace users.
    pub users: &'a [User],
}

/// The entries of `menu` for the filter `q`.
pub fn entries(menu: &Menu, q: &str, src: Sources) -> Vec<Entry> {
    match menu.kind {
        Kind::Slash | Kind::TurnInto => {
            let mut out: Vec<Entry> = slash::filter(q)
                .into_iter()
                .map(|i| Entry {
                    label: i.name.to_string(),
                    hint: i.hint.to_string(),
                    pick: Pick::Block(i.kind),
                })
                .collect();
            // not in "Turn into": a block can't become a page
            if menu.kind == Kind::Slash && (slash::word_prefix(q, "Page") || slash::word_prefix(q, "Seite")) {
                out.push(Entry { label: "Page".into(), hint: "A new subpage".into(), pick: Pick::NewPage });
            }
            out.extend(color_entries(q));
            out
        }
        Kind::Mention => {
            let mut out = page_entries(q, src);
            out.extend(user_entries(q, src.users));
            out.extend(date_entries(q, &menu.days));
            out
        }
        Kind::PageLink => page_entries(q, src),
    }
}

/// German names of [`COLORS`], in the same order.
const COLOR_DE: [&str; 10] = [
    "Standard", "Grau", "Braun", "Orange", "Gelb", "Grün", "Blau", "Lila", "Rosa", "Rot",
];

/// "Color Red" / "Color Red background" entries (German: "Farbe Rot" /
/// "Hintergrund") for a query of at least 3 characters: all 19 would bury the
/// block types on a short one.
fn color_entries(q: &str) -> Vec<Entry> {
    if q.trim().chars().count() < 3 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for bg in [false, true] {
        for (c, de) in COLORS.iter().zip(COLOR_DE) {
            if bg && *c == "default" {
                continue;
            }
            let cap = format!("{}{}", c[..1].to_uppercase(), &c[1..]);
            let (en, de) = if bg {
                (format!("Color {cap} background"), format!("Farbe {de} Hintergrund"))
            } else {
                (format!("Color {cap}"), format!("Farbe {de}"))
            };
            if slash::word_prefix(q, &en) || slash::word_prefix(q, &de) {
                out.push(Entry {
                    label: en,
                    hint: if bg { "Background" } else { "Text colour" }.into(),
                    pick: Pick::Color(if bg { format!("{c}_background") } else { (*c).into() }),
                });
            }
        }
    }
    out
}

/// The Markdown token of a mention (core's `@[label](kind:id)`).
fn mention(label: &str, json: serde_json::Value) -> String {
    to_markdown(&[RichText {
        plain_text: label.to_string(),
        href: None,
        annotations: Annotations::default(),
        kind: RichTextKind::Mention(json),
    }])
}

/// How well `title` matches the folded query `fq`: a prefix first (0), then a
/// word prefix, a substring, a subsequence (fuzzy). None for no match.
fn rank(fq: &str, title: &str) -> Option<u8> {
    let t = slash::fold(title);
    if t.starts_with(fq) {
        Some(0)
    } else if slash::word_prefix(fq, title) {
        Some(1)
    } else if t.contains(fq) {
        Some(2)
    } else {
        let mut cs = t.chars();
        fq.chars().all(|c| cs.any(|x| x == c)).then_some(3)
    }
}

/// Cached pages whose title matches `q` (see [`rank`]); recent pages before the rest.
fn page_entries(q: &str, src: Sources) -> Vec<Entry> {
    let fq = slash::fold(q.trim());
    let mut hits: Vec<(u8, usize, String, &PageEntry)> = src
        .tree
        .iter()
        .filter(|e| !e.database)
        .filter_map(|e| {
            let recent = src.recent.iter().position(|r| *r == e.id).unwrap_or(usize::MAX);
            Some((rank(&fq, &e.title)?, recent, slash::fold(&e.title), e))
        })
        .collect();
    // with no query every rank is 0: the recent pages lead
    hits.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    hits.into_iter()
        .take(MAX_PAGES)
        .map(|(.., e)| {
            let label = if e.title.is_empty() { "Untitled" } else { &e.title };
            Entry {
                label: format!("{} {label}", e.icon.as_deref().unwrap_or("📄")),
                hint: "Page".into(),
                pick: Pick::Insert(mention(
                    label,
                    json!({ "type": "page", "page": { "id": e.id } }),
                )),
            }
        })
        .collect()
}

/// Cached users whose name matches `q` like page titles do (umlauts fold):
/// persons first, then bots. Users known only by id have no name to match.
fn user_entries(q: &str, users: &[User]) -> Vec<Entry> {
    let fq = slash::fold(q.trim());
    let mut hits: Vec<(bool, u8, String, &User)> = users
        .iter()
        .filter(|u| !u.name.is_empty())
        .filter_map(|u| Some((u.is_bot(), rank(&fq, &u.name)?, slash::fold(&u.name), u)))
        .collect();
    hits.sort_by(|a, b| (a.0, a.1, &a.2, &a.3.id).cmp(&(b.0, b.1, &b.2, &b.3.id)));
    hits.into_iter()
        .take(MAX_USERS)
        .map(|(bot, .., u)| Entry {
            label: format!("{} {}", if bot { "🤖" } else { "👤" }, u.name),
            hint: if bot { "Bot" } else { "Person" }.into(),
            pick: Pick::Insert(mention(
                &u.name,
                json!({ "type": "user", "user": { "object": "user", "id": u.id } }),
            )),
        })
        .collect()
}

/// Heute / Morgen / Gestern (and the English names) as date mentions.
fn date_entries(q: &str, days: &Days) -> Vec<Entry> {
    [("Heute", "Today", 0), ("Morgen", "Tomorrow", 1), ("Gestern", "Yesterday", 2)]
        .into_iter()
        .filter(|(de, en, _)| slash::word_prefix(q, de) || slash::word_prefix(q, en))
        .map(|(de, _, n)| {
            let iso = days[n].clone();
            Entry {
                label: format!("📅 {de}"),
                hint: iso.clone(),
                pick: Pick::Insert(mention(
                    &iso,
                    json!({ "type": "date", "date": { "start": iso, "end": null } }),
                )),
            }
        })
        .collect()
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Eaten by the menu (highlight moved, or the key is ignored).
    Handled,
    /// Let the text edit have the key; the menu re-checks its query after.
    Pass,
    Pick(Pick),
    Close,
}

/// One key on the open menu with `entries` listed. Up/Down wrap around.
pub fn menu_step(menu: &mut Menu, entries: &[Entry], key: EditKey) -> Outcome {
    let n = entries.len();
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
        MenuKey::Pick => entries
            .get(sel)
            .map_or(Outcome::Close, |e| Outcome::Pick(e.pick.clone())),
        MenuKey::Close => Outcome::Close,
        MenuKey::Editor => Outcome::Pass,
    }
}

/// Draws the popup at `pos` (just under the trigger or the cursor).
pub fn show(ctx: &egui::Context, pos: egui::Pos2, menu: &Menu, entries: &[Entry]) {
    let hl = ctx.global_style().visuals.selection.bg_fill;
    egui::Area::new(Id::unique("block-menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(260.0);
                if menu.kind == Kind::TurnInto {
                    ui.add(Label::new(Text::new(format!("Turn into  {}", menu.filter)).weak()));
                }
                for (n, e) in entries.iter().enumerate() {
                    let mut t = Text::new(format!("{}   {}", e.label, e.hint));
                    if n == menu.selected {
                        t = t.strong().background_color(hl);
                    }
                    ui.add(Label::new(t).selectable(false));
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::page_in;

    fn today() -> Days {
        ["2026-11-20".into(), "2026-11-21".into(), "2026-11-19".into()]
    }

    fn menu(kind: Kind) -> Menu {
        Menu::new(kind, 0, 0, today())
    }

    fn page(id: &str, title: &str) -> PageEntry {
        PageEntry {
            id: id.into(),
            title: title.into(),
            parent: None,
            icon: None,
            database: false,
        }
    }

    fn labels(es: &[Entry]) -> Vec<String> {
        es.iter().map(|e| e.label.clone()).collect()
    }

    fn slash_entries(q: &str) -> Vec<Entry> {
        entries(&menu(Kind::Slash), q, Sources { tree: &[], recent: &[], users: &[] })
    }

    #[test]
    fn enter_picks_the_filtered_item() {
        let mut m = menu(Kind::Slash);
        assert_eq!(
            menu_step(&mut m, &slash_entries("h2"), EditKey::Enter),
            Outcome::Pick(Pick::Block(TargetKind::Heading2))
        );
        assert_eq!(
            menu_step(&mut m, &slash_entries(""), EditKey::Enter),
            Outcome::Pick(Pick::Block(TargetKind::Paragraph)),
            "empty query: first item"
        );
    }

    #[test]
    fn arrows_move_and_wrap() {
        let mut m = menu(Kind::Slash);
        let es = slash_entries("head"); // Heading 1..3, Toggle heading 1..3
        assert_eq!(es.len(), 6);
        assert_eq!(menu_step(&mut m, &es, EditKey::Down), Outcome::Handled);
        assert_eq!(m.selected, 1);
        for _ in 0..5 {
            menu_step(&mut m, &es, EditKey::Down);
        }
        assert_eq!(m.selected, 0, "wraps past the last item");
        menu_step(&mut m, &es, EditKey::Up);
        assert_eq!(m.selected, 5, "wraps before the first item");
        assert_eq!(
            menu_step(&mut m, &es, EditKey::Enter),
            Outcome::Pick(Pick::Block(TargetKind::ToggleHeading3))
        );
    }

    #[test]
    fn stale_selection_is_clamped_to_the_filtered_list() {
        let mut m = Menu {
            selected: 9,
            ..menu(Kind::Slash)
        };
        assert_eq!(
            menu_step(&mut m, &slash_entries("h1"), EditKey::Enter),
            Outcome::Pick(Pick::Block(TargetKind::Heading1))
        );
    }

    #[test]
    fn esc_closes_tab_is_ignored_backspace_goes_to_the_editor() {
        let mut m = menu(Kind::Slash);
        let es = slash_entries("x");
        assert_eq!(menu_step(&mut m, &es, EditKey::Escape), Outcome::Close);
        assert_eq!(menu_step(&mut m, &es, EditKey::Indent), Outcome::Handled);
        assert_eq!(menu_step(&mut m, &es, EditKey::Backspace), Outcome::Pass);
    }

    #[test]
    fn no_match_closes_on_enter_and_arrows_do_nothing() {
        let mut m = menu(Kind::Slash);
        let es = slash_entries("zzz");
        assert_eq!(menu_step(&mut m, &es, EditKey::Down), Outcome::Handled);
        assert_eq!(menu_step(&mut m, &es, EditKey::Enter), Outcome::Close);
    }

    #[test]
    fn the_query_follows_the_cursor_for_each_trigger() {
        let mut m = Menu::new(Kind::Slash, 0, 2, today());
        assert_eq!(m.query("ab/h1", 0, 5).as_deref(), Some("h1"));
        assert_eq!(m.query("ab/", 0, 3).as_deref(), Some(""));
        assert_eq!(m.query("ab", 0, 2), None, "backspaced past /");
        assert_eq!(m.query("ab/h1\nx", 1, 1), None, "other line");
        assert_eq!(m.replace("ab/h1cd", 5, ""), "abcd");
        m.kind = Kind::Mention;
        m.trigger = 0;
        assert_eq!(m.query("@ho", 0, 3).as_deref(), Some("ho"));
        assert_eq!(m.query("@", 0, 0), None);
        assert_eq!(m.query("x", 0, 1), None, "no @ at the trigger");
        assert_eq!(m.query("@ ho", 0, 4), None, "a space right after @ closes it");
        assert_eq!(m.query("@ho x", 0, 5).as_deref(), Some("ho x"), "spaces later are fine");
        m.kind = Kind::PageLink;
        assert_eq!(m.query("[[ a", 0, 4), None);
        assert_eq!(m.query("[[a", 0, 3).as_deref(), Some("a"));
        assert_eq!(m.query("[[", 0, 2).as_deref(), Some(""));
        assert_eq!(m.query("[[", 0, 1), None, "backspaced into [[");
        assert_eq!(m.replace("[[ab c", 4, "X"), "X c");
        m.kind = Kind::TurnInto;
        m.filter = "h2".into();
        assert_eq!(m.query("anything", 3, 7).as_deref(), Some("h2"));
    }

    #[test]
    fn pages_match_by_prefix_then_fuzzy_recent_first() {
        let tree = [
            page("a", "Mathe Notizen"),
            page("b", "Hausaufgaben"),
            page("c", "Notizen Mathe"),
            page("d", "Übersicht"),
            page("e", ""),
        ];
        let recent = ["c".to_string()];
        let src = Sources { tree: &tree, recent: &recent, users: &[] };
        let q = |s: &str| labels(&page_entries(s, src));
        assert_eq!(q("")[0], "📄 Notizen Mathe", "the recent page leads");
        assert_eq!(q("math"), ["📄 Mathe Notizen", "📄 Notizen Mathe"], "prefix before word prefix");
        assert_eq!(q("ueber"), ["📄 Übersicht"], "umlauts fold");
        assert_eq!(q("hsfg"), ["📄 Hausaufgaben"], "fuzzy subsequence");
        assert!(q("zzz").is_empty());
        assert!(q("").contains(&"📄 Untitled".to_string()));
        // a token that core reads back as a page mention
        let Pick::Insert(t) = &page_entries("haus", src)[0].pick else { panic!() };
        assert_eq!(t, "@[Hausaufgaben](page:b)");
    }

    #[test]
    fn mention_tokens_escape_the_label() {
        let tree = [page("x", "a]b *c*")];
        let es = page_entries("a", Sources { tree: &tree, recent: &[], users: &[] });
        let Pick::Insert(t) = &es[0].pick else { panic!() };
        assert_eq!(t, "@[a\\]b \\*c\\*](page:x)");
    }

    #[test]
    fn dates_are_german_or_english() {
        let src = Sources { tree: &[], recent: &[], users: &[] };
        let m = menu(Kind::Mention);
        let get = |q: &str| entries(&m, q, src);
        assert_eq!(labels(&get("")).len(), 3);
        let heute = get("heu");
        assert_eq!(heute[0].pick, Pick::Insert("@[2026-11-20](date:2026-11-20)".into()));
        assert_eq!(get("Morgen")[0].hint, "2026-11-21");
        assert_eq!(get("tomorrow")[0].hint, "2026-11-21");
        assert_eq!(get("gest")[0].hint, "2026-11-19");
        assert_eq!(get("yes")[0].hint, "2026-11-19");
        assert!(get("zzz").is_empty());
        // [[ lists pages only
        let link = entries(&menu(Kind::PageLink), "", src);
        assert!(link.is_empty());
    }

    fn person(id: &str, name: &str) -> User {
        User::from_value(&json!({ "object": "user", "id": id, "type": "person", "name": name })).unwrap()
    }

    fn bot(id: &str, name: &str) -> User {
        User::from_value(&json!({ "object": "user", "id": id, "type": "bot", "name": name })).unwrap()
    }

    #[test]
    fn users_match_like_pages_persons_before_bots() {
        let users = [
            bot("b1", "Zeta Bot"),
            person("u1", "Zoe"),
            person("u2", "Jürgen Müller"),
            person("u3", "Nick Zane"),
            person("u4", ""),
            User::from_value(&json!({ "object": "user", "id": "u5" })).unwrap(),
        ];
        let q = |s: &str| user_entries(s, &users);
        assert_eq!(labels(&q("")), ["👤 Jürgen Müller", "👤 Nick Zane", "👤 Zoe", "🤖 Zeta Bot"], "persons first; no name, no entry");
        assert_eq!(labels(&q("z")), ["👤 Zoe", "👤 Nick Zane", "🤖 Zeta Bot"], "prefix, then word prefix; bots last");
        assert_eq!(labels(&q("juergen")), ["👤 Jürgen Müller"], "umlauts fold");
        assert_eq!(labels(&q("mueller")), ["👤 Jürgen Müller"], "a word prefix");
        assert_eq!(labels(&q("jmr")), ["👤 Jürgen Müller"], "fuzzy");
        assert!(q("qqq").is_empty());
        let Pick::Insert(t) = &q("zoe")[0].pick else { panic!() };
        assert_eq!(t, "@[Zoe](user:u1)");
        assert_eq!(q("zoe")[0].hint, "Person");
        assert_eq!(q("zeta")[0].hint, "Bot");
        // the `@` menu lists pages, then users, then dates
        let src = Sources { tree: &[page("a", "Zebra")], recent: &[], users: &users };
        let all = entries(&menu(Kind::Mention), "ze", src);
        assert_eq!(labels(&all)[..4], ["📄 Zebra", "👤 Nick Zane", "👤 Zoe", "🤖 Zeta Bot"]);
        // `[[` and `/` never list users
        assert!(entries(&menu(Kind::PageLink), "zoe", src).is_empty());
        let many: Vec<User> = (0..20).map(|i| person(&format!("u{i}"), &format!("Pat {i}"))).collect();
        assert_eq!(user_entries("", &many).len(), MAX_USERS);
    }

    #[test]
    fn days_come_from_the_clock() {
        assert_eq!(days(|n| format!("d{n}")), ["d0", "d1", "d-1"]);
    }

    #[test]
    fn testkit_pages_have_their_id_as_title() {
        assert_eq!(page_in("Home", None).title(), "Home");
    }
}
