//! Persistent queue of local edits waiting to be pushed, plus the
//! local-id → remote-id map for blocks created offline.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{ts, Store};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpKind {
    CreateBlock {
        parent_id: String,
        after: Option<String>,
        block: Value,
    },
    UpdateBlock {
        block_id: String,
        body: Value,
    },
    DeleteBlock {
        block_id: String,
    },
    MoveBlock {
        block_id: String,
        new_parent_id: String,
        after: Option<String>,
    },
    UpdatePageProperties {
        page_id: String,
        properties: Value,
    },
    /// `local_id`/`local_discussion` are the cached row's temporary ids; the
    /// push maps them to the real ones (see `id_map`).
    CreateComment {
        parent: CommentTarget,
        rich_text: Value,
        local_id: String,
        local_discussion: String,
    },
    UpdateComment {
        comment_id: String,
        rich_text: Value,
    },
    DeleteComment {
        comment_id: String,
    },
    /// A new page under `parent_id`. `body` is the `POST /v1/pages` body
    /// without `parent`: `properties` (title), `icon`, `cover`. `local_id` is
    /// the cached page's temporary id; the push maps it to the real one.
    CreatePage {
        local_id: String,
        parent_id: String,
        body: Value,
    },
    /// `PATCH /v1/pages/{id}` with `icon`, `cover`, `in_trash` and/or `is_locked`.
    UpdatePage {
        page_id: String,
        body: Value,
    },
    /// `POST /v1/pages/{id}/move` under a page.
    MovePage {
        page_id: String,
        new_parent_id: String,
    },
}

/// Where a new comment goes. Notion can't start a thread on a text range,
/// only on a whole page or block, or reply into a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum CommentTarget {
    Page(String),
    Block(String),
    Discussion(String),
}

impl CommentTarget {
    pub fn id(&self) -> &str {
        match self {
            CommentTarget::Page(i) | CommentTarget::Block(i) | CommentTarget::Discussion(i) => i,
        }
    }
}

impl OpKind {
    pub fn target_id(&self) -> &str {
        match self {
            OpKind::CreateBlock { parent_id, .. } => parent_id,
            OpKind::UpdateBlock { block_id, .. }
            | OpKind::DeleteBlock { block_id }
            | OpKind::MoveBlock { block_id, .. } => block_id,
            OpKind::UpdatePageProperties { page_id, .. }
            | OpKind::UpdatePage { page_id, .. }
            | OpKind::MovePage { page_id, .. } => page_id,
            OpKind::CreatePage { local_id, .. } => local_id,
            OpKind::CreateComment { parent, .. } => parent.id(),
            OpKind::UpdateComment { comment_id, .. }
            | OpKind::DeleteComment { comment_id } => comment_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpStatus {
    Queued,
    Conflict,
    Failed,
}

impl OpStatus {
    fn as_str(self) -> &'static str {
        match self {
            OpStatus::Queued => "queued",
            OpStatus::Conflict => "conflict",
            OpStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "queued" => OpStatus::Queued,
            "conflict" => OpStatus::Conflict,
            "failed" => OpStatus::Failed,
            _ => return Err(Error::Invalid(format!("unknown op status {s}"))),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub id: i64,
    pub kind: OpKind,
    pub base_last_edited_time: Option<DateTime<Utc>>,
    /// Snapshot of the remote object the edit was based on (for conflict detection).
    pub base_raw: Option<Value>,
    pub status: OpStatus,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::Invalid(format!("bad timestamp {s}: {e}")))
}

type OpRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    String,
);

fn read_row(r: &Row) -> rusqlite::Result<OpRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn to_op((id, payload, base_time, base_raw, status, error, created_at): OpRow) -> Result<Op> {
    Ok(Op {
        id,
        kind: serde_json::from_str(&payload)?,
        base_last_edited_time: base_time.as_deref().map(parse_time).transpose()?,
        base_raw: base_raw.as_deref().map(serde_json::from_str).transpose()?,
        status: OpStatus::parse(&status)?,
        error,
        created_at: parse_time(&created_at)?,
    })
}

impl Store {
    pub fn enqueue(
        &self,
        kind: OpKind,
        base_last_edited_time: Option<DateTime<Utc>>,
        base_raw: Option<Value>,
    ) -> Result<i64> {
        let payload = serde_json::to_value(&kind)?;
        self.conn.execute(
            "INSERT INTO ops (kind, target_id, payload, base_last_edited_time, base_raw, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                payload["kind"].as_str(),
                kind.target_id(),
                payload.to_string(),
                base_last_edited_time.as_ref().map(ts),
                base_raw.map(|v| v.to_string()),
                ts(&Utc::now())
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Queued ops, oldest first.
    pub fn queued_ops(&self) -> Result<Vec<Op>> {
        self.ops_by_status(OpStatus::Queued)
    }

    pub fn ops_by_status(&self, s: OpStatus) -> Result<Vec<Op>> {
        let mut st = self.conn.prepare(
            "SELECT id, payload, base_last_edited_time, base_raw, status, error, created_at
             FROM ops WHERE status = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([s.as_str()], read_row)?;
        rows.map(|r| to_op(r?)).collect()
    }

    pub fn mark_op(&self, id: i64, s: OpStatus, err: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE ops SET status = ?2, error = ?3 WHERE id = ?1",
            params![id, s.as_str(), err],
        )?;
        Ok(())
    }

    pub fn complete_op(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM ops WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Replaces an op's kind (and payload) in place, keeping its queue position.
    pub fn update_op_kind(&self, id: i64, kind: &OpKind) -> Result<()> {
        let payload = serde_json::to_value(kind)?;
        self.conn.execute(
            "UPDATE ops SET kind = ?2, target_id = ?3, payload = ?4 WHERE id = ?1",
            params![
                id,
                payload["kind"].as_str(),
                kind.target_id(),
                payload.to_string()
            ],
        )?;
        Ok(())
    }

    /// The queued CreateBlock op that creates the block `local_id`, if any.
    pub fn queued_create_for(&self, local_id: &str) -> Result<Option<Op>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, payload, base_last_edited_time, base_raw, status, error, created_at
                 FROM ops WHERE status = 'queued' AND kind = 'create_block'
                   AND json_extract(payload, '$.block.id') = ?1",
                [local_id],
                read_row,
            )
            .optional()?;
        row.map(to_op).transpose()
    }

    /// Maps `local_id` to `remote_id`. Earlier ids that mapped to `local_id`
    /// (a block copied again by a later move) follow it, so `resolve_id`
    /// stays one hop.
    pub fn map_id(&self, local_id: &str, remote_id: &str) -> Result<()> {
        self.atomic(|| {
            self.conn.execute(
                "UPDATE id_map SET remote_id = ?2 WHERE remote_id = ?1",
                [local_id, remote_id],
            )?;
            self.conn.execute(
                "INSERT OR REPLACE INTO id_map (local_id, remote_id) VALUES (?1, ?2)",
                [local_id, remote_id],
            )?;
            Ok(())
        })
    }

    /// Remote id if `id` is a mapped local id, else `id` unchanged.
    pub fn resolve_id(&self, id: &str) -> Result<String> {
        let mapped: Option<String> = self
            .conn
            .query_row(
                "SELECT remote_id FROM id_map WHERE local_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(mapped.unwrap_or_else(|| id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn id_map_follows_chains() {
        let s = Store::open_in_memory().unwrap();
        s.map_id("x", "x1").unwrap();
        s.map_id("c", "x1").unwrap();
        s.map_id("x1", "x2").unwrap();
        assert_eq!(s.resolve_id("x").unwrap(), "x2");
        assert_eq!(s.resolve_id("c").unwrap(), "x2");
        assert_eq!(s.resolve_id("x1").unwrap(), "x2");
        assert_eq!(s.resolve_id("other").unwrap(), "other");
    }

    #[test]
    fn ops_survive_reopen_in_fifo_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let base_time = parse_time("2025-01-02T03:04:05.678Z").unwrap();
        let k1 = OpKind::UpdateBlock {
            block_id: "b1".into(),
            body: json!({ "paragraph": {} }),
        };
        let k2 = OpKind::CreateBlock {
            parent_id: "p".into(),
            after: None,
            block: json!({ "type": "divider" }),
        };
        let (id1, id2) = {
            let s = Store::open(&path).unwrap();
            let id1 = s
                .enqueue(k1.clone(), Some(base_time), Some(json!({ "id": "b1" })))
                .unwrap();
            let id2 = s.enqueue(k2.clone(), None, None).unwrap();
            (id1, id2)
        };
        let s = Store::open(&path).unwrap();
        let ops = s.queued_ops().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!((ops[0].id, &ops[0].kind), (id1, &k1));
        assert_eq!(ops[0].base_last_edited_time, Some(base_time));
        assert_eq!(ops[0].base_raw, Some(json!({ "id": "b1" })));
        assert_eq!(ops[0].status, OpStatus::Queued);
        assert_eq!(ops[0].error, None);
        assert_eq!((ops[1].id, &ops[1].kind), (id2, &k2));
        assert_eq!(
            (ops[1].base_last_edited_time, &ops[1].base_raw),
            (None, &None)
        );

        let (kind, target): (String, String) = s
            .conn
            .query_row(
                "SELECT kind, target_id FROM ops WHERE id = ?1",
                [id2],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), target.as_str()), ("create_block", "p"));
    }

    #[test]
    fn status_transitions() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .enqueue(
                OpKind::DeleteBlock {
                    block_id: "a".into(),
                },
                None,
                None,
            )
            .unwrap();
        let b = s
            .enqueue(
                OpKind::UpdatePageProperties {
                    page_id: "p".into(),
                    properties: json!({}),
                },
                None,
                None,
            )
            .unwrap();
        s.mark_op(a, OpStatus::Conflict, Some("remote changed"))
            .unwrap();
        let conflicts = s.ops_by_status(OpStatus::Conflict).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].id, a);
        assert_eq!(conflicts[0].error.as_deref(), Some("remote changed"));
        assert_eq!(
            s.queued_ops()
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            [b]
        );

        s.mark_op(b, OpStatus::Failed, Some("400 validation_error"))
            .unwrap();
        let failed = s.ops_by_status(OpStatus::Failed).unwrap();
        assert_eq!((failed.len(), failed[0].id), (1, b));
        assert_eq!(failed[0].error.as_deref(), Some("400 validation_error"));
        assert!(s.queued_ops().unwrap().is_empty());

        // retry a conflict: back to queued, error cleared
        s.mark_op(a, OpStatus::Queued, None).unwrap();
        let queued = s.queued_ops().unwrap();
        assert_eq!(
            (queued.len(), queued[0].id, &queued[0].error),
            (1, a, &None)
        );
        assert!(s.ops_by_status(OpStatus::Conflict).unwrap().is_empty());

        s.complete_op(a).unwrap();
        s.complete_op(b).unwrap();
        assert!(s.ops_by_status(OpStatus::Failed).unwrap().is_empty());
        assert!(s.ops_by_status(OpStatus::Conflict).unwrap().is_empty());
        assert!(s.queued_ops().unwrap().is_empty());
    }

    #[test]
    fn update_kind_and_find_create() {
        let s = Store::open_in_memory().unwrap();
        let create = |parent: &str| OpKind::CreateBlock {
            parent_id: parent.into(),
            after: None,
            block: json!({ "id": "local-1", "type": "divider" }),
        };
        let other = s
            .enqueue(
                OpKind::DeleteBlock {
                    block_id: "local-1".into(),
                },
                None,
                None,
            )
            .unwrap();
        let id = s.enqueue(create("p"), None, None).unwrap();
        assert_eq!(s.queued_create_for("local-2").unwrap(), None);
        assert_eq!(s.queued_create_for("local-1").unwrap().unwrap().id, id);

        s.update_op_kind(id, &create("q")).unwrap();
        let ops = s.queued_ops().unwrap();
        assert_eq!((ops[0].id, ops[1].id), (other, id));
        assert_eq!(ops[1].kind, create("q"));
        let target: String = s
            .conn
            .query_row("SELECT target_id FROM ops WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(target, "q");

        s.mark_op(id, OpStatus::Failed, None).unwrap();
        assert_eq!(s.queued_create_for("local-1").unwrap(), None);
    }

    #[test]
    fn id_mapping() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.resolve_id("local-1").unwrap(), "local-1");
        s.map_id("local-1", "real-1").unwrap();
        assert_eq!(s.resolve_id("local-1").unwrap(), "real-1");
        assert_eq!(s.resolve_id("other").unwrap(), "other");
    }
}
