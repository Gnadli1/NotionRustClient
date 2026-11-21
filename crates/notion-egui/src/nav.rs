//! Navigation state without any UI: a tab's page history, Ctrl+F's matches
//! and the outline's heading ticks.

use notion_core::model::plain_text;

use crate::outline::{Marker, Row};
use crate::slash::fold;

/// The pages one tab has shown, oldest first, and where in them it is.
/// In memory only.
#[derive(Debug, Default, PartialEq)]
pub struct History {
    pages: Vec<String>,
    pos: usize,
    /// The entry a back/forward asked for; `pos` moves there only when that
    /// page is really shown (a pull may fail, the tab may change).
    want: Option<usize>,
}

impl History {
    /// The tab opened `to` (from `from`, if it showed a page): what lay
    /// forward is dropped. Showing the page the history is at is no step, so
    /// going back/forward and then arriving there adds nothing.
    pub fn go(&mut self, from: Option<&str>, to: &str) {
        if let Some(w) = self.want.take() {
            if self.pages.get(w).is_some_and(|p| p == to) {
                self.pos = w;
                return;
            }
        }
        if self.pages.get(self.pos).is_some_and(|p| p == to) {
            return;
        }
        if self.pages.is_empty() {
            // a restored tab: its first page is the start
            self.pages.extend(from.map(String::from));
        } else {
            self.pages.truncate(self.pos + 1);
        }
        if self.pages.last().is_none_or(|p| p != to) {
            self.pages.push(to.to_string());
        }
        self.pos = self.pages.len() - 1;
    }

    /// The page one step back, if any. `pos` moves when `go` sees it shown.
    pub fn back(&mut self) -> Option<String> {
        self.want = Some(self.pos.checked_sub(1)?);
        self.want.and_then(|w| self.pages.get(w).cloned())
    }

    /// The page one step forward, if any.
    pub fn forward(&mut self) -> Option<String> {
        self.want = Some(self.pos + 1).filter(|w| *w < self.pages.len());
        self.want.and_then(|w| self.pages.get(w).cloned())
    }

    #[cfg(test)]
    pub fn pages(&self) -> &[String] {
        &self.pages
    }
}

/// Ctrl+F: the rows whose text contains the query.
#[derive(Debug, Default)]
pub struct Find {
    pub query: String,
    /// Row indexes, ascending.
    pub hits: Vec<usize>,
    /// Which of `hits` is current.
    pub cur: usize,
    /// The current row's block id: `cur` follows it when the rows change.
    cur_id: Option<String>,
    /// The text box takes the keyboard (set by Ctrl+F, once).
    pub focus: bool,
}

impl Find {
    /// Matches again over `rows` (case-insensitive, umlauts folded); `cur` stays in range.
    pub fn search(&mut self, rows: &[Row]) {
        let keep = self.cur_id.take();
        let q = fold(&self.query);
        self.hits = if q.is_empty() {
            Vec::new()
        } else {
            (0..rows.len())
                .filter(|&i| fold(&plain_text(&rows[i].rich_text)).contains(&q))
                .collect()
        };
        // stay on the same row if it still matches, else keep the position
        let same = keep.and_then(|id| self.hits.iter().position(|&i| rows[i].block_id == id));
        self.cur = same.unwrap_or_else(|| self.cur.min(self.hits.len().saturating_sub(1)));
        self.cur_id = self.hits.get(self.cur).map(|&i| rows[i].block_id.clone());
    }

    /// Back to the first match (the query changed).
    pub fn restart(&mut self, rows: &[Row]) {
        self.cur = 0;
        self.cur_id = None;
        self.search(rows);
    }

    /// Next (`1`) or previous (`-1`) match, wrapping; the row to show.
    pub fn step(&mut self, d: isize, rows: &[Row]) -> Option<usize> {
        let n = self.hits.len() as isize;
        if n > 0 {
            self.cur = (self.cur as isize + d).rem_euclid(n) as usize;
        }
        let row = self.hits.get(self.cur).copied();
        self.cur_id = row.map(|i| rows[i].block_id.clone());
        row
    }

    /// The tint strength for row `i`: strong for the current match.
    pub fn tint(&self, i: usize) -> Option<f32> {
        if self.hits.get(self.cur) == Some(&i) {
            Some(0.4)
        } else {
            self.hits.binary_search(&i).ok().map(|_| 0.15)
        }
    }
}

/// The outline's ticks: (row index, level, text) of each heading.
pub fn headings(rows: &[Row]) -> Vec<(usize, u8, String)> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| match r.marker {
            Marker::Heading(n) => Some((i, n, plain_text(&r.rich_text))),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{blk, rt};
    use notion_core::store::Store;
    use serde_json::json;

    fn h(items: &[&str]) -> History {
        let mut h = History::default();
        let mut from = None;
        for p in items {
            h.go(from, p);
            from = Some(p);
        }
        h
    }

    #[test]
    fn back_forward_and_the_ends() {
        let mut h = h(&["a", "b", "c"]);
        assert_eq!(h.forward(), None, "last entry");
        assert_eq!(h.back().as_deref(), Some("b"));
        h.go(Some("c"), "b"); // shown
        assert_eq!(h.back().as_deref(), Some("a"));
        h.go(Some("b"), "a");
        assert_eq!(h.back(), None, "first entry: nothing");
        assert_eq!(h.forward().as_deref(), Some("b"));
        h.go(Some("a"), "b");
        assert_eq!(h.forward().as_deref(), Some("c"));
    }

    #[test]
    fn a_step_that_never_arrives_changes_nothing() {
        let mut h = h(&["a", "b", "c"]);
        assert_eq!(h.back().as_deref(), Some("b"));
        assert_eq!(h.back().as_deref(), Some("b"), "still one step from c");
        // the tab opens x instead: the request is dropped
        h.go(Some("c"), "x");
        assert_eq!(h.pages(), ["a", "b", "c", "x"]);
        assert_eq!(h.back().as_deref(), Some("c"));
    }

    #[test]
    fn opening_a_page_after_back_drops_the_forward_branch() {
        let mut h = h(&["a", "b", "c"]);
        h.back();
        h.go(Some("c"), "b");
        h.back();
        h.go(Some("b"), "a");
        h.go(Some("a"), "x");
        assert_eq!(h.forward(), None);
        assert_eq!(h.back().as_deref(), Some("a"));
        // the same page again is no step
        let mut h2 = self::h(&["a"]);
        h2.go(Some("a"), "a");
        assert_eq!(h2.back(), None);
    }

    #[test]
    fn a_restored_tab_starts_from_its_page() {
        let mut h = History::default();
        h.go(Some("a"), "b");
        assert_eq!(h.back().as_deref(), Some("a"));
        // the restored tab shows its page again: one entry, not two
        let mut again = History::default();
        again.go(Some("a"), "a");
        assert_eq!(again.pages(), ["a"]);
        let mut empty = History::default();
        empty.go(None, "a");
        assert_eq!(empty.back(), None);
    }

    fn rows(texts: &[(&str, &str)]) -> Vec<Row> {
        let store = Store::open_in_memory().unwrap();
        let kids: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, (ty, t))| blk(&format!("b{i}"), "p", ty, json!({ "rich_text": rt(t) })))
            .collect();
        store.upsert_page(&crate::testkit::page_in("p", None)).unwrap();
        store.replace_children("p", "p", &kids).unwrap();
        crate::outline::flatten(&store, "p").unwrap()
    }

    #[test]
    fn find_counts_folds_and_wraps() {
        let r = rows(&[
            ("paragraph", "Grüße und Straße"),
            ("paragraph", "nothing"),
            ("paragraph", "GRUESSE again"),
            ("paragraph", "ÜBER"),
        ]);
        let mut f = Find { query: "gruesse".into(), ..Default::default() };
        f.search(&r);
        assert_eq!(f.hits, [0, 2], "umlaut and case folded");
        assert_eq!(f.cur, 0);
        assert_eq!(f.step(1, &r), Some(2));
        assert_eq!(f.step(1, &r), Some(0), "wraps forward");
        assert_eq!(f.step(-1, &r), Some(2), "wraps back");
        assert_eq!(f.tint(2), Some(0.4));
        assert_eq!(f.tint(0), Some(0.15));
        assert_eq!(f.tint(1), None);
        f.query = "ueber".into();
        f.search(&r);
        assert_eq!((f.hits.clone(), f.cur), (vec![3], 0), "cur stays in range");
        f.query.clear();
        f.search(&r);
        assert!(f.hits.is_empty());
        assert_eq!(f.step(1, &r), None);
    }

    #[test]
    fn the_current_match_follows_its_row_when_rows_move() {
        let r = rows(&[("paragraph", "hit"), ("paragraph", "hit"), ("paragraph", "hit")]);
        let mut f = Find { query: "hit".into(), ..Default::default() };
        f.search(&r);
        f.step(1, &r);
        assert_eq!(f.cur, 1);
        // the first row went away: the same row is now hit 0
        f.search(&r[1..]);
        assert_eq!((f.hits.clone(), f.cur), (vec![0, 1], 0));
    }

    #[test]
    fn headings_make_the_outline() {
        let r = rows(&[
            ("heading_1", "One"),
            ("paragraph", "text"),
            ("heading_3", "Deep"),
            ("heading_2", "Two"),
        ]);
        assert_eq!(
            headings(&r),
            [(0, 1, "One".to_string()), (2, 3, "Deep".to_string()), (3, 2, "Two".to_string())]
        );
    }
}
