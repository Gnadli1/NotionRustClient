//! Conflict and failed ops for the status bar's list, and Discard. Mirrors
//! notion-gui's `Problem`, `problem()` and its `Message::Discard` handler.

use notion_core::model::{plain_text, Block, Parent};
use notion_core::ops::{Op, OpKind, OpStatus};
use notion_core::store::Store;
use notion_core::sync;

/// A conflict or failed op, for the status bar's list.
#[derive(Debug, Clone)]
pub struct Problem {
    pub op: i64,
    pub kind: &'static str,
    /// The block to show and focus: a create's own block while cached.
    pub block: String,
    pub page: Option<String>,
    /// The block's text, or the page's title, or the id.
    pub label: String,
    pub error: String,
    /// A conflict (it can be resolved), not a failure.
    pub conflict: bool,
    /// A create's unpushed `local-*` block.
    local: Option<String>,
    /// A comment create's unpushed `local-*` comment.
    local_comment: Option<String>,
}

/// A comment op's entry: its comment's text, shown on the commented block
/// or page (the comment's parent, also for an edit or delete).
fn comment_problem(store: &Store, op: &Op) -> notion_core::Result<Option<Problem>> {
    let (kind, comment) = match &op.kind {
        OpKind::CreateComment { local_id, .. } => ("comment", store.get_comment(local_id)?),
        OpKind::UpdateComment { comment_id, .. } => {
            ("edit comment", store.get_comment(&store.resolve_id(comment_id)?)?)
        }
        OpKind::DeleteComment { comment_id } => (
            "delete comment",
            // gone from the cache: its base snapshot remains
            op.base_raw
                .clone()
                .and_then(|v| serde_json::from_value(v).ok())
                .or(store.get_comment(&store.resolve_id(comment_id)?)?),
        ),
        _ => return Ok(None),
    };
    // a create's target stands in for a comment that is gone from the cache
    let parent = match (&comment, &op.kind) {
        (Some(c), _) => c.parent.id().map(str::to_string),
        (None, OpKind::CreateComment { parent, .. }) => Some(parent.id().to_string()),
        _ => None,
    };
    let Some(parent) = parent else { return Ok(None) };
    let parent = store.resolve_id(&parent)?;
    let page = if store.get_page(&parent)?.is_some() {
        Some(parent.clone())
    } else {
        store.block_page(&parent)?
    };
    let label = comment
        .map(|c| plain_text(&c.rich_text))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| parent.clone());
    Ok(Some(Problem {
        op: op.id,
        kind,
        block: parent,
        page,
        label: label.chars().take(60).collect(),
        error: op.error.clone().unwrap_or_default(),
        conflict: op.status == OpStatus::Conflict,
        local: None,
        local_comment: match &op.kind {
            OpKind::CreateComment { local_id, .. } => Some(local_id.clone()),
            _ => None,
        },
    }))
}

/// A conflict/failed op's list entry: what it is, where, and why.
pub fn problem(store: &Store, op: &Op) -> notion_core::Result<Problem> {
    if let Some(p) = comment_problem(store, op)? {
        return Ok(p);
    }
    let target = store.resolve_id(op.kind.target_id())?;
    let (kind, local) = match &op.kind {
        OpKind::CreateBlock { block, .. } => {
            ("create", block["id"].as_str().map(|id| id.to_string()))
        }
        OpKind::UpdateBlock { .. } => ("edit", None),
        OpKind::DeleteBlock { .. } => ("delete", None),
        OpKind::MoveBlock { .. } => ("move", None),
        OpKind::UpdatePageProperties { .. } => ("title", None),
        // only reached when the comment's parent is unknown (comment_problem)
        OpKind::CreateComment { .. } => ("comment", None),
        OpKind::UpdateComment { .. } => ("edit comment", None),
        OpKind::DeleteComment { .. } => ("delete comment", None),
    };
    let shown = match &local {
        Some(l) if store.get_block(l)?.is_some() => l.clone(),
        _ => target.clone(),
    };
    // a deleted block is gone from the cache: its base snapshot remains
    let block = match store.get_block(&shown)? {
        Some(b) => Some(b),
        None => op
            .base_raw
            .clone()
            .and_then(|v| serde_json::from_value::<Block>(v).ok()),
    };
    let page = store.get_page(&shown)?;
    let page_id = match (&page, &block) {
        (Some(p), _) => Some(p.id.clone()),
        (None, Some(b)) => match store.block_page(&b.id)? {
            Some(p) => Some(p),
            // one hop up from the snapshot
            None => match &b.parent {
                Parent::Page(p) => Some(p.clone()),
                Parent::Block(p) => store.block_page(p)?,
                _ => None,
            },
        },
        (None, None) => None,
    };
    let text = block
        .as_ref()
        .and_then(|b| b.kind.text().map(|t| plain_text(&t.rich_text)))
        .or_else(|| page.map(|p| p.title()))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| shown.clone());
    Ok(Problem {
        op: op.id,
        kind,
        block: shown,
        page: page_id,
        label: text.chars().take(60).collect(),
        error: op.error.clone().unwrap_or_default(),
        conflict: op.status == OpStatus::Conflict,
        local,
        local_comment: None,
    })
}

/// Drops the op, and for a failed create the never-created block and the ops
/// on it. The caller re-pulls the page: the remote version wins.
pub fn discard(store: &Store, p: &Problem) -> notion_core::Result<()> {
    store.complete_op(p.op)?;
    // a never-created comment must not linger in the cache (pulls keep `local-*` ones)
    if let Some(c) = &p.local_comment {
        store.delete_comment(c)?;
    }
    let Some(l) = p.local.as_deref().filter(|l| l.starts_with("local-")) else {
        return Ok(());
    };
    // ops on the never-created block go too
    // ponytail: direct dependents only; ops on its children or placed after
    // it stay (they fail on push and are listed then)
    for o in store
        .queued_ops()?
        .into_iter()
        .chain(store.ops_by_status(OpStatus::Failed)?)
    {
        if sync::op_block(&o.kind) == Some(l) || o.kind.target_id() == l {
            store.complete_op(o.id)?;
        }
    }
    // pulls keep local-* blocks: drop it from the cache too
    if store.get_block(l)?.is_some() {
        store.delete_block(l)?;
    }
    Ok(())
}
