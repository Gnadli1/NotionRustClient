//! The sidebar's page tree: nesting, order and expand state (pure, no iced).

use std::collections::{HashMap, HashSet};

use notion_core::store::PageEntry;

/// One visible sidebar row.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow<'a> {
    pub entry: &'a PageEntry,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
}

/// The visible rows of the tree built from `entries`: an entry whose parent
/// isn't listed is top level; children show only under an expanded parent.
/// Siblings sort by title, case-insensitive, then id.
// ponytail: alphabetical at every level; Notion orders child pages by their
// block position, add that via the store if it matters
pub fn rows<'a>(entries: &'a [PageEntry], expanded: &HashSet<String>) -> Vec<TreeRow<'a>> {
    let key = |i: &usize| (entries[*i].title.to_lowercase(), &entries[*i].id);
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut roots = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e.parent.as_deref().and_then(|p| index.get(p)) {
            Some(&p) if p != i => kids[p].push(i),
            _ => roots.push(i),
        }
    }
    for k in &mut kids {
        k.sort_by_cached_key(key);
    }
    roots.sort_by_cached_key(key);

    struct Walk<'a, 'b> {
        entries: &'a [PageEntry],
        kids: &'b [Vec<usize>],
        expanded: &'b HashSet<String>,
        seen: Vec<bool>,
        out: Vec<TreeRow<'a>>,
    }
    impl<'a> Walk<'a, '_> {
        fn go(&mut self, i: usize, depth: usize, visible: bool) {
            self.seen[i] = true;
            let e = &self.entries[i];
            let has_children = self.kids[i].iter().any(|&k| !self.seen[k]);
            let open = self.expanded.contains(&e.id);
            if visible {
                self.out.push(TreeRow {
                    entry: e,
                    depth,
                    has_children,
                    expanded: open && has_children,
                });
            }
            for &k in self.kids[i].iter() {
                // the guard against parent cycles
                if !self.seen[k] {
                    self.go(k, depth + 1, visible && open);
                }
            }
        }
    }
    let mut w = Walk {
        entries,
        kids: &kids,
        expanded,
        seen: vec![false; entries.len()],
        out: Vec::new(),
    };
    for i in roots {
        w.go(i, 0, true);
    }
    // a cycle has no root: its first member (by title) becomes one
    let mut rest: Vec<usize> = (0..entries.len()).collect();
    rest.sort_by_cached_key(key);
    for i in rest {
        if !w.seen[i] {
            w.go(i, 0, true);
        }
    }
    w.out
}

/// The ids of the pages above `id`, nearest first; stops at a cycle.
pub fn ancestors<'a>(entries: &'a [PageEntry], id: &str) -> Vec<&'a str> {
    let parent: HashMap<&str, &str> = entries
        .iter()
        .filter_map(|e| Some((e.id.as_str(), e.parent.as_deref()?)))
        .collect();
    let mut out: Vec<&str> = Vec::new();
    let mut at = id;
    while let Some(&p) = parent.get(at) {
        if p == id || out.contains(&p) {
            break;
        }
        out.push(p);
        at = p;
    }
    out
}

/// The breadcrumb above page `id`: its listed ancestors, root first.
pub fn trail<'a>(entries: &'a [PageEntry], id: &str) -> Vec<&'a PageEntry> {
    ancestors(entries, id)
        .into_iter()
        .rev()
        .filter_map(|a| entries.iter().find(|e| e.id == a))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, title: &str, parent: Option<&str>) -> PageEntry {
        PageEntry {
            id: id.into(),
            title: title.into(),
            parent: parent.map(Into::into),
            icon: None,
            database: false,
        }
    }

    fn shown(entries: &[PageEntry], open: &[&str]) -> Vec<(String, usize, bool, bool)> {
        let open = open.iter().map(|s| s.to_string()).collect();
        rows(entries, &open)
            .into_iter()
            .map(|r| (r.entry.id.clone(), r.depth, r.has_children, r.expanded))
            .collect()
    }

    fn row(id: &str, depth: usize, kids: bool, open: bool) -> (String, usize, bool, bool) {
        (id.into(), depth, kids, open)
    }

    #[test]
    fn nests_sorts_and_hides_collapsed_children() {
        let es = [
            e("z", "zeta", None),
            e("a", "Alpha", None),
            e("a2", "beta", Some("a")),
            e("a1", "Beta", Some("a")),
            e("a11", "deep", Some("a1")),
            // parent not listed (uncached, or a filtered row): top level
            e("o", "orphan", Some("missing")),
        ];
        assert_eq!(
            shown(&es, &[]),
            [
                row("a", 0, true, false),
                row("o", 0, false, false),
                row("z", 0, false, false)
            ]
        );
        assert_eq!(
            shown(&es, &["a"]),
            [
                row("a", 0, true, true),
                row("a1", 1, true, false),
                row("a2", 1, false, false),
                row("o", 0, false, false),
                row("z", 0, false, false),
            ]
        );
        // an expanded child under a collapsed parent stays hidden
        assert_eq!(shown(&es, &["a1"]).len(), 3);
        assert_eq!(shown(&es, &["a", "a1"])[2], row("a11", 2, false, false));
        assert_eq!(ancestors(&es, "a11"), ["a1", "a"]);
        assert!(ancestors(&es, "a").is_empty());
    }

    #[test]
    fn rows_nest_under_their_database() {
        let db = PageEntry {
            database: true,
            ..e("db", "Grades", None)
        };
        let es = [
            e("r2", "b row", Some("db")),
            db,
            e("r1", "A row", Some("db")),
            e("p", "Page", None),
        ];
        // collapsed: the database is a top-level entry with rows hidden
        assert_eq!(
            shown(&es, &[]),
            [row("db", 0, true, false), row("p", 0, false, false)]
        );
        assert_eq!(
            shown(&es, &["db"]),
            [
                row("db", 0, true, true),
                row("r1", 1, false, false),
                row("r2", 1, false, false),
                row("p", 0, false, false)
            ]
        );
        assert!(rows(&es, &Default::default())[0].entry.database);
    }

    #[test]
    fn parent_cycles_terminate_and_stay_visible() {
        let es = [
            e("x", "X", Some("y")),
            e("y", "Y", Some("x")),
            e("s", "self", Some("s")),
        ];
        let all = shown(&es, &["x", "y", "s"]);
        assert_eq!(
            all,
            [
                row("s", 0, false, false),
                row("x", 0, true, true),
                row("y", 1, false, false)
            ]
        );
        assert_eq!(ancestors(&es, "x"), ["y"]);
        assert!(ancestors(&es, "s").is_empty());
    }
}
