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
    /// A create's unpushed `local-*` block.
    local: Option<String>,
}

/// A conflict/failed op's list entry: what it is, where, and why.
pub fn problem(store: &Store, op: &Op) -> notion_core::Result<Problem> {
    let target = store.resolve_id(op.kind.target_id())?;
    let (kind, local) = match &op.kind {
        OpKind::CreateBlock { block, .. } => {
            ("create", block["id"].as_str().map(|id| id.to_string()))
        }
        OpKind::UpdateBlock { .. } => ("edit", None),
        OpKind::DeleteBlock { .. } => ("delete", None),
        OpKind::MoveBlock { .. } => ("move", None),
        OpKind::UpdatePageProperties { .. } => ("title", None),
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
        local,
    })
}

/// Drops the op, and for a failed create the never-created block and the ops
/// on it. The caller re-pulls the page: the remote version wins.
pub fn discard(store: &Store, p: &Problem) -> notion_core::Result<()> {
    store.complete_op(p.op)?;
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
