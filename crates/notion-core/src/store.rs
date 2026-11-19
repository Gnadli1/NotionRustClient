//! SQLite mirror of pages, blocks and data sources. Objects are stored whole as
//! JSON in `raw`; the other columns exist for lookups and ordering.

use std::path::Path;

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

    /// Makes `blocks` the full, ordered child list of `parent_id`; cached
    /// children not in `blocks` are dropped together with their subtrees, and
    /// listed blocks' cached content is overwritten. Callers must therefore
    /// (a) not clobber blocks that still have queued ops, and (b) write every
    /// level of a fetched tree top-down, or a moved block's descendants can be
    /// lost when its old parent is replaced.
    pub fn replace_children(&self, page_id: &str, parent_id: &str, blocks: &[Block]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let keep: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
        let stale: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM blocks WHERE parent_id = ?1")?;
            let ids = st.query_map([parent_id], |r| r.get::<_, String>(0))?;
            ids.filter(|id| !matches!(id, Ok(id) if keep.contains(&id.as_str())))
                .collect::<rusqlite::Result<_>>()?
        };
        for id in &stale {
            Self::delete_subtree(&tx, id)?;
        }
        for (i, b) in blocks.iter().enumerate() {
            Self::write_block(&tx, b, page_id, parent_id, i as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_block(&self, id: &str) -> Result<Option<Block>> {
        self.get("SELECT raw FROM blocks WHERE id = ?1", id)
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

    pub fn delete_block(&self, id: &str) -> Result<()> {
        Self::delete_subtree(&self.conn, id)
    }

    /// Re-keys a block (e.g. `local-<n>` → real id), keeping its children attached.
    pub fn rename_block(&self, old_id: &str, new_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE blocks SET id = ?2, raw = json_set(raw, '$.id', ?2) WHERE id = ?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE blocks SET parent_id = ?2, raw = json_set(raw, '$.parent.block_id', ?2)
             WHERE parent_id = ?1",
            params![old_id, new_id],
        )?;
        tx.commit()?;
        Ok(())
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
