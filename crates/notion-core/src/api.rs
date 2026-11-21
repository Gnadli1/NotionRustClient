//! Async Notion REST client: rate limiting, retry on 429/529, pagination,
//! recursive block fetch. The only module that talks HTTP.

use std::time::Duration;

use reqwest::{header::RETRY_AFTER, Method};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

use crate::model::{Block, BlockKind, Comment, DataSource, Page, SearchResult, User};
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

pub struct Client {
    http: reqwest::Client,
    token: String,
    cfg: ClientConfig,
    limiter: Limiter,
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
        }
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var("NOTION_TOKEN") {
            Ok(t) if !t.is_empty() => Ok(Client::new(t, ClientConfig::default())),
            _ => Err(Error::Invalid("NOTION_TOKEN not set".into())),
        }
    }

    pub async fn get_page(&self, id: &str) -> Result<Page> {
        self.call(Method::GET, &format!("/v1/pages/{id}"), None)
            .await
    }

    pub async fn get_block(&self, id: &str) -> Result<Block> {
        self.call(Method::GET, &format!("/v1/blocks/{id}"), None)
            .await
    }

    pub async fn block_children(&self, id: &str) -> Result<Vec<Block>> {
        self.paginate(&format!("/v1/blocks/{id}/children"), None)
            .await
    }

    /// Children of `id`, recursively. child_page / child_database blocks are
    /// leaves: their "children" are another page or database.
    pub async fn block_tree(&self, id: &str) -> Result<Vec<BlockNode>> {
        let mut nodes = Vec::new();
        for block in self.block_children(id).await? {
            let leaf = matches!(
                block.kind,
                BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
            );
            let children = if block.has_children && !leaf {
                Box::pin(self.block_tree(&block.id)).await?
            } else {
                Vec::new()
            };
            nodes.push(BlockNode { block, children });
        }
        Ok(nodes)
    }

    pub async fn append_children(
        &self,
        parent: &str,
        children: &[Block],
        after: Option<&str>,
    ) -> Result<Vec<Block>> {
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

    pub async fn update_block(&self, id: &str, body: &Value) -> Result<Block> {
        self.call(Method::PATCH, &format!("/v1/blocks/{id}"), Some(body))
            .await
    }

    pub async fn delete_block(&self, id: &str) -> Result<Block> {
        self.call(Method::DELETE, &format!("/v1/blocks/{id}"), None)
            .await
    }

    pub async fn update_page_properties(&self, id: &str, props: &Value) -> Result<Page> {
        let body = json!({ "properties": props });
        self.call(Method::PATCH, &format!("/v1/pages/{id}"), Some(&body))
            .await
    }

    pub async fn get_data_source(&self, id: &str) -> Result<DataSource> {
        self.call(Method::GET, &format!("/v1/data_sources/{id}"), None)
            .await
    }

    pub async fn query_data_source(&self, id: &str, filter_sort: Value) -> Result<Vec<Page>> {
        let body = match filter_sort {
            Value::Null => json!({}),
            v @ Value::Object(_) => v,
            _ => return Err(Error::Invalid("query body must be an object".into())),
        };
        self.paginate(&format!("/v1/data_sources/{id}/query"), Some(body))
            .await
    }

    /// Unresolved comments on a page or block, all pages. Needs the
    /// integration's "read comments" capability (403 otherwise).
    pub async fn list_comments(&self, block_or_page_id: &str) -> Result<Vec<Comment>> {
        self.paginate(&format!("/v1/comments?block_id={block_or_page_id}"), None)
            .await
    }

    pub async fn get_comment(&self, id: &str) -> Result<Comment> {
        self.call(Method::GET, &format!("/v1/comments/{id}"), None)
            .await
    }

    /// `body` holds exactly one target (`parent` or `discussion_id`) and `rich_text`.
    /// Returns the raw response: with insert-only capability Notion sends just
    /// `{object, id}`, which isn't a full [`Comment`].
    pub async fn create_comment(&self, body: &Value) -> Result<Value> {
        self.send(Method::POST, "/v1/comments", &[], Some(body)).await
    }

    /// Only comments created by this integration can be edited (else 404).
    pub async fn update_comment(&self, id: &str, rich_text: &Value) -> Result<Value> {
        let body = json!({ "rich_text": rich_text });
        self.send(Method::PATCH, &format!("/v1/comments/{id}"), &[], Some(&body))
            .await
    }

    /// Only comments created by this integration can be deleted (else 404).
    pub async fn delete_comment(&self, id: &str) -> Result<()> {
        self.send(Method::DELETE, &format!("/v1/comments/{id}"), &[], None)
            .await
            .map(drop)
    }

    /// The integration's own (bot) user id: `created_by` of comments made here.
    pub async fn me(&self) -> Result<String> {
        let v = self.send(Method::GET, "/v1/users/me", &[], None).await?;
        v["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Invalid("users/me has no id".into()))
    }

    /// Every workspace user, persons and bots, all pages. Needs the
    /// integration's "read user information" capability (403 otherwise).
    pub async fn list_users(&self) -> Result<Vec<User>> {
        let raw: Vec<Value> = self.paginate("/v1/users", None).await?;
        Ok(raw.iter().filter_map(User::from_value).collect())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
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

    /// Single choke point: auth headers, rate limit, retry, error mapping.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
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
            cs.iter().map(|c| (c.id.as_str(), c.created_by.as_str())).collect::<Vec<_>>(),
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
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    comment("new", "d", json!({"type": "page_id", "page_id": "p"}), "hi"),
                ))
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
            .respond_with(ResponseTemplate::new(200).set_body_json(comment("c1", "d", pg.clone(), "x")))
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/comments/c1"))
            .and(body_json(json!({"rich_text": [{"type": "text", "text": {"content": "y"}}]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(comment("c1", "d", pg, "y")))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/comments/c1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "comment", "id": "c1"})))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(path("/v1/users/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "user", "id": "bot-1"})))
            .mount(&s)
            .await;
        let c = client(&s, 0);
        assert!(matches!(c.get_comment("c1").await.unwrap().parent, crate::model::Parent::Block(b) if b == "b"));
        let r = c
            .update_comment("c1", &json!([{"type": "text", "text": {"content": "y"}}]))
            .await
            .unwrap();
        assert_eq!(r["rich_text"][0]["plain_text"], "y");
        c.delete_comment("c1").await.unwrap();
        assert_eq!(c.me().await.unwrap(), "bot-1");
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
        let filter = json!({"filter": {"property": "Done", "checkbox": {"equals": true}}});
        Mock::given(method("POST"))
            .and(path(p))
            .and(body_partial_json(
                json!({"page_size": 100, "filter": filter["filter"]}),
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
    async fn list_users_paginates_and_parses_persons_and_bots() {
        let s = MockServer::start().await;
        let user = |id: &str, ty: &str, name: &str| {
            let mut u = json!({"object": "user", "id": id, "type": ty, "name": name, "avatar_url": null});
            if ty == "person" {
                u["avatar_url"] = json!("https://img/a.png");
                u["person"] = json!({"email": "a@x.org"});
            }
            u
        };
        Mock::given(path("/v1/users"))
            .and(query_param("page_size", "100"))
            .and(query_param_is_missing("start_cursor"))
            .respond_with(list(vec![user("u1", "person", "Ada"), user("b1", "bot", "Bot")], Some("c1")))
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
        let got: Vec<_> = us.iter().map(|u| (u.id.as_str(), u.kind.as_str(), u.name.as_str())).collect();
        assert_eq!(got, [("u1", "person", "Ada"), ("b1", "bot", "Bot"), ("u2", "person", "Bo")]);
        assert_eq!(us[0].email.as_deref(), Some("a@x.org"));
        assert_eq!(us[0].avatar_url.as_deref(), Some("https://img/a.png"));
        assert!(us[1].is_bot() && us[1].avatar_url.is_none() && us[1].email.is_none());
    }
}
