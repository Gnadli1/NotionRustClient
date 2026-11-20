//! SQLite mirror of pages, blocks and data sources. Objects are stored whole as
//! JSON in `raw`; the other columns exist for lookups and ordering.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Block, DataSource, Page, Parent};
use crate::{Error, Result};

const SCHEMA: &str = "
CREATE TABLE pages (id TEXT PRIMARY KEY, parent_type TEXT NOT NULL, parent_id TEXT, title TEXT NOT NULL DEFAULT '',
  in_trash INTEGER NOT NULL DEFAULT 0, last_edited_time TEXT NOT NULL, raw TEXT NOT NULL);
CREATE INDEX pages_parent ON pages(parent_id);
CREATE TABLE blocks (id TEXT PRIMARY KEY, page_id TEXT NOT NULL, parent_id TEXT NOT NULL, position INTEGER NOT NULL,
  type TEXT NOT NULL, has_children INTEGER NOT NULL, last_edited_time TEXT NOT NULL, raw TEXT NOT NULL);
CREATE INDEX blocks_parent ON blocks(parent_id, position);
CREATE INDEX blocks_page ON blocks(page_id);
CREATE TABLE data_sources (id TEXT PRIMARY KEY, database_id TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
  last_edited_time TEXT NOT NULL, raw TEXT NOT NULL);
CREATE TABLE ops (id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, target_id TEXT NOT NULL, payload TEXT NOT NULL,
  base_last_edited_time TEXT, base_raw TEXT,
  status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','conflict','failed')), error TEXT, created_at TEXT NOT NULL);
CREATE TABLE id_map (local_id TEXT PRIMARY KEY, remote_id TEXT NOT NULL);
PRAGMA user_version = 1;
";

/// One sidebar entry: a page, cached or known only from a `child_page` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEntry {
    pub id: String,
    pub title: String,
    /// The nearest page above this one; None at top level (workspace,
    /// database or data source parent, or a parent block that isn't cached).
    pub parent: Option<String>,
    /// The emoji icon, if the page has one.
    pub icon: Option<String>,
}

/// Rows of `id` and every block below it (recursive on parent_id).
const SUBTREE: &str = "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION \
     SELECT b.id FROM blocks b JOIN sub ON b.parent_id = sub.id) ";

pub(crate) fn ts(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Opens (and if new, initialises) the DB file. Does not create parent dirs.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode returns a row, so it can't go through execute().
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(&format!("BEGIN; {SCHEMA} COMMIT;"))?;
        }
        Ok(Store { conn })
    }

    pub fn upsert_page(&self, p: &Page) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pages (id, parent_type, parent_id, title, in_trash, last_edited_time, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.id,
                p.parent.kind_str(),
                p.parent.id(),
                p.title(),
                p.in_trash,
                ts(&p.last_edited_time),
                serde_json::to_string(p)?
            ],
        )?;
        Ok(())
    }

    pub fn get_page(&self, id: &str) -> Result<Option<Page>> {
        self.get("SELECT raw FROM pages WHERE id = ?1", id)
    }

    /// Every non-trashed cached page except database rows, plus the pages only known from a cached
    /// `child_page` block, with their parent page resolved (unordered).
    pub fn page_tree(&self) -> Result<Vec<PageEntry>> {
        let mut st = self.conn.prepare(
            "SELECT p.id, p.title,
               CASE p.parent_type WHEN 'page_id' THEN p.parent_id WHEN 'block_id' THEN b.page_id END,
               CASE WHEN json_extract(p.raw, '$.icon.type') = 'emoji'
                 THEN json_extract(p.raw, '$.icon.emoji') END
             FROM pages p LEFT JOIN blocks b ON p.parent_type = 'block_id' AND b.id = p.parent_id
             WHERE p.in_trash = 0 AND p.parent_type <> 'data_source_id'
             UNION ALL
             SELECT id, COALESCE(json_extract(raw, '$.child_page.title'), ''), page_id, NULL
             FROM blocks WHERE type = 'child_page' AND id NOT IN (SELECT id FROM pages)",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PageEntry {
                id: r.get(0)?,
                title: r.get(1)?,
                parent: r.get(2)?,
                icon: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Makes `blocks` the full, ordered child list of `parent_id`; cached
    /// children not in `blocks` are dropped together with their subtrees, and
    /// listed blocks' cached content is overwritten. Callers must therefore
    /// (a) not clobber blocks that still have queued ops, and (b) write every
    /// level of a fetched tree top-down, or a moved block's descendants can be
    /// lost when its old parent is replaced.
    pub fn replace_children(&self, page_id: &str, parent_id: &str, blocks: &[Block]) -> Result<()> {
        self.atomic(|| {
            let tx = &self.conn;
            let keep: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
            let stale: Vec<String> = {
                let mut st = tx.prepare("SELECT id FROM blocks WHERE parent_id = ?1")?;
                let ids = st.query_map([parent_id], |r| r.get::<_, String>(0))?;
                ids.filter(|id| !matches!(id, Ok(id) if keep.contains(&id.as_str())))
                    .collect::<rusqlite::Result<_>>()?
            };
            for id in &stale {
                Self::delete_subtree(tx, id)?;
            }
            for (i, b) in blocks.iter().enumerate() {
                Self::write_block(tx, b, page_id, parent_id, i as i64)?;
            }
            Ok(())
        })
    }

    pub fn get_block(&self, id: &str) -> Result<Option<Block>> {
        self.get("SELECT raw FROM blocks WHERE id = ?1", id)
    }

    /// The page a cached block sits on.
    pub fn block_page(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT page_id FROM blocks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn children(&self, parent_id: &str) -> Result<Vec<Block>> {
        self.list(
            "SELECT raw FROM blocks WHERE parent_id = ?1 ORDER BY position",
            parent_id,
        )
    }

    /// Updates a cached block in place, or appends a new one after its siblings.
    pub fn put_block(&self, b: &Block) -> Result<()> {
        let existing: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT page_id, parent_id, position FROM blocks WHERE id = ?1",
                [&b.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (page_id, parent_id, position) = match existing {
            Some(row) => row,
            None => {
                let parent_id = b
                    .parent
                    .id()
                    .ok_or_else(|| Error::Invalid(format!("block {} has no parent id", b.id)))?
                    .to_string();
                let page_id = match &b.parent {
                    Parent::Page(id) => id.clone(),
                    _ => self
                        .conn
                        .query_row(
                            "SELECT page_id FROM blocks WHERE id = ?1",
                            [&parent_id],
                            |r| r.get(0),
                        )
                        .optional()?
                        .ok_or_else(|| {
                            Error::Invalid(format!("parent block {parent_id} not cached"))
                        })?,
                };
                let position: i64 = self.conn.query_row(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM blocks WHERE parent_id = ?1",
                    [&parent_id],
                    |r| r.get(0),
                )?;
                (page_id, parent_id, position)
            }
        };
        Self::write_block(&self.conn, b, &page_id, &parent_id, position)
    }

    /// Moves a cached block (with its subtree) under `new_parent` (a cached
    /// page, else a cached block), right after its child `after`, or last if
    /// `None`. Renumbers both parents' children, patches the raw `parent` and
    /// fixes both parents' `has_children`. Moving a block into itself or its
    /// own subtree is an error.
    pub fn move_block_local(&self, id: &str, new_parent: &str, after: Option<&str>) -> Result<()> {
        self.atomic(|| {
        let tx = &self.conn;
        let old_parent: String = tx
            .query_row("SELECT parent_id FROM blocks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::Invalid(format!("block {id} not cached")))?;
        let cycle: bool = tx.query_row(
            &format!("{SUBTREE} SELECT EXISTS(SELECT 1 FROM sub WHERE id = ?2)"),
            [id, new_parent],
            |r| r.get(0),
        )?;
        if cycle {
            return Err(Error::Invalid(format!(
                "cannot move block {id} into its own subtree"
            )));
        }
        let is_page: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pages WHERE id = ?1)",
            [new_parent],
            |r| r.get(0),
        )?;
        let (parent, page_id) = if is_page {
            (Parent::Page(new_parent.into()), new_parent.to_string())
        } else {
            let page_id: String = tx
                .query_row(
                    "SELECT page_id FROM blocks WHERE id = ?1",
                    [new_parent],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::Invalid(format!("parent {new_parent} not cached")))?;
            (Parent::Block(new_parent.into()), page_id)
        };
        let mut sibs: Vec<String> = Self::child_ids(tx, new_parent)?
            .into_iter()
            .filter(|s| s != id)
            .collect();
        let at = match after {
            None => sibs.len(),
            Some(a) => {
                sibs.iter().position(|s| s == a).ok_or_else(|| {
                    Error::Invalid(format!("{a} is not another child of {new_parent}"))
                })? + 1
            }
        };
        sibs.insert(at, id.to_string());

        tx.execute(
            "UPDATE blocks SET parent_id = ?2, raw = json_set(raw, '$.parent', json(?3)) WHERE id = ?1",
            params![id, new_parent, serde_json::to_string(&parent)?],
        )?;
        tx.execute(
            &format!("{SUBTREE} UPDATE blocks SET page_id = ?2 WHERE id IN (SELECT id FROM sub)"),
            [id, &page_id],
        )?;
        Self::renumber(tx, &sibs)?;
        if old_parent != new_parent {
            Self::renumber(tx, &Self::child_ids(tx, &old_parent)?)?;
        }
        Self::refresh_has_children(tx, &old_parent)?;
        Self::refresh_has_children(tx, new_parent)?;
        Ok(())
        })
    }

    /// `id` and the ids of every block below it.
    pub(crate) fn subtree_ids(&self, id: &str) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare(&format!("{SUBTREE} SELECT id FROM sub"))?;
        let ids = st.query_map([id], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes a cached block and its subtree, and fixes the parent's `has_children`.
    pub fn delete_block(&self, id: &str) -> Result<()> {
        self.atomic(|| {
            let parent: Option<String> = self
                .conn
                .query_row("SELECT parent_id FROM blocks WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            Self::delete_subtree(&self.conn, id)?;
            match parent {
                Some(p) => Self::refresh_has_children(&self.conn, &p),
                None => Ok(()),
            }
        })
    }

    /// Runs `f` atomically: rolled back if it fails. Nests (uses a SAVEPOINT).
    pub(crate) fn atomic<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("SAVEPOINT atomic")?;
        match f() {
            Ok(v) => {
                self.conn.execute_batch("RELEASE atomic")?;
                Ok(v)
            }
            Err(e) => {
                // keep the original error; a failed rollback leaves nothing better to report
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO atomic; RELEASE atomic");
                Err(e)
            }
        }
    }

    /// Re-keys a block (e.g. `local-<n>` → real id), keeping its children attached.
    pub fn rename_block(&self, old_id: &str, new_id: &str) -> Result<()> {
        self.atomic(|| {
            let tx = &self.conn;
            tx.execute(
                "UPDATE blocks SET id = ?2, raw = json_set(raw, '$.id', ?2) WHERE id = ?1",
                params![old_id, new_id],
            )?;
            tx.execute(
                "UPDATE blocks SET parent_id = ?2, raw = json_set(raw, '$.parent.block_id', ?2)
             WHERE parent_id = ?1",
                params![old_id, new_id],
            )?;
            Ok(())
        })
    }

    pub fn upsert_data_source(&self, d: &DataSource) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO data_sources (id, database_id, title, last_edited_time, raw)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                d.id,
                d.database_id,
                d.title,
                ts(&d.last_edited_time),
                serde_json::to_string(d)?
            ],
        )?;
        Ok(())
    }

    pub fn rows(&self, data_source_id: &str) -> Result<Vec<Page>> {
        self.list(
            "SELECT raw FROM pages WHERE parent_type = 'data_source_id' AND parent_id = ?1 ORDER BY id",
            data_source_id,
        )
    }

    // ------------------------------------------------------------ helpers

    fn write_block(
        conn: &Connection,
        b: &Block,
        page_id: &str,
        parent_id: &str,
        position: i64,
    ) -> Result<()> {
        // Block's Serialize doesn't write id/parent/has_children back into raw;
        // do it here so a loaded block matches what was stored.
        let mut v = serde_json::to_value(b)?;
        v["id"] = b.id.clone().into();
        v["parent"] = serde_json::to_value(&b.parent)?;
        v["has_children"] = b.has_children.into();
        conn.execute(
            "INSERT INTO blocks (id, page_id, parent_id, position, type, has_children, last_edited_time, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET page_id = ?2, parent_id = ?3, position = ?4, type = ?5,
               has_children = ?6, last_edited_time = ?7, raw = ?8",
            params![
                b.id,
                page_id,
                parent_id,
                position,
                b.kind.type_name(),
                b.has_children,
                ts(&b.last_edited_time),
                v.to_string()
            ],
        )?;
        Ok(())
    }

    fn child_ids(conn: &Connection, parent_id: &str) -> Result<Vec<String>> {
        let mut st =
            conn.prepare("SELECT id FROM blocks WHERE parent_id = ?1 ORDER BY position")?;
        let ids = st.query_map([parent_id], |r| r.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// Sets `has_children` (column and raw) from the cached children. Pages
    /// aren't in `blocks`, so a page id is a no-op.
    fn refresh_has_children(conn: &Connection, id: &str) -> Result<()> {
        conn.execute(
            "UPDATE blocks SET has_children = EXISTS(SELECT 1 FROM blocks c WHERE c.parent_id = ?1),
               raw = json_set(raw, '$.has_children', json(CASE WHEN EXISTS(
                 SELECT 1 FROM blocks c WHERE c.parent_id = ?1) THEN 'true' ELSE 'false' END))
             WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    fn renumber(conn: &Connection, ids: &[String]) -> Result<()> {
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "UPDATE blocks SET position = ?2 WHERE id = ?1",
                params![id, i as i64],
            )?;
        }
        Ok(())
    }

    fn delete_subtree(conn: &Connection, id: &str) -> Result<()> {
        conn.execute(
            &format!("{SUBTREE} DELETE FROM blocks WHERE id IN (SELECT id FROM sub)"),
            [id],
        )?;
        Ok(())
    }

    fn get<T: serde::de::DeserializeOwned>(&self, sql: &str, arg: &str) -> Result<Option<T>> {
        let raw: Option<String> = self.conn.query_row(sql, [arg], |r| r.get(0)).optional()?;
        Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    fn list<T: serde::de::DeserializeOwned>(&self, sql: &str, arg: &str) -> Result<Vec<T>> {
        let mut st = self.conn.prepare(sql)?;
        let raws = st.query_map([arg], |r| r.get::<_, String>(0))?;
        raws.map(|s| Ok(serde_json::from_str(&s?)?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RichText;
    use serde_json::{json, Value};

    const T: &str = "2025-01-01T00:00:00.000Z";

    /// Parsed and re-serialized once, so raw is in the canonical form the store keeps.
    fn block(id: &str, parent: Value, text: &str) -> Block {
        let b: Block = serde_json::from_value(json!({
            "object": "block", "id": id, "parent": parent, "type": "paragraph",
            "paragraph": { "rich_text": [{ "type": "text", "text": { "content": text },
                "plain_text": text, "annotations": {} }], "color": "default" },
            "has_children": false, "in_trash": false, "created_time": T, "last_edited_time": T,
        }))
        .unwrap();
        serde_json::from_value(serde_json::to_value(&b).unwrap()).unwrap()
    }

    fn page(id: &str, parent: Value, title: &str) -> Page {
        serde_json::from_value(json!({
            "object": "page", "id": id, "parent": parent, "created_time": T, "last_edited_time": T,
            "in_trash": false, "url": format!("https://www.notion.so/{id}"),
            "properties": { "Name": { "id": "title", "type": "title", "title": [
                { "type": "text", "text": { "content": title }, "plain_text": title, "annotations": {} }] } },
        }))
        .unwrap()
    }

    fn pp(id: &str) -> Value {
        json!({ "type": "page_id", "page_id": id })
    }
    fn bp(id: &str) -> Value {
        json!({ "type": "block_id", "block_id": id })
    }
    fn ids(bs: &[Block]) -> Vec<&str> {
        bs.iter().map(|b| b.id.as_str()).collect()
    }

    #[test]
    fn page_round_trip_and_overwrite() {
        let s = Store::open_in_memory().unwrap();
        let p = page("p1", pp("root"), "Hello");
        s.upsert_page(&p).unwrap();
        assert_eq!(s.get_page("p1").unwrap(), Some(p));
        let title: String = s
            .conn
            .query_row("SELECT title FROM pages WHERE id='p1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "Hello");

        let p2 = page("p1", pp("root"), "Bye");
        s.upsert_page(&p2).unwrap();
        assert_eq!(s.get_page("p1").unwrap(), Some(p2));
        assert_eq!(s.get_page("nope").unwrap(), None);
    }

    #[test]
    fn replace_children_orders_and_prunes() {
        let s = Store::open_in_memory().unwrap();
        let (a, b, c) = (
            block("a", pp("p"), "A"),
            block("b", pp("p"), "B"),
            block("c", pp("p"), "C"),
        );
        s.replace_children("p", "p", &[a.clone(), b.clone(), c.clone()])
            .unwrap();
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b", "c"]);
        // grandchildren of b
        s.replace_children("p", "b", &[block("b1", bp("b"), "B1")])
            .unwrap();
        s.replace_children("p", "b1", &[block("b11", bp("b1"), "B11")])
            .unwrap();

        s.replace_children("p", "p", &[c.clone(), a.clone()])
            .unwrap();
        assert_eq!(ids(&s.children("p").unwrap()), ["c", "a"]);
        for gone in ["b", "b1", "b11"] {
            assert_eq!(s.get_block(gone).unwrap(), None, "{gone}");
        }
        assert_eq!(s.get_block("a").unwrap(), Some(a));
    }

    #[test]
    fn put_block_update_append_and_rename() {
        let s = Store::open_in_memory().unwrap();
        s.replace_children(
            "p",
            "p",
            &[block("a", pp("p"), "A"), block("b", pp("p"), "B")],
        )
        .unwrap();

        let mut a = s.get_block("a").unwrap().unwrap();
        a.kind.set_text(vec![RichText::plain("A2")]).unwrap();
        s.put_block(&a).unwrap();
        let a2 = s.get_block("a").unwrap().unwrap();
        assert_eq!(a2.kind, a.kind);
        assert_eq!(a2.raw["paragraph"]["rich_text"][0]["plain_text"], "A2");
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b"]);

        let n = block("local-1", pp("p"), "N");
        s.put_block(&n).unwrap();
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b", "local-1"]);

        // nested new block takes page_id from its parent block
        s.put_block(&block("local-2", bp("local-1"), "child"))
            .unwrap();
        let pid: String = s
            .conn
            .query_row("SELECT page_id FROM blocks WHERE id='local-2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pid, "p");
        assert!(matches!(
            s.put_block(&block("x", bp("unknown"), "x")),
            Err(Error::Invalid(_))
        ));

        s.rename_block("local-1", "real-1").unwrap();
        assert_eq!(s.get_block("local-1").unwrap(), None);
        assert_eq!(s.get_block("real-1").unwrap().unwrap().id, "real-1");
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b", "real-1"]);
        let kids = s.children("real-1").unwrap();
        assert_eq!(ids(&kids), ["local-2"]);
        assert_eq!(kids[0].parent, Parent::Block("real-1".into()));

        s.delete_block("real-1").unwrap();
        assert_eq!(s.get_block("local-2").unwrap(), None);
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b"]);
    }

    #[test]
    fn rows_filters_by_data_source() {
        let s = Store::open_in_memory().unwrap();
        let ds = |id: &str| json!({ "type": "data_source_id", "data_source_id": id, "database_id": "db" });
        s.upsert_page(&page("r1", ds("ds1"), "one")).unwrap();
        s.upsert_page(&page("r2", ds("ds2"), "two")).unwrap();
        s.upsert_page(&page("r3", pp("ds1"), "not a row")).unwrap();
        let rows = s.rows("ds1").unwrap();
        assert_eq!(
            rows.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["r1"]
        );
    }

    #[test]
    fn data_source_upsert() {
        let s = Store::open_in_memory().unwrap();
        let d: DataSource = serde_json::from_value(json!({
            "object": "data_source", "id": "ds1", "parent": { "type": "database_id", "database_id": "db1" },
            "database_parent": { "type": "page_id", "page_id": "p" },
            "title": [], "last_edited_time": T, "properties": {},
        }))
        .unwrap();
        s.upsert_data_source(&d).unwrap();
        s.upsert_data_source(&d).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM data_sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn delete_block_survives_parent_cycle() {
        let s = Store::open_in_memory().unwrap();
        s.replace_children(
            "p",
            "p",
            &[block("x", pp("p"), "X"), block("y", pp("p"), "Y")],
        )
        .unwrap();
        s.conn
            .execute_batch("UPDATE blocks SET parent_id = 'y' WHERE id = 'x'; UPDATE blocks SET parent_id = 'x' WHERE id = 'y';")
            .unwrap();
        s.delete_block("x").unwrap();
        assert_eq!(s.get_block("x").unwrap(), None);
        assert_eq!(s.get_block("y").unwrap(), None);
    }

    #[test]
    fn page_tree_without_trash() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_page(&page("p1", pp("r"), "Zeta")).unwrap();
        s.upsert_page(&page("p2", pp("r"), "Alpha")).unwrap();
        let mut gone = page("p3", pp("r"), "Beta");
        gone.in_trash = true;
        s.upsert_page(&gone).unwrap();
        s.upsert_page(&page("p4", pp("r"), "beta")).unwrap();
        let mut ids: Vec<String> = s.page_tree().unwrap().into_iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, ["p1", "p2", "p4"]);
    }

    #[test]
    fn page_tree_resolves_parents_icons_and_child_pages() {
        let s = Store::open_in_memory().unwrap();
        let ws = json!({ "type": "workspace", "workspace": true });
        let mut top = page("top", ws.clone(), "Top");
        top.raw["icon"] = json!({ "type": "emoji", "emoji": "🚀" });
        s.upsert_page(&top).unwrap();
        s.upsert_page(&page("sub", pp("top"), "Sub")).unwrap();
        // under a column block of "top"
        s.replace_children("top", "top", &[block("col", pp("top"), "")])
            .unwrap();
        s.upsert_page(&page("incol", bp("col"), "In column"))
            .unwrap();
        s.upsert_page(&page("lost", bp("nowhere"), "Lost")).unwrap();
        let mut ext = page("ext", ws.clone(), "Ext");
        ext.raw["icon"] = json!({ "type": "external", "external": { "url": "https://x/y.png" } });
        s.upsert_page(&ext).unwrap();
        let mut gone = page("gone", pp("top"), "Gone");
        gone.in_trash = true;
        s.upsert_page(&gone).unwrap();
        let ds = json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" });
        s.upsert_page(&page("row", ds, "Row")).unwrap();
        // child_page blocks: one uncached, one for the cached "sub", one for trashed "gone"
        let cp = |id: &str, title: &str| -> Block {
            serde_json::from_value(json!({
                "object": "block", "id": id, "parent": pp("top"), "type": "child_page",
                "child_page": { "title": title }, "has_children": false, "in_trash": false,
                "created_time": T, "last_edited_time": T,
            }))
            .unwrap()
        };
        s.replace_children(
            "top",
            "top",
            &[
                block("col", pp("top"), ""),
                cp("unc", "Uncached"),
                cp("sub", "Sub"),
                cp("gone", "Gone"),
            ],
        )
        .unwrap();

        let mut got = s.page_tree().unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        let e = |id: &str, title: &str, parent: Option<&str>, icon: Option<&str>| PageEntry {
            id: id.into(),
            title: title.into(),
            parent: parent.map(Into::into),
            icon: icon.map(Into::into),
        };
        assert_eq!(
            got,
            [
                e("ext", "Ext", None, None),
                e("incol", "In column", Some("top"), None),
                e("lost", "Lost", None, None),
                e("sub", "Sub", Some("top"), None),
                e("top", "Top", None, Some("🚀")),
                e("unc", "Uncached", Some("top"), None),
            ]
        );
    }

    #[test]
    fn atomic_rolls_back_and_nests() {
        let s = Store::open_in_memory().unwrap();
        s.replace_children("p", "p", &[block("a", pp("p"), "A")])
            .unwrap();
        let r: Result<()> = s.atomic(|| {
            s.replace_children("p", "p", &[block("b", pp("p"), "B")])?; // nested
            s.delete_block("b")?;
            Err(Error::Invalid("boom".into()))
        });
        assert!(r.is_err());
        assert_eq!(ids(&s.children("p").unwrap()), ["a"]);
        s.atomic(|| s.delete_block("a")).unwrap();
        assert!(s.children("p").unwrap().is_empty());
    }

    #[test]
    fn busy_timeout_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("cache.db")).unwrap();
        let ms: i64 = s
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, 5000);
    }

    #[test]
    fn move_block_local_reorders_reparents_and_rejects_cycles() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_page(&page("p", pp("r"), "P")).unwrap();
        s.upsert_page(&page("q", pp("r"), "Q")).unwrap();
        let abc = ["a", "b", "c"].map(|i| block(i, pp("p"), i));
        s.replace_children("p", "p", &abc).unwrap();
        s.replace_children("p", "a", &[block("a1", bp("a"), "A1")])
            .unwrap();
        s.replace_children("p", "a1", &[block("a11", bp("a1"), "A11")])
            .unwrap();
        let col = |id: &str, c: &str| -> String {
            s.conn
                .query_row(
                    &format!("SELECT {c} FROM blocks WHERE id = ?1"),
                    [id],
                    |r| r.get::<_, rusqlite::types::Value>(0),
                )
                .map(|v| format!("{v:?}"))
                .unwrap()
        };

        // same-parent reorder
        s.move_block_local("c", "p", None).unwrap();
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "b", "c"]);
        s.move_block_local("c", "p", Some("a")).unwrap();
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "c", "b"]);

        // into a block: old parent renumbered, has_children fixed on both
        s.move_block_local("a1", "b", None).unwrap();
        assert_eq!(ids(&s.children("b").unwrap()), ["a1"]);
        assert!(s.children("a").unwrap().is_empty());
        let a = s.get_block("a").unwrap().unwrap();
        let b = s.get_block("b").unwrap().unwrap();
        assert!(!a.has_children && b.has_children);
        assert_eq!(col("a", "has_children"), "Integer(0)");
        assert_eq!(col("b", "has_children"), "Integer(1)");
        let a1 = s.get_block("a1").unwrap().unwrap();
        assert_eq!(a1.parent, Parent::Block("b".into()));
        assert_eq!(ids(&s.children("a1").unwrap()), ["a11"]);

        // to another page: parent type flips, subtree page_id follows
        s.move_block_local("b", "q", None).unwrap();
        assert_eq!(
            s.get_block("b").unwrap().unwrap().parent,
            Parent::Page("q".into())
        );
        assert_eq!(col("a11", "page_id"), "Text(\"q\")");
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "c"]);
        assert_eq!(col("c", "position"), "Integer(1)");

        // errors: cycles, self, bad after, unknown parent / block
        for (id, parent, after) in [
            ("b", "b", None),
            ("b", "a11", None),
            ("a", "p", Some("a")),
            ("a", "p", Some("b")),
            ("a", "nowhere", None),
            ("nope", "p", None),
        ] {
            assert!(
                matches!(
                    s.move_block_local(id, parent, after),
                    Err(Error::Invalid(_))
                ),
                "{id} → {parent} after {after:?}"
            );
        }
        assert_eq!(ids(&s.children("p").unwrap()), ["a", "c"]);
    }

    #[test]
    fn reopen_initialised_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        Store::open(&path)
            .unwrap()
            .upsert_page(&page("p1", pp("r"), "x"))
            .unwrap();
        let s = Store::open(&path).unwrap();
        assert!(s.get_page("p1").unwrap().is_some());
    }
}
