//! The Ctrl+K switcher's state and keys (drawing is in `view::switcher`).
//! Empty query: the recent pages, grouped by day. Otherwise: the cached pages whose title
//! contains the query, plus Notion's title search (sent ~300 ms after the
//! last keystroke) merged in.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui::{Context, Key, Modifiers};
use notion_core::store::PageEntry;

/// Wait this long after the last keystroke before asking Notion.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Local matches shown.
const MAX_LOCAL: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub icon: String,
    pub title: String,
    /// The pages above it, root first: "A › B" (empty at top level).
    pub path: String,
    /// The day header to draw above it (the recent list only).
    pub group: Option<&'static str>,
}

/// The header for a page of local day `day` seen on day `today` (day
/// numbers: consecutive days differ by 1).
pub fn group(today: i64, day: i64) -> &'static str {
    match today - day {
        ..=0 => "Heute",
        1 => "Gestern",
        2..=7 => "Vergangene Woche",
        _ => "Älter",
    }
}

#[derive(Debug, PartialEq)]
pub enum Out {
    None,
    Close,
    Pick { id: String, new_tab: bool },
}

pub struct Switcher {
    pub query: String,
    pub sel: usize,
    /// Notion's hits for the last search sent: (page id, title).
    pub results: Vec<(String, String)>,
    /// The selection moved by key: the list scrolls to it once.
    pub moved: bool,
    /// Today and the local day each recent page counts for, set at open.
    pub today: i64,
    pub days: HashMap<String, i64>,
    /// Page id to path, see [`paths`].
    pub paths: HashMap<String, String>,
    /// The selected page's first rows (id, lines), loaded when the selection
    /// changes.
    pub preview: Option<(String, Vec<String>)>,
    typed_at: Instant,
    searched: String,
}

impl Default for Switcher {
    fn default() -> Self {
        Switcher {
            query: String::new(),
            sel: 0,
            results: Vec::new(),
            moved: false,
            today: 0,
            days: HashMap::new(),
            paths: HashMap::new(),
            preview: None,
            typed_at: Instant::now(),
            searched: String::new(),
        }
    }
}

/// Each page's path ("A › B": the pages above it, root first), built once
/// when the switcher opens.
pub fn paths(tree: &[PageEntry]) -> HashMap<String, String> {
    tree.iter()
        .map(|e| {
            let up: Vec<&str> = crate::sidebar::trail(tree, &e.id)
                .into_iter()
                .map(|a| crate::view::untitled(&a.title))
                .collect();
            (e.id.clone(), up.join(" › "))
        })
        .collect()
}

fn item(paths: &HashMap<String, String>, e: &PageEntry) -> Item {
    Item {
        id: e.id.clone(),
        icon: e.icon.clone().unwrap_or_else(|| "📄".into()),
        title: e.title.clone(),
        path: paths.get(&e.id).cloned().unwrap_or_default(),
        group: None,
    }
}

impl Switcher {
    /// What the list shows now.
    pub fn items(&self, tree: &[PageEntry], recent: &[String]) -> Vec<Item> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            // the recent order stays; a page can't be newer than the one before
            // it (a page's edit day stands in for pages not opened this session)
            let mut day = self.today;
            return recent
                .iter()
                .filter_map(|id| tree.iter().find(|e| e.id == *id))
                .map(|e| {
                    day = day.min(self.days.get(&e.id).copied().unwrap_or(day));
                    Item { group: Some(group(self.today, day)), ..item(&self.paths, e) }
                })
                .collect();
        }
        let mut local: Vec<(bool, String, &PageEntry)> = tree
            .iter()
            .filter(|e| !e.database)
            .filter_map(|e| {
                let t = e.title.to_lowercase();
                t.contains(&q).then(|| (!t.starts_with(&q), t, e))
            })
            .collect();
        // titles that start with the query first
        local.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut out: Vec<Item> = local
            .into_iter()
            .take(MAX_LOCAL)
            .map(|(_, _, e)| item(&self.paths, e))
            .collect();
        for (id, title) in &self.results {
            if !out.iter().any(|i| i.id == *id) {
                let known = tree.iter().find(|e| e.id == *id);
                out.push(Item {
                    id: id.clone(),
                    icon: known
                        .and_then(|e| e.icon.clone())
                        .unwrap_or_else(|| "📄".into()),
                    title: title.clone(),
                    path: self.paths.get(id).cloned().unwrap_or_default(),
                    group: None,
                });
            }
        }
        out
    }

    /// Takes the switcher's keys from the frame's input (before any widget
    /// runs): Esc closes, ↑/↓ move (wrapping), Enter opens, Ctrl+Enter opens
    /// in a new tab.
    // ponytail: the list is the one from before this frame's typing; a
    // keystroke and Enter in the same frame pick from the old list
    pub fn keys(&mut self, ctx: &Context, items: &[Item]) -> Out {
        let (esc, up, down, ctrl_enter, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp),
                i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown),
                // before plain Enter, which would not match a Ctrl one anyway
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        if esc {
            return Out::Close;
        }
        let n = items.len() as isize;
        if n > 0 && (up > 0 || down > 0) {
            self.sel = (self.sel.min(items.len() - 1) as isize + down as isize - up as isize)
                .rem_euclid(n) as usize;
            self.moved = true;
        }
        if ctrl_enter || enter {
            if let Some(it) = items.get(self.sel) {
                return Out::Pick {
                    id: it.id.clone(),
                    new_tab: ctrl_enter,
                };
            }
        }
        Out::None
    }

    /// The query was edited: back to the top, old hits dropped.
    pub fn query_changed(&mut self) {
        self.sel = 0;
        self.results.clear();
        self.typed_at = Instant::now();
    }

    /// The query to send to Notion now, once, if it was left alone for the
    /// debounce time.
    pub fn due_search(&mut self, now: Instant) -> Option<String> {
        let q = self.query.trim();
        if q.is_empty() || q == self.searched || now < self.typed_at + DEBOUNCE {
            return None;
        }
        self.searched = q.to_string();
        Some(self.searched.clone())
    }

    /// How long until [`due_search`](Self::due_search) fires, if one is pending.
    pub fn pending(&self, now: Instant) -> Option<Duration> {
        let q = self.query.trim();
        (!q.is_empty() && q != self.searched)
            .then(|| (self.typed_at + DEBOUNCE).saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, title: &str) -> PageEntry {
        PageEntry {
            id: id.into(),
            title: title.into(),
            parent: None,
            icon: (id == "b").then(|| "🚀".into()),
            database: id == "db",
        }
    }

    fn tree() -> Vec<PageEntry> {
        vec![e("a", "Alpha"), e("b", "Beta kit"), e("c", "Kitchen")]
    }

    fn ids(v: &[Item]) -> Vec<&str> {
        v.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn empty_query_lists_cached_recent_pages_in_order() {
        let sw = Switcher::default();
        let recent: Vec<String> = ["c", "gone", "a"].map(String::from).into();
        let items = sw.items(&tree(), &recent);
        assert_eq!(ids(&items), ["c", "a"], "uncached ids are skipped");
        assert_eq!(items[1].icon, "📄");
    }

    #[test]
    fn databases_are_not_listed_as_pages() {
        let mut t = tree();
        t.push(e("db", "Kit grades"));
        let sw = Switcher { query: "kit".into(), ..Default::default() };
        assert!(!ids(&sw.items(&t, &[])).contains(&"db"));
    }

    #[test]
    fn recency_groups() {
        let g = |d| group(100, d);
        assert_eq!(
            [101, 100, 99, 98, 93, 92, 0].map(g),
            ["Heute", "Heute", "Gestern", "Vergangene Woche", "Vergangene Woche", "Älter", "Älter"]
        );
    }

    #[test]
    fn empty_query_groups_by_day_in_recent_order_with_paths() {
        let mut t = tree();
        t.push(PageEntry { parent: Some("a".into()), ..e("d", "Delta") });
        t.push(PageEntry { parent: Some("d".into()), ..e("f", "Foxtrot") });
        let mut sw = Switcher { today: 100, paths: paths(&t), ..Default::default() };
        sw.days = [("a", 50), ("c", 100), ("d", 99), ("f", 97)]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        let recent: Vec<String> = ["c", "d", "f", "a"].map(String::from).into();
        let items = sw.items(&t, &recent);
        assert_eq!(ids(&items), ["c", "d", "f", "a"]);
        let g: Vec<_> = items.iter().map(|i| i.group).collect();
        assert_eq!(
            g,
            [Some("Heute"), Some("Gestern"), Some("Vergangene Woche"), Some("Älter")]
        );
        assert_eq!(items[2].path, "Alpha › Delta");
        assert_eq!(items[0].path, "");
        // a day never gets newer down the list
        let late: Vec<String> = ["a", "c"].map(String::from).into();
        let g: Vec<_> = sw.items(&t, &late).iter().map(|i| i.group).collect();
        assert_eq!(g, [Some("Älter"), Some("Älter")]);
        // a search is not grouped
        sw.query = "o".into();
        assert!(sw.items(&t, &recent).iter().all(|i| i.group.is_none()));
    }

    #[test]
    fn arrows_walk_the_pages_never_the_headers() {
        let ctx = Context::default();
        let mut sw = Switcher { today: 100, ..Default::default() };
        sw.days = [("a", 100), ("b", 99), ("c", 50)].map(|(k, v)| (k.to_string(), v)).into();
        let recent: Vec<String> = ["a", "b", "c"].map(String::from).into();
        let items = sw.items(&tree(), &recent);
        let press = |sw: &mut Switcher, k| {
            let ev = eframe::egui::Event::Key {
                key: k,
                physical_key: Some(k),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            };
            let raw = eframe::egui::RawInput { events: vec![ev], ..Default::default() };
            let mut out = Out::None;
            ctx.run_ui(raw, |ui| out = sw.keys(ui.ctx(), &items))
                .drop_without_applying_deltas();
            out
        };
        for want in [1, 2, 0, 1] {
            press(&mut sw, Key::ArrowDown);
            assert_eq!(sw.sel, want, "one page per press, 3 headers in between");
        }
        press(&mut sw, Key::ArrowUp);
        assert_eq!(sw.sel, 0);
        assert_eq!(press(&mut sw, Key::Enter), Out::Pick { id: "a".into(), new_tab: false });
    }

    #[test]
    fn typing_filters_locally_prefix_first_and_merges_remote_hits() {
        let mut sw = Switcher {
            query: "KIT".into(),
            ..Default::default()
        };
        assert_eq!(ids(&sw.items(&tree(), &[])), ["c", "b"], "prefix, then contains");
        sw.results = vec![
            ("b".into(), "Beta kit".into()),
            ("z".into(), "Remote only".into()),
        ];
        let items = sw.items(&tree(), &[]);
        assert_eq!(ids(&items), ["c", "b", "z"], "no duplicates");
        assert_eq!(items[1].icon, "🚀");
        assert_eq!(items[2].icon, "📄");
    }

    #[test]
    fn search_is_debounced_and_sent_once() {
        let mut sw = Switcher::default();
        let t0 = Instant::now();
        assert_eq!(sw.due_search(t0), None, "empty query");
        sw.query = "ki".into();
        sw.query_changed();
        let t = sw.typed_at;
        assert_eq!(sw.due_search(t + Duration::from_millis(100)), None);
        assert!(sw.pending(t + Duration::from_millis(100)).is_some());
        assert_eq!(
            sw.due_search(t + Duration::from_millis(300)).as_deref(),
            Some("ki")
        );
        assert_eq!(sw.due_search(t + Duration::from_secs(5)), None, "only once");
        assert_eq!(sw.pending(t + Duration::from_secs(5)), None);
        sw.query = "kit".into();
        sw.query_changed();
        assert!(sw.pending(Instant::now()).is_some());
    }
}
