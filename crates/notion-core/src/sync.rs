//! Pull remote pages / data sources into the cache; push the local op queue
//! back with conflict detection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::api::{BlockNode, Client};
use crate::model::{Block, BlockKind, Page, Parent, RichText};
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
    // matter: blocks with queued edits/moves keep their cached version, queued
    // deletes stay gone, unpushed `local-*` creates survive.
    let mut kept = HashMap::new();
    let mut deleted = HashSet::new();
    for op in store.queued_ops()? {
        match &op.kind {
            OpKind::UpdateBlock { block_id, .. } | OpKind::MoveBlock { block_id, .. } => {
                let id = store.resolve_id(block_id)?;
                if let Some(b) = store.get_block(&id)? {
                    kept.insert(id, b);
                }
            }
            OpKind::DeleteBlock { block_id } => {
                deleted.insert(store.resolve_id(block_id)?);
            }
            _ => {}
        }
    }
    let mut locals: HashMap<String, Vec<Block>> = HashMap::new();
    {
        let mut st = store.conn.prepare(
            "SELECT parent_id, raw FROM blocks WHERE id LIKE 'local-%' ORDER BY parent_id, position",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (parent, raw) = row?;
            locals
                .entry(parent)
                .or_default()
                .push(serde_json::from_str(&raw)?);
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
            if deleted.contains(&n.block.id) {
                continue; // its subtree is skipped too: no rows under a missing parent
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
        list.extend(locals.remove(&parent_id).unwrap_or_default());
        store.replace_children(&page.id, &parent_id, &list)?;
    }
    store.upsert_page(&page)?;
    Ok(report)
}

pub async fn pull_data_source(client: &Client, store: &Store, ds_id: &str) -> Result<PullReport> {
    let ds = client.get_data_source(ds_id).await?;
    store.upsert_data_source(&ds)?;
    let mut pending = HashSet::new();
    for op in store.queued_ops()? {
        if let OpKind::UpdatePageProperties { page_id, .. } = &op.kind {
            pending.insert(store.resolve_id(page_id)?);
        }
    }
    let rows = client.query_data_source(&ds.id, json!({})).await?;
    let mut report = PullReport {
        fetched: rows.len(),
        ..Default::default()
    };
    for row in rows {
        let cached = store.get_page(&row.id)?;
        if cached.map(|c| c.last_edited_time) != Some(row.last_edited_time)
            && !pending.contains(&row.id)
        {
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
    for op in store.queued_ops()? {
        match apply(client, store, &op, &mut own).await {
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
        Ok(serde_json::from_value::<Block>(raw.clone())?.kind == remote.kind)
    })?;
    Ok((remote, c))
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
            if let Some(c) = conflict(op, &id, remote.last_edited_time, own, |raw| {
                Ok(serde_json::from_value::<Page>(raw.clone())?.properties == remote.properties)
            })? {
                return Ok(Outcome::Conflict(c));
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
            store.rename_block(&local.id, &new.id)?;
            store.put_block(&new)?;
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
        }
    }
    Ok(Outcome::Applied)
}

// ---------------------------------------------------------------- local edits

/// Rewrites a cached block's text and queues the update. Returns the op id.
pub fn local_edit_text(store: &Store, block_id: &str, text: &str) -> Result<i64> {
    let mut block = store
        .get_block(block_id)?
        .ok_or_else(|| Error::Invalid(format!("block {block_id} not cached")))?;
    if block.kind.text().is_none() {
        return Err(Error::Invalid(format!(
            "block type `{}` has no text",
            block.kind.type_name()
        )));
    }
    let base_raw = serde_json::to_value(&block)?;
    let base_time = block.last_edited_time;
    // Notion caps a rich-text run at 2000 chars.
    let chars: Vec<char> = text.chars().collect();
    let runs = chars
        .chunks(2000)
        .map(|c| RichText::plain(&c.iter().collect::<String>()))
        .collect();
    block.kind.set_text(runs)?;
    store.put_block(&block)?;
    let mut body = block.kind.to_request_json()?;
    if let Some(o) = body.as_object_mut() {
        o.remove("type");
    }
    store.enqueue(
        OpKind::UpdateBlock {
            block_id: block_id.to_string(),
            body,
        },
        Some(base_time),
        Some(base_raw),
    )
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
    store.put_block(&block)?;
    store.enqueue(
        OpKind::CreateBlock {
            parent_id: parent_id.to_string(),
            after: after.map(str::to_string),
            block: serde_json::to_value(&block)?,
        },
        None,
        None,
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ClientConfig;
    use crate::model::plain_text;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method, path};
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
        get(&s, "/v1/pages/q", ok(page("q", pp("root"), T2))).await; // edited remotely
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
        local_edit_text(&store, &local, "edited").unwrap();

        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![blk("real1", pp("p"), "x", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        get(
            &s,
            "/v1/blocks/real1",
            ok(blk("real1", pp("p"), "x", T2, false)),
        )
        .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/real1"))
            .and(body_string_contains("edited"))
            .respond_with(ok(blk("real1", pp("p"), "edited", T2, false)))
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
}
