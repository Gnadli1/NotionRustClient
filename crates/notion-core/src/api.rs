//! Async Notion REST client: rate limiting, retry on 429/529, pagination,
//! recursive block fetch. The only module that talks HTTP.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{header::RETRY_AFTER, Method};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

use crate::model::{Block, BlockKind, Comment, DataSource, Page, SearchResult, User};
use crate::private::{self, PrivateAuth, Records};
use crate::{Error, Result};

pub const NOTION_VERSION: &str = "2025-09-03";

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: String,
    pub requests_per_sec: f64,
    pub burst: u32,
    pub max_retries: u32,
    /// Cap for exponential backoff and for honouring `Retry-After`.
    pub max_backoff: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            base_url: "https://api.notion.com".into(),
            requests_per_sec: 3.0,
            burst: 3,
            max_retries: 5,
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// Token bucket: refills at `rate`/s up to `burst`; waiters queue on the mutex.
struct Limiter {
    rate: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    fn new(rate: f64, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Limiter {
            rate,
            burst,
            state: Mutex::new((burst, Instant::now())),
        }
    }

    async fn acquire(&self) {
        let mut s = self.state.lock().await;
        let now = Instant::now();
        s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * self.rate).min(self.burst);
        s.1 = now;
        if s.0 < 1.0 {
            sleep(Duration::from_secs_f64((1.0 - s.0) / self.rate)).await;
            *s = (1.0, Instant::now());
        }
        s.0 -= 1.0;
    }
}

#[derive(Debug, Clone)]
pub struct BlockNode {
    pub block: Block,
    pub children: Vec<BlockNode>,
}

/// The file of a multipart request.
struct FilePart {
    filename: String,
    mime: &'static str,
    bytes: Vec<u8>,
}

pub struct Client {
    http: reqwest::Client,
    token: String,
    cfg: ClientConfig,
    limiter: Limiter,
    /// Set for a browser-session (`token_v2`) client.
    private: Option<PrivateAuth>,
    private_base: String,
    private_state: std::sync::Mutex<PrivateState>,
}

#[derive(Default)]
struct PrivateState {
    users: HashMap<String, User>,
    /// collection id -> database (collection_view_page/collection_view) block id
    database_for_collection: HashMap<String, String>,
    views_for_collection: HashMap<String, Vec<String>>,
    /// Short-lived browser-file cookie from `GET /f/refresh`.
    file_token: Option<String>,
    /// `token_v2` was refused (401, or 403 from /f/refresh); private calls fail without network.
    expired: bool,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.cfg.base_url)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(token: String, cfg: ClientConfig) -> Self {
        assert!(
            cfg.requests_per_sec.is_finite() && cfg.requests_per_sec > 0.0,
            "requests_per_sec must be finite and > 0"
        );
        Client {
            http: reqwest::Client::new(),
            token,
            limiter: Limiter::new(cfg.requests_per_sec, cfg.burst),
            cfg,
            private: None,
            private_base: private::DEFAULT_BASE.into(),
            private_state: std::sync::Mutex::new(PrivateState::default()),
        }
    }

    /// A client for the private web API, authenticated by the browser's
    /// `token_v2` cookie and scoped to one workspace.
    pub fn new_private(auth: PrivateAuth, cfg: ClientConfig) -> Self {
        let base = std::env::var("NOTION_PRIVATE_BASE")
            .ok()
            .unwrap_or_default();
        Self::new_private_at(base, auth, cfg)
    }

    /// [`Client::new_private`] with an explicit base URL (tests, or
    /// `https://app.notion.com`).
    pub fn new_private_at(base: impl Into<String>, auth: PrivateAuth, cfg: ClientConfig) -> Self {
        assert!(
            cfg.requests_per_sec.is_finite() && cfg.requests_per_sec > 0.0,
            "requests_per_sec must be finite and > 0"
        );
        Client {
            http: reqwest::Client::new(),
            token: String::new(),
            limiter: Limiter::new(cfg.requests_per_sec, cfg.burst),
            cfg,
            private: Some(auth),
            private_base: Some(base.into())
                .filter(|b| private::safe_base(b))
                .unwrap_or_else(|| private::DEFAULT_BASE.into()),
            private_state: std::sync::Mutex::new(PrivateState::default()),
        }
    }

    fn session_expired(&self) -> bool {
        self.private_state.lock().is_ok_and(|s| s.expired)
    }

    fn expire_session(&self) -> Error {
        if let Ok(mut s) = self.private_state.lock() {
            s.expired = true;
        }
        Error::SessionExpired
    }

    /// Whether the `file_token` cookie may be sent to `url`.
    pub fn file_cookie_ok(&self, url: &str) -> bool {
        private::cookie_url_ok(url, &self.private_base)
    }

    pub fn is_private(&self) -> bool {
        self.private.is_some()
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var("NOTION_TOKEN") {
            Ok(t) if !t.is_empty() => Ok(Client::new(t, ClientConfig::default())),
            _ => Err(Error::Invalid("NOTION_TOKEN not set".into())),
        }
    }

    // ------------------------------------------------------ private transport

    async fn private_post(&self, endpoint: &str, body: Value) -> Result<Value> {
        let auth = self
            .private
            .as_ref()
            .ok_or_else(|| Error::Invalid("not a browser-session client".into()))?;
        if self.session_expired() {
            return Err(Error::SessionExpired);
        }
        let url = format!(
            "{}/api/v3/{endpoint}",
            self.private_base.trim_end_matches('/')
        );
        let mut last = None;
        for attempt in 0..=self.cfg.max_retries {
            self.limiter.acquire().await;
            let mut req = self
                .http
                .post(&url)
                .header("Content-Type", "application/json")
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
                )
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("Cookie", format!("token_v2={}", auth.token_v2))
                .header("x-notion-space-id", &auth.space_id)
                .json(&body);
            if let Some(user) = &auth.user_id {
                req = req.header("x-notion-active-user-header", user);
            }
            let resp = req.send().await.map_err(|e| Error::Http(e.without_url()))?;
            let status = resp.status().as_u16();
            let wait = Self::retry_after(&resp);
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Http(e.without_url()))?;
            if (200..300).contains(&status) {
                return Ok(serde_json::from_str(&text)?);
            }
            if status == 401 {
                return Err(self.expire_session());
            }
            if status == 429 || status >= 500 {
                last = Some(status);
                if attempt < self.cfg.max_retries {
                    let backoff = wait
                        .unwrap_or_else(|| Duration::from_millis(400 * 2u64.pow(attempt.min(6))));
                    sleep(backoff.min(self.cfg.max_backoff)).await;
                    continue;
                }
                break;
            }
            let err: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let code = err
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| err.get("error").and_then(Value::as_str))
                .unwrap_or("private_api_error");
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| err.get("error_description").and_then(Value::as_str))
                .unwrap_or("request failed");
            return Err(Error::Api {
                status,
                code: code.to_string(),
                message: message.to_string(),
            });
        }
        let _ = last;
        Err(Error::RetriesExhausted)
    }

    async fn private_records(&self, root: &str) -> Result<Records> {
        let mut records = Records::default();
        // A page chunk returns the page and its descendants (and discussions).
        let mut cursor = json!({ "stack": [] });
        let mut chunk = 0u32;
        let mut first: Option<Result<Value>> = None;
        loop {
            let body = json!({
                "pageId": root,
                "limit": 100,
                "cursor": cursor,
                "chunkNumber": chunk,
                "verticalColumns": false,
            });
            let resp = self.private_post("loadPageChunk", body).await;
            match resp {
                Ok(resp) => {
                    if first.is_none() {
                        first = Some(Ok(resp.clone()));
                    }
                    records.merge_response(&resp);
                    let stack = resp
                        .pointer("/cursor/stack")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if stack.is_empty() {
                        break;
                    }
                    cursor = resp["cursor"].clone();
                    chunk += 1;
                    if chunk > 200 {
                        break;
                    }
                }
                Err(e) => {
                    if first.is_none() {
                        first = Some(Err(e));
                    }
                    break;
                }
            }
        }
        if !records.blocks.contains_key(root) {
            // A block (not a page): fetch the record directly.
            let resp = self
                .private_post(
                    "syncRecordValues",
                    json!({ "requests": [{ "pointer": { "table": "block", "id": root },
                                          "version": -1 }] }),
                )
                .await?;
            records.merge_response(&resp);
        } else if let Some(Err(e)) = first {
            if !records.blocks.contains_key(root) {
                return Err(e);
            }
        }
        for _ in 0..20 {
            let missing = records.missing_content(root);
            if missing.is_empty() {
                break;
            }
            let requests: Vec<Value> = missing
                .iter()
                .take(200)
                .map(|id| json!({ "pointer": { "table": "block", "id": id }, "version": -1 }))
                .collect();
            let resp = self
                .private_post("syncRecordValues", json!({ "requests": requests }))
                .await?;
            records.merge_response(&resp);
        }
        self.remember(&records);
        Ok(records)
    }

    fn remember(&self, records: &Records) {
        let mut state = self.private_state.lock().unwrap();
        for user in records.users() {
            state.users.insert(user.id.clone(), user);
        }
        for (id, block) in &records.blocks {
            let Some(collection) = block.get("collection_id").and_then(Value::as_str) else {
                continue;
            };
            state
                .database_for_collection
                .entry(collection.to_string())
                .or_insert_with(|| id.clone());
            let views: Vec<String> = block
                .get("view_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !views.is_empty() {
                state
                    .views_for_collection
                    .entry(collection.to_string())
                    .or_insert(views);
            }
        }
        let databases: Vec<(String, String)> = state
            .database_for_collection
            .iter()
            .map(|(collection, database)| (collection.clone(), database.clone()))
            .collect();
        for view in records.views.values() {
            let Some(parent) = view.get("parent_id").and_then(Value::as_str) else {
                continue;
            };
            for (collection, database) in &databases {
                if database == parent {
                    if let Some(id) = view.get("id").and_then(Value::as_str) {
                        let views = state
                            .views_for_collection
                            .entry(collection.clone())
                            .or_default();
                        if !views.iter().any(|v| v == id) {
                            views.push(id.to_string());
                        }
                    }
                }
            }
        }
    }

    fn private_auth(&self) -> Result<&PrivateAuth> {
        self.private
            .as_ref()
            .ok_or_else(|| Error::Invalid("not a browser-session client".into()))
    }

    /// Returns the cookie Notion's file host needs for private images.
    /// `GET /f/refresh` exchanges the API session for a `file_token` cookie,
    /// so no browser cookies beyond `token_v2` are needed.
    pub async fn file_token(&self) -> Result<String> {
        if let Some(token) = self
            .private_state
            .lock()
            .ok()
            .and_then(|s| s.file_token.clone())
        {
            return Ok(token);
        }
        let auth = self.private_auth()?;
        if self.session_expired() {
            return Err(Error::SessionExpired);
        }
        let url = format!("{}/f/refresh", self.private_base.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
            )
            .header("Cookie", format!("token_v2={}", auth.token_v2))
            .send()
            .await
            .map_err(|e| Error::Http(e.without_url()))?;
        let status = resp.status().as_u16();
        let cookies: Vec<String> = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        if !(200..300).contains(&status) {
            if status == 401 || status == 403 {
                return Err(self.expire_session());
            }
            return Err(Error::Api {
                status,
                code: "file_token_failed".into(),
                message: "could not get a file token".into(),
            });
        }
        let token = cookies.iter().find_map(|c| {
            c.strip_prefix("file_token=")
                .and_then(|v| v.split(';').next())
                .map(str::to_string)
        });
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Err(Error::Invalid(
                "Notion did not return a file token (the cookie may be expired)".into(),
            ));
        };
        if let Ok(mut state) = self.private_state.lock() {
            state.file_token = Some(token.clone());
        }
        Ok(token)
    }

    /// Drops the cached file token so the next image retries `/f/refresh`.
    pub fn clear_file_token(&self) {
        if let Ok(mut state) = self.private_state.lock() {
            state.file_token = None;
        }
    }

    async fn private_transact(&self, operations: Vec<Value>) -> Result<Value> {
        let space = self.private_auth()?.space_id.clone();
        let body = private::fanout(&space, operations.clone());
        match self.private_post("saveTransactionsFanout", body).await {
            Ok(v) => Ok(v),
            // An older web client base lacks the fanout endpoint; the plain
            // endpoint takes the same transactions. A 400 is a bad operation,
            // so retrying it elsewhere would only hide the real error.
            Err(Error::Api { status: 404, .. }) => {
                self.private_post("saveTransactions", private::fanout(&space, operations))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn private_discussions(&self, records: &mut Records, block_id: &str) -> Result<()> {
        let Some(rec) = records.blocks.get(block_id) else {
            return Ok(());
        };
        let discussions: Vec<String> = rec
            .get("discussions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if discussions.is_empty() {
            return Ok(());
        }
        let requests: Vec<Value> = discussions
            .iter()
            .map(|id| json!({ "pointer": { "table": "discussion", "id": id }, "version": -1 }))
            .collect();
        let resp = self
            .private_post("syncRecordValues", json!({ "requests": requests }))
            .await?;
        records.merge_response(&resp);
        let comment_ids: Vec<String> = discussions
            .iter()
            .filter_map(|id| records.discussions.get(id))
            .flat_map(|d| {
                d.get("comments")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if comment_ids.is_empty() {
            return Ok(());
        }
        let requests: Vec<Value> = comment_ids
            .iter()
            .map(|id| json!({ "pointer": { "table": "comment", "id": id }, "version": -1 }))
            .collect();
        let resp = self
            .private_post("syncRecordValues", json!({ "requests": requests }))
            .await?;
        records.merge_response(&resp);
        Ok(())
    }

    async fn private_record(&self, id: &str) -> Result<Value> {
        let resp = self
            .private_post(
                "syncRecordValues",
                json!({ "requests": [{ "pointer": { "table": "block", "id": id },
                                      "version": -1 }] }),
            )
            .await?;
        let mut records = Records::default();
        records.merge_response(&resp);
        records
            .blocks
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("block {id} is not in the record map")))
    }

    fn private_user_id(&self) -> Option<String> {
        self.private.as_ref().and_then(|a| a.user_id.clone())
    }

    fn private_space_id(&self) -> Result<String> {
        Ok(self.private_auth()?.space_id.clone())
    }

    pub async fn get_page(&self, id: &str) -> Result<Page> {
        if self.is_private() {
            let records = self.private_records(id).await?;
            if let Some(rec) = records.blocks.get(id) {
                if matches!(
                    rec.get("type").and_then(Value::as_str),
                    Some("collection_view_page" | "collection_view")
                ) {
                    // Make callers fall back to the database path, as they do
                    // when the public API says this id is a database.
                    return Err(Error::Api {
                        status: 400,
                        code: "validation_error".into(),
                        message: "object is a database".into(),
                    });
                }
            }
            return records.page(id);
        }
        self.call(Method::GET, &format!("/v1/pages/{id}"), None)
            .await
    }

    pub async fn get_block(&self, id: &str) -> Result<Block> {
        if self.is_private() {
            let resp = self
                .private_post(
                    "syncRecordValues",
                    json!({ "requests": [{ "pointer": { "table": "block", "id": id },
                                          "version": -1 }] }),
                )
                .await?;
            let mut records = Records::default();
            records.merge_response(&resp);
            self.remember(&records);
            return records.block(id);
        }
        self.call(Method::GET, &format!("/v1/blocks/{id}"), None)
            .await
    }

    pub async fn block_children(&self, id: &str) -> Result<Vec<Block>> {
        if self.is_private() {
            let records = self.private_records(id).await?;
            return records.children(id);
        }
        self.paginate(&format!("/v1/blocks/{id}/children"), None)
            .await
    }

    /// Children of `id`, recursively. child_page / child_database blocks are
    /// leaves: their "children" are another page or database.
    pub async fn block_tree(&self, id: &str) -> Result<Vec<BlockNode>> {
        if self.is_private() {
            let records = self.private_records(id).await?;
            let mut nodes = Vec::new();
            for child in records.children(id)? {
                nodes.push(self.private_node(&records, child)?);
            }
            return Ok(nodes);
        }
        use futures::{stream, StreamExt, TryStreamExt};
        // siblings' subtrees load a few at a time; `buffered` keeps their order
        stream::iter(self.block_children(id).await?)
            .map(|block| async move {
                let leaf = matches!(
                    block.kind,
                    BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
                );
                let children = if block.has_children && !leaf {
                    Box::pin(self.block_tree(&block.id)).await?
                } else {
                    Vec::new()
                };
                Ok(BlockNode { block, children })
            })
            .buffered(4)
            .try_collect()
            .await
    }

    fn private_node(&self, records: &Records, block: Block) -> Result<BlockNode> {
        let leaf = matches!(
            block.kind,
            BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
        );
        let children = if block.has_children && !leaf {
            records
                .children(&block.id)?
                .into_iter()
                .map(|child| self.private_node(records, child))
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Ok(BlockNode { block, children })
    }

    pub async fn append_children(
        &self,
        parent: &str,
        children: &[Block],
        after: Option<&str>,
    ) -> Result<Vec<Block>> {
        if self.is_private() {
            let mut out = Vec::new();
            let mut after = after.map(str::to_string);
            for child in children {
                let body = child.kind.to_request_json()?;
                let block = self.append_raw(parent, &body, after.as_deref()).await?;
                after = Some(block.id.clone());
                out.push(block);
            }
            return Ok(out);
        }
        // `BlockKind::to_request_json` refuses `_width`, so none reaches the public API
        let children = children
            .iter()
            .map(|b| b.kind.to_request_json())
            .collect::<Result<Vec<_>>>()?;
        let path = format!("/v1/blocks/{parent}/children");
        let mut out: Vec<Block> = Vec::new();
        // Notion caps 100 children per request; chain chunks via `after`.
        for chunk in children.chunks(100) {
            let mut body = json!({ "children": chunk });
            if let Some(a) = out.last().map(|b| b.id.as_str()).or(after) {
                body["after"] = json!(a);
            }
            out.extend(results::<Block>(
                self.send(Method::PATCH, &path, &[], Some(&body)).await?,
            )?);
        }
        Ok(out)
    }

    /// Appends one request-shaped block (`{type, <type>: …}`) that
    /// `BlockKind` can't write, e.g. an image from a file upload.
    pub async fn append_raw(
        &self,
        parent: &str,
        block: &Value,
        after: Option<&str>,
    ) -> Result<Block> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let user = self.private_user_id();
            let id = private::new_id();
            let rec = private::block_record_from_official(
                &id,
                block,
                parent,
                "block",
                &space,
                user.as_deref(),
            )?;
            let mut ops = vec![private::set_op(&id, &space, json!([]), rec.clone())];
            let args = match after {
                Some(after) => json!({ "id": &id, "after": after }),
                None => json!({ "id": &id }),
            };
            ops.push(private::list_op(
                parent,
                &space,
                "content",
                "listAfter",
                args,
            ));
            self.private_transact(ops).await?;
            let v = private::official_block(&rec)?;
            return crate::model::block_from_json(v);
        }
        let mut body = json!({ "children": [block] });
        if let Some(a) = after {
            body["after"] = json!(a);
        }
        let path = format!("/v1/blocks/{parent}/children");
        let resp = self.send(Method::PATCH, &path, &[], Some(&body)).await?;
        results::<Block>(resp)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("Notion returned no block".into()))
    }

    /// Signs an `attachment:<id>:<name>` URL from a private block/page record.
    /// The signed URL is short-lived and must not be stored.
    pub async fn signed_file_url(&self, block_id: &str, url: &str) -> Result<String> {
        if !self.is_private() {
            return Err(Error::Invalid(
                "attachments need a browser-session client".into(),
            ));
        }
        let resp = self
            .private_post(
                "getSignedFileUrls",
                json!({
                    "urls": [{
                        "url": url,
                        "permissionRecord": { "table": "block", "id": block_id },
                    }]
                }),
            )
            .await?;
        resp.get("signedUrls")
            .and_then(Value::as_array)
            .and_then(|urls| urls.first())
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Invalid("Notion did not sign the file URL".into()))
    }

    /// Uploads a local file (single part, at most 20 MB) and returns the file
    /// upload id to attach to a block: create the upload, then send the bytes
    /// as multipart. Refuses a missing or too large file before any request.
    pub async fn upload_file(&self, path: &std::path::Path) -> Result<String> {
        if self.is_private() {
            let (filename, mime) = crate::media::check_upload(path)?;
            use std::io::Read as _;
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .and_then(|f| f.take(crate::media::MAX_UPLOAD + 1).read_to_end(&mut bytes))
                .map_err(|e| Error::Invalid(format!("cannot read {}: {e}", path.display())))?;
            if bytes.len() as u64 > crate::media::MAX_UPLOAD {
                return Err(Error::Invalid(format!(
                    "{} is over the 20 MB upload limit",
                    path.display()
                )));
            }
            let created = self
                .private_post(
                    "getUploadFileUrl",
                    json!({ "bucket": "secure", "name": filename, "contentType": mime }),
                )
                .await?;
            let put_url = created
                .get("signedPutUrl")
                .or_else(|| created.get("signedUploadUrl"))
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("upload URL has no signed PUT URL".into()))?;
            self.limiter.acquire().await;
            let resp = self
                .http
                .put(put_url)
                .header("Content-Type", mime)
                .body(bytes)
                .send()
                .await
                .map_err(|e| Error::Http(e.without_url()))?;
            if !resp.status().is_success() {
                return Err(Error::Invalid(format!(
                    "upload failed with HTTP {}",
                    resp.status().as_u16()
                )));
            }
            // `url` is the stable file reference; `signedGetUrl` expires.
            return Ok(created
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("upload URL has no file URL".into()))?
                .to_string());
        }
        let (filename, mime) = crate::media::check_upload(path)?;
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(crate::media::MAX_UPLOAD + 1).read_to_end(&mut bytes))
            .map_err(|e| Error::Invalid(format!("cannot read {}: {e}", path.display())))?;
        if bytes.len() as u64 > crate::media::MAX_UPLOAD {
            return Err(Error::Invalid(format!(
                "{} is over the 20 MB upload limit",
                path.display()
            )));
        }
        let created = self
            .send(
                Method::POST,
                "/v1/file_uploads",
                &[],
                Some(&json!({ "mode": "single_part", "filename": filename, "content_type": mime })),
            )
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| Error::Invalid("file upload has no id".into()))?
            .to_string();
        let part = FilePart {
            filename,
            mime,
            bytes,
        };
        self.send_with(
            Method::POST,
            &format!("/v1/file_uploads/{id}/send"),
            &[],
            None,
            Some(&part),
        )
        .await?;
        Ok(id)
    }

    pub async fn update_block(&self, id: &str, body: &Value) -> Result<Block> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let user = self.private_user_id();
            let current = self.private_record(id).await?;
            let ptype = current.get("type").and_then(Value::as_str).unwrap_or("");
            let mut ops = Vec::new();
            if ptype == "table_row" {
                let parent_id = current
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let parent = self.private_record(parent_id).await?;
                let columns: Vec<String> = parent
                    .pointer("/format/table_block_column_order")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        current
                            .get("properties")
                            .and_then(Value::as_object)
                            .map(|o| o.keys().cloned().collect())
                            .unwrap_or_default()
                    });
                let cells = body
                    .pointer("/table_row/cells")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (i, col) in columns.iter().enumerate() {
                    if let Some(cell) = cells.get(i) {
                        ops.push(private::set_op(
                            id,
                            &space,
                            json!(["properties", col]),
                            private::rich_text_to_private(cell),
                        ));
                    }
                }
            } else {
                let parent_id = current
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let parent_table = current
                    .get("parent_table")
                    .and_then(Value::as_str)
                    .unwrap_or("block");
                let rec = private::block_record_from_official(
                    id,
                    body,
                    parent_id,
                    parent_table,
                    &space,
                    user.as_deref(),
                )?;
                for (key, value) in rec
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    ops.push(private::set_op(
                        id,
                        &space,
                        json!(["properties", key]),
                        value.clone(),
                    ));
                }
                for (key, value) in rec
                    .get("format")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    ops.push(private::set_op(
                        id,
                        &space,
                        json!(["format", key]),
                        value.clone(),
                    ));
                }
            }
            if !ops.is_empty() {
                self.private_transact(ops).await?;
            }
            return self.get_block(id).await;
        }
        // `_width` is a private-only extra the public API would reject
        let mut body = body.clone();
        if let Some(p) = body.as_object_mut().into_iter().flat_map(|o| o.values_mut()).find_map(Value::as_object_mut) {
            p.remove("_width");
            if p.is_empty() {
                // only the width was set: nothing for the public API
                return self.get_block(id).await;
            }
        }
        self.call(Method::PATCH, &format!("/v1/blocks/{id}"), Some(&body))
            .await
    }

    pub async fn delete_block(&self, id: &str) -> Result<Block> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let current = self.private_record(id).await?;
            let parent_id = current
                .get("parent_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parent_table = current
                .get("parent_table")
                .and_then(Value::as_str)
                .unwrap_or("block");
            let mut ops = vec![private::update_op(
                id,
                &space,
                json!([]),
                json!({ "alive": false }),
            )];
            if parent_table == "block" || parent_table == "page" {
                ops.push(private::list_op(
                    parent_id,
                    &space,
                    "content",
                    "listRemove",
                    json!({ "id": id }),
                ));
            }
            self.private_transact(ops).await?;
            return self.get_block(id).await;
        }
        self.call(Method::DELETE, &format!("/v1/blocks/{id}"), None)
            .await
    }

    pub async fn update_page_properties(&self, id: &str, props: &Value) -> Result<Page> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let current = self.private_record(id).await?;
            let parent_id = current
                .get("parent_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let collection = (current.get("parent_table").and_then(Value::as_str)
                == Some("collection"))
            .then_some(parent_id.as_str());
            let schema = match collection {
                Some(collection) => self.private_collection_schema(collection).await?,
                None => Map::new(),
            };
            let mut private_props = Map::new();
            for (key, value) in props.as_object().into_iter().flatten() {
                let entry = schema.get(key).cloned().or_else(|| {
                    (key == "title" || value.get("title").is_some())
                        .then(|| json!({ "id": "title", "type": "title" }))
                });
                let entry =
                    entry.ok_or_else(|| Error::Invalid(format!("unknown property `{key}`")))?;
                private_props.insert(key.clone(), private::property_to_private(&entry, value)?);
            }
            if !private_props.is_empty() {
                let ops = private_props
                    .into_iter()
                    .map(|(key, value)| {
                        private::set_op(id, &space, json!(["properties", key]), value)
                    })
                    .collect();
                self.private_transact(ops).await?;
            }
            return self.get_page(id).await;
        }
        let body = json!({ "properties": props });
        self.call(Method::PATCH, &format!("/v1/pages/{id}"), Some(&body))
            .await
    }

    /// `POST /v1/pages`. Raw: Notion may answer with just `{object, id}`.
    pub async fn create_page(&self, body: &Value) -> Result<Value> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let user = self.private_user_id();
            let parent = body.get("parent").cloned().unwrap_or(Value::Null);
            let parent_type = parent
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("page_id");
            let (parent_id, parent_table) = match parent_type {
                "page_id" => (
                    parent
                        .get("page_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    "block",
                ),
                "data_source_id" => (
                    parent
                        .get("data_source_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    "collection",
                ),
                other => {
                    return Err(Error::Invalid(format!(
                        "browser sessions cannot create a page under `{other}`"
                    )))
                }
            };
            let id = private::new_id();
            let official_props = body.get("properties").cloned().unwrap_or_else(|| json!({}));
            let schema = if parent_table == "collection" {
                self.private_collection_schema(&parent_id).await?
            } else {
                Map::new()
            };
            let mut props = Map::new();
            if parent_table == "collection" {
                for (key, entry) in &schema {
                    if let Some(value) = official_props.get(key) {
                        props.insert(key.clone(), private::property_to_private(entry, value)?);
                    } else if entry.get("type").and_then(Value::as_str) == Some("title") {
                        props.insert(key.clone(), json!([[""]]));
                    }
                }
            } else {
                let title = official_props
                    .pointer("/title/title")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                props.insert("title".into(), private::rich_text_to_private(&title));
            }
            let now = private::now_ms();
            let mut rec = json!({
                "id": id,
                "version": 1,
                "type": "page",
                "alive": true,
                "parent_id": parent_id,
                "parent_table": parent_table,
                "space_id": space,
                "content": [],
                "properties": props,
                "created_time": now,
                "last_edited_time": now,
                "created_by_id": user,
                "created_by_table": "notion_user",
                "last_edited_by_id": user,
                "last_edited_by_table": "notion_user",
            });
            if let Some(icon) = body.get("icon") {
                let icon = icon
                    .get("emoji")
                    .and_then(Value::as_str)
                    .or_else(|| icon.pointer("/external/url").and_then(Value::as_str));
                if let Some(icon) = icon {
                    rec["format"] = json!({ "page_icon": icon });
                }
            }
            let mut ops = vec![private::set_op(&id, &space, json!([]), rec.clone())];
            if parent_table == "block" {
                ops.push(private::list_op(
                    &parent_id,
                    &space,
                    "content",
                    "listAfter",
                    json!({ "id": &id }),
                ));
            }
            self.private_transact(ops).await?;
            return Ok(json!({ "object": "page", "id": id }));
        }
        self.send(Method::POST, "/v1/pages", &[], Some(body)).await
    }

    /// `PATCH /v1/pages/{id}` with `icon`, `cover`, `in_trash`, `is_locked`.
    pub async fn update_page(&self, id: &str, body: &Value) -> Result<Page> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let mut format = Map::new();
            let mut update = Map::new();
            if let Some(icon) = body.get("icon") {
                let icon = icon
                    .get("emoji")
                    .and_then(Value::as_str)
                    .or_else(|| icon.pointer("/external/url").and_then(Value::as_str));
                format.insert("page_icon".into(), json!(icon));
            }
            if let Some(cover) = body.get("cover") {
                let url = cover
                    .pointer("/external/url")
                    .and_then(Value::as_str)
                    .or_else(|| cover.pointer("/file/url").and_then(Value::as_str));
                format.insert("page_cover".into(), json!(url));
            }
            if let Some(locked) = body.get("is_locked").and_then(Value::as_bool) {
                format.insert("block_locked".into(), json!(locked));
            }
            for key in ["in_trash", "archived"] {
                if let Some(trashed) = body.get(key).and_then(Value::as_bool) {
                    update.insert("alive".into(), json!(!trashed));
                }
            }
            let mut ops = Vec::new();
            for (key, value) in format {
                ops.push(private::set_op(id, &space, json!(["format", key]), value));
            }
            if !update.is_empty() {
                ops.push(private::update_op(
                    id,
                    &space,
                    json!([]),
                    Value::Object(update),
                ));
            }
            if !ops.is_empty() {
                self.private_transact(ops).await?;
            }
            return self.get_page(id).await;
        }
        self.call(Method::PATCH, &format!("/v1/pages/{id}"), Some(body))
            .await
    }

    /// `POST /v1/pages/{id}/move` under the page `parent`.
    pub async fn move_page(&self, id: &str, parent: &str) -> Result<Value> {
        if self.is_private() {
            let space = self.private_space_id()?;
            // reject moving a page under itself or one of its descendants
            let mut cur = parent.to_string();
            let mut rooted = false;
            for _ in 0..64 {
                if cur == id {
                    return Err(Error::Invalid("cannot move a page under itself".into()));
                }
                let rec = self.private_record(&cur).await?;
                if rec.get("parent_table").and_then(Value::as_str) != Some("block") {
                    rooted = true;
                    break;
                }
                cur = rec
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            if !rooted {
                return Err(Error::Invalid("page tree too deep to move safely".into()));
            }
            let current = self.private_record(id).await?;
            let old_parent = current
                .get("parent_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut ops = Vec::new();
            match current.get("parent_table").and_then(Value::as_str) {
                Some("block") if !old_parent.is_empty() => ops.push(private::list_op(
                    old_parent,
                    &space,
                    "content",
                    "listRemove",
                    json!({ "id": id }),
                )),
                Some("collection") => {
                    return Err(Error::Invalid(
                        "a database row can't be moved out of its database here".into(),
                    ))
                }
                // a space/workspace root page has no `content` list to remove from
                _ => {}
            }
            ops.push(private::update_op(
                id,
                &space,
                json!([]),
                json!({ "parent_id": parent, "parent_table": "block" }),
            ));
            ops.push(private::list_op(
                parent,
                &space,
                "content",
                "listAfter",
                json!({ "id": id }),
            ));
            self.private_transact(ops).await?;
            return Ok(json!({ "object": "page", "id": id }));
        }
        let body = json!({ "parent": { "type": "page_id", "page_id": parent } });
        self.send(
            Method::POST,
            &format!("/v1/pages/{id}/move"),
            &[],
            Some(&body),
        )
        .await
    }

    async fn private_collection_schema(&self, collection_id: &str) -> Result<Map<String, Value>> {
        let resp = self
            .private_post(
                "syncRecordValues",
                json!({ "requests": [{ "pointer": { "table": "collection", "id": collection_id },
                                      "version": -1 }] }),
            )
            .await?;
        let mut records = Records::default();
        records.merge_response(&resp);
        let collection = records
            .collections
            .get(collection_id)
            .ok_or_else(|| Error::Invalid(format!("collection {collection_id} not found")))?;
        Ok(collection
            .get("schema")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    /// `GET /v1/databases/{id}`: its `data_sources` are `[{id, name}]`. Raw.
    pub async fn get_database(&self, id: &str) -> Result<Value> {
        if self.is_private() {
            let records = self.private_records(id).await?;
            let rec = records
                .blocks
                .get(id)
                .ok_or_else(|| Error::Invalid(format!("database {id} is not in the record map")))?;
            let collection = rec
                .get("collection_id")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            let title = records.collection_title_rich(&collection);
            let name = records
                .collection_title(&collection)
                .unwrap_or_else(|| "Database".into());
            return Ok(json!({
                "object": "database",
                "id": id,
                "title": title,
                "data_sources": [{ "id": collection, "name": name }],
                "last_edited_time": records
                    .blocks
                    .get(id)
                    .and_then(|b| b.get("last_edited_time"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }));
        }
        self.send(Method::GET, &format!("/v1/databases/{id}"), &[], None)
            .await
    }

    pub async fn get_data_source(&self, id: &str) -> Result<DataSource> {
        if self.is_private() {
            let database = self
                .private_state
                .lock()
                .unwrap()
                .database_for_collection
                .get(id)
                .cloned();
            let mut records = match database {
                Some(database) => self.private_records(&database).await?,
                None => Records::default(),
            };
            if !records.collections.contains_key(id) {
                let resp = self
                    .private_post(
                        "syncRecordValues",
                        json!({ "requests": [{ "pointer": { "table": "collection", "id": id },
                                              "version": -1 }] }),
                    )
                    .await?;
                records.merge_response(&resp);
            }
            return records.data_source(id);
        }
        self.call(Method::GET, &format!("/v1/data_sources/{id}"), None)
            .await
    }

    pub async fn query_data_source(&self, id: &str, filter_sort: Value) -> Result<Vec<Page>> {
        if self.is_private() {
            let auth = self.private_auth()?.clone();
            let (view, database) = {
                let state = self.private_state.lock().unwrap();
                (
                    state
                        .views_for_collection
                        .get(id)
                        .and_then(|v| v.first().cloned()),
                    state.database_for_collection.get(id).cloned(),
                )
            };
            let view = match view {
                Some(view) => view,
                None => {
                    if let Some(database) = database {
                        let _ = self.private_records(&database).await?;
                    }
                    self.private_state
                        .lock()
                        .unwrap()
                        .views_for_collection
                        .get(id)
                        .and_then(|v| v.first().cloned())
                        .ok_or_else(|| {
                            Error::Invalid(format!(
                                "no view found for collection {id}; open the database first"
                            ))
                        })?
                }
            };
            let mut loader = json!({
                "type": "reducer",
                "reducers": { "collection_group_results": { "type": "results", "limit": 999 } },
                "sort": [],
                "filter": {},
                "searchQuery": "",
                "userTimeZone": "UTC",
            });
            if let Some(obj) = filter_sort.as_object() {
                if let Some(sort) = obj.get("sorts") {
                    loader["sort"] = sort.clone();
                }
                if let Some(filter) = obj.get("filter") {
                    loader["filter"] = filter.clone();
                }
            }
            let resp = self
                .private_post(
                    "queryCollection",
                    json!({
                        "collection": { "id": id, "spaceId": auth.space_id },
                        "collectionView": { "id": view, "spaceId": auth.space_id },
                        "source": { "id": id, "spaceId": auth.space_id, "type": "collection" },
                        "loader": loader,
                    }),
                )
                .await?;
            let mut records = Records::default();
            records.merge_response(&resp);
            self.remember(&records);
            let ids: Vec<String> = resp
                .pointer("/result/reducerResults/collection_group_results/blockIds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let ids = if ids.is_empty() {
                records
                    .blocks
                    .values()
                    .filter(|b| {
                        b.get("parent_id").and_then(Value::as_str) == Some(id)
                            && b.get("parent_table").and_then(Value::as_str) == Some("collection")
                    })
                    .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            } else {
                ids
            };
            let mut pages = Vec::new();
            for id in ids {
                if let Ok(page) = records.page(&id) {
                    pages.push(page);
                }
            }
            return Ok(pages);
        }
        let body = match filter_sort {
            Value::Null => json!({}),
            v @ Value::Object(_) => v,
            _ => return Err(Error::Invalid("query body must be an object".into())),
        };
        self.paginate(&format!("/v1/data_sources/{id}/query"), Some(body))
            .await
    }

    /// The saved views of a data source (`GET /v1/views?data_source_id=`), raw.
    /// UNVERIFIED against live Notion: the endpoint shape is a guess and may
    /// need a newer Notion-Version; any error makes the callers fall back to
    /// local views.
    pub async fn list_views(&self, data_source_id: &str) -> Result<Vec<Value>> {
        if self.is_private() {
            let views = self
                .private_state
                .lock()
                .unwrap()
                .views_for_collection
                .get(data_source_id)
                .cloned()
                .unwrap_or_default();
            if views.is_empty() {
                return Ok(Vec::new());
            }
            let requests: Vec<Value> = views
                .iter()
                .map(|id| {
                    json!({ "pointer": { "table": "collection_view", "id": id },
                                  "version": -1 })
                })
                .collect();
            let resp = self
                .private_post("syncRecordValues", json!({ "requests": requests }))
                .await?;
            let mut records = Records::default();
            records.merge_response(&resp);
            self.remember(&records);
            return Ok(records
                .views
                .values()
                .filter_map(|v| {
                    let id = v.get("id").and_then(Value::as_str)?;
                    if !views.iter().any(|view| view == id) {
                        return None;
                    }
                    Some(json!({
                        "object": "view",
                        "id": id,
                        "name": v.get("name").cloned().unwrap_or(Value::Null),
                        "type": v.get("type").and_then(Value::as_str).map(|t| match t {
                            "table" | "gallery" | "list" | "board" | "calendar" | "timeline"
                                | "form" | "chart" | "map" | "dashboard" => t,
                            _ => "table",
                        }).unwrap_or("table"),
                        "created_time": v.get("created_time").cloned().unwrap_or(Value::Null),
                        "last_edited_time": v.get("last_edited_time").cloned().unwrap_or(Value::Null),
                    }))
                })
                .collect());
        }
        self.paginate_with("/v1/views", None, &[("data_source_id", data_source_id)])
            .await
    }

    /// Unresolved comments on a page or block, all pages. Needs the
    /// integration's "read comments" capability (403 otherwise).
    pub async fn list_comments(&self, block_or_page_id: &str) -> Result<Vec<Comment>> {
        if self.is_private() {
            let mut records = self.private_records(block_or_page_id).await?;
            self.private_discussions(&mut records, block_or_page_id)
                .await?;
            return Ok(records.comments_for(block_or_page_id));
        }
        self.paginate(&format!("/v1/comments?block_id={block_or_page_id}"), None)
            .await
    }

    pub async fn get_comment(&self, id: &str) -> Result<Comment> {
        if self.is_private() {
            let resp = self
                .private_post(
                    "syncRecordValues",
                    json!({ "requests": [{ "pointer": { "table": "comment", "id": id },
                                          "version": -1 }] }),
                )
                .await?;
            let mut records = Records::default();
            records.merge_response(&resp);
            let comment =
                records.comments.get(id).cloned().ok_or_else(|| {
                    Error::Invalid(format!("comment {id} is not in the record map"))
                })?;
            let discussion_id = comment
                .get("parent_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let disc_resp = self
                .private_post(
                    "syncRecordValues",
                    json!({ "requests": [{ "pointer": { "table": "discussion", "id": discussion_id },
                                          "version": -1 }] }),
                )
                .await?;
            records.merge_response(&disc_resp);
            let discussion = records
                .discussions
                .get(&discussion_id)
                .ok_or_else(|| Error::Invalid("discussion is missing".into()))?;
            let v = private::comment_json(&records, &discussion_id, discussion, &comment)?;
            return crate::model::comment_from_json(v);
        }
        self.call(Method::GET, &format!("/v1/comments/{id}"), None)
            .await
    }

    /// `body` holds exactly one target (`parent` or `discussion_id`) and `rich_text`.
    /// Returns the raw response: with insert-only capability Notion sends just
    /// `{object, id}`, which isn't a full [`Comment`].
    pub async fn create_comment(&self, body: &Value) -> Result<Value> {
        if self.is_private() {
            let space = self.private_space_id()?;
            let user = self.private_user_id().unwrap_or_else(|| "me".into());
            let parent = body.get("parent").cloned().unwrap_or(Value::Null);
            let block_id = parent
                .get("page_id")
                .or_else(|| parent.get("block_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid("comment has no page/block parent".into()))?;
            let rich_text = body.get("rich_text").cloned().unwrap_or_else(|| json!([]));
            let discussion_id = private::new_id();
            let comment_id = private::new_id();
            let now = private::now_ms();
            let discussion = json!({
                "id": discussion_id,
                "version": 1,
                "parent_id": block_id,
                "parent_table": "block",
                "resolved": false,
                "alive": true,
                "comments": [comment_id],
                "created_time": now,
                "last_edited_time": now,
            });
            let comment = json!({
                "id": comment_id,
                "version": 1,
                "alive": true,
                "parent_id": discussion_id,
                "parent_table": "discussion",
                "created_by": user,
                "created_time": now,
                "last_edited_time": now,
                "text": private::rich_text_to_private(&rich_text),
            });
            let ops = vec![
                private::set_op_table("discussion", &discussion_id, &space, json!([]), discussion),
                private::set_op_table("comment", &comment_id, &space, json!([]), comment),
                private::list_op(
                    block_id,
                    &space,
                    "discussions",
                    "listAfter",
                    json!({ "id": discussion_id }),
                ),
            ];
            self.private_transact(ops).await?;
            return Ok(json!({
                "object": "comment",
                "id": comment_id,
                "discussion_id": discussion_id,
                "parent": { "type": "block_id", "block_id": block_id },
                "created_time": crate::private::now_ms(),
                "last_edited_time": crate::private::now_ms(),
                "created_by": { "object": "user", "id": user },
                "rich_text": rich_text,
                "resolved": false,
            }));
        }
        self.send(Method::POST, "/v1/comments", &[], Some(body))
            .await
    }

    /// Only comments created by this integration can be edited (else 404).
    pub async fn update_comment(&self, id: &str, rich_text: &Value) -> Result<Value> {
        if self.is_private() {
            let space = self.private_space_id()?;
            self.private_transact(vec![private::update_op_table(
                "comment",
                id,
                &space,
                json!([]),
                json!({ "text": private::rich_text_to_private(rich_text),
                        "last_edited_time": private::now_ms() }),
            )])
            .await?;
            return self
                .get_comment(id)
                .await
                .map(|c| serde_json::to_value(c).unwrap_or(Value::Null));
        }
        let body = json!({ "rich_text": rich_text });
        self.send(
            Method::PATCH,
            &format!("/v1/comments/{id}"),
            &[],
            Some(&body),
        )
        .await
    }

    /// Only comments created by this integration can be deleted (else 404).
    pub async fn delete_comment(&self, id: &str) -> Result<()> {
        if self.is_private() {
            let space = self.private_space_id()?;
            self.private_transact(vec![private::update_op_table(
                "comment",
                id,
                &space,
                json!([]),
                json!({ "alive": false }),
            )])
            .await?;
            return Ok(());
        }
        self.send(Method::DELETE, &format!("/v1/comments/{id}"), &[], None)
            .await
            .map(drop)
    }

    /// Who can open `page_id` (browser sessions only; `Invalid` otherwise).
    pub async fn page_access(&self, page_id: &str) -> Result<crate::model::Access> {
        self.private_auth()?;
        let mut access = crate::model::Access::default();
        let (mut table, mut id) = ("block".to_string(), page_id.to_string());
        for _ in 0..32 {
            let resp = self
                .private_post(
                    "syncRecordValues",
                    json!({ "requests": [{ "pointer": { "table": table, "id": id },
                                          "version": -1 }] }),
                )
                .await?;
            let mut records = Records::default();
            records.merge_response(&resp);
            let Some(rec) = records
                .blocks
                .get(&id)
                .or_else(|| records.collections.get(&id))
            else {
                break;
            };
            for p in rec
                .get("permissions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(role) = p
                    .get("role")
                    .and_then(Value::as_str)
                    .filter(|r| *r != "none")
                else {
                    continue;
                };
                let role = role.to_string();
                match p.get("type").and_then(Value::as_str) {
                    Some("user_permission") => {
                        if let Some(u) = p.get("user_id").and_then(Value::as_str) {
                            if !access.people.iter().any(|(x, _)| x == u) {
                                access.people.push((u.to_string(), role));
                            }
                        }
                    }
                    Some("space_permission") => {
                        access.workspace.get_or_insert(role);
                    }
                    Some("public_permission") => {
                        access.public.get_or_insert(role);
                    }
                    _ => {}
                }
            }
            let parent = rec.get("parent_id").and_then(Value::as_str);
            let parent_table = rec.get("parent_table").and_then(Value::as_str);
            match (parent, parent_table) {
                (Some(p), Some(t @ ("block" | "collection"))) => {
                    id = p.to_string();
                    table = t.to_string();
                }
                _ => break,
            }
        }
        if !access.people.is_empty() {
            let requests: Vec<Value> = access
                .people
                .iter()
                .map(|(u, _)| json!({ "pointer": { "table": "notion_user", "id": u }, "version": -1 }))
                .collect();
            // names and photos only: without them the bar still shows initials
            if let Ok(resp) = self
                .private_post("syncRecordValues", json!({ "requests": requests }))
                .await
            {
                let mut records = Records::default();
                records.merge_response(&resp);
                access.users = records.users();
            }
        }
        Ok(access)
    }

    /// Shares `page_id` with workspace member `user_id` at `role` (browser sessions only).
    pub async fn share_page(&self, page_id: &str, user_id: &str, role: &str) -> Result<()> {
        self.private_auth()?;
        // Notion's ids: lowercase 8-4-4-4-12
        let uuid = |s: &str| {
            s.len() == 36
                && s.bytes().enumerate().all(|(i, b)| match i {
                    8 | 13 | 18 | 23 => b == b'-',
                    _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
                })
        };
        if !["editor", "read_and_write", "comment_only", "reader"].contains(&role)
            || !uuid(page_id)
            || !uuid(user_id)
        {
            return Err(Error::Invalid("bad page id, user id or role".into()));
        }
        let space = self.private_space_id()?;
        self.private_transact(vec![private::list_op(
            page_id,
            &space,
            "permissions",
            "setPermissionItem",
            json!({ "type": "user_permission", "role": role, "user_id": user_id }),
        )])
        .await
        .map(drop)
    }

    /// The integration's own (bot) user id: `created_by` of comments made here.
    pub async fn me(&self) -> Result<String> {
        Ok(self.me_identity().await?.0)
    }

    /// [`me`](Self::me) plus the person who owns the integration (`bot.owner`
    /// of type "user"; None if the workspace owns it): the "you" of the inbox.
    pub async fn me_identity(&self) -> Result<(String, Option<String>)> {
        if self.is_private() {
            let id = self
                .private_auth()?
                .user_id
                .clone()
                .unwrap_or_else(|| "me".to_string());
            return Ok((id.clone(), Some(id)));
        }
        let v = self.send(Method::GET, "/v1/users/me", &[], None).await?;
        let id = v["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Invalid("users/me has no id".into()))?;
        let owner = &v["bot"]["owner"];
        let owner = (owner["type"] == "user")
            .then(|| owner["user"]["id"].as_str().map(str::to_string))
            .flatten();
        Ok((id, owner))
    }

    /// Every workspace user, persons and bots, all pages. Needs the
    /// integration's "read user information" capability (403 otherwise).
    pub async fn list_users(&self) -> Result<Vec<User>> {
        if self.is_private() {
            return Ok(self
                .private_state
                .lock()
                .unwrap()
                .users
                .values()
                .cloned()
                .collect());
        }
        let raw: Vec<Value> = self.paginate("/v1/users", None).await?;
        Ok(raw.iter().filter_map(User::from_value).collect())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        if self.is_private() {
            let auth = self.private_auth()?.clone();
            let body = json!({
                "type": "BlocksInSpace",
                "query": query,
                "spaceId": auth.space_id,
                "limit": 100,
                "filters": {
                    "isDeletedOnly": false,
                    "excludeTemplates": false,
                    "isNavigableOnly": false,
                    "requireEditPermissions": false,
                    "ancestors": [],
                    "createdBy": [],
                    "editedBy": [],
                    "lastEditedTime": {},
                    "createdTime": {},
                },
                "sort": { "field": "relevance" },
                "source": "quick_find",
            });
            // `BlocksInSpace` with an empty query returns every page the account
            // can reach, including pages in shared workspaces. `searchPagesWithParent`
            // only lists the top-level pages of the current one, so it is the fallback.
            let resp = match self.private_post("search", body).await {
                Ok(resp) => resp,
                Err(e) if query.trim().is_empty() => {
                    match self
                        .private_post(
                            "searchPagesWithParent",
                            json!({ "query": "", "parentId": auth.space_id,
                                    "limit": 100, "spaceId": auth.space_id }),
                        )
                        .await
                    {
                        Ok(resp) => resp,
                        Err(_) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            };
            let mut records = Records::default();
            records.merge_response(&resp);
            self.remember(&records);
            let mut out = Vec::new();
            for id in resp
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().or_else(|| r.get("id").and_then(Value::as_str)))
            {
                if let Ok(result) = records.search_result(id) {
                    out.push(result);
                }
            }
            return Ok(out);
        }
        self.paginate("/v1/search", Some(json!({ "query": query })))
            .await
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        Ok(serde_json::from_value(
            self.send(method, path, &[], body).await?,
        )?)
    }

    /// All pages of a list endpoint. `body: None` → GET with query params,
    /// `Some` → POST with `page_size`/`start_cursor` merged into the body.
    async fn paginate<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<Value>,
    ) -> Result<Vec<T>> {
        self.paginate_with(path, body, &[]).await
    }

    /// `paginate` with extra GET query pairs (url-encoded by reqwest).
    async fn paginate_with<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = match &body {
                Some(b) => {
                    let mut b = b.clone();
                    b["page_size"] = json!(100);
                    if let Some(c) = &cursor {
                        b["start_cursor"] = json!(c);
                    }
                    self.send(Method::POST, path, &[], Some(&b)).await?
                }
                None => {
                    let mut q = vec![("page_size", "100")];
                    q.extend_from_slice(extra);
                    if let Some(c) = &cursor {
                        q.push(("start_cursor", c));
                    }
                    self.send(Method::GET, path, &q, None).await?
                }
            };
            let next = resp["next_cursor"].as_str().map(str::to_string);
            let more = resp["has_more"].as_bool().unwrap_or(false);
            if resp["request_status"]["type"]
                .as_str()
                .is_some_and(|t| t != "complete")
            {
                tracing::warn!(path, status = %resp["request_status"], "incomplete list result");
            }
            out.extend(results::<T>(resp)?);
            match next {
                Some(n) if more => cursor = Some(n),
                _ => return Ok(out),
            }
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value> {
        self.send_with(method, path, query, body, None).await
    }

    /// Single choke point: auth headers, rate limit, retry, error mapping.
    /// `file` makes the body multipart (field `file`) instead of JSON.
    fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
        resp.headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    }

    async fn send_with(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        file: Option<&FilePart>,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.cfg.base_url.trim_end_matches('/'));
        let mut attempt = 0u32;
        loop {
            self.limiter.acquire().await;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .header("Notion-Version", NOTION_VERSION);
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            if let Some(f) = file {
                let part = reqwest::multipart::Part::bytes(f.bytes.clone())
                    .file_name(f.filename.clone())
                    .mime_str(f.mime)?;
                req = req.multipart(reqwest::multipart::Form::new().part("file", part));
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            tracing::debug!(%method, path, status, attempt, "notion request");
            let retry_after = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            let text = resp.text().await?;
            if (200..300).contains(&status) {
                return Ok(serde_json::from_str(&text)?);
            }

            let err: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let code = err["code"].as_str().unwrap_or("unknown");
            let blocked = code == "public_api_request_blocked"
                || err["additional_data"]["rate_limit_reason"] == "public_api_request_blocked";
            if (status == 429 || status == 529) && !blocked {
                if attempt >= self.cfg.max_retries {
                    return Err(Error::RetriesExhausted);
                }
                let wait = match retry_after {
                    Some(s) => Duration::from_secs(s),
                    None => Duration::from_secs(1u64.checked_shl(attempt).unwrap_or(u64::MAX))
                        .min(self.cfg.max_backoff),
                };
                if wait > self.cfg.max_backoff {
                    return Err(Error::RetriesExhausted);
                }
                sleep(wait).await;
                attempt += 1;
                continue;
            }
            return Err(Error::Api {
                status,
                code: code.to_string(),
                message: err["message"].as_str().map(str::to_string).unwrap_or(text),
            });
        }
    }
}

fn results<T: DeserializeOwned>(mut resp: Value) -> Result<Vec<T>> {
    let r = resp
        .get_mut("results")
        .map(Value::take)
        .unwrap_or(Value::Null);
    Ok(serde_json::from_value(r)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{
        body_json, body_partial_json, header, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const T: &str = "2025-01-01T00:00:00.000Z";

    fn client(server: &MockServer, max_retries: u32) -> Client {
        Client::new(
            "test-token".into(),
            ClientConfig {
                base_url: server.uri(),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries,
                max_backoff: Duration::from_secs(60),
            },
        )
    }

    fn block(id: &str, has_children: bool) -> Value {
        json!({
            "object": "block", "id": id, "parent": {"type": "page_id", "page_id": "root"},
            "created_time": T, "last_edited_time": T, "has_children": has_children,
            "in_trash": false, "type": "paragraph",
            "paragraph": {"rich_text": [], "color": "default"}
        })
    }

    fn page(id: &str) -> Value {
        json!({
            "object": "page", "id": id, "parent": {"type": "workspace", "workspace": true},
            "created_time": T, "last_edited_time": T, "in_trash": false,
            "url": "https://notion.so/x", "properties": {}
        })
    }

    fn list(results: Vec<Value>, next: Option<&str>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "results": results,
            "has_more": next.is_some(), "next_cursor": next
        }))
    }

    fn ids(blocks: &[Block]) -> Vec<&str> {
        blocks.iter().map(|b| b.id.as_str()).collect()
    }

    #[tokio::test]
    async fn sends_auth_and_version_headers() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b1"))
            .and(header("Notion-Version", "2025-09-03"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(block("b1", false)))
            .expect(1)
            .mount(&s)
            .await;
        let b = client(&s, 0).get_block("b1").await.unwrap();
        assert_eq!(b.id, "b1");
    }

    async fn retry_once(status: u16) {
        let s = MockServer::start().await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(ResponseTemplate::new(status).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page("p1")))
            .mount(&s)
            .await;
        let p = client(&s, 5).get_page("p1").await.unwrap();
        assert_eq!(p.id, "p1");
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn retries_429() {
        retry_once(429).await;
    }

    #[tokio::test]
    async fn retries_529() {
        retry_once(529).await;
    }

    #[tokio::test]
    async fn retries_exhausted() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&s)
            .await;
        let e = client(&s, 2).get_page("p1").await.unwrap_err();
        assert!(matches!(e, Error::RetriesExhausted), "{e:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn retry_after_above_cap_gives_up() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "61"))
            .mount(&s)
            .await;
        let e = client(&s, 5).get_page("p1").await.unwrap_err();
        assert!(matches!(e, Error::RetriesExhausted), "{e:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn blocked_429_not_retried() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "0")
                    .set_body_json(json!({"object": "error", "status": 429,
                        "code": "public_api_request_blocked", "message": "blocked"})),
            )
            .mount(&s)
            .await;
        let e = client(&s, 5).get_page("p1").await.unwrap_err();
        assert!(matches!(e, Error::Api { status: 429, .. }), "{e:?}");
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn api_error_parsed() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/pages/p1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "object": "error", "status": 400, "code": "validation_error", "message": "bad"
            })))
            .mount(&s)
            .await;
        match client(&s, 5).get_page("p1").await.unwrap_err() {
            Error::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(
                    (status, code.as_str(), message.as_str()),
                    (400, "validation_error", "bad")
                )
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }

    fn comment(id: &str, disc: &str, parent: Value, text: &str) -> Value {
        json!({
            "object": "comment", "id": id, "discussion_id": disc, "parent": parent,
            "created_time": T, "last_edited_time": T,
            "created_by": {"object": "user", "id": "bot"},
            "rich_text": [{"type": "text", "text": {"content": text, "link": null},
                "annotations": {}, "plain_text": text, "href": null}],
        })
    }

    #[tokio::test]
    async fn list_comments_paginates() {
        let s = MockServer::start().await;
        let pg = json!({"type": "page_id", "page_id": "p"});
        Mock::given(method("GET"))
            .and(path("/v1/comments"))
            .and(query_param("block_id", "p"))
            .and(query_param_is_missing("start_cursor"))
            .respond_with(list(vec![comment("c1", "d1", pg.clone(), "a")], Some("n")))
            .mount(&s)
            .await;
        Mock::given(path("/v1/comments"))
            .and(query_param("block_id", "p"))
            .and(query_param("start_cursor", "n"))
            .respond_with(list(vec![comment("c2", "d1", pg, "b")], None))
            .mount(&s)
            .await;
        let cs = client(&s, 0).list_comments("p").await.unwrap();
        assert_eq!(
            cs.iter()
                .map(|c| (c.id.as_str(), c.created_by.as_str()))
                .collect::<Vec<_>>(),
            [("c1", "bot"), ("c2", "bot")]
        );
        assert_eq!(cs[0].discussion_id, "d1");
        assert_eq!(crate::model::plain_text(&cs[1].rich_text), "b");
    }

    #[tokio::test]
    async fn create_comment_on_page_block_and_reply() {
        let s = MockServer::start().await;
        let rt = json!([{"type": "text", "text": {"content": "hi"}}]);
        let bodies = [
            json!({"parent": {"type": "page_id", "page_id": "p"}, "rich_text": rt}),
            json!({"parent": {"type": "block_id", "block_id": "b"}, "rich_text": rt}),
            json!({"discussion_id": "d9", "rich_text": rt}),
        ];
        for b in &bodies {
            Mock::given(method("POST"))
                .and(path("/v1/comments"))
                .and(header("Notion-Version", NOTION_VERSION))
                .and(body_json(b))
                .respond_with(ResponseTemplate::new(200).set_body_json(comment(
                    "new",
                    "d",
                    json!({"type": "page_id", "page_id": "p"}),
                    "hi",
                )))
                .expect(1)
                .mount(&s)
                .await;
        }
        let c = client(&s, 0);
        for b in &bodies {
            assert_eq!(c.create_comment(b).await.unwrap()["id"], "new");
        }
    }

    #[tokio::test]
    async fn get_update_delete_comment_and_me() {
        let s = MockServer::start().await;
        let pg = json!({"type": "block_id", "block_id": "b"});
        Mock::given(method("GET"))
            .and(path("/v1/comments/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(comment(
                "c1",
                "d",
                pg.clone(),
                "x",
            )))
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/comments/c1"))
            .and(body_json(
                json!({"rich_text": [{"type": "text", "text": {"content": "y"}}]}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(comment("c1", "d", pg, "y")))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/comments/c1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"object": "comment", "id": "c1"})),
            )
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(path("/v1/users/me"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"object": "user", "id": "bot-1"})),
            )
            .mount(&s)
            .await;
        let c = client(&s, 0);
        assert!(
            matches!(c.get_comment("c1").await.unwrap().parent, crate::model::Parent::Block(b) if b == "b")
        );
        let r = c
            .update_comment("c1", &json!([{"type": "text", "text": {"content": "y"}}]))
            .await
            .unwrap();
        assert_eq!(r["rich_text"][0]["plain_text"], "y");
        c.delete_comment("c1").await.unwrap();
        assert_eq!(c.me().await.unwrap(), "bot-1");
    }

    #[tokio::test]
    async fn public_block_patch_drops_the_private_width() {
        let s = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .and(body_json(json!({ "image": { "caption": [] } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(block("b", false)))
            .expect(1)
            .mount(&s)
            .await;
        let body = json!({ "image": { "caption": [], "_width": 320 } });
        client(&s, 0).update_block("b", &body).await.unwrap();
    }

    #[tokio::test]
    async fn public_block_patch_drops_the_private_width_for_video_too() {
        let s = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/b"))
            .and(body_json(json!({ "video": { "caption": [] } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(block("b", false)))
            .expect(1)
            .mount(&s)
            .await;
        let body = json!({ "video": { "caption": [], "_width": 99 } });
        client(&s, 0).update_block("b", &body).await.unwrap();
    }

    #[tokio::test]
    async fn public_block_patch_with_only_a_width_sends_nothing() {
        let s = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(200).set_body_json(block("b", false)))
            .expect(0)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(block("b", false)))
            .expect(1)
            .mount(&s)
            .await;
        let body = json!({ "image": { "_width": 320 } });
        client(&s, 0).update_block("b", &body).await.unwrap();
    }

    #[tokio::test]
    async fn public_append_of_an_image_carrying_a_width_is_refused_before_any_request() {
        let s = MockServer::start().await;
        let mut v = block("tmp", false);
        v["type"] = json!("image");
        v["image"] = json!({ "type": "external", "external": { "url": "u" }, "caption": [], "_width": 300 });
        let img: Block = serde_json::from_value(v).unwrap();
        assert!(client(&s, 0).append_children("root", &[img], None).await.is_err());
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn block_children_paginates() {
        let s = MockServer::start().await;
        let p = "/v1/blocks/root/children";
        Mock::given(path(p))
            .and(query_param("page_size", "100"))
            .and(query_param_is_missing("start_cursor"))
            .respond_with(list(vec![block("a", false), block("b", false)], Some("c1")))
            .mount(&s)
            .await;
        Mock::given(path(p))
            .and(query_param("start_cursor", "c1"))
            .respond_with(list(vec![block("c", false)], Some("c2")))
            .mount(&s)
            .await;
        Mock::given(path(p))
            .and(query_param("start_cursor", "c2"))
            .respond_with(list(vec![block("d", false)], None))
            .mount(&s)
            .await;
        let bs = client(&s, 0).block_children("root").await.unwrap();
        assert_eq!(ids(&bs), ["a", "b", "c", "d"]);
    }

    fn no_cursor(r: &Request) -> bool {
        r.body_json::<Value>()
            .map(|v| v.get("start_cursor").is_none())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn query_data_source_paginates() {
        let s = MockServer::start().await;
        let p = "/v1/data_sources/ds/query";
        let filter = json!({"filter": {"property": "Done", "checkbox": {"equals": true}},
            "sorts": [{"property": "Name", "direction": "ascending"}]});
        Mock::given(method("POST"))
            .and(path(p))
            .and(body_partial_json(
                json!({"page_size": 100, "filter": filter["filter"], "sorts": filter["sorts"]}),
            ))
            .and(no_cursor)
            .respond_with(list(vec![page("p1")], Some("c1")))
            .mount(&s)
            .await;
        Mock::given(path(p))
            .and(body_partial_json(json!({"start_cursor": "c1"})))
            .respond_with(list(vec![page("p2")], Some("c2")))
            .mount(&s)
            .await;
        Mock::given(path(p))
            .and(body_partial_json(json!({"start_cursor": "c2"})))
            .respond_with(list(vec![page("p3")], None))
            .mount(&s)
            .await;
        let ps = client(&s, 0).query_data_source("ds", filter).await.unwrap();
        let got: Vec<_> = ps.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(got, ["p1", "p2", "p3"]);
    }

    #[tokio::test]
    async fn get_database_returns_its_data_sources() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/databases/db"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"object": "database", "id": "db", "data_sources": [{"id": "ds", "name": "T"}]}),
            ))
            .mount(&s)
            .await;
        let v = client(&s, 0).get_database("db").await.unwrap();
        assert_eq!(v["data_sources"][0]["id"], "ds");
    }

    #[tokio::test]
    async fn list_views_reads_the_results_and_an_error_is_an_error() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/views"))
            .and(wiremock::matchers::query_param("data_source_id", "ds"))
            .respond_with(list(vec![json!({"id": "v1", "type": "board"})], None))
            .mount(&s)
            .await;
        let v = client(&s, 0).list_views("ds").await.unwrap();
        assert_eq!(v[0]["type"], "board");
        let s = MockServer::start().await;
        Mock::given(path("/v1/views"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        assert!(client(&s, 0).list_views("ds").await.is_err());
    }

    #[tokio::test]
    async fn search_paginates() {
        let s = MockServer::start().await;
        let ds = json!({"object": "data_source", "id": "ds1", "title": [],
            "parent": {"type": "database_id", "database_id": "db1"},
            "last_edited_time": T, "properties": {}});
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_partial_json(json!({"query": "foo"})))
            .and(no_cursor)
            .respond_with(list(vec![page("p1")], Some("c1")))
            .mount(&s)
            .await;
        Mock::given(path("/v1/search"))
            .and(body_partial_json(
                json!({"query": "foo", "start_cursor": "c1"}),
            ))
            .respond_with(list(vec![ds], None))
            .mount(&s)
            .await;
        let rs = client(&s, 0).search("foo").await.unwrap();
        assert!(
            matches!(&rs[..], [SearchResult::Page(p), SearchResult::DataSource(d)]
            if p.id == "p1" && d.id == "ds1")
        );
    }

    #[tokio::test]
    async fn block_tree_recurses_but_not_into_child_pages() {
        let s = MockServer::start().await;
        let mut child_page = block("cp", true);
        child_page["type"] = json!("child_page");
        child_page["child_page"] = json!({"title": "Sub"});
        for (parent, kids) in [
            ("root", vec![block("a", true), child_page]),
            ("a", vec![block("b", true)]),
            ("b", vec![block("c", false)]),
        ] {
            Mock::given(path(format!("/v1/blocks/{parent}/children")))
                .respond_with(list(kids, None))
                .expect(1)
                .mount(&s)
                .await;
        }
        Mock::given(path("/v1/blocks/cp/children"))
            .respond_with(list(vec![], None))
            .expect(0)
            .mount(&s)
            .await;
        let tree = client(&s, 0).block_tree("root").await.unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].block.id, "a");
        assert_eq!(tree[0].children[0].block.id, "b");
        assert_eq!(tree[0].children[0].children[0].block.id, "c");
        assert!(tree[0].children[0].children[0].children.is_empty());
        assert_eq!(tree[1].block.id, "cp");
        assert!(tree[1].children.is_empty());
        s.verify().await;
    }

    #[tokio::test]
    async fn block_tree_keeps_sibling_order_whatever_answers_first() {
        let s = MockServer::start().await;
        Mock::given(path("/v1/blocks/root/children"))
            .respond_with(list(
                vec![block("a", true), block("b", true), block("c", true)],
                None,
            ))
            .mount(&s)
            .await;
        for (p, kid, ms) in [("a", "a1", 200), ("b", "b1", 0), ("c", "c1", 100)] {
            Mock::given(path(format!("/v1/blocks/{p}/children")))
                .respond_with(
                    list(vec![block(kid, false)], None)
                        .set_delay(std::time::Duration::from_millis(ms)),
                )
                .mount(&s)
                .await;
        }
        let tree = client(&s, 0).block_tree("root").await.unwrap();
        let got: Vec<_> = tree
            .iter()
            .map(|n| (n.block.id.as_str(), n.children[0].block.id.as_str()))
            .collect();
        assert_eq!(got, [("a", "a1"), ("b", "b1"), ("c", "c1")]);
    }

    #[tokio::test]
    async fn append_children_body() {
        let s = MockServer::start().await;
        let new: Block = serde_json::from_value(block("tmp", false)).unwrap();
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/root/children"))
            .and(header("Content-Type", "application/json"))
            .and(body_json(json!({
                "children": [new.kind.to_request_json().unwrap()],
                "after": "prev"
            })))
            .respond_with(list(vec![block("n1", false)], None))
            .expect(1)
            .mount(&s)
            .await;
        let out = client(&s, 0)
            .append_children("root", &[new], Some("prev"))
            .await
            .unwrap();
        assert_eq!(ids(&out), ["n1"]);
    }

    #[tokio::test]
    async fn append_children_chunks_by_100() {
        let s = MockServer::start().await;
        let new: Block = serde_json::from_value(block("tmp", false)).unwrap();
        let first: Vec<Value> = (0..100).map(|i| block(&format!("n{i}"), false)).collect();
        let second: Vec<Value> = (100..150).map(|i| block(&format!("n{i}"), false)).collect();
        Mock::given(path("/v1/blocks/root/children"))
            .and(|r: &Request| {
                let v: Value = r.body_json().unwrap();
                v["children"].as_array().unwrap().len() == 100 && v.get("after").is_none()
            })
            .respond_with(list(first, None))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(path("/v1/blocks/root/children"))
            .and(|r: &Request| {
                let v: Value = r.body_json().unwrap();
                v["children"].as_array().unwrap().len() == 50 && v["after"] == "n99"
            })
            .respond_with(list(second, None))
            .expect(1)
            .mount(&s)
            .await;
        let out = client(&s, 0)
            .append_children("root", &vec![new; 150], None)
            .await
            .unwrap();
        assert_eq!(out.len(), 150);
        assert_eq!(out[149].id, "n149");
        s.verify().await;
    }

    #[test]
    #[should_panic(expected = "requests_per_sec")]
    fn zero_rate_rejected() {
        let cfg = ClientConfig {
            requests_per_sec: 0.0,
            ..ClientConfig::default()
        };
        Client::new("t".into(), cfg);
    }

    #[test]
    fn non_object_body_is_json_error() {
        assert!(matches!(results::<Block>(json!([1])), Err(Error::Json(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_spaces_requests() {
        let l = Limiter::new(3.0, 3);
        let start = Instant::now();
        for _ in 0..6 {
            l.acquire().await;
        }
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(990), "{took:?}");
        assert!(took < Duration::from_millis(1100), "{took:?}");
    }

    /// Notion allows 3 requests a second on average: however many callers
    /// (polls included) ask, the default limiter lets through at most
    /// burst + 60 s * rate = 183 in a minute, and 3/s after the burst.
    #[tokio::test(start_paused = true)]
    async fn limiter_caps_a_minute_for_any_number_of_callers() {
        let cfg = ClientConfig::default();
        let l = Limiter::new(cfg.requests_per_sec, cfg.burst);
        let start = Instant::now();
        let mut in_first_minute = 0;
        for _ in 0..400 {
            l.acquire().await;
            if start.elapsed() <= Duration::from_secs(60) {
                in_first_minute += 1;
            }
        }
        assert!((180..=183).contains(&in_first_minute), "{in_first_minute}");
    }

    #[test]
    fn debug_hides_token() {
        let c = Client::new("secret-xyz".into(), ClientConfig::default());
        let d = format!("{c:?}");
        assert!(
            !d.contains("secret-xyz") && d.contains("api.notion.com"),
            "{d}"
        );
    }
    #[tokio::test]
    async fn me_identity_reads_the_owner_only_when_a_person() {
        for (body, want) in [
            (
                json!({"id": "b", "bot": {"owner": {"type": "user", "user": {"id": "u1"}}}}),
                Some("u1"),
            ),
            (
                json!({"id": "b", "bot": {"owner": {"type": "workspace", "workspace": true}}}),
                None,
            ),
            (json!({"id": "b"}), None),
        ] {
            let s = MockServer::start().await;
            Mock::given(path("/v1/users/me"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&s)
                .await;
            assert_eq!(
                client(&s, 0).me_identity().await.unwrap(),
                ("b".to_string(), want.map(str::to_string))
            );
        }
    }

    #[tokio::test]
    async fn list_users_paginates_and_parses_persons_and_bots() {
        let s = MockServer::start().await;
        let user = |id: &str, ty: &str, name: &str| {
            let mut u =
                json!({"object": "user", "id": id, "type": ty, "name": name, "avatar_url": null});
            if ty == "person" {
                u["avatar_url"] = json!("https://img/a.png");
                u["person"] = json!({"email": "a@x.org"});
            }
            u
        };
        Mock::given(path("/v1/users"))
            .and(query_param("page_size", "100"))
            .and(query_param_is_missing("start_cursor"))
            .respond_with(list(
                vec![user("u1", "person", "Ada"), user("b1", "bot", "Bot")],
                Some("c1"),
            ))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(path("/v1/users"))
            .and(query_param("start_cursor", "c1"))
            .respond_with(list(vec![user("u2", "person", "Bo")], None))
            .expect(1)
            .mount(&s)
            .await;
        let us = client(&s, 0).list_users().await.unwrap();
        let got: Vec<_> = us
            .iter()
            .map(|u| (u.id.as_str(), u.kind.as_str(), u.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("u1", "person", "Ada"),
                ("b1", "bot", "Bot"),
                ("u2", "person", "Bo")
            ]
        );
        assert_eq!(us[0].email.as_deref(), Some("a@x.org"));
        assert_eq!(us[0].avatar_url.as_deref(), Some("https://img/a.png"));
        assert!(us[1].is_bot() && us[1].avatar_url.is_none() && us[1].email.is_none());
    }
}

#[cfg(test)]
mod private_api_tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn auth() -> PrivateAuth {
        PrivateAuth {
            token_v2: "tok".into(),
            space_id: "s1".into(),
            user_id: Some("u1".into()),
            cookies: None,
        }
    }

    fn client(s: &MockServer) -> Client {
        Client::new_private_at(
            s.uri(),
            auth(),
            ClientConfig {
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                ..Default::default()
            },
        )
    }

    const PID: &str = "11111111-1111-1111-1111-111111111111";
    const SUB: &str = "22222222-2222-2222-2222-222222222222";
    const UA: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const UB: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    async fn mount_record(s: &MockServer, table: &str, id: &str, value: Value) {
        Mock::given(method("POST"))
            .and(path("/api/v3/syncRecordValues"))
            .and(body_partial_json(
                json!({ "requests": [{ "pointer": { "table": table, "id": id } }] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "recordMap": { table: { id: { "role": "editor", "value": value } } } }),
            ))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn page_access_inherits_and_nearer_entry_wins() {
        let s = MockServer::start().await;
        mount_record(&s, "block", SUB, json!({
            "id": SUB, "type": "page", "parent_id": PID, "parent_table": "block",
            "permissions": [{ "role": "reader", "type": "user_permission", "user_id": UA }],
        })).await;
        mount_record(&s, "block", PID, json!({
            "id": PID, "type": "page", "parent_id": "s1", "parent_table": "space",
            "permissions": [
                { "role": "editor", "type": "user_permission", "user_id": UA },
                { "role": "comment_only", "type": "user_permission", "user_id": UB },
                { "role": { "x": 1 }, "type": "bot_permission", "bot_id": "bot" },
                { "role": "reader", "type": "space_permission" },
                { "role": "reader", "type": "public_permission" },
            ],
        })).await;
        mount_record(&s, "notion_user", UA, json!({ "id": UA, "name": "Ann" })).await;
        let a = client(&s).page_access(SUB).await.unwrap();
        assert_eq!(
            a.people,
            vec![(UA.to_string(), "reader".to_string()), (UB.to_string(), "comment_only".to_string())]
        );
        assert_eq!(a.workspace.as_deref(), Some("reader"));
        assert_eq!(a.public.as_deref(), Some("reader"));
        assert_eq!(a.users.len(), 1);
    }

    #[tokio::test]
    async fn sharing_needs_a_session_and_valid_input() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&s).await;
        let c = client(&s);
        assert!(c.share_page(PID, UA, "owner").await.is_err());
        assert!(c.share_page("p1", UA, "editor").await.is_err());
        let off = Client::new("secret_x".into(), ClientConfig::default());
        assert!(off.page_access(PID).await.is_err());
        assert!(off.share_page(PID, UA, "editor").await.is_err());
    }

    #[tokio::test]
    async fn share_page_sends_one_set_permission_item() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/saveTransactionsFanout"))
            .and(body_partial_json(json!({ "transactions": [{ "spaceId": "s1", "operations": [{
                "pointer": { "id": PID, "table": "block", "spaceId": "s1" },
                "path": ["permissions"], "command": "setPermissionItem",
                "args": { "type": "user_permission", "role": "editor", "user_id": UA },
            }] }] })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&s)
            .await;
        client(&s).share_page(PID, UA, "editor").await.unwrap();
    }

    #[tokio::test]
    async fn page_access_inherits_through_a_collection() {
        let s = MockServer::start().await;
        let (col, db) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444");
        mount_record(&s, "block", SUB, json!({
            "id": SUB, "type": "page", "parent_id": col, "parent_table": "collection",
        })).await;
        mount_record(&s, "collection", col, json!({
            "id": col, "parent_id": db, "parent_table": "block",
        })).await;
        mount_record(&s, "block", db, json!({
            "id": db, "type": "collection_view_page", "parent_id": "s1", "parent_table": "space",
            "permissions": [{ "role": "editor", "type": "user_permission", "user_id": UB }],
        })).await;
        mount_record(&s, "notion_user", UB, json!({ "id": UB, "name": "Bo" })).await;
        let a = client(&s).page_access(SUB).await.unwrap();
        assert_eq!(a.people, vec![(UB.to_string(), "editor".to_string())]);
    }

    #[tokio::test]
    async fn page_access_terminates_on_self_parent_and_skips_missing_user_id() {
        let s = MockServer::start().await;
        mount_record(&s, "block", SUB, json!({
            "id": SUB, "type": "page", "parent_id": SUB, "parent_table": "block",
            "permissions": [
                { "role": "editor", "type": "user_permission" },
                { "role": "editor", "type": "user_permission", "user_id": 5 },
                { "role": "none", "type": "user_permission", "user_id": UB },
                { "role": "reader", "type": "user_permission", "user_id": UA },
            ],
        })).await;
        mount_record(&s, "notion_user", UA, json!({ "id": UA, "name": "Ann" })).await;
        let a = client(&s).page_access(SUB).await.unwrap();
        assert_eq!(a.people, vec![(UA.to_string(), "reader".to_string())]);
    }

    #[tokio::test]
    async fn share_page_rejects_uppercase_ids_without_request() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&s).await;
        let upper = UA.to_uppercase();
        assert!(client(&s).share_page(&upper, UB, "editor").await.is_err());
        assert!(client(&s).share_page(PID, &upper, "editor").await.is_err());
    }

    #[tokio::test]
    async fn share_page_rejects_garbage_ids_without_request() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&s).await;
        let c = client(&s);
        for (p, u) in [(PID, "ZZZZZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZZZZZZZZZ"), (PID, "../../x"), ("", UA), (PID, "")] {
            assert!(c.share_page(p, u, "editor").await.is_err(), "{p} {u}");
        }
    }

    fn page_value() -> Value {
        json!({
            "id": "p1", "type": "page", "alive": true,
            "parent_id": "s1", "parent_table": "space",
            "properties": { "title": [["Hello"]] },
            "content": ["b1"],
            "created_time": 1700000000000_i64,
            "last_edited_time": 1700000000000_i64,
        })
    }

    fn text_value() -> Value {
        json!({
            "id": "b1", "type": "text", "alive": true,
            "parent_id": "p1", "parent_table": "block",
            "properties": { "title": [["Hi"]] },
            "content": [],
            "created_time": 1700000000000_i64,
            "last_edited_time": 1700000000000_i64,
        })
    }

    fn chunk() -> Value {
        json!({
            "recordMap": {
                "block": {
                    "p1": { "role": "editor", "value": page_value() },
                    "b1": { "role": "editor", "value": text_value() },
                }
            },
            "cursor": { "stack": [] }
        })
    }

    #[test]
    fn a_non_notion_base_falls_back_to_the_default() {
        let c = Client::new_private_at("https://evil.example", auth(), ClientConfig::default());
        assert_eq!(c.private_base, private::DEFAULT_BASE);
        assert!(!c.file_cookie_ok("https://s3.amazonaws.com/x"));
    }

    #[tokio::test]
    async fn a_401_expires_the_session_and_stops_further_calls() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("token_v2=tok"))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        let e = c
            .private_post("loadUserContent", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::SessionExpired), "{e}");
        let e = c
            .private_post("loadUserContent", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::SessionExpired));
    }

    #[tokio::test]
    async fn browser_spaces_reads_the_cookie_and_spaces() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/getSpacesInitial"))
            .and(header("Cookie", "token_v2=tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "users": { "u1": {
                    "notion_user": { "u1": { "value": { "value": { "id": "u1", "email": "a@b.c" } } } },
                    "space": { "s1": { "value": { "id": "s1", "name": "Acme" } } },
                    "space_view": { "v1": { "value": { "spaceId": "s1", "name": "Acme" } } }
                }}
            })))
            .expect(1)
            .mount(&s)
            .await;
        let (user, spaces) = private::browser_spaces("tok", &s.uri()).await.unwrap();
        assert_eq!(user.as_deref(), Some("u1"));
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id, "s1");
        assert_eq!(spaces[0].name, "Acme");
    }

    #[tokio::test]
    async fn reads_a_page_and_its_tree() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/loadPageChunk"))
            .and(header("Cookie", "token_v2=tok"))
            .and(header("x-notion-space-id", "s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chunk()))
            .expect(2)
            .mount(&s)
            .await;
        let c = client(&s);
        let page = c.get_page("p1").await.unwrap();
        assert_eq!(page.title(), "Hello");
        assert!(matches!(page.parent, crate::model::Parent::Workspace));
        let tree = c.block_tree("p1").await.unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].block.kind.type_name(), "paragraph");
        assert_eq!(
            tree[0].block.raw["paragraph"]["rich_text"][0]["plain_text"],
            "Hi"
        );
    }

    #[tokio::test]
    async fn searches_the_space() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/search"))
            .and(body_partial_json(json!({
                "type": "BlocksInSpace", "spaceId": "s1", "query": ""
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": ["p1"],
                "recordMap": { "block": { "p1": { "value": page_value() } } }
            })))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        let results = c.search("").await.unwrap();
        assert_eq!(results.len(), 1);
        match &results[0] {
            SearchResult::Page(p) => assert_eq!(p.title(), "Hello"),
            _ => panic!("not a page"),
        }
    }

    #[tokio::test]
    async fn appending_a_block_uses_a_transaction() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/saveTransactionsFanout"))
            .and(body_partial_json(
                json!({ "transactions": [{ "spaceId": "s1" }] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        let body = json!({
            "type": "paragraph",
            "paragraph": {
                "rich_text": [{ "type": "text", "text": { "content": "Hi", "link": null },
                    "annotations": { "bold": false, "italic": false, "strikethrough": false,
                        "underline": false, "code": false, "color": "default" },
                    "plain_text": "Hi", "href": null }],
                "color": "default",
            }
        });
        let block = c.append_raw("p1", &body, None).await.unwrap();
        assert_eq!(block.kind.type_name(), "paragraph");
        assert_eq!(block.raw["paragraph"]["rich_text"][0]["plain_text"], "Hi");
        let reqs = s.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let ops = body["transactions"][0]["operations"].as_array().unwrap();
        assert_eq!(ops[0]["command"], "set");
        assert_eq!(ops[1]["command"], "listAfter");
        assert_eq!(ops[1]["pointer"]["id"], "p1");
    }

    #[tokio::test]
    async fn updating_a_block_sets_properties() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/syncRecordValues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "recordMap": { "block": { "b1": { "value": text_value() } } }
            })))
            .expect(2)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/saveTransactionsFanout"))
            .and(body_partial_json(
                json!({ "transactions": [{ "operations": [{
                "path": ["properties", "title"], "command": "set"
            }] }] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        let body = json!({
            "type": "paragraph",
            "paragraph": { "rich_text": [{ "type": "text",
                "text": { "content": "Changed", "link": null },
                "annotations": { "bold": false, "italic": false, "strikethrough": false,
                    "underline": false, "code": false, "color": "default" },
                "plain_text": "Changed", "href": null }], "color": "default" }
        });
        let block = c.update_block("b1", &body).await.unwrap();
        assert_eq!(block.kind.type_name(), "paragraph");
    }

    #[tokio::test]
    async fn creating_a_page_sets_a_page_record() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/saveTransactionsFanout"))
            .and(body_partial_json(
                json!({ "transactions": [{ "operations": [{
                "command": "set"
            }] }] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&s)
            .await;
        let c = client(&s);
        let resp = c
            .create_page(&json!({
                "parent": { "type": "page_id", "page_id": "p1" },
                "properties": { "title": { "title": [] } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["object"], "page");
        assert!(resp["id"].as_str().is_some());
    }

    fn rt(text: &str) -> Value {
        json!([{ "type": "text", "text": { "content": text, "link": null },
            "annotations": { "bold": false, "italic": false, "strikethrough": false,
                "underline": false, "code": false, "color": "default" },
            "plain_text": text, "href": null }])
    }

    fn record(id: &str, ty: &str, parent: &str, table: &str) -> Value {
        json!({
            "id": id, "type": ty, "alive": true,
            "parent_id": parent, "parent_table": table,
            "properties": { "title": [["Old"]] }, "content": [],
            "created_time": 1700000000000_i64, "last_edited_time": 1700000000000_i64,
        })
    }

    async fn serve_records(s: &MockServer, recs: Vec<Value>) {
        let blocks: Map<String, Value> = recs
            .into_iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), json!({ "value": r })))
            .collect();
        Mock::given(method("POST"))
            .and(path("/api/v3/syncRecordValues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "recordMap": { "block": blocks }
            })))
            .mount(s)
            .await;
    }

    async fn mount_fanout(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v3/saveTransactionsFanout"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(s)
            .await;
    }

    async fn fanout_ops(s: &MockServer) -> Vec<Value> {
        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("saveTransactionsFanout"))
            .expect("no transaction was sent");
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        body["transactions"][0]["operations"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn updating_a_heading_without_type_keeps_type_and_text() {
        let s = MockServer::start().await;
        serve_records(&s, vec![record("h1", "header", "p1", "block")]).await;
        mount_fanout(&s).await;
        // the shape sync's `enqueue_kind` sends: no "type" key
        let body = json!({ "heading_1": { "rich_text": rt("New"), "color": "red" } });
        client(&s).update_block("h1", &body).await.unwrap();
        let ops = fanout_ops(&s).await;
        let title = ops
            .iter()
            .find(|o| o["path"] == json!(["properties", "title"]));
        assert_eq!(title.unwrap()["args"], json!([["New"]]));
        // nothing rewrites the record or its type
        assert!(ops
            .iter()
            .all(|o| o["path"] != json!([]) && o["path"] != json!(["type"])));
    }

    #[tokio::test]
    async fn an_image_width_update_sets_only_format_block_width() {
        let s = MockServer::start().await;
        let mut r = record("i1", "image", "p1", "block");
        r["format"] = json!({ "display_source": "https://x/a.png", "block_width": 100 });
        serve_records(&s, vec![r]).await;
        mount_fanout(&s).await;
        let body = json!({ "image": { "_width": 320 } });
        client(&s).update_block("i1", &body).await.unwrap();
        let ops = fanout_ops(&s).await;
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_eq!(ops[0]["path"], json!(["format", "block_width"]));
        assert_eq!(ops[0]["command"], "set");
        assert_eq!(ops[0]["args"], json!(320));
    }

    #[tokio::test]
    async fn to_do_checked_round_trips() {
        let s = MockServer::start().await;
        serve_records(&s, vec![record("t1", "to_do", "p1", "block")]).await;
        mount_fanout(&s).await;
        let body = json!({ "to_do": { "rich_text": rt("Task"), "checked": true } });
        client(&s).update_block("t1", &body).await.unwrap();
        let ops = fanout_ops(&s).await;
        let at = |k: &str| ops.iter().find(|o| o["path"] == json!(["properties", k]));
        assert_eq!(at("checked").unwrap()["args"], json!([["Yes"]]));
        assert_eq!(at("title").unwrap()["args"], json!([["Task"]]));
    }

    #[tokio::test]
    async fn a_failed_schema_fetch_fails_the_property_write() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/syncRecordValues"))
            .and(body_partial_json(
                json!({ "requests": [{ "pointer": { "table": "collection" } }] }),
            ))
            .respond_with(ResponseTemplate::new(500))
            .mount(&s)
            .await;
        serve_records(&s, vec![record("r1", "page", "c1", "collection")]).await;
        mount_fanout(&s).await;
        let props = json!({ "Name": { "title": rt("x") } });
        assert!(client(&s)
            .update_page_properties("r1", &props)
            .await
            .is_err());
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs
            .iter()
            .all(|r| !r.url.path().ends_with("saveTransactionsFanout")));
    }

    #[tokio::test]
    async fn moving_a_page_under_its_descendant_is_refused() {
        let s = MockServer::start().await;
        serve_records(
            &s,
            vec![
                record("p1", "page", "s1", "space"),
                record("b1", "page", "p1", "block"),
            ],
        )
        .await;
        mount_fanout(&s).await;
        let c = client(&s);
        assert!(matches!(
            c.move_page("p1", "b1").await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            c.move_page("p1", "p1").await,
            Err(Error::Invalid(_))
        ));
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs
            .iter()
            .all(|r| !r.url.path().ends_with("saveTransactionsFanout")));
    }

    #[tokio::test]
    async fn moving_a_root_page_does_not_list_remove_from_the_space() {
        let s = MockServer::start().await;
        serve_records(
            &s,
            vec![
                record("p1", "page", "s1", "space"),
                record("p2", "page", "s1", "space"),
            ],
        )
        .await;
        mount_fanout(&s).await;
        client(&s).move_page("p1", "p2").await.unwrap();
        let ops = fanout_ops(&s).await;
        assert!(ops.iter().all(|o| o["command"] != "listRemove"));
    }

    #[tokio::test]
    async fn upload_stores_the_stable_url_not_the_signed_one() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/getUploadFileUrl"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "url": "attachment:abc:a.png",
                "signedGetUrl": "https://signed.example/a.png?X-Amz-Expires=1",
                "signedPutUrl": format!("{}/put", s.uri()),
            })))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path("/put"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&s)
            .await;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.png");
        std::fs::write(&f, b"png").unwrap();
        assert_eq!(
            client(&s).upload_file(&f).await.unwrap(),
            "attachment:abc:a.png"
        );
    }

    #[tokio::test]
    async fn a_403_does_not_expire_the_session() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "name": "x" })))
            .expect(2)
            .mount(&s)
            .await;
        let c = client(&s);
        for _ in 0..2 {
            let e = c
                .private_post("loadUserContent", json!({}))
                .await
                .unwrap_err();
            assert!(matches!(e, Error::Api { status: 403, .. }), "{e}");
        }
    }

    #[tokio::test]
    async fn unknown_property_key_is_refused_without_a_transaction() {
        let s = MockServer::start().await;
        serve_records(&s, vec![record("r1", "page", "p1", "block")]).await;
        mount_fanout(&s).await;
        let props = json!({ "Nope": { "number": 1 } });
        assert!(client(&s)
            .update_page_properties("r1", &props)
            .await
            .is_err());
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs
            .iter()
            .all(|r| !r.url.path().ends_with("saveTransactionsFanout")));
    }

    #[tokio::test]
    async fn moving_a_database_row_is_refused() {
        let s = MockServer::start().await;
        serve_records(
            &s,
            vec![
                record("r1", "page", "c1", "collection"),
                record("p2", "page", "s1", "space"),
            ],
        )
        .await;
        mount_fanout(&s).await;
        assert!(matches!(
            client(&s).move_page("r1", "p2").await,
            Err(Error::Invalid(_))
        ));
        let reqs = s.received_requests().await.unwrap();
        assert!(reqs
            .iter()
            .all(|r| !r.url.path().ends_with("saveTransactionsFanout")));
    }

    #[tokio::test]
    async fn bulleted_quote_toggle_updates_keep_type_and_text() {
        for (key, ty) in [
            ("bulleted_list_item", "bulleted_list"),
            ("quote", "quote"),
            ("toggle", "toggle"),
        ] {
            let s = MockServer::start().await;
            serve_records(&s, vec![record("b1", ty, "p1", "block")]).await;
            mount_fanout(&s).await;
            let body = json!({ key: { "rich_text": rt("New") } });
            client(&s).update_block("b1", &body).await.unwrap();
            let ops = fanout_ops(&s).await;
            let t = ops
                .iter()
                .find(|o| o["path"] == json!(["properties", "title"]));
            assert_eq!(t.unwrap()["args"], json!([["New"]]), "{key}");
            assert!(ops.iter().all(|o| o["path"] != json!(["type"])), "{key}");
        }
    }

    #[tokio::test]
    async fn move_page_refuses_a_tree_deeper_than_the_cap() {
        let s = MockServer::start().await;
        // every block's parent is another block, so the walk never reaches a root
        Mock::given(method("POST"))
            .and(path("/api/v3/syncRecordValues"))
            .respond_with(move |req: &wiremock::Request| {
                let b: Value = serde_json::from_slice(&req.body).unwrap();
                let id = b["requests"][0]["pointer"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let rec = json!({ "id": id, "type": "page", "alive": true,
                    "parent_id": format!("{id}x"), "parent_table": "block" });
                ResponseTemplate::new(200).set_body_json(json!({
                    "recordMap": { "block": { id.clone(): { "role": "editor", "value": rec } } }
                }))
            })
            .mount(&s)
            .await;
        let e = client(&s).move_page("a", "b").await.unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");
    }
}
