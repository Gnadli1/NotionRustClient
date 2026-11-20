//! `session.json` in the config dir: open tabs, sidebar state, recent pages
//! and window size, written on exit and read at startup.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::tabs::Tab;

const FILE: &str = "session.json";
/// Recent pages kept.
pub const RECENT_MAX: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub sidebar: bool,
    /// Expanded sidebar pages.
    pub expanded: HashSet<String>,
    /// Most recent first.
    pub recent: Vec<String>,
    /// Window width and height.
    pub window: (f32, f32),
}

impl Default for Session {
    fn default() -> Self {
        Session {
            tabs: vec![Tab::default()],
            active: 0,
            sidebar: true,
            expanded: HashSet::new(),
            recent: Vec::new(),
            window: (1024.0, 768.0),
        }
    }
}

/// The saved session, or the default with a problem for the status bar (a
/// missing file is no problem). Out-of-range values are fixed up.
pub fn load(dir: &Path) -> (Session, Option<String>) {
    load_from(dir, FILE)
}

/// [`load`] from the file `file` in `dir` (notion-egui keeps its own).
pub fn load_from(dir: &Path, file: &str) -> (Session, Option<String>) {
    let path = dir.join(file);
    let (mut s, problem) = match std::fs::read_to_string(&path) {
        Ok(src) => match serde_json::from_str::<Session>(&src) {
            Ok(s) => (s, None),
            Err(e) => (
                Session::default(),
                Some(format!("{file}: {e}; starting fresh")),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Session::default(), None),
        Err(e) => (
            Session::default(),
            Some(format!("cannot read {}: {e}", path.display())),
        ),
    };
    if s.tabs.is_empty() {
        s.tabs.push(Tab::default());
    }
    s.active = s.active.min(s.tabs.len() - 1);
    s.recent.truncate(RECENT_MAX);
    s.window = clamp(s.window);
    (s, problem)
}

/// Writes atomically: a temp file next to it, then a rename.
pub fn save(dir: &Path, s: &Session) -> std::io::Result<()> {
    save_to(dir, FILE, s)
}

/// [`save`] to the file `file` in `dir`.
pub fn save_to(dir: &Path, file: &str, s: &Session) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{file}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(tmp, dir.join(file))
}

/// A window size that fits on a screen and leaves room for the UI.
pub fn clamp((w, h): (f32, f32)) -> (f32, f32) {
    let d = Session::default().window;
    let fix =
        |v: f32, def: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { def };
    (fix(w, d.0, 480.0, 7680.0), fix(h, d.1, 320.0, 4320.0))
}

/// Moves `id` to the front of `recent`, de-duplicated and capped.
pub fn touch(recent: &mut Vec<String>, id: &str) {
    recent.retain(|r| r != id);
    recent.insert(0, id.to_string());
    recent.truncate(RECENT_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("notion-client");
        assert_eq!(load(&sub), (Session::default(), None), "missing: defaults");

        let mut tab = Tab {
            page: Some("p".into()),
            scroll: 42.0,
            ..Tab::default()
        };
        tab.open.insert("tog".into());
        let s = Session {
            tabs: vec![tab, Tab::default()],
            active: 1,
            sidebar: false,
            expanded: ["x".to_string()].into(),
            recent: vec!["p".into(), "q".into()],
            window: (800.0, 600.0),
        };
        save(&sub, &s).unwrap();
        assert_eq!(load(&sub), (s, None));
        assert!(!sub.join("session.json.tmp").exists());

        std::fs::write(sub.join(FILE), "{ nope").unwrap();
        let (got, problem) = load(&sub);
        assert_eq!(got, Session::default());
        assert!(problem.unwrap().contains("starting fresh"));

        // fixed up: no tabs, active out of range, huge window
        std::fs::write(
            sub.join(FILE),
            r#"{"tabs": [], "active": 5, "window": [99999, 1]}"#,
        )
        .unwrap();
        let (got, problem) = load(&sub);
        assert_eq!(problem, None);
        assert_eq!((got.tabs.len(), got.active), (1, 0));
        assert_eq!(got.window, (7680.0, 320.0));
        assert!(got.sidebar, "missing fields default");
    }

    #[test]
    fn clamp_bounds() {
        assert_eq!(clamp((1000.0, 700.0)), (1000.0, 700.0));
        assert_eq!(clamp((10.0, 1e9)), (480.0, 4320.0));
        assert_eq!(clamp((f32::NAN, f32::INFINITY)), (1024.0, 768.0));
    }

    #[test]
    fn touch_orders_dedups_and_caps() {
        let mut r = Vec::new();
        for id in ["a", "b", "c", "a"] {
            touch(&mut r, id);
        }
        assert_eq!(r, ["a", "c", "b"]);
        for i in 0..30 {
            touch(&mut r, &i.to_string());
        }
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], "29");
    }
}
