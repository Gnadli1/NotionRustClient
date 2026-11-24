//! Pull remote pages / data sources into the cache; push the local op queue
//! back with conflict detection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use futures::future::join_all;
use futures::{stream, StreamExt};
use serde_json::{json, Value};

use crate::api::{BlockNode, Client};
use crate::markdown::{
    from_markdown, is_markdown_editable, split_markdown, split_runs, to_markdown,
};
use crate::model::{
    plain_text, users_in, Block, BlockKind, Comment, Page, Parent, PropertyValue, RichText,
    SearchResult, TargetKind, TextBlock, User,
};
use crate::ops::{CommentTarget, Op, OpKind, OpStatus};
use crate::store::{ts, Store};
use crate::{props, Error, Result};
use rusqlite::OptionalExtension;

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

/// One entry per id, in first-seen order, keeping the fullest one (a name,
/// else a type, else just the id): a page of blocks names the same few users
/// over and over.
fn dedupe_users(found: Vec<User>) -> Vec<User> {
    let score = |u: &User| u32::from(!u.name.is_empty()) * 2 + u32::from(!u.kind.is_empty());
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<User> = Vec::new();
    for u in found {
        match at.get(&u.id) {
            Some(&i) if score(&u) > score(&out[i]) => out[i] = u,
            Some(_) => {}
            None => {
                at.insert(u.id.clone(), out.len());
                out.push(u);
            }
        }
    }
    out
}

/// When a pull may skip the block fetch because the page's `last_edited_time`
/// is unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Never,
    IfUnchanged,
    /// Notion's time has minute precision: an edit in the same minute as the
    /// last pull looks unchanged. Skip only if the time is also older than this.
    IfUnchangedAndOlder(chrono::Duration),
}

/// How old an unchanged `last_edited_time` must be for a poll to trust it.
pub fn poll_settle() -> Skip {
    Skip::IfUnchangedAndOlder(chrono::Duration::minutes(2))
}

/// The cache holds `page` at this edit time, with blocks. (A row cached by
/// `pull_data_source` has none yet: its blocks still need fetching.)
fn is_current(store: &Store, page: &Page) -> Result<bool> {
    Ok(match store.get_page(&page.id)? {
        Some(c) => {
            c.last_edited_time == page.last_edited_time && !store.children(&page.id)?.is_empty()
        }
        None => false,
    })
}

pub async fn pull_page(
    client: &Client,
    store: &Store,
    page_id: &str,
    force: bool,
) -> Result<PullReport> {
    let skip = if force {
        Skip::Never
    } else {
        Skip::IfUnchanged
    };
    pull_page_skip(client, store, page_id, skip).await
}

pub async fn pull_page_skip(
    client: &Client,
    store: &Store,
    page_id: &str,
    skip: Skip,
) -> Result<PullReport> {
    let page = client.get_page(page_id).await?;
    pull_loaded(client, store, page, skip).await
}

/// [`pull_page_skip`] for a page already fetched, e.g. by a search.
pub async fn pull_loaded(
    client: &Client,
    store: &Store,
    page: Page,
    skip: Skip,
) -> Result<PullReport> {
    match prefetch_page(client, store, &page, skip).await? {
        Some(tree) => apply_page(client, store, page, tree, true).await,
        None => Ok(PullReport {
            skipped: true,
            ..Default::default()
        }),
    }
}

/// The network half of a page pull: `None` when the cached page is current.
/// Split out so `pull_all` can fetch a batch of pages concurrently and write
/// them one by one (SQLite has one writer at a time anyway).
async fn prefetch_page(
    client: &Client,
    store: &Store,
    page: &Page,
    skip: Skip,
) -> Result<Option<Vec<BlockNode>>> {
    let settled = match skip {
        Skip::Never => false,
        Skip::IfUnchanged => true,
        Skip::IfUnchangedAndOlder(age) => Utc::now() - page.last_edited_time >= age,
    };
    if settled && is_current(store, page)? {
        return Ok(None);
    }
    Ok(Some(client.block_tree(&page.id).await?))
}

/// Writes a fetched page tree to the cache. `comments` also fetches the page's
/// comment threads; a full sync leaves that out (comments load on demand).
async fn apply_page(
    client: &Client,
    store: &Store,
    mut page: Page,
    tree: Vec<BlockNode>,
    comments: bool,
) -> Result<PullReport> {
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
    let mut found = Vec::new();
    users_in(&page.raw, &mut found);
    let mut all: Vec<&BlockNode> = tree.iter().collect();
    while let Some(n) = all.pop() {
        users_in(&n.block.raw, &mut found);
        all.extend(&n.children);
    }
    store.put_users(&dedupe_users(found))?;
    keep_queued_props(store, &mut page)?;
    store.upsert_page(&page)?;
    // ponytail: comment fetch strategy. Notion has no "all comments under this
    // page" endpoint and a block object carries no "has comments" flag, so a
    // single-page pull fetches only the page's own threads (one request, +1 on
    // a pull of any size, however many blocks). Block threads are fetched
    // lazily with `pull_comments(block_id)` when the comments panel opens for
    // that block; a row's 💬 badge counts what is cached. A full sync skips
    // this request for every page: it doubles the request count and the badge
    // catches up when the page is opened.
    // Comment trouble (a 403 when the integration lacks the read-comments
    // capability, say) never fails the page pull.
    if comments {
        if let Err(e) = pull_comments(client, store, &page.id).await {
            tracing::warn!("comments of {} not pulled: {e}", page.id);
        }
    }
    if let Err(e) = store.note_page(&page) {
        tracing::warn!("updates of {} not recorded: {e}", page.id);
    }
    Ok(report)
}

/// Caches the unresolved comments of a page or block (the endpoint lists
/// only those; a thread resolved remotely drops out of the cache). Cached
/// comments with queued ops survive: unpushed `local-*` creates stay, a
/// comment with a queued edit keeps its cached text, one with a queued
/// delete stays gone.
pub async fn pull_comments(client: &Client, store: &Store, parent_id: &str) -> Result<usize> {
    let remote = client.list_comments(parent_id).await?;
    let mut edited = HashSet::new();
    let mut deleted = HashSet::new();
    for op in store.queued_ops()? {
        match &op.kind {
            OpKind::UpdateComment { comment_id, .. } => {
                edited.insert(store.resolve_id(comment_id)?);
            }
            OpKind::DeleteComment { comment_id } => {
                deleted.insert(store.resolve_id(comment_id)?);
            }
            _ => {}
        }
    }
    let mut list = Vec::new();
    for c in remote {
        if deleted.contains(&c.id) {
            continue;
        }
        match store.get_comment(&c.id)?.filter(|_| edited.contains(&c.id)) {
            Some(cached) => list.push(cached),
            None => list.push(c),
        }
    }
    store.replace_comments(parent_id, &list)?;
    if let Err(e) = store.note_comments(parent_id, &list) {
        tracing::warn!("updates of comments on {parent_id} not recorded: {e}");
    }
    let mut found = Vec::new();
    list.iter().for_each(|c| users_in(&c.raw, &mut found));
    store.put_users(&dedupe_users(found))?;
    Ok(list.len())
}

/// Caches the workspace's users (`GET /v1/users`). A 403 (the integration
/// lacks "read user information") comes back as `Error::Api`; nothing retries it.
pub async fn pull_users(client: &Client, store: &Store) -> Result<usize> {
    let users = client.list_users().await?;
    store.put_users(&users)?;
    Ok(users.len())
}

pub async fn pull_data_source(client: &Client, store: &Store, ds_id: &str) -> Result<PullReport> {
    let ds = client.get_data_source(ds_id).await?;
    store.upsert_data_source(&ds)?;
    let rows = client.query_data_source(&ds.id, json!({})).await?;
    let ids: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
    let report = merge_rows(store, rows)?;
    // a row trashed or moved away in Notion is absent from the full query: mark it trashed
    let mut queued = HashSet::new();
    for op in store.queued_ops()? {
        queued.insert(store.resolve_id(op.kind.target_id())?);
    }
    for mut row in store.rows(&ds.id)? {
        if !row.id.starts_with("local-")
            && !row.in_trash
            && !ids.contains(&row.id)
            && !queued.contains(&row.id)
        {
            row.in_trash = true;
            store.upsert_page(&row)?;
        }
    }
    Ok(report)
}

/// Caches the rows that are new or changed (edit time or properties).
fn merge_rows(store: &Store, rows: Vec<Page>) -> Result<PullReport> {
    let mut report = PullReport {
        fetched: rows.len(),
        ..Default::default()
    };
    let mut found = Vec::new();
    rows.iter().for_each(|r| users_in(&r.raw, &mut found));
    store.put_users(&dedupe_users(found))?;
    for mut row in rows {
        let cached = store.get_page(&row.id)?;
        keep_queued_props(store, &mut row)?;
        // cached blocks are stale now: keep the old time, so the next page
        // pull refetches them instead of skipping the row as unchanged
        // (the time has minute precision: a same-minute edit shows in the properties)
        if let Some(c) = cached
            .as_ref()
            .filter(|_| !store.children(&row.id).is_ok_and(|b| b.is_empty()))
        {
            row.last_edited_time = c.last_edited_time;
        }
        // raw lags the struct fields until serialized: compare the typed properties
        // (a local 9 and a remote 9.0 are one number) and the rest of what would be stored
        let rest = |p: &Page| -> Result<Value> {
            let mut v = serde_json::to_value(p)?;
            v["properties"] = Value::Null;
            Ok(v)
        };
        let same = match &cached {
            Some(c) => c.properties == row.properties && rest(c)? == rest(&row)?,
            None => false,
        };
        if !same {
            store.upsert_page(&row)?;
            report.updated += 1;
        }
    }
    Ok(report)
}

/// The open table's cheap refresh: one request for the rows edited since the
/// newest cached one (Notion's time is rounded to the minute, so `on_or_after`
/// also re-reads that minute). Nothing cached yet: a full pull. `skipped` if
/// nothing changed. A row trashed or moved away in Notion stays cached until
/// the next full pull.
pub async fn poll_data_source(client: &Client, store: &Store, ds_id: &str) -> Result<PullReport> {
    let newest = store
        .rows(ds_id)?
        .iter()
        .filter(|r| !r.id.starts_with("local-"))
        .map(|r| r.last_edited_time)
        .max();
    let Some(newest) = newest else {
        return pull_data_source(client, store, ds_id).await;
    };
    let q = json!({ "filter": { "timestamp": "last_edited_time",
        "last_edited_time": { "on_or_after": newest.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) } } });
    let mut report = merge_rows(store, client.query_data_source(ds_id, q).await?)?;
    report.skipped = report.updated == 0;
    Ok(report)
}

/// A database id: caches its data sources (the first with its rows, so it can open).
pub async fn pull_database(client: &Client, store: &Store, db_id: &str) -> Result<PullReport> {
    let db = client.get_database(db_id).await?;
    let ids: Vec<String> = db["data_sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["id"].as_str().map(str::to_string))
        .collect();
    let Some((first, rest)) = ids.split_first() else {
        return Err(Error::Invalid("this database has no data source".into()));
    };
    let report = pull_data_source(client, store, first).await?;
    for id in rest {
        store.upsert_data_source(&client.get_data_source(id).await?)?;
    }
    Ok(report)
}

/// [`pull_page`] for any id the app opens: a cached data source pulls its
/// rows; a `child_database` block's id, or an id Notion says is no page,
/// is a database.
pub async fn pull_any(client: &Client, store: &Store, id: &str, force: bool) -> Result<PullReport> {
    if store.data_source(id)?.is_some() {
        return pull_data_source(client, store, id).await;
    }
    if matches!(
        store.get_block(id)?.map(|b| b.kind),
        Some(BlockKind::ChildDatabase { .. })
    ) {
        return pull_database(client, store, id).await;
    }
    match pull_page(client, store, id, force).await {
        Err(
            e @ Error::Api {
                status: 400 | 404, ..
            },
        ) => match pull_database(client, store, id).await {
            Err(Error::Api {
                status: 400 | 404, ..
            }) => Err(e),
            r => r,
        },
        r => r,
    }
}

/// [`pull_page_skip`], or [`poll_data_source`] for a data source.
pub async fn poll_any(client: &Client, store: &Store, id: &str, skip: Skip) -> Result<PullReport> {
    if store.data_source(id)?.is_some() {
        return poll_data_source(client, store, id).await;
    }
    pull_page_skip(client, store, id, skip).await
}

#[derive(Debug, Default, PartialEq)]
pub struct SyncAllReport {
    /// Pages fetched (new or changed).
    pub pages: usize,
    pub data_sources: usize,
    /// Pages already up to date (edit time unchanged and at least 2 minutes old).
    pub skipped: usize,
    /// One "title: message" per page or data source that failed.
    pub errors: Vec<String>,
}

/// Pulls every page and data source shared with the integration (`search("")`).
/// `progress(done, total, title)` runs after each item. A failing item is
/// recorded and skipped; a bad token or no connection aborts with the error.
///
/// Page trees are fetched in parallel batches (the limiter still owns the
/// request rate) and written one at a time, which is the only ordering SQLite
/// allows anyway. Comment threads are left to the on-demand pull, so a sync
/// costs one request per page instead of two.
pub async fn pull_all(
    client: &Client,
    store: &Store,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<SyncAllReport> {
    const BATCH: usize = 6;
    let mut found = client.search("").await?;
    // data sources last: a row cached by `pull_data_source` first would hide its changes
    found.sort_by_key(|r| matches!(r, SearchResult::DataSource(_)));
    let total = found.len();
    let mut pages = Vec::new();
    let mut data_sources = Vec::new();
    for item in found {
        match item {
            SearchResult::Page(p) => pages.push(p),
            SearchResult::DataSource(d) => data_sources.push(d),
        }
    }
    let mut report = SyncAllReport::default();
    let mut done = 0;
    for chunk in pages.chunks(BATCH) {
        // the network waits of a batch overlap; the writes below stay serial
        let fetched = join_all(
            chunk
                .iter()
                .map(|p| prefetch_page(client, store, p, poll_settle())),
        )
        .await;
        for (page, res) in chunk.iter().zip(fetched) {
            let title = page.title();
            let res = match res {
                Ok(Some(tree)) => apply_page(client, store, page.clone(), tree, false)
                    .await
                    .map(|_| (false, false)),
                Ok(None) => Ok((true, false)),
                Err(e) => Err(e),
            };
            match res {
                Ok((true, _)) => report.skipped += 1,
                Ok((false, false)) => report.pages += 1,
                Ok((false, true)) => report.data_sources += 1,
                Err(e @ (Error::Api { status: 401, .. } | Error::SessionExpired)) => return Err(e),
                Err(Error::Http(e)) if e.is_connect() || e.is_timeout() => {
                    return Err(Error::Http(e))
                }
                Err(e) => report.errors.push(format!("{title}: {e}")),
            }
            done += 1;
            progress(done, total, &title);
        }
    }
    // Notion can't list what left a data source, so only every FULL_EVERY-th
    // sync (the first of a run included) re-reads whole data sources and
    // notices trashed rows; the others fetch just the rows edited since.
    // ponytail: the count is per process, a failed full pull still uses the
    // slot, and trashed/moved rows can stay cached until the next full pull.
    static SYNCS: AtomicU64 = AtomicU64::new(0);
    let full = SYNCS.fetch_add(1, Ordering::Relaxed).is_multiple_of(FULL_EVERY);
    for chunk in data_sources.chunks(DS_BATCH) {
        let pulled = join_all(chunk.iter().map(|d| sync_data_source(client, store, &d.id, full))).await;
        for (ds, res) in chunk.iter().zip(pulled) {
            let title = ds.title.clone();
            match res {
                Ok(()) => report.data_sources += 1,
                Err(e @ (Error::Api { status: 401, .. } | Error::SessionExpired)) => return Err(e),
                Err(Error::Http(e)) if e.is_connect() || e.is_timeout() => {
                    return Err(Error::Http(e))
                }
                Err(e) => report.errors.push(format!("{title}: {e}")),
            }
            done += 1;
            progress(done, total, &title);
        }
    }
    Ok(report)
}

const FULL_EVERY: u64 = 10;
const DS_BATCH: usize = 4;

/// A full pull, or (`!full`) the schema plus the rows edited since the newest cached one.
async fn sync_data_source(client: &Client, store: &Store, id: &str, full: bool) -> Result<()> {
    if full {
        return pull_data_source(client, store, id).await.map(drop);
    }
    store.upsert_data_source(&client.get_data_source(id).await?)?;
    poll_data_source(client, store, id).await.map(drop)
}

// ---------------------------------------------------------------- push

/// Conflict GETs in flight at once (the client's rate limiter caps the rest).
const PUSH_CHECKS: usize = 4;

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
    let ops = store.queued_ops()?;
    // The conflict GETs of Update/Delete ops run up front, a few at a time; the
    // writes below stay in order. Only for a block no other op in the queue
    // touches (as target or parent), so no write of this push can make the
    // early read stale. A failed GET is simply redone in `apply`.
    let mut targets: HashMap<String, usize> = HashMap::new();
    for op in &ops {
        *targets.entry(store.resolve_id(op.kind.target_id())?).or_default() += 1;
    }
    let mut early = Vec::new();
    for op in &ops {
        if let OpKind::UpdateBlock { block_id, .. } | OpKind::DeleteBlock { block_id } = &op.kind {
            let id = store.resolve_id(block_id)?;
            if targets[&id] == 1 && !id.starts_with("local-") {
                early.push((op.id, id));
            }
        }
    }
    // ponytail: the conflict check may be up to one push old; a remote edit in
    // that window is overwritten - re-check right before each PATCH if that matters.
    let mut early: HashMap<i64, Block> = stream::iter(early)
        .map(|(op, id)| async move { client.get_block(&id).await.ok().map(|b| (op, b)) })
        .buffer_unordered(PUSH_CHECKS)
        .filter_map(|r| async move { r })
        .collect()
        .await;
    for op in ops {
        if let OpKind::DeleteBlock { block_id } = &op.kind {
            if blocked.contains(&store.resolve_id(block_id)?) {
                store.mark_op(op.id, OpStatus::Failed, Some("anchor's create failed"))?;
                report.failed += 1;
                continue;
            }
        }
        let res = apply(client, store, &op, &mut own, early.remove(&op.id)).await;
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

/// `Some(reason)` if the remote page differs from the base snapshot (our own
/// last write in this push, else the op's) in a field this op writes. The
/// page's edit time says nothing: any block edit bumps it.
fn page_conflict(op: &Op, id: &str, own: &Own, remote: &Page, keys: &[&str]) -> Option<String> {
    let base = own.get(id).map(|(_, raw)| raw).or(op.base_raw.as_ref())?;
    let null = Value::Null;
    let changed: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| base.get(*k).unwrap_or(&null) != remote.raw.get(*k).unwrap_or(&null))
        .collect();
    (!changed.is_empty()).then(|| format!("remote changed {}", changed.join(", ")))
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
    early: Option<Block>,
) -> Result<(Block, Option<String>)> {
    let remote = match early {
        Some(b) => b,
        None => client.get_block(id).await?,
    };
    let c = conflict(op, id, remote.last_edited_time, own, |raw| {
        Ok(same_content(
            &serde_json::from_value::<Block>(raw.clone())?.kind,
            &remote.kind,
        ))
    })?;
    Ok((remote, c))
}

fn record_comment(own: &mut Own, c: &Comment) -> Result<()> {
    own.insert(c.id.clone(), (c.last_edited_time, serde_json::to_value(c)?));
    Ok(())
}

/// `Some(reason)` if the remote comment was edited since the op's base.
async fn check_comment(client: &Client, op: &Op, id: &str, own: &Own) -> Result<Option<String>> {
    let remote = client.get_comment(id).await?;
    conflict(op, id, remote.last_edited_time, own, |raw| {
        Ok(serde_json::from_value::<Comment>(raw.clone())?.rich_text == remote.rich_text)
    })
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

async fn apply(
    client: &Client,
    store: &Store,
    op: &Op,
    own: &mut Own,
    early: Option<Block>,
) -> Result<Outcome> {
    match &op.kind {
        OpKind::UpdateBlock { block_id, body } => {
            let id = store.resolve_id(block_id)?;
            if let (_, Some(c)) = check_block(client, op, &id, own, early).await? {
                return Ok(Outcome::Conflict(c));
            }
            let b = client.update_block(&id, body).await?;
            store.put_block(&b)?;
            record_block(own, &b)?;
        }
        OpKind::DeleteBlock { block_id } => {
            let id = store.resolve_id(block_id)?;
            if let (_, Some(c)) = check_block(client, op, &id, own, early).await? {
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
        OpKind::CreatePage {
            local_id,
            parent_id,
            body,
        } => {
            let parent = store.resolve_id(parent_id)?;
            if parent.starts_with("local-") {
                return Ok(Outcome::Failed("its parent page was never created".into()));
            }
            let mut req = body.clone();
            // a row of a data source carries its own parent
            if req.get("parent").is_none() {
                req["parent"] = json!({ "type": "page_id", "page_id": parent });
            }
            let resp = match client.create_page(&req).await {
                // Never reached Notion (or it said no): safe to retry or fail as usual.
                Err(e @ (Error::Api { .. } | Error::RetriesExhausted | Error::SessionExpired)) => {
                    return Err(e)
                }
                Err(Error::Http(e)) if e.is_connect() => return Err(Error::Http(e)),
                // The POST may have landed: a retry could create it twice.
                Err(_) => {
                    return Ok(Outcome::Failed(
                        "may have been created; check Notion before retrying".into(),
                    ))
                }
                Ok(resp) => resp,
            };
            let Some(new_id) = resp["id"].as_str().map(str::to_string) else {
                return Ok(Outcome::Failed("Notion's answer has no page id".into()));
            };
            // The page exists remotely: cache trouble must not fail the op.
            let cached = store.atomic(|| {
                store.rename_page(local_id, &new_id)?;
                store.map_id(local_id, &new_id)?;
                if let Ok(p) = serde_json::from_value::<Page>(resp.clone()) {
                    store.upsert_page(&p)?;
                    own.insert(
                        p.id.clone(),
                        (p.last_edited_time, serde_json::to_value(&p)?),
                    );
                }
                Ok(())
            });
            if let Err(e) = cached {
                tracing::warn!("created page {local_id} not cached: {e}");
            }
        }
        OpKind::UpdatePage { page_id, body } => {
            let id = store.resolve_id(page_id)?;
            if id.starts_with("local-") {
                return Ok(Outcome::Failed("its page was never created".into()));
            }
            let remote = client.get_page(&id).await?;
            let keys: Vec<&str> = body
                .as_object()
                .into_iter()
                .flat_map(|o| o.keys().map(String::as_str))
                .collect();
            if let Some(c) = page_conflict(op, &id, own, &remote, &keys) {
                return Ok(Outcome::Conflict(c));
            }
            let p = client.update_page(&id, body).await?;
            store.upsert_page(&p)?;
            own.insert(
                p.id.clone(),
                (p.last_edited_time, serde_json::to_value(&p)?),
            );
        }
        OpKind::MovePage {
            page_id,
            new_parent_id,
        } => {
            let id = store.resolve_id(page_id)?;
            let parent = store.resolve_id(new_parent_id)?;
            if id.starts_with("local-") || parent.starts_with("local-") {
                return Ok(Outcome::Failed(
                    "its page or new parent was never created".into(),
                ));
            }
            let remote = client.get_page(&id).await?;
            if let Some(c) = page_conflict(op, &id, own, &remote, &["parent"]) {
                return Ok(Outcome::Conflict(c));
            }
            let resp = client.move_page(&id, &parent).await?;
            // the answer may be just {object, id}: the cache already shows the move
            if let Ok(p) = serde_json::from_value::<Page>(resp) {
                store.upsert_page(&p)?;
                own.insert(
                    p.id.clone(),
                    (p.last_edited_time, serde_json::to_value(&p)?),
                );
            }
        }
        OpKind::CreateBlock {
            parent_id,
            after,
            block,
        } => {
            let local: Block = serde_json::from_value(block.clone())?;
            let parent = store.resolve_id(parent_id)?;
            if parent.starts_with("local-") {
                return Ok(Outcome::Failed("its page was never created".into()));
            }
            let after = after.as_deref().map(|a| store.resolve_id(a)).transpose()?;
            let new = if let Some(path) = block["_upload"]["path"].as_str() {
                // an image/file picked offline: upload it now, then attach (see `media`)
                let ty = block["type"].as_str().unwrap_or("file");
                // ponytail: a retry after a failed attach re-uploads; the orphan upload expires in 1 h
                let up = match client.upload_file(std::path::Path::new(path)).await {
                    // file gone, moved or too big: fail this op, don't abort the push
                    Err(Error::Invalid(m)) => return Ok(Outcome::Failed(m)),
                    r => r?,
                };
                let body = json!({ "type": ty, ty: { "type": "file_upload", "file_upload": { "id": up } } });
                client.append_raw(&parent, &body, after.as_deref()).await?
            } else {
                first(
                    client
                        .append_children(&parent, std::slice::from_ref(&local), after.as_deref())
                        .await?,
                )?
            };
            // A create kept for a locally deleted block (see local_delete_block)
            // must not bring it back into the cache; its DeleteBlock follows.
            if store.get_block(&local.id)?.is_some() {
                store.rename_block(&local.id, &new.id)?;
                store.put_block(&new)?;
            }
            store.map_id(&local.id, &new.id)?;
            record_block(own, &new)?;
        }
        OpKind::CreateComment {
            parent,
            rich_text,
            local_id,
            local_discussion,
        } => {
            let target = store.resolve_id(parent.id())?;
            if target.starts_with("local-") {
                return Ok(Outcome::Failed("its target was never created".into()));
            }
            let mut body = match parent {
                CommentTarget::Page(_) => {
                    json!({ "parent": { "type": "page_id", "page_id": target } })
                }
                CommentTarget::Block(_) => {
                    json!({ "parent": { "type": "block_id", "block_id": target } })
                }
                CommentTarget::Discussion(_) => json!({ "discussion_id": target }),
            };
            body["rich_text"] = rich_text.clone();
            let resp = match client.create_comment(&body).await {
                // Never reached Notion (or it said no): safe to retry or fail as usual.
                Err(e @ (Error::Api { .. } | Error::RetriesExhausted | Error::SessionExpired)) => {
                    return Err(e)
                }
                Err(Error::Http(e)) if e.is_connect() => return Err(Error::Http(e)),
                // The POST may have landed: a retry could post it twice.
                Err(_) => {
                    return Ok(Outcome::Failed(
                        "may have been posted; check Notion before retrying".into(),
                    ))
                }
                Ok(resp) => resp,
            };
            // From here the comment exists remotely: nothing may fail the op
            // (a retry would post it twice), so cache trouble is only logged.
            // A partial response (insert-only capability) just re-keys the row;
            // the next pull fills it in.
            let cached = store.atomic(|| {
                match Comment::from_value_lenient(resp.clone()) {
                    Some(new) => {
                        store.delete_comment(local_id)?;
                        store.put_comment(&new)?;
                        if *local_discussion != new.discussion_id {
                            store.rename_discussion(local_discussion, &new.discussion_id)?;
                            store.map_id(local_discussion, &new.discussion_id)?;
                        }
                        store.map_id(local_id, &new.id)?;
                        record_comment(own, &new)?;
                    }
                    None => {
                        let new_id = resp["id"].as_str().unwrap_or_default();
                        store.rename_comment(local_id, new_id)?;
                        store.map_id(local_id, new_id)?;
                    }
                }
                Ok(())
            });
            if let Err(e) = cached {
                tracing::warn!("posted comment {local_id} not cached: {e}");
            }
        }
        OpKind::UpdateComment {
            comment_id,
            rich_text,
        } => {
            let id = store.resolve_id(comment_id)?;
            if id.starts_with("local-") {
                return Ok(Outcome::Failed("its comment was never created".into()));
            }
            if let Some(c) = check_comment(client, op, &id, own).await? {
                return Ok(Outcome::Conflict(c));
            }
            let resp = client.update_comment(&id, rich_text).await?;
            if let Some(c) = Comment::from_value_lenient(resp) {
                store.put_comment(&c)?;
                record_comment(own, &c)?;
            }
        }
        OpKind::DeleteComment { comment_id } => {
            let id = store.resolve_id(comment_id)?;
            if id.starts_with("local-") {
                return Ok(Outcome::Failed("its comment was never created".into()));
            }
            if let Some(c) = check_comment(client, op, &id, own).await? {
                return Ok(Outcome::Conflict(c));
            }
            client.delete_comment(&id).await?;
            store.delete_comment(&id)?;
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
            let (remote, c) = check_block(client, op, &id, own, None).await?;
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

// ---------------------------------------------------------------- resolving conflicts

/// The remote copy of what a conflicted op targets.
pub enum Remote {
    Block(Block),
    Page(Page),
    Comment(Comment),
}

/// What Notion has now, for the conflict panel.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// Deleted, trashed or no longer readable: only Keep theirs is left.
    Gone,
    /// Not a text edit: no texts to compare or merge.
    Other,
    /// A text edit: both versions as the editor shows them (Markdown).
    Text { mine: String, theirs: String },
}

fn conflict_op(store: &Store, id: i64) -> Result<Op> {
    store
        .ops_by_status(OpStatus::Conflict)?
        .into_iter()
        .find(|o| o.id == id)
        .ok_or_else(|| Error::Invalid(format!("op {id} is not a conflict")))
}

/// Gets the object an op targets; `None` if it is gone (404 or in the trash).
pub async fn fetch_remote(client: &Client, store: &Store, op: &Op) -> Result<Option<Remote>> {
    let id = store.resolve_id(op.kind.target_id())?;
    let got = match &op.kind {
        OpKind::UpdateBlock { .. } | OpKind::DeleteBlock { .. } | OpKind::MoveBlock { .. } => {
            client
                .get_block(&id)
                .await
                .map(|b| (!b.in_trash).then_some(Remote::Block(b)))
        }
        OpKind::UpdatePageProperties { .. } => client
            .get_page(&id)
            .await
            .map(|p| (!p.in_trash).then_some(Remote::Page(p))),
        // a restore's page is in the trash: still there
        OpKind::UpdatePage { .. } | OpKind::MovePage { .. } => {
            client.get_page(&id).await.map(|p| Some(Remote::Page(p)))
        }
        OpKind::UpdateComment { .. } | OpKind::DeleteComment { .. } => client
            .get_comment(&id)
            .await
            .map(|c| Some(Remote::Comment(c))),
        _ => {
            return Err(Error::Invalid(
                "only edits, deletes and moves conflict".into(),
            ))
        }
    };
    match got {
        Err(Error::Api { status: 404, .. }) => Ok(None),
        other => other,
    }
}

/// The title property of an UpdatePageProperties body: its key and runs.
fn title_items(props: &Value) -> Option<(&String, &Vec<Value>)> {
    props
        .as_object()?
        .iter()
        .find_map(|(k, v)| Some((k, v["title"].as_array()?)))
}

/// (yours, Notion's) as the editor shows them; `None` unless a text edit
/// that Markdown carries (else a merge would drop formatting).
fn texts(op: &Op, remote: &Remote) -> Option<(String, String)> {
    let md = |rt: &[RichText], code| {
        if code {
            plain_text(rt)
        } else {
            to_markdown(rt)
        }
    };
    let rich = |v: &Value| serde_json::from_value::<Vec<RichText>>(v.clone()).ok();
    Some(match (&op.kind, remote) {
        (OpKind::UpdateBlock { body, .. }, Remote::Block(b)) => {
            let code = matches!(b.kind, BlockKind::Code { .. });
            let mine = rich(&body[b.kind.type_name()]["rich_text"])?;
            let theirs = &b.kind.text()?.rich_text;
            if !code && !(is_markdown_editable(&mine) && is_markdown_editable(theirs)) {
                return None;
            }
            (md(&mine, code), md(theirs, code))
        }
        (OpKind::UpdatePageProperties { properties, .. }, Remote::Page(p)) => {
            // a merge rewrites the title only
            if properties.as_object()?.len() != 1 {
                return None;
            }
            let (_, items) = title_items(properties)?;
            let mine = items.iter().filter_map(|i| i["text"]["content"].as_str());
            (mine.collect(), p.title())
        }
        (OpKind::UpdateComment { rich_text, .. }, Remote::Comment(c)) => {
            let mine = rich(rich_text)?;
            if !(is_markdown_editable(&mine) && is_markdown_editable(&c.rich_text)) {
                return None;
            }
            (to_markdown(&mine), to_markdown(&c.rich_text))
        }
        _ => return None,
    })
}

/// Fetches Notion's version of a conflict op's object, for the panel.
pub async fn fetch_conflict(client: &Client, store: &Store, op_id: i64) -> Result<Fetched> {
    let op = conflict_op(store, op_id)?;
    Ok(match fetch_remote(client, store, &op).await? {
        None => Fetched::Gone,
        Some(r) => match texts(&op, &r) {
            Some((mine, theirs)) => Fetched::Text { mine, theirs },
            None => Fetched::Other,
        },
    })
}

/// Re-queues a conflict op against `remote`: that version becomes its base,
/// so the next push applies it. The cache shows what that push will write.
pub fn rebase_op(store: &Store, op_id: i64, remote: &Remote) -> Result<()> {
    let op = conflict_op(store, op_id)?;
    let (time, raw) = match remote {
        Remote::Block(b) => (b.last_edited_time, serde_json::to_value(b)?),
        Remote::Page(p) => (p.last_edited_time, serde_json::to_value(p)?),
        Remote::Comment(c) => (c.last_edited_time, serde_json::to_value(c)?),
    };
    // a later queued op on the object already left its version in the cache
    let target = store.resolve_id(op.kind.target_id())?;
    let mut later = false;
    for o in store
        .queued_ops()?
        .into_iter()
        .chain(store.ops_by_status(OpStatus::Conflict)?)
        .chain(store.ops_by_status(OpStatus::Failed)?)
    {
        later |= o.id > op_id && store.resolve_id(o.kind.target_id())? == target;
    }
    store.atomic(|| {
        match (&op.kind, remote) {
            _ if later => {}
            (OpKind::UpdateBlock { body, .. }, Remote::Block(b)) => {
                // the remote block with the body's fields written over it
                let t = b.kind.type_name();
                let mut raw = b.raw.clone();
                if let (Some(to), Some(from)) = (
                    raw.get_mut(t).and_then(Value::as_object_mut),
                    body.get(t).and_then(Value::as_object),
                ) {
                    to.extend(from.clone());
                }
                store.put_block(&serde_json::from_value(raw)?)?;
            }
            (OpKind::DeleteBlock { block_id }, _) => {
                store.delete_block(&store.resolve_id(block_id)?)?
            }
            (
                OpKind::MoveBlock {
                    block_id,
                    new_parent_id,
                    after,
                },
                _,
            ) => {
                let after = after.as_deref().map(|a| store.resolve_id(a)).transpose()?;
                if let Err(e) = store.move_block_local(
                    &store.resolve_id(block_id)?,
                    &store.resolve_id(new_parent_id)?,
                    after.as_deref(),
                ) {
                    tracing::warn!("rebased move of {block_id} not shown in cache: {e}");
                }
            }
            (OpKind::UpdatePageProperties { properties, .. }, Remote::Page(p)) => {
                let mut p = p.clone();
                if let Some((k, items)) = title_items(properties) {
                    let t: String = items
                        .iter()
                        .filter_map(|i| i["text"]["content"].as_str())
                        .collect();
                    p.properties
                        .insert(k.clone(), PropertyValue::Title(title_runs(&t)));
                }
                store.upsert_page(&p)?;
            }
            (OpKind::UpdatePage { body, .. }, Remote::Page(p)) => {
                let mut p = p.clone();
                for (k, v) in body.as_object().into_iter().flatten() {
                    match k.as_str() {
                        "in_trash" => p.in_trash = v.as_bool().unwrap_or(false),
                        k => p.raw[k] = v.clone(),
                    }
                }
                store.upsert_page(&p)?;
            }
            (OpKind::MovePage { new_parent_id, .. }, Remote::Page(p)) => {
                let np = store.resolve_id(new_parent_id)?;
                let mut p = p.clone();
                p.raw["parent"] = json!({ "type": "page_id", "page_id": np });
                p.parent = Parent::Page(np);
                store.upsert_page(&p)?;
            }
            (OpKind::UpdateComment { rich_text, .. }, Remote::Comment(c)) => {
                let mut c = c.clone();
                c.rich_text = serde_json::from_value(rich_text.clone())?;
                store.put_comment(&c)?;
            }
            _ => {}
        }
        store.conn.execute(
            "UPDATE ops SET status = 'queued', error = NULL,
               base_last_edited_time = ?2, base_raw = ?3 WHERE id = ?1",
            rusqlite::params![op_id, ts(&time), raw.to_string()],
        )?;
        Ok(())
    })
}

/// `op` with its text replaced by `md` (Markdown as the editor shows it,
/// converted as the editor's commit does).
fn merged_kind(op: &Op, remote: &Remote, md: &str) -> Result<OpKind> {
    let bad = || Error::Invalid("only text edits can be merged".into());
    let (mine, _) = texts(op, remote).ok_or_else(bad)?;
    let mut kind = op.kind.clone();
    // untouched: keep the rich text as it is, whatever Markdown can't carry
    if mine == md {
        return Ok(kind);
    }
    match (&mut kind, remote) {
        (OpKind::UpdateBlock { body, .. }, Remote::Block(b)) => {
            let rich = if matches!(b.kind, BlockKind::Code { .. }) {
                title_runs(md)
            } else {
                from_markdown(md)
            };
            let slot = body
                .get_mut(b.kind.type_name())
                .and_then(|p| p.get_mut("rich_text"))
                .ok_or_else(bad)?;
            *slot = json!(split_runs(rich));
        }
        (OpKind::UpdatePageProperties { properties, .. }, _) => {
            let key = title_items(properties).ok_or_else(bad)?.0.clone();
            properties[key.as_str()] = json!({ "title": title_body(&title_runs(md)) });
        }
        (OpKind::UpdateComment { rich_text, .. }, _) => {
            *rich_text = json!(split_runs(from_markdown(md)));
        }
        _ => return Err(bad()),
    }
    Ok(kind)
}

/// Keep mine / Merge: fetches Notion's version, rewrites the op's text from
/// `merged` if given, and re-queues it against that version. Refused when
/// the object is gone, or when Notion's text is no longer `seen` (what the
/// panel showed): the user has to look at the new one.
pub async fn resolve_mine(
    client: &Client,
    store: &Store,
    op_id: i64,
    merged: Option<&str>,
    seen: Option<&str>,
) -> Result<()> {
    let op = conflict_op(store, op_id)?;
    let remote = fetch_remote(client, store, &op)
        .await?
        .ok_or_else(|| Error::Invalid("it is gone from Notion: only Keep theirs is left".into()))?;
    if let Some(seen) = seen {
        if texts(&op, &remote).is_none_or(|(_, theirs)| theirs != seen) {
            return Err(Error::Invalid("Notion changed again: review it".into()));
        }
    }
    store.atomic(|| {
        if let Some(md) = merged {
            store.update_op_kind(op_id, &merged_kind(&op, &remote, md)?)?;
        }
        rebase_op(store, op_id, &remote)
    })
}

// ---------------------------------------------------------------- local edits

/// Moving (copy, then delete) or converting a block gives it a new id, which
/// would silently drop its comments, remotely and in the cache.
fn refuse_if_commented(store: &Store, block_id: &str, what: &str) -> Result<()> {
    if store.subtree_has_comments(block_id)? {
        return Err(Error::Invalid(format!(
            "this block has comments; {what} it would lose them"
        )));
    }
    Ok(())
}

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
        // the same block edited again with nothing queued since (typing
        // across polls): one PATCH, keeping the first op's base for conflicts
        if let Some(op) = store.queued_ops()?.pop() {
            if let OpKind::UpdateBlock {
                block_id,
                body: old,
            } = &op.kind
            {
                if store.resolve_id(block_id)? == block.id {
                    let kind = OpKind::UpdateBlock {
                        block_id: block_id.clone(),
                        body: merge_bodies(old.clone(), body),
                    };
                    store.update_op_kind(op.id, &kind)?;
                    return Ok(op.id);
                }
            }
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

/// `new` over `old`, two levels deep: `{"to_do":{"checked":true}}` then
/// `{"to_do":{"rich_text":..}}` keeps both.
fn merge_bodies(mut old: Value, new: Value) -> Value {
    match (old.as_object_mut(), new) {
        (Some(o), Value::Object(n)) => {
            for (k, v) in n {
                match (o.get_mut(&k).and_then(Value::as_object_mut), v) {
                    (Some(inner), Value::Object(vn)) => inner.extend(vn),
                    (_, v) => {
                        o.insert(k, v);
                    }
                }
            }
            old
        }
        (_, new) => new,
    }
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
    enqueue_kind(store, &base, &block)
}

/// Queues a PATCH of `block`'s whole payload (text, colour, ...).
fn enqueue_kind(store: &Store, base: &Block, block: &Block) -> Result<i64> {
    let mut body = block.kind.to_request_json()?;
    if let Some(o) = body.as_object_mut() {
        o.remove("type");
    }
    enqueue_update(store, base, block, body)
}

/// Sets a cached block's `color` (one of [`crate::model::COLORS`], plain or
/// `_background`) and queues the PATCH. Returns the op id.
pub fn local_set_color(store: &Store, block_id: &str, color: &str) -> Result<i64> {
    let base = cached(store, block_id)?;
    let mut block = base.clone();
    block.kind.set_color(color)?;
    enqueue_kind(store, &base, &block)
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

/// Sets a cached media block's display width (px) and queues
/// `{"<type>":{"_width":w}}`; only the private backend syncs it. Returns the op id.
pub fn local_set_image_width(store: &Store, block_id: &str, width: f32) -> Result<i64> {
    let base = cached(store, block_id)?;
    let mut raw = serde_json::to_value(&base)?;
    let t = raw["type"].as_str().unwrap_or_default().to_string();
    if !raw[t.as_str()].is_object() {
        return Err(Error::Invalid(format!("block {block_id} has no media payload")));
    }
    raw[t.as_str()]["_width"] = json!(width);
    let block: Block = serde_json::from_value(raw)?;
    enqueue_update(store, &base, &block, json!({ t: { "_width": width } }))
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

/// A remote `page` keeps the cached values of what a queued
/// UpdatePageProperties (properties), UpdatePage (icon, cover, trash, lock)
/// or MovePage (parent) op will write; the rest is taken from remote.
// ponytail: scans the queue per page; index by page id if queues get long
fn keep_queued_props(store: &Store, page: &mut Page) -> Result<()> {
    let Some(cached) = store.get_page(&page.id)? else {
        return Ok(());
    };
    let mut kept = false;
    if let Some(Op {
        kind: OpKind::UpdatePageProperties { properties, .. },
        ..
    }) = queued_props(store, &page.id)?
    {
        for k in properties.as_object().into_iter().flat_map(|o| o.keys()) {
            match cached.properties.get(k) {
                Some(v) => page.properties.insert(k.clone(), v.clone()),
                None => page.properties.remove(k),
            };
        }
        kept = true;
    }
    for op in store.queued_ops()? {
        match &op.kind {
            OpKind::UpdatePage { page_id, body } if store.resolve_id(page_id)? == page.id => {
                for k in body.as_object().into_iter().flat_map(|o| o.keys()) {
                    match k.as_str() {
                        "in_trash" => page.in_trash = cached.in_trash,
                        k => page.raw[k] = cached.raw.get(k).cloned().unwrap_or(Value::Null),
                    }
                }
                kept = true;
            }
            OpKind::MovePage { page_id, .. } if store.resolve_id(page_id)? == page.id => {
                page.parent = cached.parent.clone();
                page.raw["parent"] = cached.raw["parent"].clone();
                kept = true;
            }
            _ => {}
        }
    }
    // Keep the cached time too, so the pull after the op leaves the queue
    // isn't skipped as "unchanged" while the cache still holds our edit.
    if kept {
        page.last_edited_time = cached.last_edited_time;
    }
    Ok(())
}

/// Plain text as runs (none for ""), split at Notion's length limit.
fn title_runs(t: &str) -> Vec<RichText> {
    if t.is_empty() {
        Vec::new()
    } else {
        split_runs(vec![RichText::plain(t)])
    }
}

/// The runs as a title property's request items.
fn title_body(runs: &[RichText]) -> Vec<Value> {
    runs.iter()
        .map(|r| json!({ "type": "text", "text": { "content": r.plain_text } }))
        .collect()
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
    let runs = title_runs(title);
    let body = title_body(&runs);
    let mut page = base.clone();
    page.properties
        .insert(key.clone(), PropertyValue::Title(runs));
    store.atomic(|| {
        store.upsert_page(&page)?;
        // an unpushed page: its create carries the title (and its row in the parent shows it)
        if let Some(op) = queued_create_page(store, &page.id)? {
            if let Some(mut b) = store.get_block(&page.id)? {
                b.kind = BlockKind::ChildPage {
                    title: title.to_string(),
                };
                store.put_block(&b)?;
            }
            return set_create_body(
                store,
                op,
                "properties",
                json!({ "title": { "title": body } }),
            );
        }
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

/// The queued CreatePage op that creates page `local_id`, if any.
fn queued_create_page(store: &Store, local_id: &str) -> Result<Option<Op>> {
    Ok(store
        .queued_ops()?
        .into_iter()
        .find(|o| matches!(&o.kind, OpKind::CreatePage { local_id: l, .. } if l == local_id)))
}

/// Sets `body[key]` of a queued CreatePage op. Returns its id.
fn set_create_body(store: &Store, op: Op, key: &str, value: Value) -> Result<i64> {
    let OpKind::CreatePage {
        local_id,
        parent_id,
        mut body,
    } = op.kind
    else {
        return Err(Error::Invalid("not a page create".into()));
    };
    body[key] = value;
    store.update_op_kind(
        op.id,
        &OpKind::CreatePage {
            local_id,
            parent_id,
            body,
        },
    )?;
    Ok(op.id)
}

/// Caches a new untitled-or-titled page under the cached page `parent_id`
/// with a temporary `local-…` id, shows it as a `child_page` row of the
/// parent (after `after`, a block of the parent, else last) and queues its
/// creation. A page's blocks may be edited before the push: their ops map
/// through the id. Returns the page id.
pub fn local_create_page(
    store: &Store,
    parent_id: &str,
    after: Option<&str>,
    title: &str,
) -> Result<String> {
    match store.get_page(parent_id)? {
        Some(p) if !p.in_trash => {}
        _ => {
            return Err(Error::Invalid(
                "the parent page is not cached or in the trash".into(),
            ))
        }
    }
    let id = local_stamp("local");
    let now = ts(&Utc::now());
    let runs = title_runs(title);
    let parent = json!({ "type": "page_id", "page_id": parent_id });
    let page: Page = serde_json::from_value(json!({
        "object": "page", "id": id, "parent": parent, "created_time": now,
        "last_edited_time": now, "in_trash": false, "url": "", "icon": null, "cover": null,
        "properties": { "title": { "id": "title", "type": "title", "title": runs } },
    }))?;
    let row: Block = serde_json::from_value(json!({
        "object": "block", "id": id, "parent": parent, "type": "child_page",
        "child_page": { "title": title }, "has_children": false, "in_trash": false,
        "created_time": now, "last_edited_time": now,
    }))?;
    store.atomic(|| {
        store.upsert_page(&page)?;
        store.put_block(&row)?;
        store.move_block_local(&id, parent_id, after)?;
        store.enqueue(
            OpKind::CreatePage {
                local_id: id.clone(),
                parent_id: parent_id.to_string(),
                body: json!({ "properties": { "title": { "title": title_body(&runs) } } }),
            },
            None,
            None,
        )?;
        Ok(())
    })?;
    Ok(id)
}

/// Sets property `name` of a cached database row to `text` (see
/// [`props::encode`]; the data source's schema says its type) and queues
/// `{"<name>": value}`, merged into the row's queued property update; an
/// unpushed row's create carries it instead. Returns the op id.
pub fn local_set_property(store: &Store, page_id: &str, name: &str, text: &str) -> Result<i64> {
    let base = store
        .get_page(page_id)?
        .ok_or_else(|| Error::Invalid(format!("page {page_id} not cached")))?;
    let Parent::DataSource { id: ds, .. } = &base.parent else {
        return Err(Error::Invalid("not a database row".into()));
    };
    let schema = store
        .data_source(ds)?
        .map(|d| d.properties[name].clone())
        .unwrap_or_default();
    if schema.is_null() {
        return Err(Error::Invalid(format!(
            "the database has no property {name}"
        )));
    }
    let (sent, cached) = props::encode(&schema, text, &base.raw["properties"][name])?;
    let mut raw = serde_json::to_value(&base)?;
    raw["properties"][name] = cached;
    let page: Page = serde_json::from_value(raw)?;
    store.atomic(|| {
        store.upsert_page(&page)?;
        if let Some(op) = queued_create_page(store, page_id)? {
            let OpKind::CreatePage { body, .. } = &op.kind else {
                unreachable!()
            };
            let mut all = body["properties"].clone();
            all[name] = sent;
            return set_create_body(store, op, "properties", all);
        }
        if let Some(Op {
            id,
            kind:
                OpKind::UpdatePageProperties {
                    page_id,
                    mut properties,
                },
            ..
        }) = queued_props(store, page_id)?
        {
            properties[name] = sent.clone();
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
                page_id: page_id.to_string(),
                properties: json!({ name: sent }),
            },
            Some(base.last_edited_time),
            Some(serde_json::to_value(&base)?),
        )
    })
}

/// Caches a new empty row of the cached data source with a temporary
/// `local-…` id and queues its creation under `{"data_source_id": …}`.
/// Returns the row's id; edit it with [`local_set_property`] before the push.
pub fn local_create_row(store: &Store, ds_id: &str) -> Result<String> {
    let ds = store
        .data_source(ds_id)?
        .ok_or_else(|| Error::Invalid("the data source isn't cached".into()))?;
    let title = ds
        .properties
        .as_object()
        .into_iter()
        .flatten()
        .find(|(_, p)| p["type"] == "title")
        .map(|(k, _)| k.clone())
        .ok_or_else(|| Error::Invalid("the data source has no title property".into()))?;
    let id = local_stamp("local");
    let now = ts(&Utc::now());
    let parent =
        json!({ "type": "data_source_id", "data_source_id": ds_id, "database_id": ds.database_id });
    let page: Page = serde_json::from_value(json!({
        "object": "page", "id": id, "parent": parent, "created_time": now,
        "last_edited_time": now, "in_trash": false, "url": "", "icon": null, "cover": null,
        "properties": { &title: { "id": "title", "type": "title", "title": [] } },
    }))?;
    store.atomic(|| {
        store.upsert_page(&page)?;
        store.enqueue(
            OpKind::CreatePage {
                local_id: id.clone(),
                parent_id: ds_id.to_string(),
                body: json!({
                    "parent": { "type": "data_source_id", "data_source_id": ds_id },
                    "properties": { title: { "title": [] } },
                }),
            },
            None,
            None,
        )?;
        Ok(())
    })?;
    Ok(id)
}

/// Writes `key` (`icon`, `cover`, `in_trash`, `is_locked`) of a cached page
/// and queues the PATCH, merged into the page's queued one; an unpushed
/// page's create carries it instead. Returns the op id.
fn local_update_page(store: &Store, page_id: &str, key: &str, value: Value) -> Result<i64> {
    let base = store
        .get_page(page_id)?
        .ok_or_else(|| Error::Invalid(format!("page {page_id} not cached")))?;
    let mut page = base.clone();
    match key {
        "in_trash" => page.in_trash = value.as_bool().unwrap_or(false),
        k => page.raw[k] = value.clone(),
    }
    store.atomic(|| {
        store.upsert_page(&page)?;
        if let Some(op) = queued_create_page(store, page_id)? {
            return set_create_body(store, op, key, value);
        }
        for op in store.queued_ops()? {
            if let OpKind::UpdatePage {
                page_id: p,
                mut body,
            } = op.kind
            {
                if store.resolve_id(&p)? == page_id {
                    body[key] = value;
                    store.update_op_kind(op.id, &OpKind::UpdatePage { page_id: p, body })?;
                    return Ok(op.id);
                }
            }
        }
        store.enqueue(
            OpKind::UpdatePage {
                page_id: page_id.to_string(),
                body: json!({ key: value }),
            },
            Some(base.last_edited_time),
            Some(serde_json::to_value(&base)?),
        )
    })
}

/// Sets (`Some`) or removes the page's emoji icon.
pub fn local_set_icon(store: &Store, page_id: &str, emoji: Option<&str>) -> Result<i64> {
    let icon = match emoji.map(str::trim) {
        None => Value::Null,
        // ponytail: "has a char >= U+2000 and no ASCII letters/digits", not a real emoji parse
        Some(e)
            if e.chars().count() > 16
                || !e.chars().any(|c| c >= '\u{2000}')
                || e.chars().any(|c| c.is_ascii_alphanumeric()) =>
        {
            return Err(Error::Invalid("the icon must be an emoji".into()))
        }
        Some(e) => json!({ "type": "emoji", "emoji": e }),
    };
    local_update_page(store, page_id, "icon", icon)
}

/// The host of an http(s) URL, else an error.
pub fn cover_host(url: &str) -> Result<String> {
    reqwest::Url::parse(url.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(str::to_string))
        .ok_or_else(|| Error::Invalid("the cover must be an http or https image URL".into()))
}

/// Sets (`Some`, an external http(s) URL) or removes the page's cover.
pub fn local_set_cover(store: &Store, page_id: &str, url: Option<&str>) -> Result<i64> {
    let cover = match url {
        None => Value::Null,
        Some(u) => {
            cover_host(u)?;
            json!({ "type": "external", "external": { "url": u.trim() } })
        }
    };
    local_update_page(store, page_id, "cover", cover)
}

/// Locks or unlocks the page in Notion's UI (the API ignores the lock).
pub fn local_set_locked(store: &Store, page_id: &str, locked: bool) -> Result<i64> {
    if page_id.starts_with("local-") {
        return Err(Error::Invalid(
            "this page isn't pushed yet: push it (Ctrl+S) first".into(),
        ));
    }
    local_update_page(store, page_id, "is_locked", json!(locked))
}

/// Trashes or restores a page. An unpushed page can't be (undo removes it).
pub fn local_trash_page(store: &Store, page_id: &str, trashed: bool) -> Result<i64> {
    if page_id.starts_with("local-") {
        return Err(Error::Invalid(
            "this page isn't pushed yet: push it (Ctrl+S) first".into(),
        ));
    }
    // restoring under a trashed parent would leave it invisible
    let mut up = store.get_page(page_id)?.and_then(|p| match p.parent {
        Parent::Page(id) => Some(id),
        _ => None,
    });
    for _ in 0..64 {
        let Some(parent) = up.take().and_then(|id| store.get_page(&id).ok().flatten()) else {
            break;
        };
        if !trashed && parent.in_trash {
            return Err(Error::Invalid("restore the parent page first".into()));
        }
        up = match parent.parent {
            Parent::Page(id) => Some(id),
            _ => None,
        };
    }
    local_update_page(store, page_id, "in_trash", json!(trashed))
}

/// Moves a cached page under the cached page `new_parent` and queues the
/// move. Refused into itself, its own descendants, a trashed page, or when
/// either page isn't pushed yet.
pub fn local_move_page(store: &Store, page_id: &str, new_parent: &str) -> Result<i64> {
    if page_id.starts_with("local-") || new_parent.starts_with("local-") {
        return Err(Error::Invalid(
            "an unpushed page can't be moved: push it (Ctrl+S) first".into(),
        ));
    }
    let base = store
        .get_page(page_id)?
        .ok_or_else(|| Error::Invalid(format!("page {page_id} not cached")))?;
    match store.get_page(new_parent)? {
        Some(p) if !p.in_trash => {}
        _ => return Err(Error::Invalid("the new parent isn't a cached page".into())),
    }
    let tree = store.page_tree()?;
    let mut up = Some(new_parent.to_string());
    for _ in 0..=tree.len() {
        let Some(at) = up else { break };
        if at == page_id {
            return Err(Error::Invalid(
                "a page can't move into itself or its own subpages".into(),
            ));
        }
        up = tree
            .iter()
            .find(|e| e.id == at)
            .and_then(|e| e.parent.clone());
    }
    if base.parent == Parent::Page(new_parent.to_string()) {
        return Err(Error::Invalid("it is already there".into()));
    }
    let mut page = base.clone();
    page.raw["parent"] = json!({ "type": "page_id", "page_id": new_parent });
    page.parent = Parent::Page(new_parent.to_string());
    store.atomic(|| {
        store.upsert_page(&page)?;
        // its row in the old parent goes with it
        if store.get_block(page_id)?.is_some() {
            store.move_block_local(page_id, new_parent, None)?;
        }
        store.enqueue(
            OpKind::MovePage {
                page_id: page_id.to_string(),
                new_parent_id: new_parent.to_string(),
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
        OpKind::CreateComment {
            parent: CommentTarget::Block(b),
            ..
        } => Some(b),
        OpKind::UpdatePageProperties { .. }
        | OpKind::CreatePage { .. }
        | OpKind::UpdatePage { .. }
        | OpKind::MovePage { .. }
        | OpKind::CreateComment { .. }
        | OpKind::UpdateComment { .. }
        | OpKind::DeleteComment { .. } => None,
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
        // the block each op is about; a reply's is its thread's parent
        let mut targets = Vec::new();
        for op in &ops {
            targets.push(
                match &op.kind {
                    OpKind::CreateComment {
                        parent: CommentTarget::Discussion(d),
                        ..
                    } => store.discussion_parent(d)?,
                    k => op_block(k).map(str::to_string),
                }
                .map(|t| store.resolve_id(&t))
                .transpose()?,
            );
        }
        let mut drop: Vec<bool> = targets
            .iter()
            .map(|t| t.as_ref().is_some_and(|t| subtree.contains(t)))
            .collect();
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
        for ((op, d), t) in ops.iter().zip(&drop).zip(targets) {
            let t = match t {
                Some(t) => t,
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
        refuse_if_commented(store, block_id, "moving")?;
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
    local_create_json(store, parent_id, after, kind.to_request_json()?) // rejects unwritable kinds up front
}

/// [`local_create_block`] for a request-shaped block `v` (`{type, <type>: …}`).
pub(crate) fn local_create_json(
    store: &Store,
    parent_id: &str,
    after: Option<&str>,
    mut v: Value,
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

/// Copies a cached block and its subtree as new `local-*` blocks, right
/// after `after` (default: the block itself), and queues the creates.
/// Returns the copy's id. Like a move's copy it is rebuilt from the modelled
/// fields only, so a callout's custom icon is lost; a subtree with a block
/// core can't write (child page, unsupported type) is refused and nothing is
/// queued. Comments stay with the original.
pub fn local_duplicate_block(store: &Store, block_id: &str, after: Option<&str>) -> Result<String> {
    fn copy(store: &Store, id: &str, parent: &str, after: Option<&str>) -> Result<String> {
        let block = cached(store, id)?;
        let kids = store.children(id)?;
        if block.has_children && kids.is_empty() {
            return Err(Error::Invalid(
                "its children are not cached; pull the page first".into(),
            ));
        }
        let new = local_create_block(store, parent, after, block.kind)?;
        let mut prev: Option<String> = None;
        for c in kids {
            prev = Some(copy(store, &c.id, &new, prev.as_deref())?);
        }
        Ok(new)
    }
    store.atomic(|| {
        let block = cached(store, block_id)?;
        let parent = block
            .parent
            .id()
            .ok_or_else(|| Error::Invalid(format!("block {block_id} has no parent id")))?;
        copy(store, block_id, parent, Some(after.unwrap_or(block_id)))
    })
}

/// Enter at char `at` of a text block whose Markdown is `md`: the text before
/// the cut stays (queued as an edit if it changed), the text after goes to a
/// new block right after it, empty if `at` is the end. The new block continues
/// a list (bulleted, numbered, to-do) and is a paragraph otherwise. Marks
/// that span the cut stay on both halves; a cut inside a token (mention,
/// equation, link URL, mark delimiter) leaves the text whole and adds an
/// empty block. The block's children stay with it, the colour is copied.
/// Returns the new block's id.
pub fn local_split_block(store: &Store, block_id: &str, md: &str, at: usize) -> Result<String> {
    store.atomic(|| {
        let base = cached(store, block_id)?;
        let t = base.kind.text().ok_or_else(|| {
            Error::Invalid(format!(
                "block type `{}` has no text",
                base.kind.type_name()
            ))
        })?;
        let (before, after) = split_markdown(md, at);
        if to_markdown(&before) != to_markdown(&t.rich_text) {
            local_edit_rich(store, block_id, before)?;
        }
        let kind = match base.kind {
            BlockKind::BulletedListItem(_) => TargetKind::Bulleted,
            BlockKind::NumberedListItem(_) => TargetKind::Numbered,
            BlockKind::ToDo { .. } => TargetKind::ToDo,
            _ => TargetKind::Paragraph,
        }
        .block_kind(TextBlock {
            rich_text: split_runs(after),
            color: t.color.clone(),
        });
        let parent = base
            .parent
            .id()
            .ok_or_else(|| Error::Invalid(format!("block {block_id} has no parent id")))?;
        local_create_block(store, parent, Some(block_id), kind)
    })
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
/// block is created right after it and the old one deleted (new id, so
/// backlinks are lost; a block with comments is refused). An unpushed `local-*` block has its queued create
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
        refuse_if_commented(store, block_id, "converting")?;
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

// ---------------------------------------------------------------- local comments

fn local_stamp(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Caches a new comment under a `local-*` id and queues its creation.
/// A new thread (page or block target) gets a `local-disc-*` thread id that
/// replies can already target; the push maps it to Notion's. Returns
/// (op id, local comment id).
pub fn local_add_comment(
    store: &Store,
    target: CommentTarget,
    rich: Vec<RichText>,
) -> Result<(i64, String)> {
    let rich = split_runs(rich);
    let (parent, discussion) = match &target {
        CommentTarget::Page(id) => (Parent::Page(id.clone()), local_stamp("local-disc")),
        CommentTarget::Block(id) => (Parent::Block(id.clone()), local_stamp("local-disc")),
        CommentTarget::Discussion(d) => {
            // replies hang off the thread's parent
            let p = store
                .conn
                .query_row(
                    "SELECT raw FROM comments WHERE discussion_id = ?1 LIMIT 1",
                    [d],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .map(|raw| serde_json::from_str::<Comment>(&raw))
                .transpose()?
                .ok_or_else(|| Error::Invalid(format!("thread {d} not cached")))?;
            (p.parent, d.clone())
        }
    };
    let now = ts(&Utc::now());
    let id = local_stamp("local");
    let comment: Comment = serde_json::from_value(json!({
        "object": "comment", "id": id, "discussion_id": discussion,
        "parent": serde_json::to_value(&parent)?,
        "created_time": now, "last_edited_time": now,
        "created_by": { "object": "user", "id": "local" },
        "rich_text": rich,
    }))?;
    store.atomic(|| {
        store.put_comment(&comment)?;
        let op = store.enqueue(
            OpKind::CreateComment {
                parent: target,
                rich_text: json!(rich),
                local_id: id.clone(),
                local_discussion: discussion,
            },
            None,
            None,
        )?;
        Ok((op, id))
    })
}

fn queued_comment_op(store: &Store, comment_id: &str) -> Result<Option<Op>> {
    for op in store.queued_ops()? {
        let hit = match &op.kind {
            OpKind::CreateComment { local_id, .. } => local_id == comment_id,
            OpKind::UpdateComment { comment_id: c, .. } => store.resolve_id(c)? == comment_id,
            _ => false,
        };
        if hit {
            return Ok(Some(op));
        }
    }
    Ok(None)
}

/// Replaces a cached comment's text and queues the edit (an unpushed
/// create, or an earlier queued edit of it, is rewritten instead). Only the
/// integration's own comments can be edited remotely; Notion answers 404 for
/// others and the op then fails. Returns the op id.
pub fn local_edit_comment(store: &Store, comment_id: &str, rich: Vec<RichText>) -> Result<i64> {
    let base = store
        .get_comment(comment_id)?
        .ok_or_else(|| Error::Invalid(format!("comment {comment_id} not cached")))?;
    let mut c = base.clone();
    c.rich_text = split_runs(rich);
    let rich_json = json!(c.rich_text);
    store.atomic(|| {
        store.put_comment(&c)?;
        match queued_comment_op(store, comment_id)? {
            Some(Op {
                id,
                kind:
                    OpKind::CreateComment {
                        parent,
                        local_id,
                        local_discussion,
                        ..
                    },
                ..
            }) => {
                let kind = OpKind::CreateComment {
                    parent,
                    rich_text: rich_json,
                    local_id,
                    local_discussion,
                };
                store.update_op_kind(id, &kind)?;
                Ok(id)
            }
            Some(Op {
                id,
                kind: OpKind::UpdateComment { comment_id, .. },
                ..
            }) => {
                let kind = OpKind::UpdateComment {
                    comment_id,
                    rich_text: rich_json,
                };
                store.update_op_kind(id, &kind)?;
                Ok(id)
            }
            _ => store.enqueue(
                OpKind::UpdateComment {
                    comment_id: comment_id.to_string(),
                    rich_text: rich_json,
                },
                Some(base.last_edited_time),
                Some(serde_json::to_value(&base)?),
            ),
        }
    })
}

/// Removes a cached comment and queues the delete. An unpushed `local-*`
/// comment is just dropped with its queued ops (and, if it started a
/// thread, the thread's unpushed replies); returns None then.
pub fn local_delete_comment(store: &Store, comment_id: &str) -> Result<Option<i64>> {
    store.atomic(|| {
        let base = store
            .get_comment(comment_id)?
            .ok_or_else(|| Error::Invalid(format!("comment {comment_id} not cached")))?;
        if comment_id.starts_with("local-") {
            // the comment itself, plus every local comment in a local thread it started
            let mut gone = vec![comment_id.to_string()];
            let starts_thread = matches!(
                queued_comment_op(store, comment_id)?,
                Some(Op { kind: OpKind::CreateComment { parent, .. }, .. })
                    if !matches!(parent, CommentTarget::Discussion(_))
            );
            if starts_thread {
                gone = store
                    .conn
                    .prepare(
                        "SELECT id FROM comments WHERE discussion_id = ?1 AND id LIKE 'local-%'",
                    )?
                    .query_map([&base.discussion_id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
            }
            for op in store.queued_ops()? {
                let hit = match &op.kind {
                    OpKind::CreateComment { local_id, .. } => gone.contains(local_id),
                    OpKind::UpdateComment { comment_id: c, .. } => gone.contains(c),
                    _ => false,
                };
                if hit {
                    store.complete_op(op.id)?;
                }
            }
            for id in &gone {
                store.delete_comment(id)?;
            }
            return Ok(None);
        }
        // a queued edit of it is moot; keep the first base
        let (time, raw) = match queued_comment_op(store, comment_id)? {
            Some(op) => {
                store.complete_op(op.id)?;
                (op.base_last_edited_time, op.base_raw)
            }
            None => (None, None),
        };
        store.delete_comment(comment_id)?;
        store
            .enqueue(
                OpKind::DeleteComment {
                    comment_id: comment_id.to_string(),
                },
                time.or(Some(base.last_edited_time)),
                raw.or(Some(serde_json::to_value(&base)?)),
            )
            .map(Some)
    })
}

// ---------------------------------------------------------------- undo

/// All columns of a `pages`, `blocks`, `comments` or `ops` row, in table order.
type SqlRow = Vec<rusqlite::types::Value>;
/// (table, id as text) → row.
type Rows = HashMap<(&'static str, String), SqlRow>;

/// The cached rows of some pages (the page row, its blocks and their comments) plus the whole
/// op table, taken before a local edit. [`Checkpoint::changes`] keeps only the
/// rows the edit touched.
pub struct Checkpoint {
    pages: Vec<String>,
    rows: Rows,
}

/// One local edit as the rows it changed, before and after. Undo with
/// [`restore`]; [`Change::inverse`] gives the redo.
#[derive(Debug, Clone, PartialEq)]
pub struct Change(Vec<RowChange>);

#[derive(Debug, Clone, PartialEq)]
struct RowChange {
    table: &'static str,
    id: rusqlite::types::Value,
    before: Option<SqlRow>,
    after: Option<SqlRow>,
}

fn key_text(v: &rusqlite::types::Value) -> String {
    match v {
        rusqlite::types::Value::Text(s) => s.clone(),
        v => format!("{v:?}"),
    }
}

fn snapshot(store: &Store, pages: &[String]) -> Result<Rows> {
    let pages = serde_json::to_string(pages)?;
    let mut rows = Rows::new();
    // ponytail: the whole op table each time; fine while the queue is small
    for (table, sql, arg) in [
        (
            "pages",
            // a new page isn't named before it exists: every unpushed one is in
            "SELECT * FROM pages WHERE id IN (SELECT value FROM json_each(?1)) OR id LIKE 'local-%'",
            true,
        ),
        (
            "blocks",
            // a page's own child_page row (same id) lives in its parent
            "SELECT * FROM blocks WHERE page_id IN (SELECT value FROM json_each(?1))
               OR id IN (SELECT value FROM json_each(?1))",
            true,
        ),
        (
            "comments",
            "SELECT * FROM comments WHERE parent_id IN (SELECT value FROM json_each(?1))
               OR parent_id IN (SELECT id FROM blocks WHERE page_id IN (SELECT value FROM json_each(?1)))",
            true,
        ),
        ("ops", "SELECT * FROM ops", false),
    ] {
        let mut st = store.conn.prepare(sql)?;
        let n = st.column_count();
        let mut q = if arg {
            st.query([&pages])?
        } else {
            st.query([])?
        };
        while let Some(r) = q.next()? {
            let row = (0..n)
                .map(|i| r.get(i))
                .collect::<rusqlite::Result<SqlRow>>()?;
            rows.insert((table, key_text(&row[0])), row);
        }
    }
    Ok(rows)
}

/// Snapshots `pages` (their page rows and cached blocks) and the op queue.
pub fn checkpoint(store: &Store, pages: &[&str]) -> Result<Checkpoint> {
    let pages: Vec<String> = pages.iter().map(|p| p.to_string()).collect();
    Ok(Checkpoint {
        rows: snapshot(store, &pages)?,
        pages,
    })
}

impl Checkpoint {
    /// What changed since the checkpoint; None if nothing did.
    pub fn changes(&self, store: &Store) -> Result<Option<Change>> {
        let mut now = snapshot(store, &self.pages)?;
        let mut out = Vec::new();
        for (k, before) in &self.rows {
            let after = now.remove(k);
            if after.as_ref() != Some(before) {
                out.push(RowChange {
                    table: k.0,
                    id: before[0].clone(),
                    before: Some(before.clone()),
                    after,
                });
            }
        }
        for ((table, _), after) in now {
            out.push(RowChange {
                table,
                id: after[0].clone(),
                before: None,
                after: Some(after),
            });
        }
        Ok((!out.is_empty()).then_some(Change(out)))
    }
}

impl Change {
    /// The same change the other way round.
    pub fn inverse(&self) -> Change {
        Change(
            self.0
                .iter()
                .map(|c| RowChange {
                    before: c.after.clone(),
                    after: c.before.clone(),
                    ..c.clone()
                })
                .collect(),
        )
    }
}

/// Puts the changed rows back to before `c`, verbatim (ops keep their ids).
/// Returns false, changing nothing, unless every one of them is still exactly
/// as `c` left it: refuses once an op was completed or marked (conflict,
/// failed), or a row was rewritten since. Row checks can't see another
/// process that is mid-push: callers pause undo while commands are in flight.
pub fn restore(store: &Store, c: &Change) -> Result<bool> {
    store.atomic(|| {
        for r in &c.0 {
            let now: Option<SqlRow> = {
                let mut st = store
                    .conn
                    .prepare(&format!("SELECT * FROM {} WHERE id = ?1", r.table))?;
                let n = st.column_count();
                st.query_row([&r.id], |row| (0..n).map(|i| row.get(i)).collect())
                    .optional()?
            };
            if now != r.after {
                return Ok(false);
            }
        }
        for r in &c.0 {
            store
                .conn
                .execute(&format!("DELETE FROM {} WHERE id = ?1", r.table), [&r.id])?;
        }
        for r in &c.0 {
            if let Some(row) = &r.before {
                let marks = vec!["?"; row.len()].join(",");
                store.conn.execute(
                    &format!("INSERT INTO {} VALUES ({marks})", r.table),
                    rusqlite::params_from_iter(row),
                )?;
            }
        }
        Ok(true)
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

    /// Mounts the page at `time`; the block fetch must happen `n` times.
    async fn serve_page(s: &MockServer, time: &str, text: &str, n: u64) {
        s.reset().await;
        get(s, "/v1/pages/p", ok(page("p", pp("root"), time))).await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![blk("a", pp("p"), text, time, false)]))
            .expect(n)
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn poll_trusts_an_unchanged_time_only_when_it_is_old() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        // old: one request (the page), no block fetch
        serve_page(&s, T, "A", 1).await;
        pull_page(&c, &store, "p", false).await.unwrap();
        serve_page(&s, T, "A", 0).await;
        let r = pull_page_skip(&c, &store, "p", poll_settle())
            .await
            .unwrap();
        assert!(r.skipped);
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
        s.verify().await;

        // recent: the same minute may hide an edit, so it is pulled in full...
        let recent = (Utc::now() - chrono::Duration::seconds(20))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let store = Store::open_in_memory().unwrap();
        serve_page(&s, &recent, "A", 1).await;
        pull_page(&c, &store, "p", false).await.unwrap();
        serve_page(&s, &recent, "B", 1).await;
        let r = pull_page_skip(&c, &store, "p", poll_settle())
            .await
            .unwrap();
        assert!(!r.skipped);
        assert_eq!(text(&store, "a"), "B");
        s.verify().await;
        // ...unless the caller already did that lately (`IfUnchanged`)
        serve_page(&s, &recent, "C", 0).await;
        let r = pull_page_skip(&c, &store, "p", Skip::IfUnchanged)
            .await
            .unwrap();
        assert!(r.skipped);
        s.verify().await;
    }

    /// A poll while the user edits a block that changed remotely: the queued
    /// edit keeps its cached version and its old base, so the push says conflict.
    #[tokio::test]
    async fn poll_keeps_a_queued_edit_and_the_push_finds_the_conflict() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "mine").unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "theirs", T2, false)]),
        )
        .await;
        get(
            &s,
            "/v1/blocks/b",
            ok(blk("b", pp("p"), "theirs", T2, false)),
        )
        .await;
        let r = pull_page_skip(&c, &store, "p", poll_settle())
            .await
            .unwrap();
        assert_eq!((r.skipped, r.updated), (false, 1));
        assert_eq!(text(&store, "b"), "mine");
        let r = push(&c, &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts), (0, 1));
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
        seed(&s, &store, &["b", "c"]).await;
        local_edit_text(&store, "b", "one").unwrap();
        // another op in between: two ops for b, not merged
        local_edit_text(&store, "c", "x").unwrap();
        local_edit_text(&store, "b", "two").unwrap();
        assert_eq!(queue(&store).len(), 3);
        get(&s, "/v1/blocks/c", ok(blk("c", pp("p"), "c", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/c"))
            .respond_with(ok(blk("c", pp("p"), "x", T2, false)))
            .mount(&s)
            .await;
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
                applied: 2,
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

    #[test]
    fn merged_bodies_keep_both_halves_of_a_payload() {
        let m = merge_bodies(
            json!({ "to_do": { "checked": true } }),
            json!({ "to_do": { "rich_text": [] }, "color": "red" }),
        );
        assert_eq!(
            m,
            json!({ "to_do": { "checked": true, "rich_text": [] }, "color": "red" })
        );
    }

    #[tokio::test]
    async fn two_edits_of_same_block_are_one_patch_of_the_last_text() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_edit_text(&store, "b", "one").unwrap();
        local_edit_text(&store, "b", "two").unwrap();
        assert_eq!(queue(&store).len(), 1);
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "b", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .and(body_string_contains("two"))
            .respond_with(ok(blk("b", pp("p"), "two", T2, false)))
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

    /// Mounts a workspace of pages p1, p2 and data source ds (rows r1, r2);
    /// `p2_status` is what p2's blocks answer.
    async fn workspace(s: &MockServer, p2_status: u16) {
        let ds = json!({ "object": "data_source", "id": "ds", "title": [], "last_edited_time": T,
            "parent": { "type": "database_id", "database_id": "db" }, "properties": {} });
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![
                titled("p1", T, "One"),
                titled("p2", T, "Two"),
                ds.clone(),
            ]))
            .mount(s)
            .await;
        get(s, "/v1/data_sources/ds", ok(ds)).await;
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
            .mount(s)
            .await;
        for id in ["p1", "p2"] {
            get(s, &format!("/v1/pages/{id}"), ok(titled(id, T, id))).await;
            let r = if id == "p2" && p2_status != 200 {
                ResponseTemplate::new(p2_status).set_body_json(json!({ "object": "error", "status": p2_status, "code": "x", "message": "boom" }))
            } else {
                list(vec![blk(&format!("{id}b"), pp(id), "x", T, false)])
            };
            get(s, &format!("/v1/blocks/{id}/children"), r).await;
        }
    }

    #[tokio::test]
    async fn pull_all_counts_skips_and_survives_an_error() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        workspace(&s, 500).await;
        let mut seen = Vec::new();
        let r = pull_all(&c, &store, |d, t, title| {
            seen.push((d, t, title.to_string()))
        })
        .await
        .unwrap();
        assert_eq!(
            (r.pages, r.data_sources, r.skipped, r.errors.len()),
            (1, 1, 0, 1),
            "{r:?}"
        );
        assert!(r.errors[0].starts_with("Two: ") && r.errors[0].contains("boom"));
        assert_eq!(
            seen.iter().map(|(d, t, _)| (*d, *t)).collect::<Vec<_>>(),
            [(1, 3), (2, 3), (3, 3)]
        );
        assert_eq!(seen[0].2, "One");
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        assert!(store
            .page_tree()
            .unwrap()
            .iter()
            .any(|e| e.id == "ds" && e.database));

        // p2 heals; p1 is skipped without a request (its blocks are gone)
        s.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![titled("p1", T, "One"), titled("p2", T, "Two")]))
            .mount(&s)
            .await;
        get(
            &s,
            "/v1/blocks/p2/children",
            list(vec![blk("p2b", pp("p2"), "x", T, false)]),
        )
        .await;
        let r = pull_all(&c, &store, |_, _, _| {}).await.unwrap();
        assert_eq!(
            r,
            SyncAllReport {
                pages: 1,
                skipped: 1,
                ..Default::default()
            }
        );
        // and a third run touches nothing but the search
        s.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![titled("p1", T, "One"), titled("p2", T, "Two")]))
            .mount(&s)
            .await;
        let r = pull_all(&c, &store, |_, _, _| {}).await.unwrap();
        assert_eq!(
            r,
            SyncAllReport {
                skipped: 2,
                ..Default::default()
            }
        );
    }

    /// Notion's times have minute precision: an unchanged but recent time may hide an edit.
    #[tokio::test]
    async fn pull_all_refetches_pages_edited_in_the_last_minutes() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let recent = (Utc::now() - chrono::Duration::seconds(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![titled("p1", &recent, "One")]))
            .mount(&s)
            .await;
        get(
            &s,
            "/v1/blocks/p1/children",
            list(vec![blk("a", pp("p1"), "old", &recent, false)]),
        )
        .await;
        let c = client(&s);
        let r = pull_all(&c, &store, |_, _, _| {}).await.unwrap();
        assert_eq!((r.pages, r.skipped), (1, 0));
        s.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![titled("p1", &recent, "One")]))
            .mount(&s)
            .await;
        get(
            &s,
            "/v1/blocks/p1/children",
            list(vec![blk("a", pp("p1"), "new", &recent, false)]),
        )
        .await;
        let r = pull_all(&c, &store, |_, _, _| {}).await.unwrap();
        assert_eq!(
            (r.pages, r.skipped),
            (1, 0),
            "same recent time: pulled again"
        );
        assert_eq!(text(&store, "a"), "new");
    }

    /// A data source listed before its row, and the row changed since it was cached.
    #[tokio::test]
    async fn pull_all_updates_the_blocks_of_a_changed_row() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        let dsp = json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" });
        let ds = json!({ "object": "data_source", "id": "ds", "title": [], "last_edited_time": T,
            "parent": { "type": "database_id", "database_id": "db" }, "properties": {} });
        // the row, cached at T with an old body
        get(&s, "/v1/pages/r", ok(page("r", dsp.clone(), T))).await;
        get(
            &s,
            "/v1/blocks/r/children",
            list(vec![blk("rb", pp("r"), "old", T, false)]),
        )
        .await;
        pull_page(&c, &store, "r", true).await.unwrap();
        s.reset().await;
        // now changed: the data source comes first in the search
        let row = page("r", dsp, T2);
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![ds.clone(), row.clone()]))
            .mount(&s)
            .await;
        get(&s, "/v1/data_sources/ds", ok(ds)).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row]))
            .mount(&s)
            .await;
        get(
            &s,
            "/v1/blocks/r/children",
            list(vec![blk("rb", pp("r"), "new", T2, false)]),
        )
        .await;
        let r = pull_all(&c, &store, |_, _, _| {}).await.unwrap();
        assert_eq!(
            (r.pages, r.data_sources, r.errors.len()),
            (1, 1, 0),
            "{r:?}"
        );
        assert_eq!(text(&store, "rb"), "new");
        // and a row only the data source reports keeps its old time until its blocks are pulled
        s.reset().await;
        let row3 = page(
            "r",
            json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" }),
            "2025-01-03T00:00:00.000Z",
        );
        get(
            &s,
            "/v1/data_sources/ds",
            ok(
                json!({ "object": "data_source", "id": "ds", "title": [], "last_edited_time": T,
            "parent": { "type": "database_id", "database_id": "db" }, "properties": {} }),
            ),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row3]))
            .mount(&s)
            .await;
        pull_data_source(&c, &store, "ds").await.unwrap();
        assert_eq!(
            store
                .get_page("r")
                .unwrap()
                .unwrap()
                .last_edited_time
                .to_rfc3339(),
            "2025-01-02T00:00:00+00:00"
        );
    }

    #[tokio::test]
    async fn pull_all_aborts_on_a_bad_token() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        // mounted first, so it wins over the workspace's p1
        get(&s, "/v1/blocks/p1/children", ResponseTemplate::new(401).set_body_json(
            json!({ "object": "error", "status": 401, "code": "unauthorized", "message": "bad token" }))).await;
        workspace(&s, 200).await;
        let e = pull_all(&client(&s), &store, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Api { status: 401, .. }), "{e:?}");
        assert!(store.get_page("p2").unwrap().is_none(), "stopped at p1");
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
    async fn local_set_image_width_caches_and_queues() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let mut img = blk("i", pp("p"), "", T, false);
        img["type"] = json!("image");
        img["image"] = json!({ "type": "external", "external": { "url": "u" }, "caption": [] });
        store
            .put_block(&serde_json::from_value(img).unwrap())
            .unwrap();
        local_set_image_width(&store, "i", 320.0).unwrap();
        let b = store.get_block("i").unwrap().unwrap();
        assert_eq!(crate::model::image_width(&b.raw), Some(320.0));
        let q = store.queued_ops().unwrap();
        let OpKind::UpdateBlock { body, .. } = &q[0].kind else {
            panic!()
        };
        assert_eq!(*body, json!({ "image": { "_width": 320.0 } }));
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

        // the same row again with nothing queued since: one op carrying both edits
        local_edit_table_cell(&store, "r1", 0, vec![RichText::plain("A")]).unwrap();
        let q = queue(&store);
        assert_eq!(q.len(), 1);
        let OpKind::UpdateBlock { body, .. } = &q[0] else {
            panic!()
        };
        assert_eq!(body["table_row"]["cells"][0][0]["plain_text"], "A");
        assert_eq!(body["table_row"]["cells"][1][0]["plain_text"], "B");

        assert!(local_edit_table_cell(&store, "r1", 2, vec![]).is_err());
        assert!(local_edit_table_cell(&store, "tbl", 0, vec![]).is_err());
        assert_eq!(queue(&store).len(), 1);
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

    // ------------------------------------------------------------ undo

    /// Page "p" with paragraphs a, b, c; a has a child a1.
    fn undo_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_page(&serde_json::from_value(page("p", pp("root"), T)).unwrap())
            .unwrap();
        let b = |id: &str, parent: Value, kids: bool| -> Block {
            serde_json::from_value(blk(id, parent, id, T, kids)).unwrap()
        };
        store
            .replace_children(
                "p",
                "p",
                &[
                    b("a", pp("p"), true),
                    b("b", pp("p"), false),
                    b("c", pp("p"), false),
                ],
            )
            .unwrap();
        store
            .replace_children("p", "a", &[b("a1", bp("a"), false)])
            .unwrap();
        store
    }

    fn snap(store: &Store) -> Rows {
        snapshot(store, &["p".to_string()]).unwrap()
    }

    /// Runs `edit` under a checkpoint; returns its change.
    fn record(store: &Store, edit: impl FnOnce(&Store)) -> Change {
        let cp = checkpoint(store, &["p"]).unwrap();
        edit(store);
        cp.changes(store).unwrap().expect("something changed")
    }

    #[test]
    fn restore_reverts_an_edit_and_redo_reapplies_it() {
        let store = undo_store();
        let before = snap(&store);
        let c = record(&store, |s| {
            local_edit_text(s, "a", "x").unwrap();
        });
        let after = snap(&store);
        assert!(restore(&store, &c).unwrap());
        assert_eq!(text(&store, "a"), "a");
        assert!(queue(&store).is_empty(), "the op is gone");
        assert_eq!(snap(&store), before);
        // redo
        assert!(restore(&store, &c.inverse()).unwrap());
        assert_eq!(snap(&store), after);
        assert_eq!(text(&store, "a"), "x");
    }

    #[test]
    fn restore_reverts_a_merged_op_body() {
        let store = undo_store();
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: Vec::new(),
            color: "default".into(),
        });
        let l = local_create_block(&store, "p", None, para).unwrap();
        local_edit_text(&store, &l, "x").unwrap();
        let before = snap(&store);
        let c = record(&store, |s| {
            local_edit_text(s, &l, "y").unwrap();
        });
        assert_eq!(queue(&store).len(), 1, "merged into the create");
        assert!(restore(&store, &c).unwrap());
        assert_eq!(snap(&store), before);
        assert_eq!(text(&store, &l), "x");
        let OpKind::CreateBlock { block, .. } = &queue(&store)[0] else {
            panic!()
        };
        assert!(block.to_string().contains("\"x\""), "{block}");
    }

    #[test]
    fn undoing_merged_edits_of_one_block_steps_back_one_edit_at_a_time() {
        let store = undo_store();
        let before = snap(&store);
        let c1 = record(&store, |s| {
            local_edit_text(s, "b", "one").unwrap();
        });
        let mid = snap(&store);
        let c2 = record(&store, |s| {
            local_edit_text(s, "b", "two").unwrap();
        });
        assert_eq!(queue(&store).len(), 1, "merged");
        assert!(restore(&store, &c2).unwrap());
        assert_eq!(snap(&store), mid);
        assert_eq!(text(&store, "b"), "one");
        let q = queue(&store);
        assert_eq!(q.len(), 1);
        let OpKind::UpdateBlock { body, .. } = &q[0] else {
            panic!()
        };
        assert!(body.to_string().contains("one"), "{body}");
        assert!(restore(&store, &c1).unwrap());
        assert_eq!(snap(&store), before);
        assert!(queue(&store).is_empty());
    }

    #[test]
    fn edits_of_an_unpushed_block_fold_into_its_create_not_an_update() {
        let store = undo_store();
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: Vec::new(),
            color: "default".into(),
        });
        let l = local_create_block(&store, "p", None, para).unwrap();
        local_edit_text(&store, &l, "x").unwrap();
        local_edit_text(&store, &l, "y").unwrap();
        let q = queue(&store);
        assert_eq!(q.len(), 1);
        let OpKind::CreateBlock { block, .. } = &q[0] else {
            panic!("not a create: {:?}", q[0])
        };
        assert!(block.to_string().contains("\"y\""), "{block}");
    }

    #[test]
    fn restore_brings_back_a_deleted_subtree() {
        let store = undo_store();
        let before = snap(&store);
        let c = record(&store, |s| {
            local_delete_block(s, "a").unwrap();
        });
        assert!(store.get_block("a1").unwrap().is_none());
        assert!(restore(&store, &c).unwrap());
        assert_eq!(snap(&store), before);
        assert_eq!(ids(&store.children("a").unwrap()), ["a1"]);
        assert!(queue(&store).is_empty(), "no DeleteBlock");
    }

    #[test]
    fn restore_drops_a_local_create() {
        let store = undo_store();
        let before = snap(&store);
        let mut new = String::new();
        let c = record(&store, |s| {
            let para = BlockKind::Paragraph(TextBlock {
                rich_text: Vec::new(),
                color: "default".into(),
            });
            new = local_create_block(s, "p", Some("a"), para).unwrap();
        });
        assert!(restore(&store, &c).unwrap());
        assert!(store.get_block(&new).unwrap().is_none());
        assert!(queue(&store).is_empty());
        assert_eq!(snap(&store), before);
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "b", "c"]);
    }

    #[test]
    fn restore_reverts_a_move() {
        let store = undo_store();
        let before = snap(&store);
        // indent c under b
        let c = record(&store, |s| {
            local_move_block(s, "c", "b", None).unwrap();
        });
        assert_eq!(ids(&store.children("b").unwrap()), ["c"]);
        assert!(restore(&store, &c).unwrap());
        assert_eq!(snap(&store), before);
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "b", "c"]);
        assert!(!store.get_block("b").unwrap().unwrap().has_children);
        assert!(queue(&store).is_empty());
    }

    #[test]
    fn restore_refuses_once_the_op_left_the_queue() {
        for leave in ["pushed", "conflict", "pulled"] {
            let store = undo_store();
            let c = record(&store, |s| {
                local_edit_text(s, "a", "x").unwrap();
            });
            let op = store.queued_ops().unwrap()[0].id;
            match leave {
                "pushed" => store.complete_op(op).unwrap(),
                "conflict" => store.mark_op(op, OpStatus::Conflict, Some("x")).unwrap(),
                // the row itself changed under us
                _ => {
                    let mut b = store.get_block("a").unwrap().unwrap();
                    b.has_children = false;
                    store.put_block(&b).unwrap();
                }
            }
            let now = snap(&store);
            assert!(!restore(&store, &c).unwrap(), "{leave}");
            assert_eq!(snap(&store), now, "{leave}: nothing written");
        }
    }

    // ------------------------------------------------------------ comments

    fn cm(id: &str, disc: &str, parent: Value, text: &str, time: &str) -> Value {
        json!({
            "object": "comment", "id": id, "discussion_id": disc, "parent": parent,
            "created_time": T, "last_edited_time": time,
            "created_by": { "object": "user", "id": "bot" },
            "rich_text": [{ "type": "text", "text": { "content": text },
                "plain_text": text, "annotations": {} }],
        })
    }

    fn rt(t: &str) -> Vec<RichText> {
        vec![RichText::plain(t)]
    }

    fn comment_text(store: &Store, id: &str) -> String {
        plain_text(&store.get_comment(id).unwrap().unwrap().rich_text)
    }

    async fn post_comment(s: &MockServer, partial_body: Value, resp: Value) {
        Mock::given(method("POST"))
            .and(path("/v1/comments"))
            .and(body_partial_json(partial_body))
            .respond_with(ok(resp))
            .expect(1)
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn offline_comment_is_pushed_and_mapped() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let (op, local) =
            local_add_comment(&store, CommentTarget::Page("p".into()), rt("hi")).unwrap();
        assert!(local.starts_with("local-"));
        assert_eq!(comment_text(&store, &local), "hi");
        assert_eq!(store.comments_for("p").unwrap().len(), 1);
        // a reply to the still-local thread, queued behind it
        let disc = store.get_comment(&local).unwrap().unwrap().discussion_id;
        let (_, reply) =
            local_add_comment(&store, CommentTarget::Discussion(disc), rt("re")).unwrap();
        assert_eq!(store.queued_ops().unwrap()[0].id, op);

        post_comment(
            &s,
            json!({ "parent": { "type": "page_id", "page_id": "p" },
                    "rich_text": [{ "text": { "content": "hi" } }] }),
            cm("c1", "d1", pp("p"), "hi", T2),
        )
        .await;
        post_comment(
            &s,
            json!({ "discussion_id": "d1", "rich_text": [{ "text": { "content": "re" } }] }),
            cm("c2", "d1", pp("p"), "re", T2),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (2, 0), "{r:?}");
        assert_eq!(store.resolve_id(&local).unwrap(), "c1");
        assert_eq!(store.resolve_id(&reply).unwrap(), "c2");
        assert!(store.get_comment(&local).unwrap().is_none());
        let cs = store.comments_for("p").unwrap();
        assert_eq!(
            cs.iter()
                .map(|c| (c.id.as_str(), c.discussion_id.as_str()))
                .collect::<Vec<_>>(),
            [("c1", "d1"), ("c2", "d1")]
        );
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn comment_on_local_block_waits_for_its_create() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let l = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        local_add_comment(&store, CommentTarget::Block(l.clone()), rt("on new")).unwrap();
        assert_eq!(store.comments_for(&l).unwrap().len(), 1);

        let mut made = blk("real", pp("p"), "", T2, false);
        made["type"] = json!("divider");
        made["divider"] = json!({});
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(list(vec![made]))
            .expect(1)
            .mount(&s)
            .await;
        post_comment(
            &s,
            json!({ "parent": { "type": "block_id", "block_id": "real" } }),
            cm("c1", "d1", bp("real"), "on new", T2),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (2, 0), "{r:?}");
        assert!(store.comments_for(&l).unwrap().is_empty());
        assert_eq!(store.comments_for("real").unwrap()[0].id, "c1");
    }

    #[tokio::test]
    async fn comment_on_a_block_whose_create_failed_fails_without_a_request() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let l = local_create_block(&store, "p", Some("a"), BlockKind::Divider).unwrap();
        local_add_comment(&store, CommentTarget::Block(l), rt("x")).unwrap();
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"code": "validation_error", "message": "no"})),
            )
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (0, 2), "{r:?}");
    }

    #[tokio::test]
    async fn push_edit_and_delete_comment_with_conflict_check() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        store
            .put_comment(&serde_json::from_value(cm("c1", "d1", bp("a"), "old", T)).unwrap())
            .unwrap();
        store
            .put_comment(&serde_json::from_value(cm("c2", "d2", bp("a"), "gone", T)).unwrap())
            .unwrap();
        local_edit_comment(&store, "c1", rt("new")).unwrap();
        local_edit_comment(&store, "c1", rt("newer")).unwrap(); // merged into the queued op
        local_delete_comment(&store, "c2").unwrap().unwrap();
        assert_eq!(store.queued_ops().unwrap().len(), 2);
        assert_eq!(comment_text(&store, "c1"), "newer");
        assert!(store.get_comment("c2").unwrap().is_none());

        get(&s, "/v1/comments/c1", ok(cm("c1", "d1", bp("a"), "old", T))).await;
        get(
            &s,
            "/v1/comments/c2",
            ok(cm("c2", "d2", bp("a"), "gone", T)),
        )
        .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/comments/c1"))
            .and(body_partial_json(
                json!({ "rich_text": [{ "text": { "content": "newer" } }] }),
            ))
            .respond_with(ok(cm("c1", "d1", bp("a"), "newer", T2)))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/comments/c2"))
            .respond_with(ok(json!({ "object": "comment", "id": "c2" })))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (2, 0, 0), "{r:?}");
        assert_eq!(comment_text(&store, "c1"), "newer");
    }

    #[tokio::test]
    async fn comment_edit_conflicts_when_remote_was_edited() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        store
            .put_comment(&serde_json::from_value(cm("c1", "d1", bp("a"), "old", T)).unwrap())
            .unwrap();
        local_edit_comment(&store, "c1", rt("mine")).unwrap();
        get(
            &s,
            "/v1/comments/c1",
            ok(cm("c1", "d1", bp("a"), "theirs", T2)),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts), (0, 1), "{r:?}");
    }

    #[tokio::test]
    async fn pull_fetches_page_comments_and_keeps_those_with_queued_ops() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let put = |id: &str, t: &str| {
            store
                .put_comment(&serde_json::from_value(cm(id, "d", pp("p"), t, T)).unwrap())
                .unwrap()
        };
        put("e", "mine-edited");
        put("x", "doomed");
        put("r", "resolved remotely");
        local_edit_comment(&store, "e", rt("mine-edited")).unwrap();
        local_delete_comment(&store, "x").unwrap();
        local_add_comment(&store, CommentTarget::Page("p".into()), rt("unpushed")).unwrap();

        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("a", pp("p"), "a", T, false)]),
        )
        .await;
        get(
            &s,
            "/v1/comments",
            list(vec![
                cm("e", "d", pp("p"), "remote text", T2),
                cm("x", "d", pp("p"), "doomed", T2),
                cm("n", "d", pp("p"), "new remote", T2),
            ]),
        )
        .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
        let cs = store.comments_for("p").unwrap();
        let got: Vec<_> = cs
            .iter()
            .map(|c| (c.id.as_str(), plain_text(&c.rich_text)))
            .collect();
        assert!(got.contains(&("e", "mine-edited".to_string())), "{got:?}");
        assert!(got.contains(&("n", "new remote".to_string())), "{got:?}");
        assert!(
            got.iter()
                .any(|(id, t)| id.starts_with("local-") && t == "unpushed"),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(id, _)| *id == "x" || *id == "r"),
            "{got:?}"
        );
        assert_eq!(got.len(), 3);
    }

    #[tokio::test]
    async fn pull_comments_of_a_block_is_one_request_and_a_failure_does_not_fail_pull_page() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        Mock::given(method("GET"))
            .and(path("/v1/comments"))
            .and(wiremock::matchers::query_param("block_id", "a"))
            .respond_with(list(vec![cm("c1", "d1", bp("a"), "x", T)]))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(pull_comments(&client(&s), &store, "a").await.unwrap(), 1);
        assert_eq!(store.comment_counts("p").unwrap()["a"], 1);

        // 403 (no read-comments capability): the page pull still succeeds
        s.reset().await;
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("a", pp("p"), "a", T, false)]),
        )
        .await;
        Mock::given(path("/v1/comments"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"code": "restricted_resource", "message": "no"})),
            )
            .mount(&s)
            .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
    }

    #[test]
    fn deleting_a_local_comment_drops_its_ops_and_thread() {
        let store = Store::open_in_memory().unwrap();
        let (_, a) = local_add_comment(&store, CommentTarget::Page("p".into()), rt("a")).unwrap();
        let disc = store.get_comment(&a).unwrap().unwrap().discussion_id;
        let (_, b) =
            local_add_comment(&store, CommentTarget::Discussion(disc.clone()), rt("b")).unwrap();
        // deleting only the reply keeps the thread
        assert_eq!(local_delete_comment(&store, &b).unwrap(), None);
        assert_eq!(store.queued_ops().unwrap().len(), 1);
        let (_, b) = local_add_comment(&store, CommentTarget::Discussion(disc), rt("b")).unwrap();
        local_edit_comment(&store, &a, rt("a2")).unwrap();
        assert_eq!(comment_text(&store, &a), "a2");
        // deleting the root takes the thread's unpushed replies with it
        assert_eq!(local_delete_comment(&store, &a).unwrap(), None);
        assert!(store.get_comment(&b).unwrap().is_none());
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_block_drops_its_comments_and_comment_ops() {
        let store = undo_store();
        local_add_comment(&store, CommentTarget::Block("b".into()), rt("x")).unwrap();
        local_add_comment(&store, CommentTarget::Page("p".into()), rt("keep")).unwrap();
        local_delete_block(&store, "b").unwrap();
        assert!(store.comments_for("b").unwrap().is_empty());
        let kinds: Vec<_> = store
            .queued_ops()
            .unwrap()
            .into_iter()
            .map(|o| o.kind)
            .collect();
        assert_eq!(kinds.len(), 2, "{kinds:?}"); // page comment create + delete block
        assert!(kinds.iter().any(|k| matches!(
            k,
            OpKind::CreateComment {
                parent: CommentTarget::Page(_),
                ..
            }
        )));
    }

    #[test]
    fn comment_ops_undo_and_redo() {
        let store = undo_store();
        let before = snap(&store);
        let c = record(&store, |s| {
            local_add_comment(s, CommentTarget::Block("b".into()), rt("x")).unwrap();
        });
        assert_eq!(store.comments_for("b").unwrap().len(), 1);
        let after = snap(&store);
        assert!(restore(&store, &c).unwrap());
        assert_eq!(snap(&store), before);
        assert!(store.comments_for("b").unwrap().is_empty());
        assert!(store.queued_ops().unwrap().is_empty());
        assert!(restore(&store, &c.inverse()).unwrap());
        assert_eq!(snap(&store), after);
    }

    #[test]
    fn renaming_a_block_moves_its_comments() {
        let store = undo_store();
        let l = local_create_block(&store, "p", None, BlockKind::Divider).unwrap();
        local_add_comment(&store, CommentTarget::Block(l.clone()), rt("x")).unwrap();
        store.rename_block(&l, "real").unwrap();
        assert_eq!(store.comments_for("real").unwrap().len(), 1);
        assert!(store.comments_for(&l).unwrap().is_empty());
    }

    /// Caches a pulled comment (a real thread) on `parent`.
    fn real_thread(store: &Store, id: &str, disc: &str, parent: Value) {
        let c: Comment = serde_json::from_value(cm(id, disc, parent, "t", T)).unwrap();
        store.put_comment(&c).unwrap();
    }

    #[test]
    fn deleting_a_subtree_drops_comments_of_all_descendants() {
        let store = undo_store();
        let b = |id: &str, parent: Value| -> Block {
            serde_json::from_value(blk(id, parent, id, T, false)).unwrap()
        };
        store
            .replace_children("p", "a1", &[b("a11", bp("a1"))])
            .unwrap();
        for (id, blk) in [("c-a", "a"), ("c-a1", "a1"), ("c-a11", "a11")] {
            real_thread(&store, id, &format!("d-{blk}"), bp(blk));
        }
        real_thread(&store, "c-b", "d-b", bp("b"));
        local_delete_block(&store, "a").unwrap();
        for blk in ["a", "a1", "a11"] {
            assert!(store.comments_for(blk).unwrap().is_empty(), "{blk}");
        }
        assert_eq!(store.comments_for("b").unwrap().len(), 1, "others stay");
    }

    #[test]
    fn deleting_a_block_drops_queued_replies_to_its_threads() {
        let store = undo_store();
        real_thread(&store, "c1", "d1", bp("b"));
        local_add_comment(&store, CommentTarget::Discussion("d1".into()), rt("reply")).unwrap();
        let (_, l) =
            local_add_comment(&store, CommentTarget::Block("b".into()), rt("new")).unwrap();
        let d = store.get_comment(&l).unwrap().unwrap().discussion_id;
        local_add_comment(&store, CommentTarget::Discussion(d), rt("reply2")).unwrap();
        // survives: a reply on another block's thread
        real_thread(&store, "c2", "d2", bp("c"));
        local_add_comment(&store, CommentTarget::Discussion("d2".into()), rt("other")).unwrap();
        local_delete_block(&store, "b").unwrap();
        let kinds: Vec<_> = store
            .queued_ops()
            .unwrap()
            .into_iter()
            .map(|o| o.kind)
            .collect();
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert!(
            matches!(&kinds[0], OpKind::CreateComment { parent: CommentTarget::Discussion(d), .. } if d == "d2")
        );
        assert!(matches!(&kinds[1], OpKind::DeleteBlock { .. }));
    }

    #[test]
    fn blocks_with_comments_cannot_be_moved_or_converted() {
        let store = undo_store();
        local_add_comment(&store, CommentTarget::Block("b".into()), rt("x")).unwrap();
        real_thread(&store, "c1", "d1", bp("a1"));
        for id in ["b", "a"] {
            // a's comment sits on its child: the copy would lose it too
            let e = local_move_block(&store, id, "c", None)
                .unwrap_err()
                .to_string();
            assert!(e.contains("has comments") && e.contains("moving"), "{e}");
        }
        let e = local_change_type(&store, "b", TargetKind::Heading1)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("has comments") && e.contains("converting"),
            "{e}"
        );
        assert_eq!(store.comments_for("b").unwrap().len(), 1);
        assert_eq!(
            store.queued_ops().unwrap().len(),
            1,
            "only the comment create"
        );
        // a commentless block still moves
        local_move_block(&store, "c", "b", None).unwrap();
    }

    #[tokio::test]
    async fn comment_post_with_an_unreadable_reply_is_failed_not_retried() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let (_, local) =
            local_add_comment(&store, CommentTarget::Page("p".into()), rt("hi")).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (0, 1), "{r:?}");
        assert!(store.queued_ops().unwrap().is_empty());
        let failed = store.ops_by_status(OpStatus::Failed).unwrap();
        assert!(failed[0]
            .error
            .as_deref()
            .unwrap()
            .contains("may have been posted"));
        assert!(
            store.get_comment(&local).unwrap().is_some(),
            "kept until the user decides"
        );
        // a second push must not POST again (expect(1) above)
        push(&client(&s), &store).await.unwrap();
    }

    #[tokio::test]
    async fn comment_post_that_never_connected_stays_queued() {
        let store = Store::open_in_memory().unwrap();
        local_add_comment(&store, CommentTarget::Page("p".into()), rt("hi")).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let c = Client::new(
            "t".into(),
            ClientConfig {
                base_url: format!("http://127.0.0.1:{port}"),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                max_backoff: Duration::from_secs(1),
            },
        );
        assert!(push(&c, &store).await.is_err());
        assert_eq!(store.queued_ops().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cache_failure_after_a_comment_post_does_not_fail_the_op() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        local_add_comment(&store, CommentTarget::Page("p".into()), rt("hi")).unwrap();
        store.conn.execute_batch("DROP TABLE comments").unwrap();
        post_comment(
            &s,
            json!({ "parent": { "type": "page_id", "page_id": "p" } }),
            cm("c1", "d1", pp("p"), "hi", T2),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (1, 0), "{r:?}");
        assert!(store.queued_ops().unwrap().is_empty());
    }
    fn user_obj(id: &str, name: Option<&str>) -> Value {
        match name {
            Some(n) => json!({ "object": "user", "id": id, "name": n, "type": "person",
                "avatar_url": null, "person": { "email": format!("{id}@x.org") } }),
            None => json!({ "object": "user", "id": id }),
        }
    }

    #[tokio::test]
    async fn pull_caches_inline_users_without_wiping_known_names() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        // known from /v1/users
        store
            .put_users(&[crate::model::User::from_value(&user_obj("u1", Some("Ada"))).unwrap()])
            .unwrap();
        let mut pg = page("p", pp("root"), T);
        pg["created_by"] = user_obj("u1", None);
        pg["last_edited_by"] = user_obj("u2", None);
        let mut a = blk("a", pp("p"), "A", T, false);
        a["last_edited_by"] = user_obj("u1", None);
        let mut b = blk("b", pp("p"), "B", T, false);
        // a mention carries the full user
        b["paragraph"]["rich_text"] = json!([{ "type": "mention", "plain_text": "@Bob",
            "mention": { "type": "user", "user": user_obj("u3", Some("Bob")) }, "annotations": {} }]);
        get(&s, "/v1/pages/p", ok(pg)).await;
        get(&s, "/v1/blocks/p/children", list(vec![a, b])).await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();

        let name = |id: &str| store.user(id).unwrap().map(|u| (u.name, u.email));
        assert_eq!(
            name("u1"),
            Some(("Ada".into(), Some("u1@x.org".into()))),
            "id-only inline kept the name"
        );
        assert_eq!(
            name("u2"),
            Some((String::new(), None)),
            "unknown name: row with the id"
        );
        assert_eq!(name("u3").unwrap().0, "Bob", "mention carried the name");
        assert_eq!(
            store.get_page("p").unwrap().unwrap().last_edited_by(),
            Some("u2")
        );
        assert_eq!(
            store.get_block("a").unwrap().unwrap().last_edited_by(),
            Some("u1")
        );
        assert_eq!(store.users().unwrap().len(), 3);
    }

    #[test]
    fn inline_users_are_deduped_keeping_the_fullest() {
        let u = |v: Value| User::from_value(&v).unwrap();
        let stub = |id: &str| u(json!({ "object": "user", "id": id }));
        let typed = u(json!({ "object": "user", "id": "u2", "type": "bot" }));
        let named = u(json!({ "object": "user", "id": "u1", "type": "person", "name": "Ada" }));
        let got = dedupe_users(vec![
            stub("u1"),
            stub("u2"),
            typed,
            named,
            stub("u1"),
            stub("u3"),
            stub("u2"),
        ]);
        let view: Vec<_> = got
            .iter()
            .map(|u| (u.id.as_str(), u.kind.as_str(), u.name.as_str()))
            .collect();
        assert_eq!(
            view,
            [("u1", "person", "Ada"), ("u2", "bot", ""), ("u3", "", "")]
        );
    }

    #[tokio::test]
    async fn pull_users_caches_and_a_403_is_one_request() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        get(
            &s,
            "/v1/users",
            list(vec![
                user_obj("u1", Some("Ada")),
                json!({ "object": "user", "id": "bot", "type": "bot", "name": "Notion Client" }),
            ]),
        )
        .await;
        assert_eq!(pull_users(&client(&s), &store).await.unwrap(), 2);
        assert_eq!(store.user("bot").unwrap().unwrap().kind, "bot");

        s.reset().await;
        Mock::given(method("GET"))
            .and(path("/v1/users"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "object": "error", "status": 403, "code": "restricted_resource", "message": "no user capability" })))
            .expect(1)
            .mount(&s)
            .await;
        let e = pull_users(&client(&s), &Store::open_in_memory().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Api { status: 403, .. }), "{e:?}");
        assert_eq!(
            s.received_requests().await.unwrap().len(),
            1,
            "no retry loop"
        );
    }

    // ---- resolving conflicts

    /// Block "b" edited locally to "mine" and found in conflict with a remote
    /// "theirs" at T2; returns the conflict op's id.
    async fn conflicted(s: &MockServer, store: &Store) -> i64 {
        seed(s, store, &["b"]).await;
        let op = local_edit_text(store, "b", "mine").unwrap();
        get(
            s,
            "/v1/blocks/b",
            ok(blk("b", pp("p"), "theirs", T2, false)),
        )
        .await;
        let r = push(&client(s), store).await.unwrap();
        assert_eq!((r.applied, r.conflicts), (0, 1));
        op
    }

    #[tokio::test]
    async fn keep_mine_rebases_and_the_next_push_applies_it_with_later_ops() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let op = conflicted(&s, &store).await;
        // a later edit of the same block, queued after the conflict
        local_edit_text(&store, "b", "mine 2").unwrap();
        resolve_mine(&client(&s), &store, op, None, None)
            .await
            .unwrap();
        assert!(store.ops_by_status(OpStatus::Conflict).unwrap().is_empty());
        let q = store.queued_ops().unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].id, op);
        assert_eq!(q[0].error, None);
        assert_eq!(ts(&q[0].base_last_edited_time.unwrap()), T2);
        assert_eq!(
            text(&store, "b"),
            "mine 2",
            "the later edit stays in the cache"
        );
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk(
                "b",
                pp("p"),
                "mine 2",
                "2025-01-03T00:00:00.000Z",
                false,
            )))
            .expect(2)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (2, 0, 0), "{r:?}");
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn keep_mine_shows_the_pushed_version_in_the_cache() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let op = conflicted(&s, &store).await;
        // a pull in between brought Notion's text into the cache
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "theirs", T2, false)]),
        )
        .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
        assert_eq!(text(&store, "b"), "theirs");
        resolve_mine(&client(&s), &store, op, None, None)
            .await
            .unwrap();
        assert_eq!(text(&store, "b"), "mine");
    }

    #[tokio::test]
    async fn keep_mine_on_a_gone_remote_is_refused() {
        for gone in [
            ResponseTemplate::new(404).set_body_json(json!({
                "object": "error", "status": 404, "code": "object_not_found", "message": "gone" })),
            ok({
                let mut b = blk("b", pp("p"), "theirs", T2, false);
                b["in_trash"] = json!(true);
                b
            }),
        ] {
            let s = MockServer::start().await;
            let store = Store::open_in_memory().unwrap();
            let op = conflicted(&s, &store).await;
            s.reset().await;
            get(&s, "/v1/blocks/b", gone).await;
            assert_eq!(
                fetch_conflict(&client(&s), &store, op).await.unwrap(),
                Fetched::Gone
            );
            let e = resolve_mine(&client(&s), &store, op, None, None)
                .await
                .unwrap_err();
            assert!(e.to_string().contains("gone"), "{e}");
            assert_eq!(
                store.ops_by_status(OpStatus::Conflict).unwrap().len(),
                1,
                "still a conflict"
            );
        }
    }

    #[tokio::test]
    async fn merge_writes_the_merged_rich_text() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let op = conflicted(&s, &store).await;
        assert_eq!(
            fetch_conflict(&client(&s), &store, op).await.unwrap(),
            Fetched::Text {
                mine: "mine".into(),
                theirs: "theirs".into()
            }
        );
        resolve_mine(&client(&s), &store, op, Some("mine and **theirs**"), None)
            .await
            .unwrap();
        assert_eq!(text(&store, "b"), "mine and theirs");
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .and(body_partial_json(json!({ "paragraph": { "rich_text": [
                { "text": { "content": "mine and " } },
                { "text": { "content": "theirs" }, "annotations": { "bold": true } } ] } })))
            .respond_with(ok(blk(
                "b",
                pp("p"),
                "mine and theirs",
                "2025-01-03T00:00:00.000Z",
                false,
            )))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (1, 0, 0), "{r:?}");
    }

    #[tokio::test]
    async fn merge_a_title_and_a_comment() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        // title
        let c = client(&s);
        store
            .upsert_page(&serde_json::from_value(titled("t", T, "old")).unwrap())
            .unwrap();
        let op = local_set_title(&store, "t", "mine").unwrap();
        get(&s, "/v1/pages/t", ok(titled("t", T2, "theirs"))).await;
        let r = push(&c, &store).await.unwrap();
        assert_eq!(r.conflicts, 1);
        assert_eq!(
            fetch_conflict(&c, &store, op).await.unwrap(),
            Fetched::Text {
                mine: "mine".into(),
                theirs: "theirs".into()
            }
        );
        resolve_mine(&c, &store, op, Some("mine + theirs"), None)
            .await
            .unwrap();
        assert_eq!(
            store.get_page("t").unwrap().unwrap().title(),
            "mine + theirs"
        );
        Mock::given(method("PATCH"))
            .and(path("/v1/pages/t"))
            .and(body_partial_json(
                json!({ "properties": { "Name": { "title": [
                { "text": { "content": "mine + theirs" } }] } } }),
            ))
            .respond_with(ok(titled("t", "2025-01-03T00:00:00.000Z", "mine + theirs")))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&c, &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (1, 0, 0), "{r:?}");

        // comment
        store
            .put_comment(&serde_json::from_value(cm("c1", "d1", bp("a"), "old", T)).unwrap())
            .unwrap();
        let op = local_edit_comment(&store, "c1", rt("mine")).unwrap();
        get(
            &s,
            "/v1/comments/c1",
            ok(cm("c1", "d1", bp("a"), "theirs", T2)),
        )
        .await;
        assert_eq!(push(&c, &store).await.unwrap().conflicts, 1);
        assert_eq!(
            fetch_conflict(&c, &store, op).await.unwrap(),
            Fetched::Text {
                mine: "mine".into(),
                theirs: "theirs".into()
            }
        );
        resolve_mine(&c, &store, op, Some("both"), None)
            .await
            .unwrap();
        assert_eq!(comment_text(&store, "c1"), "both");
        let q = store.queued_ops().unwrap();
        assert!(
            matches!(&q[0].kind, OpKind::UpdateComment { rich_text, .. } if rich_text[0]["text"]["content"] == "both")
        );
    }

    #[tokio::test]
    async fn keep_mine_requeues_a_delete_and_other_ops_have_no_texts() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_delete_block(&store, "b").unwrap().unwrap();
        get(
            &s,
            "/v1/blocks/b",
            ok(blk("b", pp("p"), "theirs", T2, false)),
        )
        .await;
        assert_eq!(push(&client(&s), &store).await.unwrap().conflicts, 1);
        let op = store.ops_by_status(OpStatus::Conflict).unwrap()[0].id;
        assert_eq!(
            fetch_conflict(&client(&s), &store, op).await.unwrap(),
            Fetched::Other
        );
        assert!(
            resolve_mine(&client(&s), &store, op, Some("x"), None)
                .await
                .is_err(),
            "nothing to merge"
        );
        resolve_mine(&client(&s), &store, op, None, None)
            .await
            .unwrap();
        assert!(
            store.get_block("b").unwrap().is_none(),
            "still deleted in the cache"
        );
        Mock::given(method("DELETE"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "theirs", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts), (1, 0), "{r:?}");
    }

    #[tokio::test]
    async fn a_remote_change_after_the_fetch_is_refused_and_nothing_is_written() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let op = conflicted(&s, &store).await;
        let Fetched::Text { theirs, .. } = fetch_conflict(&client(&s), &store, op).await.unwrap()
        else {
            panic!()
        };
        // Notion changes again before the user decides
        s.reset().await;
        get(
            &s,
            "/v1/blocks/b",
            ok(blk(
                "b",
                pp("p"),
                "theirs, edited again",
                "2025-01-03T00:00:00.000Z",
                false,
            )),
        )
        .await;
        for merged in [None, Some("mine and theirs")] {
            let e = resolve_mine(&client(&s), &store, op, merged, Some(&theirs))
                .await
                .unwrap_err();
            assert!(e.to_string().contains("changed again"), "{e}");
            let ops = store.ops_by_status(OpStatus::Conflict).unwrap();
            assert_eq!(ops.len(), 1, "still a conflict");
            assert_eq!(ts(&ops[0].base_last_edited_time.unwrap()), T, "not rebased");
            assert_eq!(text(&store, "b"), "mine", "cache untouched");
        }
        // seeing the new text lets it through
        let Fetched::Text { theirs, .. } = fetch_conflict(&client(&s), &store, op).await.unwrap()
        else {
            panic!()
        };
        resolve_mine(&client(&s), &store, op, None, Some(&theirs))
            .await
            .unwrap();
        assert_eq!(store.queued_ops().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn formatting_markdown_cannot_carry_offers_no_merge() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let op = conflicted(&s, &store).await;
        // Notion's text holds a link preview, which has no Markdown form
        let mut theirs = blk("b", pp("p"), "theirs", T2, false);
        theirs["paragraph"]["rich_text"] = json!([{ "type": "mention", "plain_text": "x", "annotations": {},
            "mention": { "type": "link_preview", "link_preview": { "url": "https://x.y" } } }]);
        s.reset().await;
        get(&s, "/v1/blocks/b", ok(theirs)).await;
        assert_eq!(
            fetch_conflict(&client(&s), &store, op).await.unwrap(),
            Fetched::Other
        );
        assert!(resolve_mine(&client(&s), &store, op, Some("x"), None)
            .await
            .is_err());
        // Keep mine still works
        resolve_mine(&client(&s), &store, op, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_untouched_merge_keeps_the_original_rich_text() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let mut u = RichText::plain("mine");
        u.annotations.underline = true;
        local_edit_rich(&store, "b", vec![u]).unwrap();
        get(
            &s,
            "/v1/blocks/b",
            ok(blk("b", pp("p"), "theirs", T2, false)),
        )
        .await;
        push(&client(&s), &store).await.unwrap();
        let op = store.ops_by_status(OpStatus::Conflict).unwrap()[0].id;
        resolve_mine(&client(&s), &store, op, Some("mine"), None)
            .await
            .unwrap();
        let q = store.queued_ops().unwrap();
        assert!(
            matches!(&q[0].kind, OpKind::UpdateBlock { body, .. }
            if body["paragraph"]["rich_text"][0]["annotations"]["underline"] == true),
            "{:?}",
            q[0].kind
        );
    }

    #[tokio::test]
    async fn a_title_op_with_other_properties_offers_no_merge() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        store
            .upsert_page(&serde_json::from_value(titled("t", T, "old")).unwrap())
            .unwrap();
        let op = store
            .enqueue(
                OpKind::UpdatePageProperties {
                    page_id: "t".into(),
                    properties: json!({ "Name": { "title": [{ "type": "text", "text": { "content": "mine" } }] },
                        "Tag": { "select": { "name": "x" } } }),
                },
                Some(ts_parse(T)),
                None,
            )
            .unwrap();
        store.mark_op(op, OpStatus::Conflict, Some("x")).unwrap();
        get(&s, "/v1/pages/t", ok(titled("t", T2, "theirs"))).await;
        assert_eq!(
            fetch_conflict(&c, &store, op).await.unwrap(),
            Fetched::Other
        );
    }

    // ------------------------------------------------ block editing (F2)

    #[tokio::test]
    async fn duplicate_pushes_a_create_after_the_original_and_its_children() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
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
        pull_page(&c, &store, "p", true).await.unwrap();
        s.reset().await;

        let copy = local_duplicate_block(&store, "a", None).unwrap();
        assert_eq!(
            ids(&store.children("p").unwrap()),
            ["a", copy.as_str(), "b"]
        );
        assert_eq!(text(&store, &copy), "A");
        let kid = store.children(&copy).unwrap();
        assert_eq!((kid.len(), text(&store, &kid[0].id)), (1, "A1".into()));
        assert_eq!(store.queued_ops().unwrap().len(), 2);

        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_partial_json(json!({ "after": "a" })))
            .and(body_string_contains("\"A\""))
            .respond_with(list(vec![blk("na", pp("p"), "A", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/na/children"))
            .and(body_string_contains("A1"))
            .respond_with(list(vec![blk("na1", bp("na"), "A1", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&c, &store).await.unwrap().applied, 2);
        assert_eq!(ids(&store.children("p").unwrap()), ["a", "na", "b"]);
        assert_eq!(ids(&store.children("na").unwrap()), ["na1"]);
        s.verify().await;
    }

    #[tokio::test]
    async fn duplicate_refuses_what_core_cannot_write_and_queues_nothing() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let mut img = blk("i", pp("p"), "", T, false);
        img["type"] = json!("image");
        store
            .replace_children("p", "p", &[serde_json::from_value(img).unwrap()])
            .unwrap();
        assert!(local_duplicate_block(&store, "i", None).is_err());
        assert!(store.queued_ops().unwrap().is_empty());
        assert_eq!(ids(&store.children("p").unwrap()), ["i"]);
    }

    #[tokio::test]
    async fn moving_a_block_up_or_down_pushes_copy_then_delete() {
        for (moved, anchor, order) in [
            // up: c after a; down: a after b
            ("c", "a", ["a", "n", "b"]),
            ("a", "b", ["b", "n", "c"]),
        ] {
            let s = MockServer::start().await;
            let store = Store::open_in_memory().unwrap();
            seed(&s, &store, &["a", "b", "c"]).await;
            local_move_block(&store, moved, "p", Some(anchor))
                .unwrap()
                .unwrap();
            get(
                &s,
                &format!("/v1/blocks/{moved}"),
                ok(blk(moved, pp("p"), moved, T, false)),
            )
            .await;
            Mock::given(method("PATCH"))
                .and(path("/v1/blocks/p/children"))
                .and(body_partial_json(json!({ "after": anchor })))
                .respond_with(list(vec![blk("n", pp("p"), moved, T2, false)]))
                .expect(1)
                .mount(&s)
                .await;
            Mock::given(method("DELETE"))
                .and(path(format!("/v1/blocks/{moved}")))
                .respond_with(ok(blk(moved, pp("p"), moved, T2, false)))
                .expect(1)
                .mount(&s)
                .await;
            assert_eq!(push(&client(&s), &store).await.unwrap().applied, 1);
            let mut want = order.to_vec();
            if moved == "a" {
                want = vec!["b", "n", "c"];
            }
            assert_eq!(ids(&store.children("p").unwrap()), want);
            s.verify().await;
        }
    }

    #[tokio::test]
    async fn a_commented_block_is_not_moved() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b"]).await;
        let c: Comment = serde_json::from_value(json!({
            "object": "comment", "id": "c1", "discussion_id": "d1",
            "parent": { "type": "block_id", "block_id": "b" },
            "created_time": T, "last_edited_time": T,
            "created_by": { "object": "user", "id": "u" },
            "rich_text": [{ "type": "text", "text": { "content": "hi" }, "plain_text": "hi", "annotations": {} }],
        }))
        .unwrap();
        store.put_comment(&c).unwrap();
        let e = local_move_block(&store, "b", "p", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("comments"), "{e}");
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_colour_change_pushes_a_patch_with_color() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        local_set_color(&store, "a", "red_background").unwrap();
        assert!(local_set_color(&store, "a", "teal").is_err());
        assert!(local_set_color(&store, "a", "default_background").is_err());
        get(&s, "/v1/blocks/a", ok(blk("a", pp("p"), "a", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/a"))
            .and(body_partial_json(
                json!({ "paragraph": { "color": "red_background" } }),
            ))
            .respond_with(ok(blk("a", pp("p"), "a", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&client(&s), &store).await.unwrap().applied, 1);
        s.verify().await;
        // a code block has no colour
        let mut code = blk("k", pp("p"), "x", T, false);
        code["type"] = json!("code");
        code["code"] = json!({ "rich_text": code["paragraph"]["rich_text"], "language": "rust" });
        store
            .replace_children("p", "p", &[serde_json::from_value(code).unwrap()])
            .unwrap();
        assert!(local_set_color(&store, "k", "red").is_err());
    }

    #[tokio::test]
    async fn split_produces_an_edit_and_a_create_and_keeps_marks() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        seed(&s, &store, &["a", "b"]).await;
        store
            .replace_children(
                "p",
                "p",
                &[
                    serde_json::from_value(blk("a", pp("p"), "bold tail", T, false)).unwrap(),
                    serde_json::from_value(blk("b", pp("p"), "B", T, false)).unwrap(),
                ],
            )
            .unwrap();
        // bold across the cut: **bold tail**, cut after "bold" (char 6)
        let new = local_split_block(&store, "a", "**bold tail**", 6).unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["a", new.as_str(), "b"]);
        let (l, r) = (rich(&store, "a"), rich(&store, &new));
        assert_eq!(
            (plain_text(&l), plain_text(&r)),
            ("bold".into(), " tail".into())
        );
        assert!(l[0].annotations.bold && r[0].annotations.bold);
        let kinds: Vec<_> = store
            .queued_ops()
            .unwrap()
            .iter()
            .map(|o| matches!(o.kind, OpKind::UpdateBlock { .. }))
            .collect();
        assert_eq!(kinds, [true, false], "an edit, then a create");

        get(
            &s,
            "/v1/blocks/a",
            ok(blk("a", pp("p"), "bold tail", T, false)),
        )
        .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/a"))
            .and(body_string_contains("\"bold\""))
            .respond_with(ok(blk("a", pp("p"), "bold", T2, false)))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .and(body_partial_json(json!({ "after": "a" })))
            .and(body_string_contains(" tail"))
            .respond_with(list(vec![blk("nt", pp("p"), " tail", T2, false)]))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&c, &store).await.unwrap().applied, 2);
        s.verify().await;
    }

    #[tokio::test]
    async fn duplicate_refuses_uncached_children_and_split_keeps_the_colour() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a"]).await;
        let mut parent = blk("a", pp("p"), "a", T, true);
        parent["paragraph"]["color"] = json!("blue");
        store
            .replace_children("p", "p", &[serde_json::from_value(parent).unwrap()])
            .unwrap();
        // has_children, none cached: refused, nothing queued
        let e = local_duplicate_block(&store, "a", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not cached"), "{e}");
        assert!(store.queued_ops().unwrap().is_empty());
        let new = local_split_block(&store, "a", "a", 1).unwrap();
        assert_eq!(
            store
                .get_block(&new)
                .unwrap()
                .unwrap()
                .kind
                .text()
                .unwrap()
                .color,
            "blue"
        );
    }

    #[test]
    fn split_at_the_end_only_creates_and_a_list_continues() {
        let store = Store::open_in_memory().unwrap();
        let mut bullet = blk("a", pp("p"), "x", T, false);
        bullet["type"] = json!("bulleted_list_item");
        bullet["bulleted_list_item"] = bullet["paragraph"].take();
        store
            .upsert_page(&serde_json::from_value(page("p", pp("root"), T)).unwrap())
            .unwrap();
        store
            .replace_children("p", "p", &[serde_json::from_value(bullet).unwrap()])
            .unwrap();
        let new = local_split_block(&store, "a", "x", 1).unwrap();
        assert_eq!(
            store.queued_ops().unwrap().len(),
            1,
            "no edit: nothing changed"
        );
        let b = store.get_block(&new).unwrap().unwrap();
        assert!(matches!(b.kind, BlockKind::BulletedListItem(_)));
        assert_eq!(plain_text(&b.kind.text().unwrap().rich_text), "");
    }

    async fn post(s: &MockServer, p: &str, resp: ResponseTemplate, expect: u64) {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(resp)
            .expect(expect)
            .mount(s)
            .await;
    }
    async fn patch(s: &MockServer, p: &str, body: Value, resp: ResponseTemplate) {
        Mock::given(method("PATCH"))
            .and(path(p))
            .and(body_partial_json(body))
            .respond_with(resp)
            .expect(1)
            .mount(s)
            .await;
    }
    fn report(applied: usize, conflicts: usize) -> PushReport {
        PushReport {
            applied,
            conflicts,
            failed: 0,
        }
    }

    #[tokio::test]
    async fn a_new_page_is_created_under_its_parent_and_later_blocks_follow_its_id() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let id = local_create_page(&store, "p", Some("b"), "Notes").unwrap();
        assert!(id.starts_with("local-"));
        assert_eq!(ids(&store.children("p").unwrap()), ["b", id.as_str()]);
        assert_eq!(
            store
                .page_tree()
                .unwrap()
                .iter()
                .filter(|e| e.id == id)
                .count(),
            1
        );
        // renaming and editing before the push rewrite the one create
        local_set_title(&store, &id, "Ideas").unwrap();
        local_set_icon(&store, &id, Some("📝")).unwrap();
        let blk = local_create_block(&store, &id, None, BlockKind::Divider).unwrap();
        let ops = store.queued_ops().unwrap();
        assert_eq!(ops.len(), 2, "one create for the page, one for the block");
        assert!(local_trash_page(&store, &id, true).is_err());

        let mut real = page("real1", pp("p"), T2);
        real["properties"] = json!({ "title": { "id": "title", "type": "title", "title": [
            { "type": "text", "text": { "content": "Ideas" }, "plain_text": "Ideas", "annotations": {} }] } });
        real["icon"] = json!({ "type": "emoji", "emoji": "📝" });
        post(&s, "/v1/pages", ok(real), 1).await;
        Mock::given(method("POST"))
            .and(path("/v1/pages"))
            .and(body_partial_json(json!({
                "parent": { "type": "page_id", "page_id": "p" },
                "icon": { "type": "emoji", "emoji": "📝" },
                "properties": { "title": { "title": [{ "text": { "content": "Ideas" } }] } },
            })))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&s)
            .await;
        let mut div = blk_json_divider("d");
        div["parent"] = pp("real1");
        patch(
            &s,
            "/v1/blocks/real1/children",
            json!({ "children": [{ "type": "divider" }] }),
            list(vec![div]),
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(r, report(2, 0));
        assert_eq!(store.resolve_id(&id).unwrap(), "real1");
        assert!(store.get_page(&id).unwrap().is_none());
        assert_eq!(store.get_page("real1").unwrap().unwrap().title(), "Ideas");
        assert_eq!(ids(&store.children("p").unwrap()), ["b", "real1"]);
        assert_eq!(ids(&store.children("real1").unwrap()), ["d"]);
        assert!(store.get_block(&blk).unwrap().is_none());
        assert!(store.queued_ops().unwrap().is_empty());
    }

    fn blk_json_divider(id: &str) -> Value {
        json!({ "object": "block", "id": id, "parent": pp("p"), "type": "divider", "divider": {},
            "has_children": false, "in_trash": false, "created_time": T, "last_edited_time": T2 })
    }

    #[test]
    fn a_page_can_only_be_created_under_a_cached_live_page() {
        let store = Store::open_in_memory().unwrap();
        assert!(local_create_page(&store, "nope", None, "x").is_err());
    }

    /// Seeded "p" with a queued page op from `queue`, pushed against remote `remote`
    /// and PATCH `want` (answered with `done`); returns the report.
    async fn push_page_op(
        queue: impl Fn(&Store),
        remote: Value,
        want: Value,
        done: Value,
    ) -> (PushReport, Store) {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        queue(&store);
        get(&s, "/v1/pages/p", ok(remote)).await;
        patch(&s, "/v1/pages/p", want, ok(done)).await;
        let r = push(&client(&s), &store).await.unwrap();
        (r, store)
    }

    #[tokio::test]
    async fn icon_cover_trash_and_restore_are_one_patch_each() {
        let mut done = page("p", pp("root"), T2);
        done["icon"] = json!({ "type": "emoji", "emoji": "🚀" });
        let (r, store) = push_page_op(
            |st| {
                local_set_icon(st, "p", Some("🚀")).unwrap();
                // a second change merges into the queued PATCH
                local_set_cover(st, "p", Some("https://img.example.com/a.png")).unwrap();
                assert_eq!(st.queued_ops().unwrap().len(), 1);
            },
            page("p", pp("root"), T2),
            json!({ "icon": { "type": "emoji", "emoji": "🚀" },
                    "cover": { "type": "external", "external": { "url": "https://img.example.com/a.png" } } }),
            done,
        )
        .await;
        assert_eq!(r, report(1, 0));
        assert_eq!(
            store.get_page("p").unwrap().unwrap().raw["icon"]["emoji"],
            "🚀"
        );

        // remove both
        let (r, _) = push_page_op(
            |st| {
                local_set_icon(st, "p", None).unwrap();
                local_set_cover(st, "p", None).unwrap();
            },
            page("p", pp("root"), T2),
            json!({ "icon": null, "cover": null }),
            page("p", pp("root"), T2),
        )
        .await;
        assert_eq!(r, report(1, 0));

        // trash, then restore from the cache of a trashed page
        let mut trashed = page("p", pp("root"), T2);
        trashed["in_trash"] = json!(true);
        let (r, store) = push_page_op(
            |st| {
                local_trash_page(st, "p", true).unwrap();
                assert!(st.page_tree().unwrap().is_empty(), "hidden at once");
                assert_eq!(st.trashed_pages().unwrap().len(), 1);
            },
            page("p", pp("root"), T2),
            json!({ "in_trash": true }),
            trashed.clone(),
        )
        .await;
        assert_eq!(r, report(1, 0));
        assert!(store.get_page("p").unwrap().unwrap().in_trash);
        local_trash_page(&store, "p", false).unwrap();
        assert!(store.trashed_pages().unwrap().is_empty());
        assert_eq!(store.page_tree().unwrap().len(), 1);

        let s = MockServer::start().await;
        get(&s, "/v1/pages/p", ok(trashed)).await;
        patch(
            &s,
            "/v1/pages/p",
            json!({ "in_trash": false }),
            ok(page("p", pp("root"), T2)),
        )
        .await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(1, 0));
        assert!(!store.get_page("p").unwrap().unwrap().in_trash);
    }

    #[test]
    fn icon_and_cover_are_validated() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_page(&serde_json::from_value(page("p", pp("root"), T)).unwrap())
            .unwrap();
        assert!(local_set_icon(&store, "p", Some("abc")).is_err());
        assert!(local_set_icon(&store, "p", Some("")).is_err());
        assert!(local_set_cover(&store, "p", Some("ftp://x/y.png")).is_err());
        assert!(local_set_cover(&store, "p", Some("javascript:alert(1)")).is_err());
        assert_eq!(
            cover_host("https://img.example.com/a.png").unwrap(),
            "img.example.com"
        );
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_remote_icon_change_conflicts_but_an_edited_block_does_not() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_set_icon(&store, "p", Some("🚀")).unwrap();
        let mut theirs = page("p", pp("root"), T2);
        theirs["icon"] = json!({ "type": "emoji", "emoji": "🔥" });
        get(&s, "/v1/pages/p", ok(theirs)).await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(0, 1));
        let c = store.ops_by_status(OpStatus::Conflict).unwrap();
        assert_eq!(c[0].error.as_deref(), Some("remote changed icon"));
    }

    #[tokio::test]
    async fn a_pull_keeps_a_queued_trash_and_icon() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_trash_page(&store, "p", true).unwrap();
        local_set_icon(&store, "p", Some("🚀")).unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "b", T2, false)]),
        )
        .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
        let p = store.get_page("p").unwrap().unwrap();
        assert!(p.in_trash);
        assert_eq!(p.raw["icon"]["emoji"], "🚀");
    }

    #[tokio::test]
    async fn a_page_is_moved_with_the_move_endpoint() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        store
            .upsert_page(&serde_json::from_value(page("q", pp("root"), T)).unwrap())
            .unwrap();
        store
            .upsert_page(&serde_json::from_value(page("r", pp("p"), T)).unwrap())
            .unwrap();
        assert!(local_move_page(&store, "p", "p").is_err());
        assert!(
            local_move_page(&store, "p", "r").is_err(),
            "into a descendant"
        );
        assert!(local_move_page(&store, "p", "nope").is_err());
        assert!(local_move_page(&store, "p", "root").is_err());
        assert!(store.queued_ops().unwrap().is_empty());
        local_move_page(&store, "p", "q").unwrap();
        assert_eq!(
            store.get_page("p").unwrap().unwrap().parent,
            Parent::Page("q".into())
        );
        assert!(
            local_move_page(&store, "q", "r").is_err(),
            "r is under p, now under q"
        );

        // the move endpoint is a POST
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        Mock::given(method("POST"))
            .and(path("/v1/pages/p/move"))
            .and(body_partial_json(
                json!({ "parent": { "type": "page_id", "page_id": "q" } }),
            ))
            .respond_with(ok(page("p", pp("q"), T2)))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(1, 0));
        assert_eq!(
            store.get_page("p").unwrap().unwrap().parent,
            Parent::Page("q".into())
        );
    }

    #[tokio::test]
    async fn a_move_conflicts_when_the_parent_changed_remotely() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        store
            .upsert_page(&serde_json::from_value(page("q", pp("root"), T)).unwrap())
            .unwrap();
        local_move_page(&store, "p", "q").unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("elsewhere"), T2))).await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(0, 1));
    }

    #[tokio::test]
    async fn a_pull_of_the_parent_keeps_an_unpushed_page_and_its_row() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let id = local_create_page(&store, "p", Some("b"), "New").unwrap();
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T2))).await;
        get(
            &s,
            "/v1/blocks/p/children",
            list(vec![blk("b", pp("p"), "b", T2, false)]),
        )
        .await;
        pull_page(&client(&s), &store, "p", true).await.unwrap();
        assert_eq!(ids(&store.children("p").unwrap()), ["b", id.as_str()]);
        assert!(store.get_page(&id).unwrap().is_some());
        assert_eq!(store.queued_ops().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_page_under_a_parent_that_was_never_created_fails_without_a_request() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let a = local_create_page(&store, "p", None, "A").unwrap();
        local_create_page(&store, &a, None, "B").unwrap();
        post(
            &s,
            "/v1/pages",
            ResponseTemplate::new(400).set_body_json(json!({
            "object": "error", "status": 400, "code": "validation_error", "message": "no" })),
            1,
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!(
            (r.applied, r.failed),
            (0, 2),
            "the first fails at Notion, its child never starts"
        );
    }

    #[tokio::test]
    async fn a_block_on_a_page_that_failed_to_create_fails_without_a_request() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        let a = local_create_page(&store, "p", None, "A").unwrap();
        local_create_block(&store, &a, None, BlockKind::Divider).unwrap();
        post(
            &s,
            "/v1/pages",
            ResponseTemplate::new(400).set_body_json(json!({
            "object": "error", "status": 400, "code": "validation_error", "message": "no" })),
            1,
        )
        .await;
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.failed), (0, 2));
        let f = store.ops_by_status(OpStatus::Failed).unwrap();
        assert_eq!(f[1].error.as_deref(), Some("its page was never created"));
    }

    #[test]
    fn lock_and_trash_refuse_unpushed_pages_and_restore_needs_a_live_parent() {
        let store = Store::open_in_memory().unwrap();
        for (id, parent) in [("p", "root"), ("c", "p")] {
            store
                .upsert_page(&serde_json::from_value(page(id, pp(parent), T)).unwrap())
                .unwrap();
        }
        let new = local_create_page(&store, "p", None, "N").unwrap();
        assert!(local_set_locked(&store, &new, true).is_err());
        assert!(local_set_locked(&store, "c", true).is_ok());
        local_trash_page(&store, "c", true).unwrap();
        local_trash_page(&store, "p", true).unwrap();
        let e = local_trash_page(&store, "c", false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("restore the parent page first"), "{e}");
        local_trash_page(&store, "p", false).unwrap();
        local_trash_page(&store, "c", false).unwrap();
        assert!(local_set_icon(&store, "p", Some("🦄")).is_ok());
        assert!(local_set_icon(&store, "p", Some("a🦄")).is_err());
    }

    // ---------------------------------------------------------- databases (F4)

    fn ds_parent() -> Value {
        json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" })
    }

    fn prop(id: &str, name: &str, kind: &str, config: Value) -> Value {
        json!({ "id": id, "name": name, "type": kind, kind: config })
    }

    fn ds_json() -> Value {
        let mut props = serde_json::Map::new();
        let mut add = |name: &str, kind: &str, config: Value| {
            let id = if kind == "title" {
                "title".to_string()
            } else {
                format!("id-{name}")
            };
            props.insert(name.into(), prop(&id, name, kind, config));
        };
        add("Name", "title", json!({}));
        add("Notes", "rich_text", json!({}));
        add("Qty", "number", json!({ "format": "number" }));
        add(
            "Kind",
            "select",
            json!({ "options": [{ "id": "o1", "name": "A", "color": "red" }] }),
        );
        add("Tags", "multi_select", json!({ "options": [] }));
        add(
            "State",
            "status",
            json!({ "options": [{ "id": "s1", "name": "Done", "color": "green" }] }),
        );
        add("Due", "date", json!({}));
        add("Done", "checkbox", json!({}));
        add("Site", "url", json!({}));
        add("Mail", "email", json!({}));
        add("Tel", "phone_number", json!({}));
        add("Who", "people", json!({}));
        add("Calc", "formula", json!({ "expression": "1" }));
        json!({ "object": "data_source", "id": "ds", "parent": { "type": "database_id", "database_id": "db" },
            "title": [{ "type": "text", "text": { "content": "Tasks" }, "plain_text": "Tasks", "annotations": {} }],
            "last_edited_time": T, "properties": props })
    }

    fn row(id: &str, time: &str) -> Value {
        let mut v = page(id, ds_parent(), time);
        v["properties"]["Qty"] = json!({ "id": "id-Qty", "type": "number", "number": null });
        v["properties"]["Done"] = json!({ "id": "id-Done", "type": "checkbox", "checkbox": false });
        v
    }

    /// A store with the data source "ds" and its row "r1" (at time T).
    fn rows_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_data_source(&serde_json::from_value(ds_json()).unwrap())
            .unwrap();
        store
            .upsert_page(&serde_json::from_value(row("r1", T)).unwrap())
            .unwrap();
        store
    }

    #[tokio::test]
    async fn each_property_type_pushes_its_own_body() {
        let cases = [
            (
                "Name",
                "Hi",
                json!({ "title": [{ "type": "text", "text": { "content": "Hi" } }] }),
            ),
            (
                "Notes",
                "x",
                json!({ "rich_text": [{ "type": "text", "text": { "content": "x" } }] }),
            ),
            ("Qty", "3", json!({ "number": 3 })),
            ("Kind", "A", json!({ "select": { "name": "A" } })),
            (
                "Tags",
                "x,y",
                json!({ "multi_select": [{ "name": "x" }, { "name": "y" }] }),
            ),
            ("State", "Done", json!({ "status": { "name": "Done" } })),
            (
                "Due",
                "2025-01-01/2025-01-02",
                json!({ "date": { "start": "2025-01-01", "end": "2025-01-02", "time_zone": null } }),
            ),
            ("Done", "true", json!({ "checkbox": true })),
            ("Site", "https://a.at", json!({ "url": "https://a.at" })),
            ("Mail", "a@b.at", json!({ "email": "a@b.at" })),
            ("Tel", "+43 1", json!({ "phone_number": "+43 1" })),
            (
                "Who",
                "u1",
                json!({ "people": [{ "object": "user", "id": "u1" }] }),
            ),
        ];
        for (name, text, sent) in cases {
            let s = MockServer::start().await;
            let store = rows_store();
            local_set_property(&store, "r1", name, text).unwrap();
            assert_eq!(
                store.get_page("r1").unwrap().unwrap().raw["properties"][name]["type"],
                ds_json()["properties"][name]["type"],
                "{name} cached"
            );
            get(&s, "/v1/pages/r1", ok(row("r1", T))).await;
            Mock::given(method("PATCH"))
                .and(path("/v1/pages/r1"))
                .and(wiremock::matchers::body_json(
                    json!({ "properties": { name: sent } }),
                ))
                .respond_with(ok(row("r1", T2)))
                .expect(1)
                .mount(&s)
                .await;
            assert_eq!(
                push(&client(&s), &store).await.unwrap(),
                report(1, 0),
                "{name}"
            );
            s.verify().await;
        }
    }

    #[test]
    fn property_edits_are_refused_for_read_only_types_and_non_rows() {
        let store = rows_store();
        for bad in [
            ("Calc", "1"),
            ("Qty", "abc"),
            ("State", "Nope"),
            ("Site", "ftp://a"),
            ("Nope", "x"),
        ] {
            assert!(
                local_set_property(&store, "r1", bad.0, bad.1).is_err(),
                "{bad:?}"
            );
        }
        page_in_store(&store);
        assert!(local_set_property(&store, "pg", "Name", "x").is_err());
        assert!(store.queued_ops().unwrap().is_empty());
    }

    fn page_in_store(store: &Store) {
        store
            .upsert_page(&serde_json::from_value(page("pg", pp("root"), T)).unwrap())
            .unwrap();
    }

    #[tokio::test]
    async fn several_cell_edits_on_one_row_are_one_op_and_other_rows_get_their_own() {
        let s = MockServer::start().await;
        let store = rows_store();
        store
            .upsert_page(&serde_json::from_value(row("r2", T)).unwrap())
            .unwrap();
        local_set_property(&store, "r1", "Qty", "3").unwrap();
        local_set_property(&store, "r1", "Name", "Hi").unwrap();
        local_set_property(&store, "r1", "Qty", "4").unwrap();
        local_set_property(&store, "r2", "Qty", "9").unwrap();
        assert_eq!(store.queued_ops().unwrap().len(), 2);
        for id in ["r1", "r2"] {
            get(&s, &format!("/v1/pages/{id}"), ok(row(id, T))).await;
        }
        patch(&s, "/v1/pages/r1", json!({ "properties": { "Qty": { "number": 4 }, "Name": { "title": [{ "text": { "content": "Hi" } }] } } }), ok(row("r1", T2))).await;
        patch(
            &s,
            "/v1/pages/r2",
            json!({ "properties": { "Qty": { "number": 9 } } }),
            ok(row("r2", T2)),
        )
        .await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(2, 0));
        s.verify().await;
    }

    #[tokio::test]
    async fn only_a_remote_change_to_an_edited_property_conflicts() {
        let remote = |qty: Value, done: bool| {
            let mut v = row("r1", T2);
            v["properties"]["Qty"]["number"] = qty;
            v["properties"]["Done"]["checkbox"] = json!(done);
            v
        };
        for (remote, applied, conflicts) in [
            (remote(json!(5), false), 0, 1),
            (remote(json!(null), true), 1, 0),
        ] {
            let s = MockServer::start().await;
            let store = rows_store();
            local_set_property(&store, "r1", "Qty", "3").unwrap();
            get(&s, "/v1/pages/r1", ok(remote)).await;
            Mock::given(method("PATCH"))
                .and(path("/v1/pages/r1"))
                .respond_with(ok(row("r1", T2)))
                .mount(&s)
                .await;
            assert_eq!(
                push(&client(&s), &store).await.unwrap(),
                report(applied, conflicts)
            );
            if conflicts == 1 {
                let c = store.ops_by_status(OpStatus::Conflict).unwrap();
                assert_eq!(c[0].error.as_deref(), Some("remote changed Qty"));
            }
        }
    }

    #[tokio::test]
    async fn a_new_row_is_created_under_its_data_source_and_its_id_mapped() {
        let s = MockServer::start().await;
        let store = rows_store();
        let id = local_create_row(&store, "ds").unwrap();
        assert!(id.starts_with("local-"));
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        // edits before the push go into the create, not into a second op
        local_set_property(&store, &id, "Name", "Hello").unwrap();
        local_set_property(&store, &id, "Qty", "2").unwrap();
        assert_eq!(store.queued_ops().unwrap().len(), 1);
        let mut made = row("real1", T2);
        made["properties"]["Name"]["title"] = json!([{ "type": "text", "text": { "content": "Hello" }, "plain_text": "Hello", "annotations": {} }]);
        Mock::given(method("POST"))
            .and(path("/v1/pages"))
            .and(wiremock::matchers::body_json(json!({
                "parent": { "type": "data_source_id", "data_source_id": "ds" },
                "properties": { "Name": { "title": [{ "type": "text", "text": { "content": "Hello" } }] }, "Qty": { "number": 2 } },
            })))
            .respond_with(ok(made))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(1, 0));
        s.verify().await;
        assert_eq!(store.resolve_id(&id).unwrap(), "real1");
        let mut ids: Vec<_> = store
            .rows("ds")
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["r1", "real1"]);
        assert_eq!(store.get_page("real1").unwrap().unwrap().title(), "Hello");
        assert!(store.queued_ops().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_row_is_trashed_with_one_patch_and_stays_out_of_a_pull() {
        let s = MockServer::start().await;
        let store = rows_store();
        local_trash_page(&store, "r1", true).unwrap();
        assert!(store.get_page("r1").unwrap().unwrap().in_trash);
        // a pull before the push keeps the queued trash
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        post(
            &s,
            "/v1/data_sources/ds/query",
            list(vec![row("r1", T2)]),
            1,
        )
        .await;
        pull_data_source(&client(&s), &store, "ds").await.unwrap();
        assert!(store.get_page("r1").unwrap().unwrap().in_trash);
        s.reset().await;
        get(&s, "/v1/pages/r1", ok(row("r1", T2))).await;
        let mut done = row("r1", T2);
        done["in_trash"] = json!(true);
        patch(&s, "/v1/pages/r1", json!({ "in_trash": true }), ok(done)).await;
        assert_eq!(push(&client(&s), &store).await.unwrap(), report(1, 0));
    }

    #[tokio::test]
    async fn pulling_a_database_caches_its_data_sources_and_the_first_ones_rows() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        get(
            &s,
            "/v1/databases/db",
            ok(json!({ "object": "database", "id": "db",
            "data_sources": [{ "id": "ds", "name": "Tasks" }, { "id": "ds2", "name": "More" }] })),
        )
        .await;
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        let mut two = ds_json();
        two["id"] = json!("ds2");
        two["title"] = json!([]);
        get(&s, "/v1/data_sources/ds2", ok(two)).await;
        post(
            &s,
            "/v1/data_sources/ds/query",
            list(vec![row("r1", T), row("r2", T)]),
            1,
        )
        .await;
        post(&s, "/v1/data_sources/ds2/query", list(vec![]), 0).await;
        let r = pull_database(&client(&s), &store, "db").await.unwrap();
        assert_eq!(r.updated, 2);
        assert_eq!(
            store
                .data_sources_of("db")
                .unwrap()
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["ds2", "ds"]
        );
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        s.verify().await;
    }

    async fn setup(s: &MockServer) {
        get(s, "/v1/data_sources/ds", ok(ds_json())).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T)]))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn pull_any_tells_data_sources_databases_and_pages_apart() {
        let err = |status: u16| {
            ResponseTemplate::new(status).set_body_json(
                json!({ "object": "error", "status": status, "code": "x", "message": "m" }),
            )
        };
        let db = || {
            ok(
                json!({ "object": "database", "id": "x", "data_sources": [{ "id": "ds", "name": "T" }] }),
            )
        };
        // a cached data source: its rows, no page request
        let s = MockServer::start().await;
        let store = rows_store();
        setup(&s).await;
        get(&s, "/v1/pages/ds", err(500)).await;
        assert_eq!(
            pull_any(&client(&s), &store, "ds", false)
                .await
                .unwrap()
                .fetched,
            1
        );
        // a child_database block's id is a database, without asking for a page
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        setup(&s).await;
        get(&s, "/v1/databases/cd", db()).await;
        get(&s, "/v1/pages/cd", err(500)).await;
        let blk: Block = serde_json::from_value(json!({ "object": "block", "id": "cd", "parent": pp("p"), "type": "child_database",
            "child_database": { "title": "T" }, "has_children": false, "in_trash": false, "created_time": T, "last_edited_time": T })).unwrap();
        store.put_block(&blk).unwrap();
        assert_eq!(
            pull_any(&client(&s), &store, "cd", false)
                .await
                .unwrap()
                .fetched,
            1
        );
        // Notion says the id is a database, not a page
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        setup(&s).await;
        get(&s, "/v1/pages/x", err(400)).await;
        get(&s, "/v1/databases/x", db()).await;
        assert_eq!(
            pull_any(&client(&s), &store, "x", false)
                .await
                .unwrap()
                .fetched,
            1
        );
        // neither: the page's error
        let s = MockServer::start().await;
        get(&s, "/v1/pages/x", err(404)).await;
        get(&s, "/v1/databases/x", err(404)).await;
        let e = pull_any(&client(&s), &Store::open_in_memory().unwrap(), "x", false)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Api { status: 404, .. }));
        // anything else is a page
        let s = MockServer::start().await;
        get(&s, "/v1/pages/p", ok(page("p", pp("root"), T))).await;
        get(&s, "/v1/blocks/p/children", list(vec![])).await;
        assert!(
            !pull_any(&client(&s), &Store::open_in_memory().unwrap(), "p", false)
                .await
                .unwrap()
                .skipped
        );
    }

    #[tokio::test]
    async fn sync_all_fetches_only_new_rows_after_the_first_pull() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T)]))
            .mount(&s)
            .await;
        sync_data_source(&c, &store, "ds", true).await.unwrap();
        s.reset().await;
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        let q = json!({ "filter": { "timestamp": "last_edited_time", "last_edited_time": { "on_or_after": T } } });
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .and(body_partial_json(q))
            .respond_with(list(vec![row("r1", T), row("r2", T2)]))
            .expect(1)
            .mount(&s)
            .await;
        sync_data_source(&c, &store, "ds", false).await.unwrap();
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        s.verify().await;
    }

    #[tokio::test]
    async fn push_writes_in_queue_order_and_still_finds_the_conflict() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["a", "b", "c"]).await;
        for k in ["a", "b", "c"] {
            local_edit_text(&store, k, &format!("mine-{k}")).unwrap();
            // b was edited remotely since the base: a conflict
            let t = if k == "b" { T2 } else { T };
            get(&s, &format!("/v1/blocks/{k}"), ok(blk(k, pp("p"), k, t, false))).await;
            Mock::given(method("PATCH"))
                .and(path(format!("/v1/blocks/{k}")))
                .respond_with(ok(blk(k, pp("p"), k, T2, false)))
                .mount(&s)
                .await;
        }
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (2, 1, 0));
        let patched: Vec<String> = s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(patched, ["/v1/blocks/a", "/v1/blocks/c"]);
    }

    #[tokio::test]
    async fn a_second_op_on_the_same_block_sees_the_first_write_not_a_stale_prefetch() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b", "c"]).await;
        local_edit_text(&store, "b", "one").unwrap();
        local_edit_text(&store, "c", "x").unwrap();
        local_edit_text(&store, "b", "two").unwrap();
        get(&s, "/v1/blocks/c", ok(blk("c", pp("p"), "c", T, false))).await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/c"))
            .respond_with(ok(blk("c", pp("p"), "x", T2, false)))
            .mount(&s)
            .await;
        // b's GETs: the base, then (after our own first write) our own write
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ok(blk("b", pp("p"), "b", T, false)))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        get(&s, "/v1/blocks/b", ok(blk("b", pp("p"), "one", T2, false))).await;
        for t in ["one", "two"] {
            Mock::given(method("PATCH"))
                .and(path("/v1/blocks/b"))
                .respond_with(ok(blk("b", pp("p"), t, T2, false)))
                .up_to_n_times(1)
                .expect(1)
                .mount(&s)
                .await;
        }
        let r = push(&client(&s), &store).await.unwrap();
        assert_eq!((r.applied, r.conflicts, r.failed), (3, 0, 0), "{r:?}");
        s.verify().await;
    }

    #[tokio::test]
    async fn a_failed_prefetch_is_redone_when_the_op_applies() {
        for status in [404, 500] {
            let s = MockServer::start().await;
            let store = Store::open_in_memory().unwrap();
            seed(&s, &store, &["a", "b"]).await;
            local_edit_text(&store, "a", "mine-a").unwrap();
            local_edit_text(&store, "b", "mine-b").unwrap();
            // a: the early GET fails, the redo in apply succeeds
            Mock::given(method("GET"))
                .and(path("/v1/blocks/a"))
                .respond_with(ResponseTemplate::new(status))
                .up_to_n_times(1)
                .mount(&s)
                .await;
            get(&s, "/v1/blocks/a", ok(blk("a", pp("p"), "a", T, false))).await;
            // b: both GETs fail: that op fails, the push goes on
            get(&s, "/v1/blocks/b", ResponseTemplate::new(status)).await;
            Mock::given(method("PATCH"))
                .and(path("/v1/blocks/a"))
                .respond_with(ok(blk("a", pp("p"), "mine-a", T2, false)))
                .expect(1)
                .mount(&s)
                .await;
            Mock::given(method("PATCH"))
                .and(path("/v1/blocks/b"))
                .respond_with(ok(json!({})))
                .expect(0)
                .mount(&s)
                .await;
            let r = push(&client(&s), &store).await.unwrap();
            assert_eq!((r.applied, r.conflicts, r.failed), (1, 0, 1), "{status}: {r:?}");
            s.verify().await;
        }
    }

    #[tokio::test]
    async fn a_full_sync_of_a_data_source_notices_trashed_rows_the_incremental_one_cannot() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T), row("r2", T)]))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        sync_data_source(&c, &store, "ds", true).await.unwrap();
        // r2 left the data source; every later query lists only r1
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T)]))
            .mount(&s)
            .await;
        let trashed = |id: &str| store.get_page(id).unwrap().unwrap().in_trash;
        sync_data_source(&c, &store, "ds", false).await.unwrap();
        assert!(!trashed("r2"), "incremental must not trash");
        sync_data_source(&c, &store, "ds", true).await.unwrap();
        assert!(trashed("r2") && !trashed("r1"));
    }

    #[tokio::test]
    async fn polling_a_data_source_asks_only_for_rows_edited_since_the_newest() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        let rows_at = |v: Vec<Value>| list(v);
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(rows_at(vec![row("r1", T), row("r2", T2)]))
            .mount(&s)
            .await;
        // nothing cached: a full pull (and the schema)
        assert!(!poll_data_source(&c, &store, "ds").await.unwrap().skipped);
        assert_eq!(store.rows("ds").unwrap().len(), 2);
        s.reset().await;
        // nothing changed: one request, skipped
        let q = json!({ "filter": { "timestamp": "last_edited_time", "last_edited_time": { "on_or_after": "2025-01-02T00:00:00.000Z" } } });
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .and(body_partial_json(q.clone()))
            .respond_with(rows_at(vec![row("r2", T2)]))
            .expect(1)
            .mount(&s)
            .await;
        let r = poll_data_source(&c, &store, "ds").await.unwrap();
        assert!(r.skipped && r.updated == 0, "{r:?}");
        // a same-minute edit of another row is seen through its properties
        s.reset().await;
        let mut edited = row("r2", T2);
        edited["properties"]["Qty"]["number"] = json!(7);
        let new = row("r3", "2025-01-03T00:00:00.000Z");
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .and(body_partial_json(q))
            .respond_with(rows_at(vec![edited, new]))
            .expect(1)
            .mount(&s)
            .await;
        let r = poll_data_source(&c, &store, "ds").await.unwrap();
        assert!(!r.skipped && r.updated == 2, "{r:?}");
        assert_eq!(
            store.get_page("r2").unwrap().unwrap().raw["properties"]["Qty"]["number"],
            7
        );
        assert_eq!(store.rows("ds").unwrap().len(), 3);
        // only the filtered queries were sent (no second data source GET)
        assert!(s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() == "POST"));
    }

    #[tokio::test]
    async fn a_poll_with_a_queued_prop_edit_updates_nothing() {
        let s = MockServer::start().await;
        let store = rows_store();
        local_set_property(&store, "r1", "Qty", "9").unwrap();
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T2)]))
            .mount(&s)
            .await;
        for _ in 0..2 {
            let r = poll_data_source(&client(&s), &store, "ds").await.unwrap();
            assert!(r.skipped && r.updated == 0, "{r:?}");
        }
        assert_eq!(store.queued_ops().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_full_pull_trashes_rows_missing_from_the_query_except_local_and_queued_ones() {
        let s = MockServer::start().await;
        let store = rows_store();
        let local = local_create_row(&store, "ds").unwrap();
        for id in ["r2", "r3"] {
            store
                .upsert_page(&serde_json::from_value(row(id, T)).unwrap())
                .unwrap();
        }
        local_set_property(&store, "r3", "Qty", "1").unwrap();
        get(&s, "/v1/data_sources/ds", ok(ds_json())).await;
        Mock::given(method("POST"))
            .and(path("/v1/data_sources/ds/query"))
            .respond_with(list(vec![row("r1", T)]))
            .mount(&s)
            .await;
        pull_data_source(&client(&s), &store, "ds").await.unwrap();
        let trashed = |id: &str| store.get_page(id).unwrap().unwrap().in_trash;
        assert!(trashed("r2") && !trashed("r1") && !trashed("r3") && !trashed(&local));
    }

    fn ts_parse(t: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc)
    }

    fn private_client(s: &MockServer) -> Client {
        Client::new_private_at(
            s.uri(),
            crate::private::PrivateAuth {
                token_v2: "tok".into(),
                space_id: "s1".into(),
                user_id: None,
                cookies: None,
            },
            ClientConfig {
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn a_pull_on_a_dead_private_session_aborts() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        Mock::given(method("POST"))
            .and(path("/api/v3/search"))
            .respond_with(ok(json!({
                "results": ["p1"],
                "recordMap": { "block": { "p1": { "role": "editor", "value": {
                    "id": "p1", "type": "page", "alive": true,
                    "parent_id": "s1", "parent_table": "space",
                    "properties": { "title": [["Hello"]] }, "content": [],
                    "created_time": 1700000000000_i64,
                    "last_edited_time": 1700000000000_i64 } } } }
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/loadPageChunk"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let e = pull_all(&private_client(&s), &store, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(matches!(e, Error::SessionExpired), "{e:?}");
    }

    #[tokio::test]
    async fn a_create_page_on_a_dead_session_stays_queued() {
        let s = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        seed(&s, &store, &["b"]).await;
        local_create_page(&store, "p", None, "New").unwrap();
        let p = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&p)
            .await;
        let e = push(&private_client(&p), &store).await.unwrap_err();
        assert!(matches!(e, Error::SessionExpired), "{e:?}");
        assert_eq!(store.queued_ops().unwrap().len(), 1);
    }
}
