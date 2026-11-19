# Phase 1 plan: `notion-core` + `notion-cli`

Status: **Approved 2026-11-19 with the default answers to Q1–Q5; Phase 1 built (see §6).** API facts come from `docs/api-notes.md`, which was researched from developers.notion.com.
Unresolved points and decisions that need you are in `docs/open-questions.md`.

## 0. Prerequisites (not code)

- **No Rust toolchain is installed** on this machine (`cargo` and `rustup` are both missing). Proposed fix: `sudo pacman -S rustup && rustup default stable`, which also brings clippy.
- The project folder is empty and not a git repo. I'll run `git init` and add a `.gitignore` covering `target/`, `*.db` and `.env`.

## 1. Workspace layout

```
Cargo.toml                     # [workspace] members = crates/*, shared deps in [workspace.dependencies]
crates/notion-core/
  Cargo.toml
  src/lib.rs                   # pub mod api, model, store, ops, sync, error; re-exports
  src/error.rs                 # Error enum (thiserror)
  src/model.rs                 # serde types
  src/api.rs                   # Client, rate limiter, retry, pagination, recursive fetch
  src/store.rs                 # SQLite schema + mirror read/write
  src/ops.rs                   # op queue (same DB)
  src/sync.rs                  # pull / push
  tests/live_smoke.rs          # #[ignore], needs NOTION_TOKEN
crates/notion-cli/
  Cargo.toml
  src/main.rs                  # clap subcommands, anyhow
docs/plan.md, docs/open-questions.md, docs/api-notes.md
```

Each module is one file. Unit tests sit inline (`#[cfg(test)]`). Wiremock tests live in `api.rs` and `sync.rs`.

## 2. Dependencies

**notion-core:**
- `tokio` (rt, macros, time, sync)
- `reqwest` (default-features off; `json` and `rustls-tls`)
- `serde` and `serde_json`
- `thiserror`
- `tracing`
- `chrono` (with `serde`)
- `rusqlite` (with `bundled`)

**Dev-only:** `wiremock`, `tempfile`, and `tokio` with `test-util`.

**notion-cli:** `clap` (derive), `anyhow` and `tokio`.

These are all on the allowed list; nothing else is added.

### SQLite crate: **rusqlite** (`bundled`)

- **Bundled build.** It compiles SQLite in, so there's no system lib or version drift, and the binaries are reproducible.
- **Sync API.** Store calls are small, local and fast. A plain `Connection` owned by `Store` is simpler than sqlx's pool, runtime and macro or offline-metadata setup. The sync engine awaits the network, then calls the store synchronously. No `spawn_blocking` is needed at this scale.
- **Plain SQL is enough.** We only need one schema and a few queries, so sqlx's compile-time query checking buys nothing. sqlx would also pull in a large dependency tree.
- **Known ceiling:** if a GUI thread and a sync task need the DB at the same time, wrap the store in `Arc<Mutex<Store>>` or open a second connection; WAL mode allows that. That's a Phase 2 call.

## 3. Public API of `notion-core`

```rust
// error.rs
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("http: {0}")]        Http(#[from] reqwest::Error),
    #[error("notion {status} {code}: {message}")]
                                 Api { status: u16, code: String, message: String },
    #[error("rate limited, retries exhausted")] RetriesExhausted,
    #[error("sqlite: {0}")]      Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]        Json(#[from] serde_json::Error),
    #[error("{0}")]              Invalid(String),   // e.g. edit on a non-text block
}
pub type Result<T> = std::result::Result<T, Error>;
```

### `api`

```rust
pub const NOTION_VERSION: &str = "2025-09-03";

pub struct ClientConfig {
    pub base_url: String,        // default "https://api.notion.com"; wiremock overrides it
    pub requests_per_sec: f64,   // default 3.0
    pub burst: u32,              // default 3
    pub max_retries: u32,        // default 5
    pub max_backoff: Duration,   // default 60s, which also caps how long we honour Retry-After
}
pub struct Client { /* reqwest::Client, token, cfg, limiter */ }  // Debug impl redacts the token

impl Client {
    pub fn new(token: String, cfg: ClientConfig) -> Self;
    pub fn from_env() -> Result<Self>;          // reads NOTION_TOKEN; error if missing
    pub async fn get_page(&self, id: &str) -> Result<Page>;
    pub async fn get_block(&self, id: &str) -> Result<Block>;
    pub async fn block_children(&self, id: &str) -> Result<Vec<Block>>;    // all pages of results
    pub async fn block_tree(&self, id: &str) -> Result<Vec<BlockNode>>;    // recursive on has_children
    pub async fn append_children(&self, parent: &str, children: &[Block], after: Option<&str>) -> Result<Vec<Block>>;
    pub async fn update_block(&self, id: &str, body: &serde_json::Value) -> Result<Block>;
    pub async fn delete_block(&self, id: &str) -> Result<Block>;
    pub async fn update_page_properties(&self, id: &str, props: &serde_json::Value) -> Result<Page>;
    pub async fn get_data_source(&self, id: &str) -> Result<DataSource>;
    pub async fn query_data_source(&self, id: &str, filter_sort: serde_json::Value) -> Result<Vec<Page>>; // all pages of results
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>>;  // all pages of results
}
pub struct BlockNode { pub block: Block, pub children: Vec<BlockNode> }
```

**Internals:**
- `send()` is the single choke point for every request:
  - It sets `Authorization: Bearer …`, `Notion-Version` and the JSON content type.
  - It waits on the token bucket.
  - On 429 or 529 it sleeps for `Retry-After` seconds, capped at `max_backoff`. If the header is missing, it falls back to exponential backoff (1, 2, 4, … s, also capped), up to `max_retries` attempts.
  - It never retries 429 with code `public_api_request_blocked`.
  - Any other non-2xx response is parsed into `Error::Api`.
- The token bucket is about 20 lines of hand-written code: `tokio::sync::Mutex<{tokens, last_refill}>` plus `tokio::time::sleep`.
- One generic paginator, `paginate<T>(…)`, loops on `has_more` and `next_cursor` with `page_size=100`. GET endpoints send these as query params; search and query send them in the body.
- **Token safety.** The token is never part of `Debug` or `Display` output and never appears in a `tracing` field. `tracing` logs only the method, path, status and retry count.

### `model`

Responses are read leniently: unknown fields are ignored, and fields we don't use aren't modelled. Each object also keeps its full `raw: serde_json::Value`, so writes and the cache are lossless.

```rust
pub struct RichText { pub plain_text: String, pub href: Option<String>,
                      pub annotations: Annotations, pub kind: RichTextKind }   // text | mention | equation
pub enum RichTextKind { Text { content: String, link: Option<String> }, Mention(Value), Equation { expression: String } }
pub struct Annotations { bold, italic, strikethrough, underline, code: bool, color: String }

pub struct Block {
    pub id: String, pub parent: Parent, pub has_children: bool, pub in_trash: bool,
    pub created_time: DateTime<Utc>, pub last_edited_time: DateTime<Utc>,
    pub kind: BlockKind,
}
pub enum BlockKind {
    Paragraph(TextBlock), Heading1(Heading), Heading2(Heading), Heading3(Heading),
    BulletedListItem(TextBlock), NumberedListItem(TextBlock), ToDo { text: TextBlock, checked: bool },
    Toggle(TextBlock), Quote(TextBlock), Callout(TextBlock), Code { text: TextBlock, language: String },
    Divider, ChildPage { title: String }, ChildDatabase { title: String },
    Unsupported { raw: serde_json::Value },  // every other type, incl. API "unsupported"
}
pub struct TextBlock { pub rich_text: Vec<RichText>, pub color: String }
pub struct Heading   { pub text: TextBlock, pub is_toggleable: bool }

pub enum Parent { Page(String), Block(String), Database(String), DataSource { id: String, database_id: String }, Workspace }

pub struct Page { pub id, parent: Parent, last_edited_time, in_trash, url: String,
                  pub properties: BTreeMap<String, PropertyValue> }
impl Page { pub fn title(&self) -> String }      // the property whose type is "title"

pub enum PropertyValue {
    Title(Vec<RichText>), RichText(Vec<RichText>), Number(Option<f64>), Select(Option<SelectOption>),
    MultiSelect(Vec<SelectOption>), Status(Option<SelectOption>), Date(Option<DateValue>),
    Checkbox(bool), Url(Option<String>), Email(Option<String>), PhoneNumber(Option<String>),
    Relation(Vec<String>), Formula(Value), Rollup(Value),   // read-only; kept as raw JSON
    CreatedTime(DateTime<Utc>), LastEditedTime(DateTime<Utc>),
    Unsupported { raw: Value },                            // people, files, unique_id, button, verification, place, …
}
pub struct DataSource { pub id, database_id: String, title: String, last_edited_time,
                        pub properties: Value /* schema, raw */ }
pub enum SearchResult { Page(Page), DataSource(DataSource) }
```

**Serde strategy for `BlockKind` and `PropertyValue`:**
- Deserialize: read the value as a `Value`, match on `"type"`, and deserialize the known payload. Anything unknown, or anything that fails to deserialize, becomes `Unsupported { raw }`.
- Serialize: write `raw` back unchanged for unsupported values, and `{"type": t, t: {...}}` for known ones.
- This guarantees the unsupported round-trip. The typed set is deliberately just the text blocks the outliner needs. Images, tables, columns and so on stay `Unsupported` in Phase 1 and are still cached and shown as `[<type>]`.

### `store`

```rust
pub struct Store { conn: rusqlite::Connection }
impl Store {
    pub fn open(path: &Path) -> Result<Self>;          // WAL, foreign_keys, migrate via PRAGMA user_version
    pub fn open_in_memory() -> Result<Self>;
    pub fn upsert_page(&self, p: &Page) -> Result<()>;
    pub fn get_page(&self, id: &str) -> Result<Option<Page>>;
    pub fn replace_children(&self, page_id: &str, parent_id: &str, blocks: &[Block]) -> Result<()>; // ordered, drops removed
    pub fn get_block(&self, id: &str) -> Result<Option<Block>>;
    pub fn children(&self, parent_id: &str) -> Result<Vec<Block>>;     // ordered by position
    pub fn put_block(&self, b: &Block) -> Result<()>;                  // local edit applied to cache
    pub fn upsert_data_source(&self, d: &DataSource) -> Result<()>;
    pub fn rows(&self, data_source_id: &str) -> Result<Vec<Page>>;
}
```

### `ops`

```rust
pub enum OpKind {
    CreateBlock { parent_id: String, after: Option<String>, block: Value },
    UpdateBlock { block_id: String, body: Value },
    DeleteBlock { block_id: String },
    MoveBlock   { block_id: String, new_parent_id: String, after: Option<String> }, // see open-questions Q1
    UpdatePageProperties { page_id: String, properties: Value },
}
pub enum OpStatus { Queued, Conflict, Failed }
pub struct Op { pub id: i64, pub kind: OpKind, pub base_last_edited_time: Option<DateTime<Utc>>,
                pub status: OpStatus, pub error: Option<String>, pub created_at: DateTime<Utc> }

impl Store {
    pub fn enqueue(&self, kind: OpKind, base: Option<DateTime<Utc>>) -> Result<i64>;
    pub fn queued_ops(&self) -> Result<Vec<Op>>;            // FIFO
    pub fn ops_by_status(&self, s: OpStatus) -> Result<Vec<Op>>;
    pub fn mark_op(&self, id: i64, s: OpStatus, err: Option<&str>) -> Result<()>;
    pub fn complete_op(&self, id: i64) -> Result<()>;       // deletes the row
}
```

**Local-first behaviour:**
- Ops live in the same DB file, so they survive restarts.
- `edit-block` updates the cached block immediately and enqueues `UpdateBlock`. The base is the block's cached `last_edited_time`.
- A created block gets a temporary id, `local-<op_id>`. During push, an `id_map` table rewrites later ops that reference that temp id.

### `sync`

```rust
pub struct PullReport { pub fetched: usize, pub updated: usize, pub skipped: bool }
pub struct PushReport { pub applied: usize, pub conflicts: usize, pub failed: usize }

pub async fn pull_page(client: &Client, store: &Store, page_id: &str) -> Result<PullReport>;
pub async fn pull_data_source(client: &Client, store: &Store, ds_id: &str) -> Result<PullReport>;
pub async fn push(client: &Client, store: &Store) -> Result<PushReport>;
```

**pull_page:**
1. GET the page. If its `last_edited_time` equals the cached value, stop with `skipped=true`.
2. Otherwise, `block_tree`. Then upsert only blocks whose `last_edited_time` differs from the cache, and remove cached blocks that are gone.
3. Blocks with a queued op are not overwritten; local edits win until push.
4. `child_page` blocks are not recursed into; they are separate pages.
5. Caveat: whether a nested block edit bumps the page timestamp is open question Q3.

**pull_data_source:** GET the data source, then query all rows, upsert them as pages, and skip unchanged rows by `last_edited_time`.

**push:**
1. Ops are processed in FIFO order.
2. For update, delete and move: GET the remote block or page. If `remote.last_edited_time > op.base`, mark the op `Conflict` and leave the remote untouched. Otherwise apply the op, write the response into the cache, and complete the op.
3. On an API error, mark the op `Failed` with the message and continue.
4. The equal-timestamp case is open question Q2.

### `notion-cli`

The DB file defaults to `$XDG_DATA_HOME/notion-client/cache.db`, falling back to `~/.local/share/...`. `--db <path>` overrides it.

| Command | Action |
|---|---|
| `search <query>` | `client.search`, prints `type  id  title` |
| `pull <page-id>` | `sync::pull_page`, prints the report |
| `cat <page-id>` | from cache only: page title, then blocks as indented Markdown-ish text (`#`, `-`, `1.`, `[ ]`/`[x]`, `>`, fenced code, `---`, `[<type>]` for unsupported) |
| `edit-block <block-id> <text>` | replaces `rich_text` with a single plain text run; errors on non-text blocks or blocks that aren't cached |
| `push` | `sync::push`, prints the report |
| `status` | lists queued and conflicted (and failed) ops |

## 4. SQLite schema (v1)

```sql
PRAGMA journal_mode = WAL;
PRAGMA user_version = 1;

CREATE TABLE pages (
  id               TEXT PRIMARY KEY,
  parent_type      TEXT NOT NULL,          -- page_id | block_id | database_id | data_source_id | workspace
  parent_id        TEXT,                   -- data_source_id for DB rows => "rows" = pages WHERE parent_type='data_source_id'
  title            TEXT NOT NULL DEFAULT '',
  in_trash         INTEGER NOT NULL DEFAULT 0,
  last_edited_time TEXT NOT NULL,          -- RFC 3339 UTC
  raw              TEXT NOT NULL           -- full JSON from the API
);
CREATE INDEX pages_parent ON pages(parent_id);

CREATE TABLE blocks (
  id               TEXT PRIMARY KEY,
  page_id          TEXT NOT NULL,          -- root page this block belongs to
  parent_id        TEXT NOT NULL,          -- page id or block id
  position         INTEGER NOT NULL,       -- 0-based order among siblings
  type             TEXT NOT NULL,
  has_children     INTEGER NOT NULL,
  last_edited_time TEXT NOT NULL,
  raw              TEXT NOT NULL
);
CREATE INDEX blocks_parent ON blocks(parent_id, position);
CREATE INDEX blocks_page   ON blocks(page_id);

CREATE TABLE data_sources (
  id               TEXT PRIMARY KEY,
  database_id      TEXT NOT NULL,
  title            TEXT NOT NULL DEFAULT '',
  last_edited_time TEXT NOT NULL,
  raw              TEXT NOT NULL
);

CREATE TABLE ops (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  kind                  TEXT NOT NULL,     -- create_block | update_block | delete_block | move_block | update_page_properties
  target_id             TEXT NOT NULL,
  payload               TEXT NOT NULL,     -- serde_json of OpKind
  base_last_edited_time TEXT,
  status                TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','conflict','failed')),
  error                 TEXT,
  created_at            TEXT NOT NULL
);

CREATE TABLE id_map (local_id TEXT PRIMARY KEY, remote_id TEXT NOT NULL);
```

**Why these choices:**
- `raw` JSON is the source of truth. The other columns are just for querying and ordering.
- Sibling order is a plain integer `position`. Inserting renumbers that parent's siblings, which is fine for outline-sized lists.

## 5. Task breakdown (after approval)

| # | Task | Files | Acceptance tests |
|---|---|---|---|
| T0 | Toolchain, git init, workspace skeleton (done by me) | root `Cargo.toml`, both crate `Cargo.toml`s, stub `lib.rs`/`main.rs`, `.gitignore` | `cargo build`, `cargo clippy --all-targets -- -D warnings` |
| T1 | `error` + `model` | `error.rs`, `model.rs`, `lib.rs` | fixture JSON (from docs examples) for every typed block and property type parses; an unknown block type round-trips byte-equal as JSON; `type:"unsupported"` maps to `Unsupported`; `Page::title` |
| T2 | `api` | `api.rs` | wiremock tests: `Notion-Version` and auth headers present; 429 with `Retry-After: 1` then 200 → succeeds, 2 calls; 529 retried; retries exhausted → error; pagination across 3 pages; recursive `block_tree` 3 levels deep; limiter spaces requests (`tokio::time::pause`); `Debug` output has no token |
| T3 | `store` + `ops` | `store.rs`, `ops.rs` | in-memory: upsert/get page, child ordering, `replace_children` drops removed blocks; tempfile: enqueue, drop the `Store`, reopen, op still queued; status transitions |
| T4 | `sync` | `sync.rs` | wiremock + in-memory store: pull fills the cache; a second pull with unchanged page timestamp makes no block calls; push applies an update and clears the op; a newer remote timestamp marks a conflict and sends no PATCH; local edit not overwritten by pull |
| T5 | `notion-cli` + live smoke test + doc update | `notion-cli/src/main.rs`, `tests/live_smoke.rs`, `docs/plan.md` | `cat` rendering unit test from an in-memory store; `#[ignore]` smoke test: pull + cat a real page with NOTION_TOKEN; then a manual `pull`/`cat` run by you |

**Order and parallelism:**
- T1 runs first.
- T2 and T3 then run in parallel; their files don't overlap.
- After that T4, then T5.

**Process per task:**
- Each task goes to a `general-purpose` implementer subagent, then a separate reviewer subagent, with at most 2 review rounds.
- The `developer` and `reviewer` agents defined on this machine are scoped to another project (Lunch Map Wels), so I'll brief generic agents instead.
- One commit per approved task.

## 6. What was built (Phase 1)

Commits: T0 skeleton → T1 model → T2 api → T3 store+ops → T4 sync → T5 CLI. Each task was implemented by one agent and reviewed by a separate one, with at most 2 review rounds. Gate: `cargo test` and `cargo clippy --all-targets -- -D warnings`. Tests never hit the real API, apart from `tests/live_smoke.rs` (`#[ignore]`, needs `NOTION_TOKEN` and `NOTION_TEST_PAGE_ID`, read-only).

**Changes from §3, made during implementation (all small additions):**
- **`Parent::Other(Value)`.** Unknown parent types such as `agent_id` stay lossless instead of failing the whole response. You approved this.
- **`Op.base_raw` and the `ops.base_raw` column.** A snapshot of the object the edit was based on, used for the Q2 equal-timestamp content comparison.
- **`sync::pull_page(.., force)`.** Plus the CLI `pull --full` (Q3).
- **Two local-edit helpers:** `sync::local_edit_text` and `sync::local_create_block`.
- **`Store` additions:** `delete_block` and `rename_block`. **Op-queue additions:** `map_id` and `resolve_id`, for temporary `local-*` ids.
- **`append_children` chunks requests to ≤100 children.** Later chunks pass `after` = the last created id.
- **The paginator warns on an incomplete `request_status`.** This happens when a query hits Notion's 10k result cap.
- **Pull skip rule.** A page is skipped only if its cached `last_edited_time` is equal **and** it already has cached blocks.
- **Conflict check details.**
  - Conflict if `remote > base`, or if the times are equal and the content differs from base.
  - Inside one push, our own writes become the new base, so two queued edits to one block both apply.
  - A same-minute remote edit after our write is still caught.
- **MoveBlock does copy-then-delete (Q1).**
  - It is refused, with no writes, if the subtree contains `unsupported`, `child_page` or `child_database`.
  - Every copied block's old id is mapped to its new id.
- **Error handling in push.**
  - `Error::Api` marks the op `Failed` and push continues.
  - Network errors, `RetriesExhausted` and SQLite errors abort push and leave the op queued.
- **CLI id input.** Ids are normalised from dashed or undashed form or a Notion URL (query string stripped). `cat`, `edit-block` and `status` work offline without a token.

## 7. Known gaps (for Phase 2)

1. **Partial failures aren't idempotent.**
   - If a create or move fails halfway, or a cache write fails after the remote write succeeded, the op stays queued. Re-running it can duplicate blocks on Notion.
   - Chunked `append_children` isn't atomic either.
2. **Own-writes tracking only lives within one push.** If a push aborts after a create, a later queued edit of that block can be flagged as a false conflict.
3. **Rows can skip their blocks.** `pull_data_source` updates a row's `last_edited_time` in the cache. A row already pulled with blocks can then be skipped by a later unforced `pull_page`. Workaround: `pull --full`. A "blocks pulled at" marker would fix it.
4. **Does a nested edit bump the page's `last_edited_time`?** Not stated in the docs (Q3). Verify with the live run; if it doesn't, pull must always walk the tree.
5. **Rich text sent back to Notion.** We send rich text in the response shape (`plain_text`, `href`, read-only mention types). Notion may reject some of it in PATCH/append bodies (open-questions B13).
6. **Sync futures are `!Send`.** `&Store` (a rusqlite `Connection`) is held across awaits. The GUI will need a current-thread runtime, a dedicated sync thread, or `Arc<Mutex<Store>>` / a connection per task.
7. **No 5xx retry** (Q5 default). A 503 on a write is left `Failed` for a manual re-push.
8. **Only text-ish blocks are typed.** Images, tables, columns, synced blocks and so on are cached and shown as `[type]` but can't be edited. Most property types are read-only or `Unsupported`.
9. **Data source queries return at most 10k results.** Beyond that we only log a warning.
10. **Creating a local block ignores `after` for the cached position.** It's appended at the end until the next pull.
11. **No CLI command for data sources** (`pull_data_source` is core-only), and **no conflict resolution UI.** Conflicted ops just stay listed in `status`.
