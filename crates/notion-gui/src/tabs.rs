//! The tab list (pure, no iced). The active tab's state lives in `State`
//! (`current`, `open`, `scroll`); its slot here is a stale copy, written on
//! switch-out by [`switch`].

use std::collections::HashSet;

/// A page in a tab, plus what the view needs to come back to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tab {
    /// None: an empty "New tab".
    pub page: Option<String>,
    /// Vertical scroll offset of the page.
    pub scroll: f32,
    /// Expanded toggles.
    pub open: HashSet<String>,
    /// The page title when parked, for pages the sidebar doesn't list.
    pub title: String,
}

/// Parks `live` in tab `from` and hands back tab `to`'s state.
pub fn switch(tabs: &mut [Tab], from: usize, to: usize, live: Tab) -> Tab {
    tabs[from] = live;
    tabs[to].clone()
}

/// Removes tab `i`; returns the new active index. Closing the active tab
/// activates its right neighbour, else its left; closing the last one leaves
/// a single empty tab.
pub fn close(tabs: &mut Vec<Tab>, active: usize, i: usize) -> usize {
    tabs.remove(i);
    if tabs.is_empty() {
        tabs.push(Tab::default());
        return 0;
    }
    match i.cmp(&active) {
        std::cmp::Ordering::Less => active - 1,
        std::cmp::Ordering::Equal => i.min(tabs.len() - 1),
        std::cmp::Ordering::Greater => active,
    }
}

/// The tab `step` places from `active`, wrapping around.
pub fn cycle(len: usize, active: usize, step: isize) -> usize {
    (active as isize + step).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(p: &str) -> Tab {
        Tab {
            page: Some(p.into()),
            ..Tab::default()
        }
    }

    fn pages(tabs: &[Tab]) -> Vec<&str> {
        tabs.iter()
            .map(|t| t.page.as_deref().unwrap_or("-"))
            .collect()
    }

    #[test]
    fn close_picks_the_neighbour() {
        let mut v = vec![t("a"), t("b"), t("c")];
        // middle, active: the right neighbour moves into its place
        assert_eq!(close(&mut v, 1, 1), 1);
        assert_eq!(pages(&v), ["a", "c"]);
        // active rightmost: the left one
        assert_eq!(close(&mut v, 1, 1), 0);
        assert_eq!(pages(&v), ["a"]);
        // the last: one empty tab
        assert_eq!(close(&mut v, 0, 0), 0);
        assert_eq!(v, [Tab::default()]);
    }

    #[test]
    fn closing_a_background_tab_keeps_the_active_one() {
        let mut v = vec![t("a"), t("b"), t("c")];
        assert_eq!(close(&mut v, 1, 0), 0, "left of it: index shifts");
        assert_eq!(pages(&v), ["b", "c"]);
        let mut v = vec![t("a"), t("b"), t("c")];
        assert_eq!(close(&mut v, 1, 2), 1, "right of it: unchanged");
    }

    #[test]
    fn cycle_wraps() {
        assert_eq!(cycle(3, 2, 1), 0);
        assert_eq!(cycle(3, 0, -1), 2);
        assert_eq!(cycle(3, 1, 1), 2);
        assert_eq!(cycle(1, 0, 1), 0);
    }

    #[test]
    fn switch_parks_and_restores() {
        let mut v = vec![t("a"), t("b")];
        let mut live = t("a2");
        live.scroll = 120.0;
        live.open.insert("tog".into());
        let got = switch(&mut v, 0, 1, live.clone());
        assert_eq!(got, t("b"));
        assert_eq!(v[0], live, "the live state is parked");
        // and back
        assert_eq!(switch(&mut v, 1, 0, got), live);
    }
}
