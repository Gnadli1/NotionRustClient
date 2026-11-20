//! The Ctrl+K switcher's state and keys (drawing is in `view::switcher`).
//! Empty query: the recent pages. Otherwise: the cached pages whose title
//! contains the query, plus Notion's title search (sent ~300 ms after the
//! last keystroke) merged in.

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
            typed_at: Instant::now(),
            searched: String::new(),
        }
    }
}

fn item(e: &PageEntry) -> Item {
    Item {
        id: e.id.clone(),
        icon: e.icon.clone().unwrap_or_else(|| "📄".into()),
        title: e.title.clone(),
    }
}

impl Switcher {
    /// What the list shows now.
    pub fn items(&self, tree: &[PageEntry], recent: &[String]) -> Vec<Item> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return recent
                .iter()
                .filter_map(|id| tree.iter().find(|e| e.id == *id))
                .map(item)
                .collect();
        }
        let mut local: Vec<(bool, String, &PageEntry)> = tree
            .iter()
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
            .map(|(_, _, e)| item(e))
            .collect();
        for (id, title) in &self.results {
            if !out.iter().any(|i| i.id == *id) {
                let icon = tree
                    .iter()
                    .find(|e| e.id == *id)
                    .and_then(|e| e.icon.clone())
                    .unwrap_or_else(|| "📄".into());
                out.push(Item {
                    id: id.clone(),
                    icon,
                    title: title.clone(),
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
