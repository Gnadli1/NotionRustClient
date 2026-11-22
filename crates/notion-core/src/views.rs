//! F5: a data source's saved views (read when the API has them) and the
//! locally remembered view choice.

use rusqlite::{params, OptionalExtension};
use serde_json::Value;

use crate::store::Store;
use crate::Result;

/// A saved view of a data source: its name and type (`table`, `board`, `list`,
/// `calendar`, `gallery`, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct SavedView {
    pub id: String,
    pub name: String,
    pub kind: String,
}

impl SavedView {
    /// None unless it has an id and a type.
    pub fn from_value(v: &Value) -> Option<SavedView> {
        Some(SavedView {
            id: v["id"].as_str()?.to_string(),
            name: v["name"].as_str().unwrap_or_default().to_string(),
            kind: v["type"].as_str()?.to_string(),
        })
    }
}

impl Store {
    /// The view chosen for data source `ds` (its label), if any.
    pub fn view_pref(&self, ds: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [format!("view:{ds}")], |r| r.get(0))
            .optional()?)
    }

    pub fn set_view_pref(&self, ds: &str, label: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR REPLACE INTO meta VALUES (?1, ?2)", params![format!("view:{ds}"), label])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_saved_view_needs_an_id_and_a_type_and_the_choice_persists() {
        assert_eq!(
            SavedView::from_value(&json!({"id": "v", "name": "Board", "type": "board"})),
            Some(SavedView { id: "v".into(), name: "Board".into(), kind: "board".into() })
        );
        assert_eq!(SavedView::from_value(&json!({"name": "x", "type": "board"})), None);
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.view_pref("ds").unwrap(), None);
        s.set_view_pref("ds", "Board").unwrap();
        s.set_view_pref("ds", "Calendar").unwrap();
        assert_eq!(s.view_pref("ds").unwrap().as_deref(), Some("Calendar"));
        assert_eq!(s.view_pref("other").unwrap(), None);
    }
}
