//! Pull remote pages / data sources into the cache; push the local op queue
//! back with conflict detection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::api::{BlockNode, Client};
use crate::markdown::split_runs;
use crate::model::{
    Block, BlockKind, Page, Parent, PropertyValue, RichText, TargetKind, TextBlock,
};
use crate::ops::{Op, OpKind, OpStatus};
use crate::store::{ts, Store};
use crate::{Error, Result};

#[derive(Debug, Default, PartialEq)]
pub struct PullReport {
    pub fetched: usize,
    pub updated: usize,
    pub skipped: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct PushReport {
    pub applied: usize,
    pub conflicts: usize,
    pub failed: usize,
}

// ---------------------------------------------------------------- pull

pub async fn pull_page(
    client: &Client,
    store: &Store,
    page_id: &str,
    force: bool,
) -> Result<PullReport> {
    let page = client.get_page(page_id).await?;
    if !force {
        if let Some(cached) = store.get_page(&page.id)? {
            // a row cached by pull_data_source has no blocks yet: fetch them
            if cached.last_edited_time == page.last_edited_time
                && !store.children(&page.id)?.is_empty()
            {
                return Ok(PullReport {
                    skipped: true,
                    ..Default::default()
                });
            }
        }
    }
    let tree = client.block_tree(&page.id).await?;

    // Snapshot local state before writing anything, so traversal order can't
    // matter: blocks with queued edits keep their cached version, queued
    // deletes stay gone, unpushed `local-*` creates and queued moves keep their
    // cached place (and subtree).
    let mut kept = HashMap::new();
    let mut deleted = HashSet::new();
    let mut moved = HashSet::new();
    for op in store.queued_ops()? {
        match &op.kind {
            OpKind::UpdateBlock { block_id, .. } => {
                let id = store.resolve_id(block_id)?;
                if let Some(b) = store.get_block(&id)? {
                    kept.insert(id, b);
                }
            }
            OpKind::MoveBlock { block_id, .. } => {
                moved.insert(store.resolve_id(block_id)?);
            }
            OpKind::DeleteBlock { block_id } => {
                deleted.insert(store.resolve_id(block_id)?);
            }
            _ => {}
        }
    }
    // parent id → [(cached previous sibling, block)] in cached order
    let mut pinned: HashMap<String, Vec<(Option<String>, Block)>> = HashMap::new();
    {
        // ponytail: windows over the whole blocks table; filter by page if caches get big
        let mut st = store.conn.prepare(
            "SELECT parent_id, prev, raw FROM (SELECT id, parent_id, position, raw,
               LAG(id) OVER (PARTITION BY parent_id ORDER BY position) AS prev FROM blocks)
             WHERE id LIKE 'local-%' OR id IN (SELECT value FROM json_each(?1))
             ORDER BY parent_id, position",
        )?;
        let moved_json = serde_json::to_string(&moved)?;
        let rows = st.query_map([moved_json], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (parent, prev, raw) = row?;
            pinned
                .entry(parent)
                .or_default()
                .push((prev, serde_json::from_str(&raw)?));
        }
    }

    let mut report = PullReport::default();
    let mut all: Vec<&BlockNode> = tree.iter().collect();
    while let Some(n) = all.pop() {
        report.fetched += 1;
        let cached = store.get_block(&n.block.id)?;
        if cached.map(|c| c.last_edited_time) != Some(n.block.last_edited_time) {
            report.updated += 1;
        }
        all.extend(&n.children);
    }

    // Top-down, every level, so moved blocks keep their descendants.
    let mut todo: Vec<(String, &[BlockNode])> = vec![(page.id.clone(), &tree)];
    while let Some((parent_id, nodes)) = todo.pop() {
        let mut list = Vec::new();
        for n in nodes {
            // Skipped subtrees: no rows under a missing parent, and a locally
            // moved block's cached subtree stays as it is.
            if deleted.contains(&n.block.id) || moved.contains(&n.block.id) {
                continue;
            }
            match kept.get(&n.block.id) {
                Some(c) => {
                    let mut c = c.clone();
                    c.parent = n.block.parent.clone();
                    c.has_children = n.block.has_children;
                    list.push(c);
                }
                None => list.push(n.block.clone()),
            }
            // A child page's id is that page's id: block_tree didn't fetch its
            // content, so leave its cached blocks alone.
            if !matches!(
                n.block.kind,
                BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
            ) {
                todo.push((n.block.id.clone(), &n.children));
            }
        }
        // Re-insert pinned blocks after their cached predecessor (first if
        // they were first, last if it's gone).
        for (prev, b) in pinned.remove(&parent_id).unwrap_or_default() {
            let at = match prev {
                None => 0,
                Some(p) => list
                    .iter()
                    .position(|x| x.id == p)
                    .map_or(list.len(), |i| i + 1),
            };
            list.insert(at, b);
        }
        store.replace_children(&page.id, &parent_id, &list)?;
    }
    let mut page = page;
    keep_queued_props(store, &mut page)?;
    store.upsert_page(&page)?;
    Ok(report)
}

pub async fn pull_data_source(client: &Client, store: &Store, ds_id: &str) -> Result<PullReport> {
    let ds = client.get_data_source(ds_id).await?;
    store.upsert_data_source(&ds)?;
    let rows = client.query_data_source(&ds.id, json!({})).await?;
    let mut report = PullReport {
        fetched: rows.len(),
        ..Default::default()
    };
    for mut row in rows {
        let cached = store.get_page(&row.id)?;
        if cached.map(|c| c.last_edited_time) != Some(row.last_edited_time) {
            keep_queued_props(store, &mut row)?;
            store.upsert_page(&row)?;
            report.updated += 1;
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------- push

enum Outcome {
    Applied,
    Conflict(String),
    Failed(String),
}

/// Applies queued ops oldest first. Api errors fail just that op; any other
/// error stops the push and leaves the op queued.
pub async fn push(client: &Client, store: &Store) -> Result<PushReport> {
    let mut report = PushReport::default();
    // last_edited_time of objects written by this push, so a later op on the
    // same object doesn't see our own write as a remote edit.
    let mut own = HashMap::new();
    // Anchors of failed creates: a later DeleteBlock of the anchor (a type
    // conversion's replace) must not run, or the original is lost.
    let mut blocked = HashSet::new();
    for op in store.queued_ops()? {
        if let OpKind::DeleteBlock { block_id } = &op.kind {
            if blocked.contains(&store.resolve_id(block_id)?) {
                store.mark_op(op.id, OpStatus::Failed, Some("anchor's create failed"))?;
                report.failed += 1;
                continue;
            }
        }
        let res = apply(client, store, &op, &mut own).await;
        if let (
            OpKind::CreateBlock { after: Some(a), .. },
            Ok(Outcome::Failed(_)) | Err(Error::Api { .. }),
        ) = (&op.kind, &res)
        {
            blocked.insert(store.resolve_id(a)?);
        }
        match res {
            Ok(Outcome::Applied) => {
                store.complete_op(op.id)?;
                report.applied += 1;
            }
            Ok(Outcome::Conflict(msg)) => {
                store.mark_op(op.id, OpStatus::Conflict, Some(&msg))?;
                report.conflicts += 1;
            }
            Ok(Outcome::Failed(msg)) => {
                store.mark_op(op.id, OpStatus::Failed, Some(&msg))?;
                report.failed += 1;
            }
            Err(e @ Error::Api { .. }) => {
                store.mark_op(op.id, OpStatus::Failed, Some(&e.to_string()))?;
                report.failed += 1;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(report)
}

/// Objects written by this push: last_edited_time and the written JSON, used as
/// the conflict base for later ops on the same object.
type Own = HashMap<String, (DateTime<Utc>, Value)>;

/// `Some(reason)` if the remote object changed since the base snapshot (our own
/// last write in this push if any, else the op's base).
fn conflict(
    op: &Op,
    id: &str,
    remote_time: DateTime<Utc>,
    own: &Own,
    same_as_base: impl FnOnce(&Value) -> Result<bool>,
) -> Result<Option<String>> {
    if op.base_last_edited_time.is_none() {
        return Ok(None);
    }
    let (base, base_raw) = match own.get(id) {
        Some((t, raw)) => (*t, Some(raw)),
        None => (op.base_last_edited_time.unwrap(), op.base_raw.as_ref()),
    };
    // Notion timestamps are minute-rounded: equal time needs a content check.
    let changed = remote_time > base
        || (remote_time == base
            && match base_raw {
                Some(raw) => !same_as_base(raw)?,
                None => false,
            });
    Ok(changed.then(|| format!("remote edited at {}, base {}", ts(&remote_time), ts(&base))))
}

fn record_block(own: &mut Own, b: &Block) -> Result<()> {
    own.insert(b.id.clone(), (b.last_edited_time, serde_json::to_value(b)?));
    Ok(())
}

async fn check_block(
    client: &Client,
    op: &Op,
    id: &str,
    own: &Own,
) -> Result<(Block, Option<String>)> {
    let remote = client.get_block(id).await?;
    let c = conflict(op, id, remote.last_edited_time, own, |raw| {
        Ok(same_content(
            &serde_json::from_value::<Block>(raw.clone())?.kind,
            &remote.kind,
        ))
    })?;
    Ok((remote, c))
}

/// Raw-backed kinds hold the whole block, which a single GET pads with fields
/// a listed copy lacks (`request_id`): compare only their type payload.
fn same_content(a: &BlockKind, b: &BlockKind) -> bool {
    match (a, b) {
        (BlockKind::Unsupported { raw: x }, BlockKind::Unsupported { raw: y }) => {
            let t = x["type"].as_str().unwrap_or_default();
            x["type"] == y["type"] && x[t] == y[t]
        }
        _ => a == b,
    }
}

fn unmovable(kind: &BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Unsupported { .. }
            | BlockKind::ChildPage { .. }
            | BlockKind::ChildDatabase { .. }
    )
}

fn find_unmovable(nodes: &[BlockNode]) -> Option<&BlockKind> {
    nodes.iter().find_map(|n| {
        if unmovable(&n.block.kind) {
            Some(&n.block.kind)
        } else {
            find_unmovable(&n.children)
        }
    })
}

fn first(mut made: Vec<Block>) -> Result<Block> {
    if made.is_empty() {
        return Err(Error::Invalid("append returned no blocks".into()));
    }
    Ok(made.swap_remove(0))
}

async fn apply(client: &Client, store: &Store, op: &Op, own: &mut Own) -> Result<Outcome> {
    match &op.kind {
        OpKind::UpdateBlock { block_id, body } => {
            let id = store.resolve_id(block_id)?;
            if let (_, Some(c)) = check_block(client, op, &id, own).await? {
                return Ok(Outcome::Conflict(c));
            }
            let b = client.update_block(&id, body).await?;
            store.put_block(&b)?;
            record_block(own, &b)?;
        }
        OpKind::DeleteBlock { block_id } => {
            let id = store.resolve_id(block_id)?;
            if let (_, Some(c)) = check_block(client, op, &id, own).await? {
                return Ok(Outcome::Conflict(c));
            }
            client.delete_block(&id).await?;
            store.delete_block(&id)?;
        }
        OpKind::UpdatePageProperties {
            page_id,
            properties,
        } => {
            let id = store.resolve_id(page_id)?;
            let remote = client.get_page(&id).await?;
            // Any block edit bumps a page's last_edited_time, so the time says
            // nothing here: only a remote change to a property this op writes
            // is a conflict (base: our own write in this push, else the op's).
            let base = match own.get(&id) {
                Some((_, raw)) => Some(raw),
                None => op.base_raw.as_ref(),
            };
            if let Some(base) = base {
                let base: Page = serde_json::from_value(base.clone())?;
                let changed: Vec<&String> = properties
                    .as_object()
                    .into_iter()
                    .flat_map(|o| o.keys())
                    .filter(|k| base.properties.get(*k) != remote.properties.get(*k))
                    .collect();
                if !changed.is_empty() {
                    return Ok(Outcome::Conflict(format!(
                        "remote changed {}",
                        changed
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
            let p = client.update_page_properties(&id, properties).await?;
            store.upsert_page(&p)?;
            own.insert(
                p.id.clone(),
                (p.last_edited_time, serde_json::to_value(&p)?),
            );
        }
        OpKind::CreateBlock {
            parent_id,
            after,
            block,
        } => {
            let local: Block = serde_json::from_value(block.clone())?;
            let parent = store.resolve_id(parent_id)?;
            let after = after.as_deref().map(|a| store.resolve_id(a)).transpose()?;
            let new = first(
                client
                    .append_children(&parent, std::slice::from_ref(&local), after.as_deref())
                    .await?,
            )?;
            // A create kept for a locally deleted block (see local_delete_block)
            // must not bring it back into the cache; its DeleteBlock follows.
            if store.get_block(&local.id)?.is_some() {
                store.rename_block(&local.id, &new.id)?;
                store.put_block(&new)?;
            }
            store.map_id(&local.id, &new.id)?;
            record_block(own, &new)?;
        }
        OpKind::MoveBlock {
            block_id,
            new_parent_id,
            after,
        } => {
            // The API can't move blocks: copy the subtree, then delete the original.
            let id = store.resolve_id(block_id)?;
            let new_parent = store.resolve_id(new_parent_id)?;
            let after = after.as_deref().map(|a| store.resolve_id(a)).transpose()?;
            let (remote, c) = check_block(client, op, &id, own).await?;
            if let Some(c) = c {
                return Ok(Outcome::Conflict(c));
            }
            let tree = if remote.has_children {
                client.block_tree(&id).await?
            } else {
                Vec::new()
            };
            let bad = if unmovable(&remote.kind) {
                Some(&remote.kind)
            } else {
                find_unmovable(&tree)
            };
            if let Some(k) = bad {
                return Ok(Outcome::Failed(format!(
                    "cannot move: subtree contains {}",
                    k.type_name()
                )));
            }
            let top = first(
                client
                    .append_children(&new_parent, &[remote], after.as_deref())
                    .await?,
            )?;
            // (old id, copy), every parent before its children
            let mut created = vec![(id.clone(), top.clone())];
            let mut todo: Vec<(String, &[BlockNode])> = vec![(top.id.clone(), &tree)];
            while let Some((pid, nodes)) = todo.pop() {
                if nodes.is_empty() {
                    continue;
                }
                let blocks: Vec<Block> = nodes.iter().map(|n| n.block.clone()).collect();
                let made = client.append_children(&pid, &blocks, None).await?;
                if made.len() != nodes.len() {
                    return Err(Error::Invalid(format!(
                        "append under {pid} returned {} of {} blocks",
                        made.len(),
                        nodes.len()
                    )));
                }
                for (n, m) in nodes.iter().zip(made) {
                    todo.push((m.id.clone(), &n.children));
                    created.push((n.block.id.clone(), m));
                }
            }
            client.delete_block(&id).await?;
            store.delete_block(&id)?;
            for (old, b) in &created {
                store.put_block(b)?;
                store.map_id(old, &b.id)?;
                record_block(own, b)?;
            }
            // put_block appended the copy last; cache order only, a pull fixes it otherwise
            if let Err(e) = store.move_block_local(&top.id, &new_parent, after.as_deref()) {
                tracing::warn!("moved block {} not repositioned in cache: {e}", top.id);
            }
        }
    }
    Ok(Outcome::Applied)
}

// ---------------------------------------------------------------- local edits

fn cached(store: &Store, block_id: &str) -> Result<Block> {
    store
        .get_block(block_id)?
        .ok_or_else(|| Error::Invalid(format!("block {block_id} not cached")))
}

/// Caches `block` and queues a PATCH of it with `body`, based on `base`. An
/// unpushed `local-*` block has its queued create rewritten with the new
/// content instead; that create's op id is returned.
fn enqueue_update(store: &Store, base: &Block, block: &Block, body: Value) -> Result<i64> {
    store.atomic(|| {
        store.put_block(block)?;
        if let Some(Op {
            id,
            kind: OpKind::CreateBlock {
                parent_id, after, ..
            },
            ..
        }) = store.queued_create_for(&block.id)?
        {
            let kind = OpKind::CreateBlock {
                parent_id,
                after,
                block: serde_json::to_value(cached(store, &block.id)?)?,
            };
            store.update_op_kind(id, &kind)?;
            return Ok(id);
        }
        store.enqueue(
            OpKind::UpdateBlock {
                block_id: block.id.clone(),
                body,
            },
            Some(base.last_edited_time),
            Some(serde_json::to_value(base)?),
        )
    })
}

/// Rewrites a cached block's text and queues the update. Returns the op id.
pub fn local_edit_text(store: &Store, block_id: &str, text: &str) -> Result<i64> {
    let runs = if text.is_empty() {
        Vec::new()
    } else {
        vec![RichText::plain(text)]
    };
    local_edit_rich(store, block_id, runs)
}

/// Replaces a cached block's rich text (runs over 2000 chars are split) and
/// queues the update. Returns the op id.
pub fn local_edit_rich(store: &Store, block_id: &str, rich_text: Vec<RichText>) -> Result<i64> {
    let base = cached(store, block_id)?;
    if base.kind.text().is_none() {
        return Err(Error::Invalid(format!(
            "block type `{}` has no text",
            base.kind.type_name()
        )));
    }
    let mut block = base.clone();
    block.kind.set_text(split_runs(rich_text))?;
    let mut body = block.kind.to_request_json()?;
    if let Some(o) = body.as_object_mut() {
        o.remove("type");
    }
    enqueue_update(store, &base, &block, body)
}

/// Replaces cell `col` of a cached `table_row` and queues
/// `{"table_row":{"cells":[..all cells..]}}`. Returns the op id.
pub fn local_edit_table_cell(
    store: &Store,
    row_id: &str,
    col: usize,
    rich: Vec<RichText>,
) -> Result<i64> {
    let base = cached(store, row_id)?;
    let mut raw = serde_json::to_value(&base)?;
    if raw["type"] != "table_row" {
        return Err(Error::Invalid(format!(
            "block type `{}` is not a table row",
            base.kind.type_name()
        )));
    }
    // untouched cells stay as received
    let cell = raw["table_row"]["cells"]
        .get_mut(col)
        .ok_or_else(|| Error::Invalid(format!("table row {row_id} has no cell {col}")))?;
    *cell = serde_json::to_value(split_runs(rich))?;
    let body = json!({ "table_row": { "cells": raw["table_row"]["cells"] } });
    let block: Block = serde_json::from_value(raw)?;
    enqueue_update(store, &base, &block, body)
}

/// Sets a cached equation block's expression and queues
/// `{"equation":{"expression":..}}`. Returns the op id.
pub fn local_edit_equation(store: &Store, block_id: &str, expression: &str) -> Result<i64> {
    let base = cached(store, block_id)?;
    let mut block = base.clone();
    match &mut block.kind {
        BlockKind::Equation { expression: e } => *e = expression.to_string(),
        k => {
            return Err(Error::Invalid(format!(
                "block type `{}` is not an equation",
                k.type_name()
            )))
        }
    }
    enqueue_update(
        store,
        &base,
        &block,
        json!({ "equation": { "expression": expression } }),
    )
}

/// Sets a cached to-do's checkbox and queues `{"to_do":{"checked":..}}`.
pub fn local_set_checked(store: &Store, block_id: &str, checked: bool) -> Result<i64> {
    let base = cached(store, block_id)?;
    let mut block = base.clone();
    match &mut block.kind {
        BlockKind::ToDo { checked: c, .. } => *c = checked,
        k => {
            return Err(Error::Invalid(format!(
                "block type `{}` is not a to-do",
                k.type_name()
            )))
        }
    }
    enqueue_update(
        store,
        &base,
        &block,
        json!({ "to_do": { "checked": checked } }),
    )
}

/// The queued UpdatePageProperties op for `page_id`, if any.
fn queued_props(store: &Store, page_id: &str) -> Result<Option<Op>> {
    for op in store.queued_ops()? {
        if let OpKind::UpdatePageProperties { page_id: p, .. } = &op.kind {
            if store.resolve_id(p)? == page_id {
                return Ok(Some(op));
            }
        }
    }
    Ok(None)
}

/// A remote `page` keeps the cached values of the properties a queued
/// UpdatePageProperties op will write; the rest is taken from remote.
// ponytail: scans the queue per page; index by page id if queues get long
fn keep_queued_props(store: &Store, page: &mut Page) -> Result<()> {
    let Some(Op {
        kind: OpKind::UpdatePageProperties { properties, .. },
        ..
    }) = queued_props(store, &page.id)?
    else {
        return Ok(());
    };
    let Some(cached) = store.get_page(&page.id)? else {
        return Ok(());
    };
    for k in properties.as_object().into_iter().flat_map(|o| o.keys()) {
        match cached.properties.get(k) {
            Some(v) => page.properties.insert(k.clone(), v.clone()),
            None => page.properties.remove(k),
        };
    }
    // Keep the cached time too, so the pull after the op leaves the queue
    // isn't skipped as "unchanged" while the cache still holds our title.
    page.last_edited_time = cached.last_edited_time;
    Ok(())
}

/// Sets a cached page's title (plain text) and queues
/// `{"<title key>":{"title":[..]}}`, merged into this page's queued property
/// update if there is one. Returns the op id.
pub fn local_set_title(store: &Store, page_id: &str, title: &str) -> Result<i64> {
    let base = store
        .get_page(page_id)?
        .ok_or_else(|| Error::Invalid(format!("page {page_id} not cached")))?;
    let key = base
        .properties
        .iter()
        .find(|(_, v)| matches!(v, PropertyValue::Title(_)))
        .map(|(k, _)| k.clone())
        .ok_or_else(|| Error::Invalid(format!("page {page_id} has no title property")))?;
    let runs = if title.is_empty() {
        Vec::new()
    } else {
        split_runs(vec![RichText::plain(title)])
    };
    let body: Vec<Value> = runs
        .iter()
        .map(|r| json!({ "type": "text", "text": { "content": r.plain_text } }))
        .collect();
    let mut page = base.clone();
    page.properties
        .insert(key.clone(), PropertyValue::Title(runs));
    store.atomic(|| {
        store.upsert_page(&page)?;
        if let Some(Op {
            id,
            kind:
                OpKind::UpdatePageProperties {
                    page_id,
                    mut properties,
                },
            ..
        }) = queued_props(store, &page.id)?
        {
            properties[&key] = json!({ "title": body });
            store.update_op_kind(
                id,
                &OpKind::UpdatePageProperties {
                    page_id,
                    properties,
                },
            )?;
            return Ok(id);
        }
        store.enqueue(
            OpKind::UpdatePageProperties {
                page_id: page.id.clone(),
                properties: json!({ key: { "title": body } }),
            },
            Some(base.last_edited_time),
            Some(serde_json::to_value(&base)?),
        )
    })
}

/// (placed block, parent, after) of an op that puts a block somewhere.
fn placement(k: &OpKind) -> Option<(&str, &str, Option<&str>)> {
    match k {
        OpKind::CreateBlock {
            parent_id,
            after,
            block,
        } => Some((
            block["id"].as_str().unwrap_or_default(),
            parent_id,
            after.as_deref(),
        )),
        OpKind::MoveBlock {
            block_id,
            new_parent_id,
            after,
        } => Some((block_id, new_parent_id, after.as_deref())),
        _ => None,
    }
}

/// The block an op is about (for a create: the block it creates).
pub fn op_block(k: &OpKind) -> Option<&str> {
    match k {
        OpKind::CreateBlock { block, .. } => block["id"].as_str(),
        OpKind::UpdateBlock { block_id, .. }
        | OpKind::DeleteBlock { block_id }
        | OpKind::MoveBlock { block_id, .. } => Some(block_id),
        OpKind::UpdatePageProperties { .. } => None,
    }
}

/// Deletes a cached block and its subtree.
///
/// Queued ops on blocks in the subtree are dropped, except a `local-*`
/// block's create that a surviving op is placed relative to (as parent or
/// `after`): it stays, so the queue still replays in order. A real block
/// whose dropped MoveBlock put it into the subtree gets its own DeleteBlock,
/// since remotely it never moved. Returns the root's DeleteBlock op, or None
/// if the root is a `local-*` block whose create was dropped.
pub fn local_delete_block(store: &Store, block_id: &str) -> Result<Option<i64>> {
    store.atomic(|| {
        let block = cached(store, block_id)?;
        let subtree: HashSet<String> = store.subtree_ids(block_id)?.into_iter().collect();
        let ops = store.queued_ops()?;
        let mut drop = Vec::new();
        for op in &ops {
            let t = op_block(&op.kind)
                .map(|t| store.resolve_id(t))
                .transpose()?;
            drop.push(t.is_some_and(|t| subtree.contains(&t)));
        }
        loop {
            let mut used = HashSet::new();
            for (op, _) in ops.iter().zip(&drop).filter(|(_, d)| !**d) {
                if let Some((_, parent, after)) = placement(&op.kind) {
                    used.insert(parent);
                    used.extend(after);
                }
            }
            let mut kept_one = false;
            for (op, d) in ops.iter().zip(drop.iter_mut()) {
                if *d
                    && matches!(op.kind, OpKind::CreateBlock { .. })
                    && op_block(&op.kind).is_some_and(|b| used.contains(b))
                {
                    *d = false;
                    kept_one = true;
                }
            }
            if !kept_one {
                break;
            }
        }

        // first dropped base per block: what the remote looked like before our edits
        let mut bases = HashMap::new();
        let mut moved_in = Vec::new();
        let mut root_create_kept = false;
        for (op, d) in ops.iter().zip(&drop) {
            let t = match op_block(&op.kind) {
                Some(t) => store.resolve_id(t)?,
                None => continue,
            };
            if !d {
                root_create_kept |= t == block_id;
                continue;
            }
            store.complete_op(op.id)?;
            if op.base_last_edited_time.is_some() {
                bases
                    .entry(t.clone())
                    .or_insert((op.base_last_edited_time, op.base_raw.clone()));
            }
            if matches!(op.kind, OpKind::MoveBlock { .. })
                && !t.starts_with("local-")
                && t != block_id
                && !moved_in.contains(&t)
            {
                moved_in.push(t);
            }
        }
        store.delete_block(block_id)?;
        for t in moved_in {
            let (time, raw) = bases.remove(&t).unwrap_or_default();
            store.enqueue(OpKind::DeleteBlock { block_id: t }, time, raw)?;
        }
        let (time, raw) = if block_id.starts_with("local-") {
            if !root_create_kept {
                return Ok(None);
            }
            (None, None) // our own create: nothing remote to conflict with
        } else {
            bases.remove(block_id).unwrap_or((
                Some(block.last_edited_time),
                Some(serde_json::to_value(&block)?),
            ))
        };
        store
            .enqueue(
                OpKind::DeleteBlock {
                    block_id: block_id.to_string(),
                },
                time,
                raw,
            )
            .map(Some)
    })
}

/// Moves a cached block under `new_parent` after `after` (None = last) and
/// queues the move.
///
/// An unpushed `local-*` block has its queued create rewritten instead
/// (returns None) when that keeps the queue's replay order: no other op is
/// placed after the block, the block has no queued move, and no later op
/// places the new parent or anchor, or anything into the new parent.
/// Otherwise a MoveBlock is queued.
pub fn local_move_block(
    store: &Store,
    block_id: &str,
    new_parent: &str,
    after: Option<&str>,
) -> Result<Option<i64>> {
    store.atomic(|| {
        let base = cached(store, block_id)?;
        store.move_block_local(block_id, new_parent, after)?;
        let create = store.queued_create_for(block_id)?;
        if let Some(c) = &create {
            let mut safe = true;
            for op in store.queued_ops()? {
                if let Some((b, parent, aft)) = placement(&op.kind).filter(|_| op.id != c.id) {
                    let later = op.id > c.id;
                    safe &= b != block_id
                        && aft != Some(block_id)
                        && !(later
                            && (b == new_parent || Some(b) == after || parent == new_parent));
                }
            }
            if safe {
                let kind = OpKind::CreateBlock {
                    parent_id: new_parent.to_string(),
                    after: after.map(str::to_string),
                    block: serde_json::to_value(cached(store, block_id)?)?,
                };
                store.update_op_kind(c.id, &kind)?;
                return Ok(None);
            }
        }
        // A local block's base is our own create: nothing remote to conflict with.
        let (base_time, base_raw) = if create.is_some() {
            (None, None)
        } else {
            (
                Some(base.last_edited_time),
                Some(serde_json::to_value(&base)?),
            )
        };
        store
            .enqueue(
                OpKind::MoveBlock {
                    block_id: block_id.to_string(),
                    new_parent_id: new_parent.to_string(),
                    after: after.map(str::to_string),
                },
                base_time,
                base_raw,
            )
            .map(Some)
    })
}

/// Caches a new block under a temporary `local-…` id and queues its creation.
pub fn local_create_block(
    store: &Store,
    parent_id: &str,
    after: Option<&str>,
    kind: BlockKind,
) -> Result<String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = Utc::now();
    let id = format!(
        "local-{}-{}",
        now.timestamp_nanos_opt().unwrap_or_default(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let parent = if store.get_page(parent_id)?.is_some() {
        Parent::Page(parent_id.to_string())
    } else {
        Parent::Block(parent_id.to_string())
    };
    let mut v = kind.to_request_json()?; // rejects unwritable kinds up front
    v["object"] = json!("block");
    v["id"] = json!(id);
    v["parent"] = serde_json::to_value(&parent)?;
    v["has_children"] = json!(false);
    v["in_trash"] = json!(false);
    v["created_time"] = json!(ts(&now));
    v["last_edited_time"] = json!(ts(&now));
    let block: Block = serde_json::from_value(v)?;
    store.atomic(|| {
        store.put_block(&block)?;
        store.move_block_local(&id, parent_id, after)?;
        store.enqueue(
            OpKind::CreateBlock {
                parent_id: parent_id.to_string(),
                after: after.map(str::to_string),
                block: serde_json::to_value(&block)?,
            },
            None,
            None,
        )
    })?;
    Ok(id)
}

/// What `local_change_type` did to the block's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// Same block, same id (same type, or an unpushed create rewritten).
    Kept,
    /// The block was replaced by this new `local-*` block at its position.
    Replaced(String),
}

/// Converts a cached block to `target`, keeping its rich text (Divider drops
/// it, Code keeps it as plain text).
///
/// Notion can't change a block's type, so an existing block is replaced: a new
/// block is created right after it and the old one deleted (new id, comments
/// and backlinks are lost). An unpushed `local-*` block has its queued create
/// rewritten instead. Blocks with children are refused.
pub fn local_change_type(
    store: &Store,
    block_id: &str,
    target: TargetKind,
) -> Result<ChangeOutcome> {
    store.atomic(|| {
        let mut block = cached(store, block_id)?;
        let from = TargetKind::of(&block.kind).ok_or_else(|| {
            Error::Invalid(format!(
                "block type `{}` cannot be converted",
                block.kind.type_name()
            ))
        })?;
        if from == target {
            return Ok(ChangeOutcome::Kept);
        }
        if block.has_children || !store.children(block_id)?.is_empty() {
            return Err(Error::Invalid(
                "cannot convert a block that has children".into(),
            ));
        }
        let text = match &block.kind {
            // the expression becomes plain text
            BlockKind::Equation { expression } if !expression.is_empty() => TextBlock {
                rich_text: vec![RichText::plain(expression)],
                color: "default".into(),
            },
            k => k.text().cloned().unwrap_or(TextBlock {
                rich_text: Vec::new(),
                color: "default".into(),
            }),
        };
        let kind = target.block_kind(TextBlock {
            rich_text: split_runs(text.rich_text),
            ..text
        });
        if let Some(Op {
            id,
            kind: OpKind::CreateBlock {
                parent_id, after, ..
            },
            ..
        }) = store.queued_create_for(block_id)?
        {
            block.kind = kind;
            store.put_block(&block)?;
            let kind = OpKind::CreateBlock {
                parent_id,
                after,
                block: serde_json::to_value(cached(store, block_id)?)?,
            };
            store.update_op_kind(id, &kind)?;
            return Ok(ChangeOutcome::Kept);
        }
        if block_id.starts_with("local-") {
            return Err(Error::Invalid("block's create is not queued".into()));
        }
        // local_delete_block would drop the unpushed move, leaving the block
        // remotely where it was, so the create's `after` would be in the
        // wrong parent.
        for op in store.queued_ops()? {
            if let OpKind::MoveBlock { block_id: m, .. } = &op.kind {
                if store.resolve_id(m)? == block_id {
                    return Err(Error::Invalid(
                        "cannot convert a moved block before pushing the move".into(),
                    ));
                }
            }
        }
        let parent = block
            .parent
            .id()
            .ok_or_else(|| Error::Invalid(format!("block {block_id} has no parent id")))?
            .to_string();
        let new = local_create_block(store, &parent, Some(block_id), kind)?;
        local_delete_block(store, block_id)?;
        Ok(ChangeOutcome::Replaced(new))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ClientConfig;
    use crate::model::plain_text;
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const T: &str = "2025-01-01T00:00:00.000Z";
    const T2: &str = "2025-01-02T00:00:00.000Z";

    fn client(s: &MockServer) -> Client {
        Client::new(
            "t".into(),
            ClientConfig {
                base_url: s.uri(),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                max_backoff: Duration::from_secs(1),
            },
        )
    }

    fn pp(id: &str) -> Value {
        json!({ "type": "page_id", "page_id": id })
    }
    fn bp(id: &str) -> Value {
        json!({ "type": "block_id", "block_id": id })
    }

    fn blk(id: &str, parent: Value, text: &str, time: &str, has_children: bool) -> Value {
        json!({
            "object": "block", "id": id, "parent": parent, "type": "paragraph",
            "paragraph": { "rich_text": [{ "type": "text", "text": { "content": text },
                "plain_text": text, "annotations": {} }], "color": "default" },
            "has_children": has_children, "in_trash": false, "created_time": T, "last_edited_time": time,
        })
    }

    fn page(id: &str, parent: Value, time: &str) -> Value {
        json!({
            "object": "page", "id": id, "parent": parent, "created_time": T, "last_edited_time": time,
            "in_trash": false, "url": "https://notion.so/x",
            "properties": { "Name": { "id": "title", "type": "title", "title": [] } },
        })
    }

    /// page() under "root" with title `t`.
    fn titled(id: &str, time: &str, t: &str) -> Value {
        let mut v = page(id, pp("root"), time);
        v["properties"]["Name"]["title"] = json!([{ "type": "text", "text": { "content": t },
            "plain_text": t, "annotations": {} }]);
        v
    }

    fn ok(v: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(v)
    }
    fn list(results: Vec<Value>) -> ResponseTemplate {
        ok(json!({ "object": "list", "results": results, "has_more": false, "next_cursor": null }))
    }

    async fn get(s: &MockServer, p: &str, resp: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(resp)
            .mount(s)
            .await;
    }

    fn ids(bs: &[Block]) -> Vec<&str> {
        bs.iter().map(|b| b.id.as_str()).collect()
    }
    fn text(store: &Store, id: &str) -> String {
        plain_text(
            &store
                .get_block(id)
                .unwrap()
                .unwrap()
                .kind
                .text()
                .unwrap()
                .rich_text,
        )
    }

    /// Cache page "p" with leaf children `kids` (all at time T).
    async fn seed(s: &MockServer, store: &Store, kids: &[&str]) {
        get(s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        let results = kids.iter().map(|k| blk(k, pp("p"), k, T, false)).collect();
        get(s, "/v1/blocks/p/children", list(results)).await;
        pull_page(&client(s), store, "p", true).await.unwrap();
        s.reset().await;
    }

    #[tokio::test]
    async fn pull_fills_cache() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("a", pp("p"), "A", T, true),
                blk("b", pp("p"), "B", T, false),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/a/children",
            list(vec![blk("a1", bp("a"), "A1", T, false)]),
        )
        .await;

        let r = pull_page(&client(&s), &store, "p", false).await.unwrap();
        assert_eq!(
            r,
            PullReport {
                fetched: 3,
                updated: 3,
                skipped: false
            }
        );
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "b"]);
        assert_eq!(ids(&store.children("a").unwrap()), ["a1"]);
        assert!(store.get_page("p").unwrap().is_some());
    }

    #[tokio::test]
    async fn unchanged_page_is_skipped_unless_forced() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![blk("a", pp("p"), "A", T, false)]))
            .expect(1)
            .mount(&s)
            .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        let r = pull_page(&c, &store, "p", false).await.unwrap();
        assert!(r.skipped);
        s.verify().await;
        s.reset().await;

        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![blk("a", pp("p"), "A", T, false)]))
            .expect(1)
            .mount(&s)
            .await;
        let r = pull_page(&c, &store, "p", true).await.unwrap();
        assert_eq!(
            r,
            PullReport {
                fetched: 1,
                updated: 0,
                skipped: false
            }
        );
    }

    #[tokio::test]
    async fn pull_prunes_removed_and_keeps_local_edits() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_edit_text(&store, "b", "local B").unwrap();
        let local = local_create_block(
            &store,
            "p",
            None,
            store.get_block("b").unwrap().unwrap().kind,
        )
        .unwrap();

        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "remote B", T2, false)]),
        )
        .await;
        pull_page(&client(&s), &store, "p", false).await.unwrap();
        assert_eq!(store.get_block("a").unwrap(), None);
        assert_eq!(text(&store, "b"), "local B");
        assert_eq!(ids(&store.children("p").unwrap()), ["b", local.as_str()]);
    }

    #[tokio::test]
    async fn parent_pull_keeps_child_page_blocks() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        store
            .replace_children(
                "c",
                "c",
                &[serde_json::from_value(blk("c1", pp("c"), "C1", T, false)).unwrap()],
            )
            .unwrap();
        let mut child = blk("c", pp("p"), "", T, true);
        child["type"] = json!("child_page");
        child["child_page"] = json!({ "title": "Child" });
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(&s, "/v1/blocks/p/children", list(vec![child])).await;
        pull_page(&client(&s), &store, "p", false).await.unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["c"]);
        assert_eq!(ids(&store.children("c").unwrap()), ["c1"]);
    }

    #[tokio::test]
    async fn locally_edited_block_moved_remotely_keeps_edit() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("b", pp("p"), "B", T, false),
                blk("a", pp("p"), "A", T, true),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/a/children",
            list(vec![blk("e", bp("a"), "E", T, false)]),
        )
        .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        s.reset().await;
        local_edit_text(&store, "e", "mine").unwrap();
        let kid = local_create_block(&store, "e", None, BlockKind::Divider).unwrap();

        // e moved a → b remotely; a is visited (and emptied) before b
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("b", pp("p"), "B", T2, true),
                blk("a", pp("p"), "A", T2, false),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/b/children",
            list(vec![blk("e", bp("b"), "E", T2, false)]),
        )
        .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        assert!(store.children("a").unwrap().is_empty());
        assert_eq!(ids(&store.children("b").unwrap()), ["e"]);
        assert_eq!(text(&store, "e"), "mine");
        assert_eq!(ids(&store.children("e").unwrap()), [kid.as_str()]);
    }

    #[tokio::test]
    async fn cached_page_without_blocks_is_not_skipped() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_page(&serde_json::from_value(page("p", pp("root"), T)).unwrap())
            .unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("a", pp("p"), "A", T, false)]),
        )
        .await;
        let r = pull_page(&client(&s), &store, "p", false).await.unwrap();
        assert!(!r.skipped);
        assert_eq!(ids(&store.children("p").unwrap()), ["a"]);
    }

    #[tokio::test]
    async fn queued_delete_omitted_during_pull() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let del = OpKind::DeleteBlock {
            block_id: "a".into(),
        };
        store.enqueue(del, None, None).unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("a", pp("p"), "a", T, false),
                blk("b", pp("p"), "b", T, false),
            ]),
        )
        .await;
        pull_page(&client(&s), &store, "p", false).await.unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["b"]);
    }

    #[tokio::test]
    async fn push_delete_block() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let a = store.get_block("a").unwrap().unwrap();
        let del = OpKind::DeleteBlock {
            block_id: "a".into(),
        };
        let raw = serde_json::to_value(&a).unwrap();
        store
            .enqueue(del, Some(a.last_edited_time), Some(raw))
            .unwrap();
        get(&s, "/v1/blocks/a", ok(blk("a", pp("p"), "a", T, false))).await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/a"))
            .respond_with(ok(blk("a", pp("p"), "a", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(ids(&store.children("p").unwrap()), ["b"]);
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn push_page_properties_with_conflict_check() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let props = json!({ "Name": { "title": [{ "text": { "content": "New" } }] } });
        for id in ["r", "q"] {
            let p: Page = serde_json::from_value(page(id, pp("root"), T)).unwrap();
            store.upsert_page(&p).unwrap();
            let op = OpKind::UpdatePageProperties {
                page_id: id.into(),
                properties: props.clone(),
            };
            let raw = serde_json::to_value(&p).unwrap();
            store
                .enqueue(op, Some(p.last_edited_time), Some(raw))
                .unwrap();
        }
        get(&s, "/v1/pages/r", ok(page("r", pp("root"), T))).await;
        // its title edited remotely
        get(&s, "/v1/pages/q", ok(titled("q", T2, "Other"))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/pages/r"))
            .and(body_string_contains("New"))
            .respond_with(ok(page("r", pp("root"), T2)))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/pages/q"))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 1,
                failed: 0
            }
        );
        assert_eq!(
            ts(&store.get_page("r").unwrap().unwrap().last_edited_time),
            T2
        );
        assert_eq!(store.ops_by_status(OpStatus::Conflict).unwrap().len(), 1);
    }

    #[test]
    fn local_set_title_uses_the_title_key_and_rewrites_its_op() {
        let store = Store::open_in_memory().unwrap();
        let p: Page = serde_json::from_value(page("p", pp("root"), T)).unwrap();
        store.upsert_page(&p).unwrap();
        let id = local_set_title(&store, "p", "One").unwrap();
        let cached = store.get_page("p").unwrap().unwrap();
        assert_eq!(cached.title(), "One");
        assert_eq!(store.page_tree().unwrap()[0].title, "One");
        // the base stays the pulled page
        assert_eq!(ts(&cached.last_edited_time), T);
        let id2 = local_set_title(&store, "p", "Two").unwrap();
        assert_eq!(id, id2);
        let ops = store.queued_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].kind,
            OpKind::UpdatePageProperties {
                page_id: "p".into(),
                properties: json!({ "Name": { "title": [
                    { "type": "text", "text": { "content": "Two" } }] } }),
            }
        );
        assert_eq!(
            ops[0].base_raw.as_ref(),
            Some(&serde_json::to_value(&p).unwrap())
        );
        assert!(local_set_title(&store, "nope", "x").is_err());
    }

    #[tokio::test]
    async fn title_push_after_a_block_edit_is_not_a_conflict() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "x").unwrap();
        local_set_title(&store, "p", "New").unwrap();

        // a pull meanwhile keeps the local title, takes the rest from remote
        let mut remote = page("p", pp("root"), T);
        remote["properties"]["Note"] = json!({ "id": "n", "type": "rich_text", "rich_text": [] });
        get(&s, "/v1/pages/p", ok(remote)).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "b", T, false)]),
        )
        .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
        let cached = store.get_page("p").unwrap().unwrap();
        assert_eq!(cached.title(), "New");
        assert!(cached.properties.contains_key("Note"));
        s.reset().await;

        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "x", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        // the block PATCH bumped the page's time; its properties are unchanged
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        let done = titled("p", T2, "New");
        Mock::given(method("PATCH"))
            .and(path("/v1/pages/p"))
            .and(body_partial_json(
                json!({ "properties": { "Name": { "title": [
                { "type": "text", "text": { "content": "New" } }] } } }),
            ))
            .respond_with(ok(done))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 2,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(store.get_page("p").unwrap().unwrap().title(), "New");
        assert!(store.queued_ops().unwrap().is_empty());
    }

    /// Title op on seeded page "p" pushed against a remote page `remote`.
    async fn push_title(s: &MockServer, store: &Store, remote: Value) -> PushReport {
        local_set_title(store, "p", "New").unwrap();
        get(s, "/v1/pages/p", ok(remote)).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/pages/p"))
            .respond_with(ok(titled("p", T2, "New")))
            .mount(s)
            .await;
        push(&client(s), store).await.unwrap()
    }

    #[tokio::test]
    async fn title_conflicts_only_on_a_remote_title_change() {
        let report = |applied, conflicts| PushReport {
            applied,
            conflicts,
            failed: 0,
        };
        // a delete in the same push: the block is gone from the cache
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_delete_block(&store, "b").unwrap();
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push_title(&s, &store, page("p", pp("root"), T2)).await;
        assert_eq!(r, report(2, 0));

        // a block edited elsewhere since the pull bumped the page's time
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let r = push_title(&s, &store, page("p", pp("root"), T2)).await;
        assert_eq!(r, report(1, 0));
        assert_eq!(store.get_page("p").unwrap().unwrap().title(), "New");

        // the title itself changed remotely
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let r = push_title(&s, &store, titled("p", T2, "Theirs")).await;
        assert_eq!(r, report(0, 1));
        let c = store.ops_by_status(OpStatus::Conflict).unwrap();
        assert_eq!(c[0].error.as_deref(), Some("remote changed Name"));
    }

    #[tokio::test]
    async fn remote_edit_in_same_minute_as_own_write_conflicts() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "one").unwrap();
        local_edit_text(&store, "b", "two").unwrap();
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T, false)))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        // someone else edited after our write, same minute
        get(
            &s,
            "/v1/blocks/b",
            ok(blk("b", pp("p"), "theirs", T2, false)),
        )
        .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "one", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 1,
                failed: 0
            }
        );
    }

    #[tokio::test]
    async fn moved_block_keeps_descendants() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("x", pp("p"), "X", T, true),
                blk("y", pp("p"), "Y", T, false),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/x/children",
            list(vec![blk("m", bp("x"), "M", T, true)]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/m/children",
            list(vec![blk("m1", bp("m"), "M1", T, false)]),
        )
        .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        s.reset().await;

        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("x", pp("p"), "X", T, false),
                blk("y", pp("p"), "Y", T2, true),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/y/children",
            list(vec![blk("m", bp("y"), "M", T2, true)]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/m/children",
            list(vec![blk("m1", bp("m"), "M1", T, false)]),
        )
        .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        assert!(store.children("x").unwrap().is_empty());
        assert_eq!(ids(&store.children("y").unwrap()), ["m"]);
        assert_eq!(ids(&store.children("m").unwrap()), ["m1"]);
    }

    #[tokio::test]
    async fn push_update_block() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "new text").unwrap();
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .and(body_string_contains("new text"))
            .respond_with(ok(blk("b", pp("p"), "new text", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 0
            }
        );
        assert!(store.queued_ops().unwrap().is_empty());
        let b = store.get_block("b").unwrap().unwrap();
        assert_eq!(ts(&b.last_edited_time), T2);
        assert_eq!(text(&store, "b"), "new text");
    }

    #[tokio::test]
    async fn newer_remote_is_conflict() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "mine").unwrap();
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T2, false))).await;
        Mock::given(method("PATCH"))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 0,
                conflicts: 1,
                failed: 0
            }
        );
        let c = store.ops_by_status(OpStatus::Conflict).unwrap();
        assert_eq!(c.len(), 1);
        assert!(c[0].error.as_deref().unwrap().contains(T2));
    }

    #[tokio::test]
    async fn equal_time_compares_content() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_edit_text(&store, "a", "mine a").unwrap();
        local_edit_text(&store, "b", "mine b").unwrap();
        // a: same time, changed remotely → conflict; b: same time, same content → applied
        get(
            &s,
            "/v1/blocks/a",
            ok(blk("a", pp("p"), "theirs", T, false)),
        )
        .await;
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/a"))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "mine b", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 1,
                failed: 0
            }
        );
    }

    #[tokio::test]
    async fn two_edits_of_same_block_both_apply() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "one").unwrap();
        local_edit_text(&store, "b", "two").unwrap();
        // first GET sees the base; afterwards the remote carries our first write's time
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T, false)))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "one", T2, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "one", T2, false)))
            .expect(2)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 2,
                conflicts: 0,
                failed: 0
            }
        );
    }

    #[tokio::test]
    async fn api_error_fails_one_op_only() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_edit_text(&store, "a", "x").unwrap();
        local_edit_text(&store, "b", "y").unwrap();
        get(&s, "/v1/blocks/a", ok(blk("a", pp("p"), "a", T, false))).await;
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/a"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "object": "error", "status": 400, "code": "validation_error", "message": "bad body"
            })))
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "y", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 1
            }
        );
        let f = store.ops_by_status(OpStatus::Failed).unwrap();
        assert!(f[0].error.as_deref().unwrap().contains("bad body"));
    }

    #[tokio::test]
    async fn create_then_edit_goes_to_real_id() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let kind = serde_json::from_value::<Block>(blk("z", pp("p"), "x", T, false))
            .unwrap()
            .kind;
        let local = local_create_block(&store, "p", None, kind).unwrap();
        assert!(local.starts_with("local-"));
        // the edit is folded into the pending create: one op, created with the new text
        let create = store.queued_ops().unwrap()[0].id;
        assert_eq!(local_edit_text(&store, &local, "edited").unwrap(), create);
        assert_eq!(store.queued_ops().unwrap().len(), 1);

        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_string_contains("edited"))
            .respond_with(list(vec![blk("real1", pp("p"), "edited", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(ids(&store.children("p").unwrap()), ["real1"]);
        assert_eq!(text(&store, "real1"), "edited");
        let n: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM blocks WHERE id LIKE 'local-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn move_block_copies_subtree() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["m", "t"]).await;
        let op = |id: &str| OpKind::MoveBlock {
            block_id: id.into(),
            new_parent_id: "t".into(),
            after: None,
        };
        store
            .enqueue(op("m"), Some(T.parse().unwrap()), None)
            .unwrap();
        get(&s, "/v1/blocks/m", ok(blk("m", pp("p"), "M", T, true))).await;
        get(
            &s,
            "/v1/blocks/m/children",
            list(vec![blk("m1", bp("m"), "M1", T, false)]),
        )
        .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/t/children"))
            .respond_with(list(vec![blk("nm", bp("t"), "M", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/nm/children"))
            .and(body_string_contains("M1"))
            .respond_with(list(vec![blk("nm1", bp("nm"), "M1", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/m"))
            .respond_with(ok(blk("m", pp("p"), "M", T2, true)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(store.get_block("m").unwrap(), None);
        assert_eq!(ids(&store.children("p").unwrap()), ["t"]);
        assert_eq!(ids(&store.children("t").unwrap()), ["nm"]);
        assert_eq!(ids(&store.children("nm").unwrap()), ["nm1"]);
        assert_eq!(store.resolve_id("m").unwrap(), "nm");
        assert_eq!(store.resolve_id("m1").unwrap(), "nm1");
        s.verify().await;
        s.reset().await;

        // a subtree with an unsupported block is refused before any write
        store.enqueue(op("u"), None, None).unwrap();
        get(&s, "/v1/blocks/u", ok(blk("u", pp("p"), "U", T, true))).await;
        let mut img = blk("i", bp("u"), "", T, false);
        img["type"] = json!("image");
        get(&s, "/v1/blocks/u/children", list(vec![img])).await;
        Mock::given(method("PATCH"))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 0,
                conflicts: 0,
                failed: 1
            }
        );
        let f = store.ops_by_status(OpStatus::Failed).unwrap();
        assert_eq!(
            f[0].error.as_deref(),
            Some("cannot move: subtree contains image")
        );
    }

    #[tokio::test]
    async fn edit_after_two_moves_goes_to_final_copy() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b", "m"]).await;
        // Tab, Shift+Tab, then an edit
        local_move_block(&store, "m", "a", None).unwrap().unwrap();
        local_move_block(&store, "m", "p", Some("a"))
            .unwrap()
            .unwrap();
        local_edit_text(&store, "m", "edited").unwrap();

        get(&s, "/v1/blocks/m", ok(blk("m", pp("p"), "m", T, false))).await;
        get(&s, "/v1/blocks/m1", ok(blk("m1", bp("a"), "m", T2, false))).await;
        get(&s, "/v1/blocks/m2", ok(blk("m2", pp("p"), "m", T2, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/a/children"))
            .respond_with(list(vec![blk("m1", bp("a"), "m", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![blk("m2", pp("p"), "m", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        for id in ["m", "m1"] {
            Mock::given(method("DELETE"))
                .and(path(format!("/v1/blocks/{id}")))
                .respond_with(ok(blk(id, pp("p"), "m", T2, false)))
                .expect(1)
                .mount(&s)
                .await;
        }
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/m2"))
            .and(body_string_contains("edited"))
            .respond_with(ok(blk("m2", pp("p"), "edited", T2, false)))
            .expect(1)
            .mount(&s)
            .await;

        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 3,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(store.resolve_id("m").unwrap(), "m2");
        assert_eq!(text(&store, "m2"), "edited");
        s.verify().await;
    }

    #[tokio::test]
    async fn pull_data_source_upserts_changed_rows() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(
            &s,
            "/v1/data_sources/ds",
            ok(json!({
                "object": "data_source", "id": "ds", "parent": { "type": "database_id", "database_id": "db" },
                "title": [], "last_edited_time": T, "properties": {},
            })),
        )
        .await;
        let row = |id: &str| {
            page(
                id,
                json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" }),
                T,
            )
        };
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1"), row("r2")]))
            .mount(&s)
            .await;
        let r = pull_data_source(&c, &store, "ds").await.unwrap();
        assert_eq!(
            r,
            PullReport {
                fetched: 2,
                updated: 2,
                skipped: false
            }
        );
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        let r = pull_data_source(&c, &store, "ds").await.unwrap();
        assert_eq!(
            r,
            PullReport {
                fetched: 2,
                updated: 0,
                skipped: false
            }
        );
    }

    #[tokio::test]
    async fn long_text_is_split_into_runs() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let long = "é".repeat(4500);
        local_edit_text(&store, "b", &long).unwrap();
        let runs = &store
            .get_block("b")
            .unwrap()
            .unwrap()
            .kind
            .text()
            .unwrap()
            .rich_text
            .clone();
        let lens: Vec<usize> = runs.iter().map(|r| r.plain_text.chars().count()).collect();
        assert_eq!(lens, [2000, 2000, 500]);
        assert_eq!(text(&store, "b"), long);
        let ops = store.queued_ops().unwrap();
        let OpKind::UpdateBlock { body, .. } = &ops[0].kind else {
            panic!()
        };
        assert_eq!(body["paragraph"]["rich_text"].as_array().unwrap().len(), 3);
        assert!(body.get("type").is_none());
    }

    fn queue(store: &Store) -> Vec<OpKind> {
        store
            .queued_ops()
            .unwrap()
            .into_iter()
            .map(|o| o.kind)
            .collect()
    }

    #[tokio::test]
    async fn local_create_respects_after() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let x = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["a", x.as_str(), "b"]);
        let OpKind::CreateBlock { after, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(after.as_deref(), Some("a"));
        // unknown `after`: nothing cached, nothing queued
        assert!(local_create_block(&store, "p", Some("zz"), BlockKind::Divider).is_err());
        assert_eq!(ids(&store.children("p").unwrap()), ["a", x.as_str(), "b"]);
        assert_eq!(queue(&store).len(), 1);
    }

    #[tokio::test]
    async fn local_edit_rich_keeps_annotations() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let rt = crate::markdown::from_markdown("x **bold**");
        local_edit_rich(&store, "b", rt.clone()).unwrap();
        let b = store.get_block("b").unwrap().unwrap();
        assert_eq!(b.kind.text().unwrap().rich_text, rt);
        let OpKind::UpdateBlock { body, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(
            body["paragraph"]["rich_text"][1]["annotations"]["bold"],
            true
        );
        // empty text still clears the runs
        local_edit_text(&store, "b", "").unwrap();
        assert!(store
            .get_block("b")
            .unwrap()
            .unwrap()
            .kind
            .text()
            .unwrap()
            .rich_text
            .is_empty());
    }

    #[tokio::test]
    async fn local_set_checked_patches_only_checked() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let mut todo = blk("t", pp("p"), "task", T, false);
        todo["type"] = json!("to_do");
        todo["to_do"] = json!({ "rich_text": [], "checked": false });
        store
            .put_block(&serde_json::from_value(todo.clone()).unwrap())
            .unwrap();
        assert!(local_set_checked(&store, "b", true).is_err());
        local_set_checked(&store, "t", true).unwrap();
        let t = store.get_block("t").unwrap().unwrap();
        assert!(matches!(t.kind, BlockKind::ToDo { checked: true, .. }));
        assert_eq!(
            queue(&store),
            [OpKind::UpdateBlock {
                block_id: "t".into(),
                body: json!({ "to_do": { "checked": true } })
            }]
        );
        get(&s, "/v1/blocks/t", ok(todo.clone())).await;
        todo["last_edited_time"] = json!(T2);
        todo["to_do"]["checked"] = json!(true);
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/t"))
            .and(wiremock::matchers::body_json(
                json!({ "to_do": { "checked": true } }),
            ))
            .respond_with(ok(todo))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&client(&s), &store).await.unwrap().applied, 1);
    }

    #[tokio::test]
    async fn local_delete_drops_subtree_ops_and_keeps_base() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let orig = serde_json::to_value(store.get_block("a").unwrap().unwrap()).unwrap();
        local_edit_text(&store, "a", "edited").unwrap();
        let kid = local_create_block(&store, "a", None, BlockKind::Divider).unwrap();
        local_edit_text(&store, "b", "keep").unwrap();

        let del = local_delete_block(&store, "a").unwrap().unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["b"]);
        assert_eq!(store.get_block(&kid).unwrap(), None);
        let ops = store.queued_ops().unwrap();
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0].kind, OpKind::UpdateBlock { block_id, .. } if block_id == "b"));
        assert_eq!(ops[1].id, del);
        assert_eq!(
            ops[1].kind,
            OpKind::DeleteBlock {
                block_id: "a".into()
            }
        );
        assert_eq!(ops[1].base_raw.as_ref(), Some(&orig));
    }

    #[tokio::test]
    async fn local_blocks_rewrite_or_drop_their_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let x = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        let y = local_create_block(&store, "p", Some(&x), BlockKind::Divider).unwrap();
        let z = local_create_block(&store, "p", Some(&y), BlockKind::Divider).unwrap();
        let creates = |store: &Store| -> Vec<String> {
            queue(store)
                .iter()
                .filter_map(|k| match k {
                    OpKind::CreateBlock { block, .. } => Some(block["id"].as_str().unwrap().into()),
                    _ => None,
                })
                .collect()
        };

        // nothing is placed after z: its create just goes
        assert_eq!(local_delete_block(&store, &z).unwrap(), None);
        assert_eq!(creates(&store), [x.clone(), y.clone()]);
        // y is placed after x: x's create stays and a delete of it is queued
        let d = local_delete_block(&store, &x).unwrap().unwrap();
        assert_eq!(creates(&store), [x.clone(), y.clone()]);
        let last = store.queued_ops().unwrap().pop().unwrap();
        assert_eq!(
            (last.id, last.kind, last.base_last_edited_time),
            (
                d,
                OpKind::DeleteBlock {
                    block_id: x.clone()
                },
                None
            )
        );
        // nothing depends on y: its create is rewritten in place
        assert_eq!(local_move_block(&store, &y, "b", None).unwrap(), None);
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "b"]);
        assert_eq!(ids(&store.children("b").unwrap()), [y.as_str()]);
        let OpKind::CreateBlock {
            parent_id,
            after,
            block,
        } = &queue(&store)[1]
        else {
            panic!()
        };
        assert_eq!(
            (parent_id.as_str(), after, block["id"].as_str()),
            ("b", &None, Some(y.as_str()))
        );
        assert_eq!(block["parent"]["block_id"], "b");
    }

    #[tokio::test]
    async fn local_move_falls_back_when_order_depends_on_it() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        // p=[a,b]: x after b, then y after a → [a,y,b,x]; moving x after a in
        // place would replay as x-after-a, y-after-a = [a,y,x,b]
        let x = local_create_block(&store, "p", Some("b"), BlockKind::Divider).unwrap();
        let y = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        let m = local_move_block(&store, &x, "p", Some("a"))
            .unwrap()
            .unwrap();
        assert_eq!(
            ids(&store.children("p").unwrap()),
            ["a", x.as_str(), y.as_str(), "b"]
        );
        let ops = store.queued_ops().unwrap();
        let OpKind::CreateBlock { after, .. } = &ops[0].kind else {
            panic!()
        };
        assert_eq!(after.as_deref(), Some("b"));
        assert_eq!(
            (ops[2].id, &ops[2].kind, ops[2].base_last_edited_time),
            (
                m,
                &OpKind::MoveBlock {
                    block_id: x.clone(),
                    new_parent_id: "p".into(),
                    after: Some("a".into())
                },
                None
            )
        );
        // y is placed after... nothing; but w is placed after y: y must move by op
        let _w = local_create_block(&store, "p", Some(&y), BlockKind::Divider).unwrap();
        assert!(local_move_block(&store, &y, "b", None).unwrap().is_some());
        // x already has a queued move: later moves queue too
        assert!(local_move_block(&store, &x, "b", None).unwrap().is_some());
        assert_eq!(ids(&store.children("b").unwrap()), [y.as_str(), x.as_str()]);
    }

    #[tokio::test]
    async fn kept_create_and_delete_replay_in_order() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        // x after a, z after a, y after x → [a,z,x,y]; delete x → [a,z,y]
        let x = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        let z = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        let y = local_create_block(&store, "p", Some(&x), BlockKind::Divider).unwrap();
        local_delete_block(&store, &x).unwrap().unwrap();
        assert_eq!(
            ids(&store.children("p").unwrap()),
            ["a", z.as_str(), y.as_str()]
        );

        let made = |id: &str| list(vec![blk(id, pp("p"), "", T2, false)]);
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_string_contains("\"after\":\"x1\""))
            .respond_with(made("y1"))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_string_contains("\"after\":\"a\""))
            .respond_with(made("x1"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_string_contains("\"after\":\"a\""))
            .respond_with(made("z1"))
            .expect(1)
            .mount(&s)
            .await;
        // the DeleteBlock queued on the local id resolves to the real one
        get(&s, "/v1/blocks/x1", ok(blk("x1", pp("p"), "", T2, false))).await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/x1"))
            .respond_with(ok(blk("x1", pp("p"), "", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&client(&s), &store).await.unwrap().applied, 4);
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "z1", "y1"]);
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleted_parent_takes_moved_in_blocks_with_it() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b", "c"]).await;
        let orig_b = serde_json::to_value(store.get_block("b").unwrap().unwrap()).unwrap();
        local_move_block(&store, "b", "a", None).unwrap().unwrap();
        let d = local_delete_block(&store, "a").unwrap().unwrap();
        let ops = store.queued_ops().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0].kind,
            OpKind::DeleteBlock {
                block_id: "b".into()
            }
        );
        assert_eq!(ops[0].base_raw.as_ref(), Some(&orig_b));
        assert_eq!(
            (ops[1].id, &ops[1].kind),
            (
                d,
                &OpKind::DeleteBlock {
                    block_id: "a".into()
                }
            )
        );

        // same under an unpushed local root: its create goes, b's delete stays
        let x = local_create_block(
            &store,
            "p",
            None,
            BlockKind::Paragraph(serde_json::from_value(json!({ "rich_text": [] })).unwrap()),
        )
        .unwrap();
        local_move_block(&store, "c", &x, None).unwrap().unwrap();
        assert_eq!(local_delete_block(&store, &x).unwrap(), None);
        assert_eq!(
            queue(&store)[2..],
            [OpKind::DeleteBlock {
                block_id: "c".into()
            }]
        );
        assert!(store.children("p").unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_delete_fixes_parent_has_children() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_move_block(&store, "b", "a", None).unwrap();
        assert!(store.get_block("a").unwrap().unwrap().has_children);
        local_delete_block(&store, "b").unwrap();
        assert!(!store.get_block("a").unwrap().unwrap().has_children);
    }

    #[tokio::test]
    async fn local_checked_on_unpushed_todo_rewrites_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let todo = BlockKind::ToDo {
            text: serde_json::from_value(json!({ "rich_text": [] })).unwrap(),
            checked: false,
        };
        let t = local_create_block(&store, "p", None, todo).unwrap();
        let create = store.queued_ops().unwrap()[0].id;
        assert_eq!(local_set_checked(&store, &t, true).unwrap(), create);
        let OpKind::CreateBlock { block, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(queue(&store).len(), 1);
        assert_eq!(block["to_do"]["checked"], true);
    }

    #[tokio::test]
    async fn local_create_not_rewritten_past_a_later_move() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["g", "r"]).await;
        let x = local_create_block(&store, "p", Some("r"), BlockKind::Divider).unwrap();
        assert_eq!(local_move_block(&store, &x, "r", None).unwrap(), None); // Tab
        assert!(local_move_block(&store, "r", "g", None).unwrap().is_some()); // Shift+Tab on r
                                                                              // Shift+Tab on x: x's create runs before r's move, so it can't say "after r under g"
        assert!(local_move_block(&store, &x, "g", Some("r"))
            .unwrap()
            .is_some());
        let OpKind::CreateBlock { parent_id, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(parent_id, "r");
        assert_eq!(ids(&store.children("g").unwrap()), ["r", x.as_str()]);
    }

    #[tokio::test]
    async fn local_move_of_real_block_queues_move() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b", "c"]).await;
        let id = local_move_block(&store, "c", "a", None).unwrap().unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "b"]);
        assert_eq!(ids(&store.children("a").unwrap()), ["c"]);
        assert!(store.get_block("a").unwrap().unwrap().has_children);
        let op = &store.queued_ops().unwrap()[0];
        assert_eq!(op.id, id);
        assert_eq!(
            op.kind,
            OpKind::MoveBlock {
                block_id: "c".into(),
                new_parent_id: "a".into(),
                after: None
            }
        );
        assert!(op.base_last_edited_time.is_some());
        assert!(local_move_block(&store, "a", "c", None).is_err());
    }

    #[tokio::test]
    async fn locally_moved_block_survives_pull() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        let remote = |time: &'static str| {
            let s = &s;
            async move {
                get(s, "/v1/pages/p", ok(page("p", pp("root"), time))).await;
                get(
                    s,
                    "/v1/blocks/p/children",
                    list(vec![
                        blk("a", pp("p"), "A", T, true),
                        blk("b", pp("p"), "B", T, true),
                    ]),
                )
                .await;
                get(
                    s,
                    "/v1/blocks/a/children",
                    list(vec![
                        blk("a1", bp("a"), "A1", T, true),
                        blk("a2", bp("a"), "A2", T, false),
                    ]),
                )
                .await;
                get(
                    s,
                    "/v1/blocks/a1/children",
                    list(vec![blk("a11", bp("a1"), "A11", T, false)]),
                )
                .await;
                get(
                    s,
                    "/v1/blocks/b/children",
                    list(vec![blk("b1", bp("b"), "B1", T, false)]),
                )
                .await;
            }
        };
        remote(T).await;
        pull_page(&c, &store, "p", false).await.unwrap();
        s.reset().await;

        // b's children are written before a's, so a stale a1 would win last
        local_move_block(&store, "a1", "b", Some("b1")).unwrap();
        local_edit_text(&store, "a1", "mine").unwrap();
        local_move_block(&store, "b", "a", Some("a2")).unwrap();
        remote(T2).await;
        pull_page(&c, &store, "p", false).await.unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["a"]);
        assert_eq!(ids(&store.children("a").unwrap()), ["a2", "b"]);
        assert_eq!(ids(&store.children("b").unwrap()), ["b1", "a1"]);
        assert_eq!(ids(&store.children("a1").unwrap()), ["a11"]);
        assert_eq!(text(&store, "a1"), "mine");
    }

    #[tokio::test]
    async fn pushed_move_lands_after_its_anchor() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![
                blk("m", pp("p"), "M", T, false),
                blk("t", pp("p"), "T", T, true),
            ]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/t/children",
            list(vec![
                blk("t1", bp("t"), "T1", T, false),
                blk("t2", bp("t"), "T2", T, false),
            ]),
        )
        .await;
        pull_page(&c, &store, "p", false).await.unwrap();
        s.reset().await;

        local_move_block(&store, "m", "t", Some("t1")).unwrap();
        get(&s, "/v1/blocks/m", ok(blk("m", pp("p"), "M", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/t/children"))
            .and(body_string_contains("\"after\":\"t1\""))
            .respond_with(list(vec![blk("nm", bp("t"), "M", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/m"))
            .respond_with(ok(blk("m", pp("p"), "M", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&c, &store).await.unwrap().applied, 1);
        assert_eq!(ids(&store.children("t").unwrap()), ["t1", "nm", "t2"]);
        assert_eq!(ids(&store.children("p").unwrap()), ["t"]);
    }

    fn bold(t: &str) -> RichText {
        let mut r = RichText::plain(t);
        r.annotations.bold = true;
        r
    }
    fn h2(id: &str, text: &str, time: &str) -> Value {
        let mut v = blk(id, pp("p"), text, time, false);
        let o = v.as_object_mut().unwrap();
        let payload = o.remove("paragraph").unwrap();
        o.insert("type".into(), json!("heading_2"));
        o.insert("heading_2".into(), payload);
        v
    }
    fn rich(store: &Store, id: &str) -> Vec<RichText> {
        store
            .get_block(id)
            .unwrap()
            .unwrap()
            .kind
            .text()
            .unwrap()
            .rich_text
            .clone()
    }

    #[tokio::test]
    async fn change_type_rewrites_local_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let local = local_create_block(
            &store,
            "p",
            None,
            TargetKind::Paragraph.block_kind(TextBlock {
                rich_text: vec![bold("x")],
                color: "default".into(),
            }),
        )
        .unwrap();
        let out = local_change_type(&store, &local, TargetKind::Heading1).unwrap();
        assert_eq!(out, ChangeOutcome::Kept);
        assert!(matches!(
            store.get_block(&local).unwrap().unwrap().kind,
            BlockKind::Heading1(_)
        ));
        assert_eq!(rich(&store, &local), [bold("x")]);
        let q = queue(&store);
        assert_eq!(q.len(), 1);
        let OpKind::CreateBlock { block, .. } = &q[0] else {
            panic!("{q:?}")
        };
        assert_eq!(block["id"], json!(local));
        assert_eq!(block["type"], "heading_1");
        assert!(block.get("paragraph").is_none());
        assert_eq!(
            block["heading_1"]["rich_text"][0]["annotations"]["bold"],
            true
        );
        // same type: no-op
        assert_eq!(
            local_change_type(&store, &local, TargetKind::Heading1).unwrap(),
            ChangeOutcome::Kept
        );
        assert_eq!(queue(&store), q);
    }

    #[tokio::test]
    async fn change_type_replaces_existing_block() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b", "c"]).await;
        let mut b = store.get_block("b").unwrap().unwrap();
        b.kind.set_text(vec![bold("B")]).unwrap();
        store.put_block(&b).unwrap();

        let ChangeOutcome::Replaced(new) =
            local_change_type(&store, "b", TargetKind::Heading2).unwrap()
        else {
            panic!("kept")
        };
        assert!(new.starts_with("local-"));
        assert_eq!(ids(&store.children("p").unwrap()), ["a", new.as_str(), "c"]);
        assert!(store.get_block("b").unwrap().is_none());
        assert!(matches!(
            store.get_block(&new).unwrap().unwrap().kind,
            BlockKind::Heading2(_)
        ));
        assert_eq!(rich(&store, &new), [bold("B")]);
        let q = queue(&store);
        assert!(
            matches!(&q[..], [OpKind::CreateBlock { parent_id, after: Some(a), .. }, OpKind::DeleteBlock { block_id }]
                if parent_id == "p" && a == "b" && block_id == "b"),
            "{q:?}"
        );
        // edit before push folds into the create
        local_edit_text(&store, &new, "edited").unwrap();
        assert_eq!(store.queued_ops().unwrap().len(), 2);

        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_string_contains(r#""after":"b""#))
            .and(body_string_contains("heading_2"))
            .and(body_string_contains("edited"))
            .respond_with(list(vec![h2("real1", "edited", T2)]))
            .expect(1)
            .mount(&s)
            .await;
        get(&s, "/v1/blocks/b", ok(serde_json::to_value(&b).unwrap())).await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 2,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "real1", "c"]);
        assert_eq!(store.resolve_id(&new).unwrap(), "real1");
        s.reset().await;

        // a later edit goes to the new real id
        local_edit_text(&store, "real1", "again").unwrap();
        get(&s, "/v1/blocks/real1", ok(h2("real1", "edited", T2))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/real1"))
            .and(body_string_contains("again"))
            .respond_with(ok(h2("real1", "again", T2)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 1,
                conflicts: 0,
                failed: 0
            }
        );
        assert_eq!(text(&store, "real1"), "again");
    }

    #[tokio::test]
    async fn change_type_refuses_children_and_unconvertible() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let kid = BlockKind::Paragraph(TextBlock {
            rich_text: vec![],
            color: "default".into(),
        });
        local_create_block(&store, "a", None, kid).unwrap();
        let q = queue(&store);
        let e = local_change_type(&store, "a", TargetKind::Quote).unwrap_err();
        assert!(e.to_string().contains("children"), "{e}");
        assert_eq!(queue(&store), q);

        let mut v = blk("cp", pp("p"), "", T, false);
        let o = v.as_object_mut().unwrap();
        o.remove("paragraph");
        o.insert("type".into(), json!("child_page"));
        o.insert("child_page".into(), json!({ "title": "Sub" }));
        store
            .put_block(&serde_json::from_value(v).unwrap())
            .unwrap();
        assert!(local_change_type(&store, "cp", TargetKind::Paragraph).is_err());
        assert_eq!(queue(&store), q);
    }

    #[tokio::test]
    async fn change_type_refuses_unpushed_move() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_move_block(&store, "b", "a", None).unwrap();
        let q = queue(&store);
        let e = local_change_type(&store, "b", TargetKind::Heading2).unwrap_err();
        assert!(e.to_string().contains("moved"), "{e}");
        assert_eq!(queue(&store), q);
        assert!(store.get_block("b").unwrap().is_some());
    }

    #[tokio::test]
    async fn failed_conversion_create_keeps_original() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        local_change_type(&store, "b", TargetKind::Quote).unwrap();
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "object": "error", "status": 400, "code": "validation_error", "message": "bad"
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T2, false)))
            .expect(0)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            r,
            PushReport {
                applied: 0,
                conflicts: 0,
                failed: 2
            }
        );
        let failed = store.ops_by_status(OpStatus::Failed).unwrap();
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[1].error.as_deref(), Some("anchor's create failed"));
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn change_type_refuses_local_without_queued_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let local = local_create_block(
            &store,
            "p",
            None,
            TargetKind::Paragraph.block_kind(TextBlock {
                rich_text: vec![],
                color: "default".into(),
            }),
        )
        .unwrap();
        let op = store.queued_ops().unwrap()[0].id;
        store.mark_op(op, OpStatus::Failed, Some("x")).unwrap();
        let e = local_change_type(&store, &local, TargetKind::Quote).unwrap_err();
        assert!(e.to_string().contains("not queued"), "{e}");
    }

    #[tokio::test]
    async fn change_type_rewrites_moved_local_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let local = local_create_block(
            &store,
            "p",
            None,
            TargetKind::Paragraph.block_kind(TextBlock {
                rich_text: vec![],
                color: "default".into(),
            }),
        )
        .unwrap();
        // a later op placed after the local block forces a queued MoveBlock
        local_create_block(
            &store,
            "p",
            Some(&local),
            TargetKind::Paragraph.block_kind(TextBlock {
                rich_text: vec![],
                color: "default".into(),
            }),
        )
        .unwrap();
        local_move_block(&store, &local, "a", None).unwrap();
        assert!(queue(&store)
            .iter()
            .any(|k| matches!(k, OpKind::MoveBlock { .. })));
        assert_eq!(
            local_change_type(&store, &local, TargetKind::Heading1).unwrap(),
            ChangeOutcome::Kept
        );
    }

    #[tokio::test]
    async fn change_type_to_divider_drops_text() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let ChangeOutcome::Replaced(new) =
            local_change_type(&store, "a", TargetKind::Divider).unwrap()
        else {
            panic!("kept")
        };
        assert_eq!(
            store.get_block(&new).unwrap().unwrap().kind,
            BlockKind::Divider
        );
        let OpKind::CreateBlock { block, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(block["type"], "divider");
        assert!(!block.to_string().contains("rich_text"), "{block}");
    }

    #[tokio::test]
    async fn change_type_to_code_strips_annotations() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let mut link = RichText::plain("y");
        link.href = Some("https://x".into());
        link.kind = crate::model::RichTextKind::Text {
            content: "y".into(),
            link: Some("https://x".into()),
        };
        let kind = BlockKind::Quote(TextBlock {
            rich_text: vec![bold("x"), link],
            color: "default".into(),
        });
        let local = local_create_block(&store, "p", None, kind).unwrap();
        local_change_type(&store, &local, TargetKind::Code).unwrap();
        match store.get_block(&local).unwrap().unwrap().kind {
            BlockKind::Code { text, language } => {
                assert_eq!(text.rich_text, [RichText::plain("xy")]);
                assert_eq!(language, "plain text");
            }
            k => panic!("{k:?}"),
        }
        // long text stays within Notion's per-run limit
        let long = "z".repeat(2500);
        let p2 = local_create_block(
            &store,
            "p",
            None,
            TargetKind::Paragraph.block_kind(TextBlock {
                rich_text: vec![bold(&long)],
                color: "default".into(),
            }),
        )
        .unwrap();
        local_change_type(&store, &p2, TargetKind::Code).unwrap();
        let runs = rich(&store, &p2);
        assert!(runs.len() == 2 && runs.iter().all(|r| r.plain_text.chars().count() <= 2000));
        assert_eq!(plain_text(&runs), long);
        // and back: the plain text is kept
        local_change_type(&store, &local, TargetKind::ToDo).unwrap();
        match store.get_block(&local).unwrap().unwrap().kind {
            BlockKind::ToDo { text, checked } => {
                assert_eq!(text.rich_text, [RichText::plain("xy")]);
                assert!(!checked);
            }
            k => panic!("{k:?}"),
        }
        assert_eq!(queue(&store).len(), 2);
    }

    #[tokio::test]
    async fn equation_blocks_convert_and_edit() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let expr = |store: &Store, id: &str| match store.get_block(id).unwrap().unwrap().kind {
            BlockKind::Equation { expression } => expression,
            k => panic!("{k:?}"),
        };
        // real paragraph → equation: new block, text becomes the expression
        let ChangeOutcome::Replaced(new) =
            local_change_type(&store, "a", TargetKind::Equation).unwrap()
        else {
            panic!("kept")
        };
        assert_eq!(expr(&store, &new), "a");
        // editing the unpushed create rewrites it
        local_edit_equation(&store, &new, "x^2").unwrap();
        let OpKind::CreateBlock { block, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(block["equation"]["expression"], "x^2");
        assert!(local_edit_rich(&store, &new, vec![]).is_err());
        assert!(local_edit_equation(&store, "p", "x").is_err());
        // and back: the expression becomes plain text
        assert_eq!(
            local_change_type(&store, &new, TargetKind::Paragraph).unwrap(),
            ChangeOutcome::Kept
        );
        assert_eq!(rich(&store, &new), [RichText::plain("x^2")]);
    }

    #[tokio::test]
    async fn equation_edit_on_a_real_block_queues_an_update() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &[]).await;
        let mut v = blk("e", pp("p"), "", T, false);
        let o = v.as_object_mut().unwrap();
        o.remove("paragraph");
        o.insert("type".into(), json!("equation"));
        o.insert("equation".into(), json!({ "expression": "a" }));
        store
            .replace_children("p", "p", &[serde_json::from_value(v).unwrap()])
            .unwrap();
        local_edit_equation(&store, "e", "b").unwrap();
        let OpKind::UpdateBlock { block_id, body } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(block_id, "e");
        assert_eq!(body, &json!({ "equation": { "expression": "b" } }));
    }

    fn table_row(id: &str, cells: &[&str], time: &str) -> Value {
        let cells: Vec<Value> = cells
            .iter()
            .map(|c| {
                json!([{ "type": "text", "text": { "content": c, "link": null },
                "plain_text": c, "annotations": {}, "href": null }])
            })
            .collect();
        json!({
            "object": "block", "id": id, "parent": bp("tbl"), "type": "table_row",
            "table_row": { "cells": cells }, "has_children": false, "in_trash": false,
            "created_time": T, "last_edited_time": time,
        })
    }

    /// Cache page "p" holding table "tbl" with row "r1" = [a, b].
    async fn seed_table(s: &MockServer, store: &Store) {
        get(s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        let tbl = json!({
            "object": "block", "id": "tbl", "parent": pp("p"), "type": "table",
            "table": { "table_width": 2, "has_column_header": true, "has_row_header": false },
            "has_children": true, "in_trash": false, "created_time": T, "last_edited_time": T,
        });
        get(s, "/v1/blocks/p/children", list(vec![tbl])).await;
        get(
            s,
            "/v1/blocks/tbl/children",
            list(vec![table_row("r1", &["a", "b"], T)]),
        )
        .await;
        pull_page(&client(s), store, "p", true).await.unwrap();
        s.reset().await;
    }

    fn cells(store: &Store, id: &str) -> Value {
        serde_json::to_value(store.get_block(id).unwrap().unwrap()).unwrap()["table_row"]["cells"]
            .clone()
    }

    #[tokio::test]
    async fn table_cell_edit_updates_cache_and_queues_all_cells() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed_table(&s, &store).await;
        local_edit_table_cell(&store, "r1", 1, vec![RichText::plain("B")]).unwrap();
        assert_eq!(cells(&store, "r1")[0][0]["plain_text"], "a");
        assert_eq!(cells(&store, "r1")[1][0]["plain_text"], "B");
        let OpKind::UpdateBlock { block_id, body } = &queue(&store)[0] else {
            panic!()
        };
        assert_eq!(block_id, "r1");
        assert_eq!(body["table_row"]["cells"], cells(&store, "r1"));

        // no coalescing (same as text edits): a second op carrying both edits
        local_edit_table_cell(&store, "r1", 0, vec![RichText::plain("A")]).unwrap();
        let q = queue(&store);
        assert_eq!(q.len(), 2);
        let OpKind::UpdateBlock { body, .. } = &q[1] else {
            panic!()
        };
        assert_eq!(body["table_row"]["cells"][0][0]["plain_text"], "A");
        assert_eq!(body["table_row"]["cells"][1][0]["plain_text"], "B");

        assert!(local_edit_table_cell(&store, "r1", 2, vec![]).is_err());
        assert!(local_edit_table_cell(&store, "tbl", 0, vec![]).is_err());
        assert_eq!(queue(&store).len(), 2);
    }

    #[tokio::test]
    async fn push_table_cell_sends_every_cell() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed_table(&s, &store).await;
        local_edit_table_cell(&store, "r1", 0, vec![RichText::plain("new")]).unwrap();
        // a single-object GET carries fields the listed copy doesn't
        let mut remote = table_row("r1", &["a", "b"], T);
        remote["request_id"] = json!("req-1");
        get(&s, "/v1/blocks/r1", ok(remote)).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/r1"))
            .and(body_partial_json(json!({ "table_row": { "cells": [
                [{ "type": "text", "text": { "content": "new" } }],
                [{ "type": "text", "text": { "content": "b" } }],
            ] } })))
            .respond_with(ok(table_row("r1", &["new", "b"], T2)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(r.applied, 1, "{r:?}");
        assert!(store.queued_ops().unwrap().is_empty());
        assert_eq!(cells(&store, "r1")[0][0]["plain_text"], "new");

        // someone else changed a cell within the same minute: conflict
        s.reset().await;
        local_edit_table_cell(&store, "r1", 1, vec![RichText::plain("mine")]).unwrap();
        get(
            &s,
            "/v1/blocks/r1",
            ok(table_row("r1", &["new", "theirs"], T2)),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(r.conflicts, 1, "{r:?}");
    }
}
