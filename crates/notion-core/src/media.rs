//! Media: image sources, the on-disk download cache and uploads.
//!
//! Notion-hosted file URLs expire after an hour, so a fetch re-pulls the block
//! (or page, for a cover) when the cached URL is about to expire or a download
//! answers 403, once, never in a loop. Signed URLs are never stored in full or
//! put into errors: the cache index keeps the URL without its query string.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::api::Client;
use crate::store::{ts, Store};
use crate::sync::local_create_json;
use crate::{Error, Result};

/// Notion's single-part upload limit.
pub const MAX_UPLOAD: u64 = 20 * 1024 * 1024;
/// A download over this is refused (it is held in memory).
const MAX_DOWNLOAD: u64 = 50 * 1024 * 1024;
// ponytail: fixed 500 MB cap, oldest-fetched evicted first (not LRU: a hit
// doesn't refresh). Make it a setting / true LRU if that bites.
pub const CACHE_CAP: u64 = 500 * 1024 * 1024;
/// Refresh a hosted URL this long before it expires.
const REFRESH_BEFORE: i64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Block,
    Page,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    File,
}

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Src {
    /// The full URL (a credential when Notion-hosted: never log or store it).
    pub url: String,
    /// `url` without query and fragment: stable across re-pulls.
    pub key: String,
    /// When a Notion-hosted URL stops working.
    pub expiry: Option<DateTime<Utc>>,
    pub hosted: bool,
    /// An upload that isn't pushed yet: read this file, no download.
    pub local: Option<PathBuf>,
}

impl Src {
    /// A hosted URL past (or within 5 minutes of) its expiry, or without one.
    fn stale(&self) -> bool {
        self.hosted
            && self
                .expiry
                .is_none_or(|e| Utc::now() + chrono::Duration::seconds(REFRESH_BEFORE) >= e)
    }
}

fn file_src(d: &Value) -> Option<Src> {
    let hosted = match d["type"].as_str()? {
        "file" => true,
        "external" => false,
        _ => return None,
    };
    let t = d["type"].as_str()?;
    let url = d[t]["url"].as_str()?.to_string();
    let key = url.split(['?', '#']).next().unwrap_or_default().to_string();
    let expiry = d[t]["expiry_time"].as_str().and_then(|s| s.parse().ok());
    Some(Src {
        url,
        key,
        expiry,
        hosted,
        local: None,
    })
}

/// The picture of an image block (raw block JSON), or of an unpushed image upload.
pub fn block_source(raw: &Value) -> Option<Src> {
    if raw["type"] != "image" {
        return None;
    }
    if let Some(p) = raw["_upload"]["path"].as_str() {
        return Some(Src {
            url: String::new(),
            key: format!("local:{p}"),
            expiry: None,
            hosted: false,
            local: Some(p.into()),
        });
    }
    file_src(&raw["image"])
}

/// The cover of a page (raw page JSON).
pub fn page_source(raw: &Value) -> Option<Src> {
    file_src(&raw["cover"])
}

fn io(e: std::io::Error) -> Error {
    Error::Invalid(format!("cache file: {e}"))
}

/// The picture block/page `id` shows, from the cache.
pub fn current(store: &Store, id: &str, owner: Owner) -> Result<Src> {
    let raw = match owner {
        Owner::Block => store
            .get_block(id)?
            .map(|b| serde_json::to_value(&b))
            .transpose()?,
        Owner::Page => store.get_page(id)?.map(|p| p.raw),
    };
    let src = raw.and_then(|r| {
        if owner == Owner::Block {
            block_source(&r)
        } else {
            page_source(&r)
        }
    });
    src.ok_or_else(|| Error::Invalid("no picture here".into()))
}

/// What a pick of a file to upload needs: its name and content type. Refuses a
/// missing file, a directory and anything over [`MAX_UPLOAD`].
pub fn check_upload(path: &Path) -> Result<(String, &'static str)> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::Invalid(format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Error::Invalid(format!("{} is not a file", path.display())));
    }
    if meta.len() > MAX_UPLOAD {
        return Err(Error::Invalid(format!(
            "{} is over the 20 MB upload limit",
            path.display()
        )));
    }
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let mime = match name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    };
    Ok((name, mime))
}

/// Queues an image or file block made of the local file `path`, after `after`
/// under `parent_id`, like `local_create_block`: cached under a `local-*` id
/// (shown straight from the file), uploaded and attached on the next push.
pub fn local_create_media(
    store: &Store,
    parent_id: &str,
    after: Option<&str>,
    path: &Path,
    kind: MediaKind,
) -> Result<String> {
    check_upload(path)?;
    let abs = std::fs::canonicalize(path).map_err(io)?;
    let ty = if kind == MediaKind::Image {
        "image"
    } else {
        "file"
    };
    let v = json!({
        "type": ty,
        ty: { "type": "file_upload", "file_upload": { "id": "" }, "caption": [] },
        "_upload": { "path": abs.to_string_lossy() },
    });
    local_create_json(store, parent_id, after, v)
}

// ------------------------------------------------------------------ cache

/// One cached file.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedMedia {
    pub owner_id: String,
    /// The source URL without its signed query string.
    pub url: String,
    pub fetched_at: String,
    pub path: PathBuf,
    pub size: u64,
}

impl Store {
    /// The cache directory: `media/` next to the DB file (None for in-memory).
    pub fn media_dir(&self) -> Option<PathBuf> {
        let db = self.conn.path().filter(|p| !p.is_empty())?;
        Some(Path::new(db).parent()?.join("media"))
    }

    pub fn media_get(&self, owner_id: &str) -> Result<Option<CachedMedia>> {
        Ok(self
            .conn
            .query_row(
                "SELECT owner_id, url, fetched_at, path, size FROM media WHERE owner_id = ?1",
                [owner_id],
                |r| {
                    Ok(CachedMedia {
                        owner_id: r.get(0)?,
                        url: r.get(1)?,
                        fetched_at: r.get(2)?,
                        path: PathBuf::from(r.get::<_, String>(3)?),
                        size: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn media_put(&self, m: &CachedMedia) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO media (owner_id, url, fetched_at, path, size) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![m.owner_id, m.url, m.fetched_at, m.path.to_string_lossy(), m.size],
        )?;
        Ok(())
    }

    /// Deletes oldest-fetched files and rows until the cache is at most `cap` bytes.
    pub fn media_evict(&self, cap: u64) -> Result<()> {
        let mut st = self
            .conn
            .prepare("SELECT owner_id, path, size FROM media ORDER BY fetched_at, rowid")?;
        let rows: Vec<(String, String, u64)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut total: u64 = rows.iter().map(|r| r.2).sum();
        for (id, path, size) in rows {
            if total <= cap {
                break;
            }
            let _ = std::fs::remove_file(path);
            self.conn
                .execute("DELETE FROM media WHERE owner_id = ?1", [id])?;
            total -= size;
        }
        Ok(())
    }
}

/// FNV-1a: a stable file name from an id (std's hasher isn't stable across releases).
fn file_name(id: &str) -> String {
    let h = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{h:016x}")
}

/// Re-pulls the owner for a fresh URL. Not written to the cache: a blind
/// `get_page` must not overwrite queued local edits.
async fn refresh(client: &Client, id: &str, owner: Owner) -> Result<Src> {
    let raw = match owner {
        Owner::Block => serde_json::to_value(client.get_block(id).await?)?,
        Owner::Page => client.get_page(id).await?.raw,
    };
    let src = if owner == Owner::Block {
        block_source(&raw)
    } else {
        page_source(&raw)
    };
    src.ok_or_else(|| Error::Invalid("the picture is gone".into()))
}

enum Dl {
    Forbidden,
    Failed(Error),
}

async fn download(
    http: &reqwest::Client,
    url: &str,
    max: u64,
    cookie: Option<&str>,
) -> std::result::Result<Vec<u8>, Dl> {
    // `without_url`: the signed URL must not end up in an error message
    let e = |e: reqwest::Error| Dl::Failed(Error::Http(e.without_url()));
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(Dl::Failed(Error::Invalid(
            "only http(s) pictures are fetched".into(),
        )));
    }
    let mut req = http.get(url);
    if let Some(cookie) = cookie {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    let mut resp = req.send().await.map_err(e)?;
    let status = resp.status();
    if status.as_u16() == 403 {
        return Err(Dl::Forbidden);
    }
    if !status.is_success() {
        return Err(Dl::Failed(Error::Invalid(format!(
            "download failed: HTTP {status}"
        ))));
    }
    let over = || Dl::Failed(Error::Invalid("picture is over 50 MB".into()));
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(over());
    }
    // no Content-Length needed: stop as soon as the running total passes the cap
    let mut bytes = Vec::new();
    while let Some(c) = resp.chunk().await.map_err(e)? {
        if (bytes.len() + c.len()) as u64 > max {
            return Err(over());
        }
        bytes.extend_from_slice(&c);
    }
    Ok(bytes)
}

/// Removes leftover `*.part` files of interrupted downloads, once per process.
fn sweep_parts(dir: &Path) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        remove_parts(dir);
    }
}

fn remove_parts(dir: &Path) {
    for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if f.path().extension().is_some_and(|x| x == "part") {
            let _ = std::fs::remove_file(f.path());
        }
    }
}

/// Drops the cached file and row of `id`, e.g. when it didn't decode, so the
/// next fetch downloads it again.
pub fn forget(store: &Store, id: &str) -> Result<()> {
    if let Some(m) = store.media_get(id)? {
        let _ = std::fs::remove_file(m.path);
    }
    store
        .conn
        .execute("DELETE FROM media WHERE owner_id = ?1", [id])?;
    Ok(())
}

/// The local file of the picture of block/page `id`: from the cache, else
/// downloaded (after a re-pull if its URL expired or the download says 403,
/// once). An unpushed upload answers with its own file.
pub async fn fetch(client: &Client, store: &Store, id: &str, owner: Owner) -> Result<PathBuf> {
    let mut src = current(store, id, owner)?;
    if let Some(p) = src.local {
        return if p.is_file() {
            Ok(p)
        } else {
            Err(Error::Invalid("the file to upload is gone".into()))
        };
    }
    if let Some(m) = store
        .media_get(id)?
        .filter(|m| m.url == src.key && m.path.is_file())
    {
        return Ok(m.path);
    }
    let dir = store
        .media_dir()
        .ok_or_else(|| Error::Invalid("no cache directory (in-memory store)".into()))?;
    sweep_parts(&dir);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;
    if src.stale() {
        src = refresh(client, id, owner).await?;
    }
    let mut attachment = false;
    if src.url.starts_with("attachment:") {
        attachment = true;
        src.url = client.signed_file_url(id, &src.url).await?;
    }
    let mut file_cookie = if attachment && client.file_cookie_ok(&src.url) {
        Some(format!("file_token={}", client.file_token().await?))
    } else {
        None
    };
    let bytes = match download(&http, &src.url, MAX_DOWNLOAD, file_cookie.as_deref()).await {
        Err(Dl::Forbidden) if file_cookie.is_some() => {
            // The file token can expire; exchange the API session once more.
            client.clear_file_token();
            file_cookie = Some(format!("file_token={}", client.file_token().await?));
            match download(&http, &src.url, MAX_DOWNLOAD, file_cookie.as_deref()).await {
                Ok(b) => b,
                Err(Dl::Forbidden) => {
                    return Err(Error::Invalid("Notion refused the download (403)".into()))
                }
                Err(Dl::Failed(e)) => return Err(e),
            }
        }
        Err(Dl::Forbidden) if src.hosted => {
            src = refresh(client, id, owner).await?;
            match download(&http, &src.url, MAX_DOWNLOAD, None).await {
                Ok(b) => b,
                Err(Dl::Forbidden) => {
                    return Err(Error::Invalid("Notion refused the download (403)".into()))
                }
                Err(Dl::Failed(e)) => return Err(e),
            }
        }
        Err(Dl::Forbidden) => {
            return Err(Error::Invalid("the host refused the download (403)".into()))
        }
        Err(Dl::Failed(e)) => return Err(e),
        Ok(b) => b,
    };
    std::fs::create_dir_all(&dir).map_err(io)?;
    let path = dir.join(file_name(id));
    let part = path.with_extension("part");
    std::fs::write(&part, &bytes).map_err(io)?;
    std::fs::rename(&part, &path).map_err(io)?;
    store.media_put(&CachedMedia {
        owner_id: id.into(),
        url: src.key,
        fetched_at: ts(&Utc::now()),
        path: path.clone(),
        size: bytes.len() as u64,
    })?;
    store.media_evict(CACHE_CAP)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ClientConfig;
    use crate::model::Block;
    use wiremock::matchers::{body_json, header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const T: &str = "2025-01-01T00:00:00.000Z";

    fn client(s: &MockServer) -> Client {
        Client::new(
            "test-token".into(),
            ClientConfig {
                base_url: s.uri(),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                max_backoff: Duration::from_secs(1),
            },
        )
    }

    fn private_client(s: &MockServer) -> Client {
        Client::new_private_at(
            s.uri(),
            crate::private::PrivateAuth {
                token_v2: "test-token".into(),
                space_id: "s1".into(),
                user_id: None,
                cookies: Some("notion_user_id=u1".into()),
            },
            ClientConfig {
                base_url: s.uri(),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                max_backoff: Duration::from_secs(1),
            },
        )
    }

    fn image(id: &str, url: &str, expiry: DateTime<Utc>) -> Value {
        json!({ "object": "block", "id": id, "parent": { "type": "page_id", "page_id": "p" }, "created_time": T,
            "last_edited_time": T, "has_children": false, "in_trash": false, "type": "image",
            "image": { "type": "file", "caption": [], "file": { "url": url, "expiry_time": expiry.to_rfc3339() } } })
    }

    fn store(dir: &Path, blocks: &[Value]) -> Store {
        let s = Store::open(&dir.join("cache.db")).unwrap();
        for b in blocks {
            s.put_block(&serde_json::from_value::<Block>(b.clone()).unwrap())
                .unwrap();
        }
        s
    }

    fn soon() -> DateTime<Utc> {
        Utc::now() + chrono::Duration::minutes(50)
    }

    async fn serve(s: &MockServer, p: &str, status: u16, body: &[u8], times: u64) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(status).set_body_bytes(body.to_vec()))
            .expect(times)
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn a_cache_hit_downloads_nothing() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        serve(&s, "/a.png", 200, b"PIC", 1).await;
        let st = store(
            d.path(),
            &[image("b", &format!("{}/a.png?sig=1", s.uri()), soon())],
        );
        let c = client(&s);
        let p = fetch(&c, &st, "b", Owner::Block).await.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"PIC");
        assert_eq!(fetch(&c, &st, "b", Owner::Block).await.unwrap(), p);
        assert_eq!(
            st.media_get("b").unwrap().unwrap().url,
            format!("{}/a.png", s.uri()),
            "no signature stored"
        );
    }

    #[tokio::test]
    async fn an_expired_url_re_pulls_the_block_then_downloads() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let old = Utc::now() - chrono::Duration::minutes(5);
        let st = store(
            d.path(),
            &[image("b", &format!("{}/old.png?sig=1", s.uri()), old)],
        );
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(image(
                "b",
                &format!("{}/new.png?sig=2", s.uri()),
                soon(),
            )))
            .expect(1)
            .mount(&s)
            .await;
        serve(&s, "/old.png", 200, b"OLD", 0).await;
        serve(&s, "/new.png", 200, b"NEW", 1).await;
        let p = fetch(&client(&s), &st, "b", Owner::Block).await.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"NEW");
    }

    #[tokio::test]
    async fn a_403_refreshes_once_and_never_loops() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st = store(
            d.path(),
            &[image("b", &format!("{}/a.png?sig=1", s.uri()), soon())],
        );
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(image(
                "b",
                &format!("{}/b.png?sig=2", s.uri()),
                soon(),
            )))
            .expect(1)
            .mount(&s)
            .await;
        serve(&s, "/a.png", 403, b"", 1).await;
        serve(&s, "/b.png", 403, b"", 1).await; // the fresh URL is refused too
        let e = fetch(&client(&s), &st, "b", Owner::Block)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("403") && !e.contains("sig="), "{e}");
        assert!(st.media_get("b").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_403_then_a_fresh_url_works() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st = store(
            d.path(),
            &[image("b", &format!("{}/a.png?sig=1", s.uri()), soon())],
        );
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(image(
                "b",
                &format!("{}/b.png?sig=2", s.uri()),
                soon(),
            )))
            .expect(1)
            .mount(&s)
            .await;
        serve(&s, "/a.png", 403, b"", 1).await;
        serve(&s, "/b.png", 200, b"OK", 1).await;
        let p = fetch(&client(&s), &st, "b", Owner::Block).await.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"OK");
    }

    #[test]
    fn eviction_removes_the_oldest_over_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let st = store(d.path(), &[]);
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let path = d.path().join(id);
            std::fs::write(&path, [0u8; 10]).unwrap();
            st.media_put(&CachedMedia {
                owner_id: (*id).into(),
                url: "u".into(),
                fetched_at: format!("2025-01-0{}", i + 1),
                path,
                size: 10,
            })
            .unwrap();
        }
        st.media_evict(20).unwrap();
        assert!(st.media_get("a").unwrap().is_none() && !d.path().join("a").exists());
        assert!(st.media_get("b").unwrap().is_some() && st.media_get("c").unwrap().is_some());
        st.media_evict(0).unwrap();
        assert!(st.media_get("c").unwrap().is_none());
    }

    #[test]
    fn local_uploads_show_their_own_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.png");
        std::fs::write(&f, b"x").unwrap();
        let st = store(d.path(), &[]);
        st.upsert_page(&serde_json::from_value(json!({ "object": "page", "id": "p", "parent": { "type": "workspace", "workspace": true },
            "created_time": T, "last_edited_time": T, "in_trash": false, "url": "", "properties": {} })).unwrap()).unwrap();
        let id = local_create_media(&st, "p", None, &f, MediaKind::Image).unwrap();
        let raw = serde_json::to_value(st.get_block(&id).unwrap().unwrap()).unwrap();
        assert_eq!(
            block_source(&raw).unwrap().local,
            Some(std::fs::canonicalize(&f).unwrap())
        );
    }

    #[tokio::test]
    async fn upload_creates_sends_multipart_then_push_attaches() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let f = d.path().join("pic.png");
        std::fs::write(&f, b"PNGDATA").unwrap();
        let st = store(d.path(), &[]);
        st.upsert_page(&serde_json::from_value(json!({ "object": "page", "id": "p", "parent": { "type": "workspace", "workspace": true },
            "created_time": T, "last_edited_time": T, "in_trash": false, "url": "", "properties": {} })).unwrap()).unwrap();
        let local = local_create_media(&st, "p", None, &f, MediaKind::Image).unwrap();
        Mock::given(method("POST")).and(path("/v1/file_uploads"))
            .and(body_json(json!({ "mode": "single_part", "filename": "pic.png", "content_type": "image/png" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "up1", "upload_url": "x" }))).expect(1).mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/v1/file_uploads/up1/send"))
            .and(header_regex(
                "content-type",
                "multipart/form-data; boundary=.+",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": "up1", "status": "uploaded" })),
            )
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH")).and(path("/v1/blocks/p/children"))
            .and(body_json(json!({ "children": [{ "type": "image", "image": { "type": "file_upload", "file_upload": { "id": "up1" } } }] })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [image("real", "https://h/x.png", soon())] }))).expect(1).mount(&s).await;
        let rep = crate::sync::push(&client(&s), &st).await.unwrap();
        assert_eq!(rep.failed, 0);
        assert!(
            st.get_block(&local).unwrap().is_none() && st.get_block("real").unwrap().is_some(),
            "remapped to the real id"
        );
        let reqs = s.received_requests().await.unwrap();
        let send = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/send"))
            .unwrap();
        let body = String::from_utf8_lossy(&send.body);
        assert!(
            body.contains("name=\"file\"")
                && body.contains("filename=\"pic.png\"")
                && body.contains("PNGDATA"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn too_big_and_missing_files_are_refused_before_any_request() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let big = d.path().join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_UPLOAD + 1)
            .unwrap();
        let c = client(&s);
        assert!(c
            .upload_file(&big)
            .await
            .unwrap_err()
            .to_string()
            .contains("20 MB"));
        assert!(c
            .upload_file(&d.path().join("nope.png"))
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot read"));
        let st = store(d.path(), &[]);
        assert!(local_create_media(&st, "p", None, &big, MediaKind::File).is_err());
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    fn page_with_cover(id: &str, url: &str, expiry: DateTime<Utc>) -> Value {
        json!({ "object": "page", "id": id, "parent": { "type": "workspace", "workspace": true }, "created_time": T,
            "last_edited_time": T, "in_trash": false, "url": "", "properties": {},
            "cover": { "type": "file", "file": { "url": url, "expiry_time": expiry.to_rfc3339() } } })
    }

    #[tokio::test]
    async fn downloads_carry_no_authorization_but_the_re_pull_does() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let old = Utc::now() + chrono::Duration::minutes(4); // inside the 5 min window
        let st = store(
            d.path(),
            &[image("b", &format!("{}/old.png?sig=1", s.uri()), old)],
        );
        Mock::given(method("GET"))
            .and(path("/v1/blocks/b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(image(
                "b",
                &format!("{}/new.png?sig=2", s.uri()),
                soon(),
            )))
            .expect(1)
            .mount(&s)
            .await;
        serve(&s, "/new.png", 200, b"NEW", 1).await;
        fetch(&client(&s), &st, "b", Owner::Block).await.unwrap();
        for r in s.received_requests().await.unwrap() {
            let api = r.url.path().starts_with("/v1/");
            assert_eq!(
                r.headers.contains_key("authorization"),
                api,
                "{}",
                r.url.path()
            );
        }
    }

    #[tokio::test]
    async fn a_url_expiring_in_over_5_minutes_is_not_re_pulled() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st = store(
            d.path(),
            &[image(
                "b",
                &format!("{}/a.png?sig=1", s.uri()),
                Utc::now() + chrono::Duration::minutes(6),
            )],
        );
        serve(&s, "/a.png", 200, b"A", 1).await;
        fetch(&client(&s), &st, "b", Owner::Block).await.unwrap();
        assert!(s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().starts_with("/v1/")));
    }

    #[tokio::test]
    async fn a_page_cover_is_re_pulled_via_the_page_endpoint() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st = store(d.path(), &[]);
        let old = Utc::now() - chrono::Duration::minutes(1);
        st.upsert_page(
            &serde_json::from_value(page_with_cover(
                "p",
                &format!("{}/old.png?sig=1", s.uri()),
                old,
            ))
            .unwrap(),
        )
        .unwrap();
        Mock::given(method("GET"))
            .and(path("/v1/pages/p"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page_with_cover(
                "p",
                &format!("{}/new.png?sig=2", s.uri()),
                soon(),
            )))
            .expect(1)
            .mount(&s)
            .await;
        serve(&s, "/new.png", 200, b"COVER", 1).await;
        let p = fetch(&client(&s), &st, "p", Owner::Page).await.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"COVER");
        assert_eq!(
            st.media_get("p").unwrap().unwrap().url,
            format!("{}/new.png", s.uri())
        );
    }

    #[tokio::test]
    async fn external_urls_are_never_re_pulled() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let mut b = image("b", &format!("{}/e.png?x=1", s.uri()), soon());
        b["image"] = json!({ "type": "external", "caption": [], "external": { "url": format!("{}/e.png?x=1", s.uri()) } });
        let st = store(d.path(), &[b]);
        serve(&s, "/e.png", 403, b"", 1).await;
        let e = fetch(&client(&s), &st, "b", Owner::Block)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("403") && !e.contains("x=1"), "{e}");
        assert!(s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| !r.url.path().starts_with("/v1/")));
    }

    #[tokio::test]
    async fn errors_never_contain_the_signed_url() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st = store(
            d.path(),
            &[image("b", &format!("{}/a.png?sig=SECRET", s.uri()), soon())],
        );
        serve(&s, "/a.png", 500, b"", 1).await;
        let e = fetch(&client(&s), &st, "b", Owner::Block)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("500") && !e.contains("SECRET") && !e.contains("/a.png"),
            "{e}"
        );
        // connection refused: reqwest errors normally embed the URL
        let url = format!("{}/c.png?sig=SECRET", s.uri());
        drop(s);
        let (s2, d2) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let st2 = store(d2.path(), &[image("b", &url, soon())]);
        let e = fetch(&client(&s2), &st2, "b", Owner::Block)
            .await
            .unwrap_err();
        assert!(!format!("{e} {e:?}").contains("SECRET"), "{e:?}");
        assert!(st2.media_get("b").unwrap().is_none());
    }

    #[test]
    fn eviction_deletes_only_the_evicted_files_and_keeps_the_newest() {
        let d = tempfile::tempdir().unwrap();
        let st = store(d.path(), &[]);
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            let path = d.path().join(id);
            std::fs::write(&path, [0u8; 10]).unwrap();
            st.media_put(&CachedMedia {
                owner_id: (*id).into(),
                url: "u".into(),
                fetched_at: format!("2025-01-0{}", i + 1),
                path,
                size: 10,
            })
            .unwrap();
        }
        st.media_evict(40).unwrap(); // exactly at the cap: nothing goes
        assert!(["a", "b", "c", "d"]
            .iter()
            .all(|i| d.path().join(i).exists() && st.media_get(i).unwrap().is_some()));
        st.media_evict(25).unwrap(); // 40 -> 20: a, b go
        for i in ["a", "b"] {
            assert!(!d.path().join(i).exists() && st.media_get(i).unwrap().is_none());
        }
        for i in ["c", "d"] {
            assert!(d.path().join(i).exists() && st.media_get(i).unwrap().is_some());
        }
    }

    #[test]
    fn file_names_are_stable_and_distinct() {
        assert_eq!(file_name("abc"), "e71fa2190541574b"); // FNV-1a 64 of "abc"
        assert_ne!(file_name("abc"), file_name("abd"));
        assert_eq!(file_name("abc").len(), 16);
    }

    #[test]
    fn upload_check_handles_extension_case_and_none() {
        let d = tempfile::tempdir().unwrap();
        for (n, mime) in [
            ("A.PNG", "image/png"),
            ("b.JpG", "image/jpeg"),
            ("c.WEBP", "image/webp"),
            ("noext", "application/octet-stream"),
            ("d.xyz", "application/octet-stream"),
        ] {
            let f = d.path().join(n);
            std::fs::write(&f, b"x").unwrap();
            assert_eq!(check_upload(&f).unwrap(), (n.to_string(), mime));
        }
        assert!(check_upload(d.path())
            .unwrap_err()
            .to_string()
            .contains("not a file"));
        let at = d.path().join("at.bin");
        std::fs::File::create(&at)
            .unwrap()
            .set_len(MAX_UPLOAD)
            .unwrap();
        assert!(check_upload(&at).is_ok(), "exactly 20 MB is allowed");
    }

    #[tokio::test]
    async fn a_vanished_upload_fails_its_op_and_later_ops_still_push() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let (f1, f2) = (d.path().join("a.png"), d.path().join("b.png"));
        std::fs::write(&f1, b"A").unwrap();
        std::fs::write(&f2, b"B").unwrap();
        let st = store(d.path(), &[]);
        st.upsert_page(&serde_json::from_value(json!({ "object": "page", "id": "p", "parent": { "type": "workspace", "workspace": true },
            "created_time": T, "last_edited_time": T, "in_trash": false, "url": "", "properties": {} })).unwrap()).unwrap();
        local_create_media(&st, "p", None, &f1, MediaKind::Image).unwrap();
        local_create_media(&st, "p", None, &f2, MediaKind::Image).unwrap();
        std::fs::remove_file(&f1).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/file_uploads"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": "up1", "upload_url": "x" })),
            )
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/file_uploads/up1/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "up1" })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1/blocks/p/children"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({ "results": [image("real", "https://h/x.png", soon())] }),
                ),
            )
            .expect(1)
            .mount(&s)
            .await;
        let rep = crate::sync::push(&client(&s), &st).await.unwrap();
        assert_eq!((rep.failed, rep.applied), (1, 1));
    }

    /// A chunked answer (no Content-Length) streaming forever-ish.
    fn chunked_stub(chunks: usize) -> String {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/big.png", l.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            for _ in 0..chunks {
                if c.write_all(b"100\r\n")
                    .and_then(|()| c.write_all(&[0u8; 256]))
                    .and_then(|()| c.write_all(b"\r\n"))
                    .is_err()
                {
                    return;
                }
            }
            let _ = c.write_all(b"0\r\n\r\n");
        });
        url
    }

    #[tokio::test]
    async fn a_chunked_download_over_the_cap_is_aborted() {
        let http = reqwest::Client::new();
        let r = download(&http, &chunked_stub(20), 1000, None).await;
        assert!(matches!(r, Err(Dl::Failed(Error::Invalid(m))) if m.contains("50 MB")));
        let ok = download(&http, &chunked_stub(2), 1000, None).await;
        assert!(matches!(ok, Ok(b) if b.len() == 512));
        assert!(matches!(
            download(&http, "file:///etc/passwd", 10, None).await,
            Err(Dl::Failed(_))
        ));
    }

    #[tokio::test]
    async fn private_attachments_are_signed_before_download() {
        let (s, d) = (MockServer::start().await, tempfile::tempdir().unwrap());
        let att = "attachment:42c46cf1-3918-4fe5-ad5e-89e6168c1c4c:image.png";
        let mut b = image("b", "unused", soon());
        b["image"] = json!({
            "type": "external",
            "external": { "url": att },
            "caption": [],
        });
        let st = store(d.path(), &[b]);
        Mock::given(method("POST"))
            .and(path("/api/v3/getSignedFileUrls"))
            .and(body_json(json!({
                "urls": [{
                    "url": att,
                    "permissionRecord": { "table": "block", "id": "b" },
                }]
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "signedUrls": [format!("{}/a.png?sig=1", s.uri())] })),
            )
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/f/refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Set-Cookie", "file_token=ft; Domain=.notion.so; Path=/f"),
            )
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/a.png"))
            .and(header("cookie", "file_token=ft"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PIC"))
            .expect(1)
            .mount(&s)
            .await;
        let p = fetch(&private_client(&s), &st, "b", Owner::Block)
            .await
            .unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"PIC");
        assert_eq!(
            st.media_get("b").unwrap().unwrap().url,
            att,
            "the stable attachment URL is cached, not the signature"
        );
    }

    #[tokio::test]
    async fn a_failed_signed_download_leaves_the_query_out_of_the_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let url = format!("http://127.0.0.1:{port}/a.png?X-Amz-Signature=SECRET");
        let Err(Dl::Failed(e)) = download(&reqwest::Client::new(), &url, 10, None).await else {
            panic!("expected a failure");
        };
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    #[test]
    fn forget_drops_row_and_file_and_stale_parts_are_swept() {
        let d = tempfile::tempdir().unwrap();
        let st = store(d.path(), &[]);
        let p = d.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        st.media_put(&CachedMedia {
            owner_id: "a".into(),
            url: "u".into(),
            fetched_at: "2025".into(),
            path: p.clone(),
            size: 1,
        })
        .unwrap();
        forget(&st, "a").unwrap();
        assert!(st.media_get("a").unwrap().is_none() && !p.exists());
        std::fs::write(d.path().join("x.part"), b"x").unwrap();
        remove_parts(d.path());
        assert!(!d.path().join("x.part").exists() && d.path().join("cache.db").exists());
    }
}
