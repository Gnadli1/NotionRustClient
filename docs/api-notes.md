# Notion public REST API — reference notes for the Rust client

Target: `Notion-Version: 2025-09-03`. Researched 2026-11-19 from developers.notion.com.

**Caveat that applies to everything below:** the live reference pages are now rendered for
the newer version `2026-03-11` (their OpenAPI schemas pin `Notion-Version: 2026-03-11`).
Facts specific to 2025-09-03 come from the upgrade guides, the "changes by version" page,
and the deprecation notes inside the reference. Each `2026-03-11` difference is marked
**[v2026]**. Anything the docs don't state is marked **UNCLEAR**.

Sources (main): /reference/intro, /reference/request-limits, /reference/status-codes,
/reference/versioning, /reference/changes-by-version, /reference/pagination,
/guides/get-started/upgrade-guide-2025-09-03, /guides/get-started/upgrade-guide-2026-03-11,
/reference/block, /reference/{retrieve-a-block,get-block-children,patch-block-children,
update-a-block,delete-a-block}, /reference/page, /reference/{retrieve-a-page,
retrieve-a-page-property,post-page,patch-page,move-page}, /reference/parent-object,
/reference/{retrieve-a-data-source,query-a-data-source,database-retrieve,
filter-data-source-entries,post-database-query-sort}, /reference/post-search,
/reference/rich-text, /reference/page-property-values, /reference/property-item-object,
/reference/property-object, /page/changelog, /guides/resources/historical-changelog.

---------------------------------------------------------------------------------------
## 0. Transport basics

- Base URL: `https://api.notion.com`, HTTPS only. All paths below are prefixed `/v1/`.
- Headers on every request:
  - `Authorization: Bearer <token>`
  - `Notion-Version: 2025-09-03` (required on all REST calls; missing → 400 `missing_version`)
  - `Content-Type: application/json` when there is a JSON body (file uploads use multipart).
  - Optional `Notion-Beta: <name>` for beta features (malformed/unknown → 400 `invalid_beta`).
- IDs: UUIDs; page and database IDs are accepted with or without dashes.
- Field names are `snake_case`. Every top-level resource has an `object` discriminator string.
- Timestamps/dates: ISO 8601 (example in docs `"2026-03-11T09:30:00.000Z"`). See §10.
- Property IDs are URL-encoded strings (e.g. `f%5C%5C%3Ap`); pass them **as returned** in
  paths, don't re-encode. Title property ID is always `title`.

---------------------------------------------------------------------------------------
## 1. Request limits & errors

### 1.1 Rate limits (request-limits page, current text)
- Per connection (integration), fixed 60-second window:
  - Business/Enterprise: "600 requests per minute (an average of 10 per second)".
  - All other plans: "180 requests per minute (an average of 3 per second)".
  - Requests may be spent at any pace inside the window.
- Data-source query has its own extra limit: "about 3 requests per second per connection".
- Per-workspace limit shared by all connections of the workspace, scaled to plan; value not
  published. "`Retry-After` for this limit can be longer than a minute." (added 2026-06-16)
- Per-connection `Retry-After` is "at most 60 seconds" (changed 2026-09-09; previously up to 15 min).
- Notion "plans to adjust rate limits" — treat numbers as non-contractual.

### 1.2 429 / 529 behaviour
- 429, `code: "rate_limited"`. `additional_data.rate_limit_reason` says which limit:
  `public_api_request_rate_limit`, `public_api_space_request_rate_limit`,
  `public_api_endpoint_rate_limit`, `mcp_tool_rate_limit`,
  `collection_router_too_many_requests`, `collection_router_upstream_429`,
  `public_api_request_blocked` (→ "Retrying won't help. Contact Notion support.").
- `Retry-After` header: "The header value is an integer number of seconds."
- Body repeats it as `additional_data.retry_after`: "a string of whole seconds" (since 2026-09-24,
  on any 429/529 that has the header).
- 529, `code: "service_overload"`: "Notion is temporarily overloaded" — retry like 429, respect `Retry-After`.

### 1.3 Retry guidance (request-limits + status-codes)
- Retry: 429 (except `public_api_request_blocked`) and 529.
- Retry 500/502/503/504 **only for idempotent requests (GET, DELETE)** unless you have your own
  idempotency protection. Exponential backoff with jitter + a retry cap.
- 503 on a write "may already be saved": check `additional_data.retry_guidance` and re-read the
  object instead of repeating the write. Shape/values of `retry_guidance`: **UNCLEAR**.
- 409 `conflict_error`: "Make sure the parameters are up to date and try again."
- The JS SDK caps the Retry-After wait at 60 s and never retries 502/504 (informational).

### 1.4 Error body
From the OpenAPI schemas on reference pages:
```
{ "object": "error",            // const, required
  "status": 400,                // int, required (per-status variants)
  "code": "validation_error",   // string enum, required
  "message": "...",             // string, required
  "additional_data": { ... } }  // optional; values are string | [string]
```
- Schemas set `additionalProperties: false` and do **not** list `request_id`, but the
  status-codes page says to include `request_id` "if available" when contacting support.
  → treat `request_id` as optional/unknown-field-tolerant. **UNCLEAR**.
- Deserialize `code` as an open enum (schemas include codes not on the status-codes page).

| HTTP | code | meaning |
|---|---|---|
| 400 | `invalid_json` | body not decodable as JSON |
| 400 | `invalid_request_url` | URL not valid |
| 400 | `invalid_request` | request not supported |
| 400 | `invalid_grant` | OAuth grant invalid/expired/revoked |
| 400 | `validation_error` | body doesn't match schema (also size-limit violations) |
| 400 | `missing_version` | no `Notion-Version` header |
| 400 | `invalid_beta` | bad `Notion-Beta` header |
| 400 | `invalid_credit_limit` | (schema only, undocumented) |
| 401 | `unauthorized` | bearer token invalid |
| 403 | `restricted_resource` | lacks permission or workspace block limit exceeded |
| 403 | `status_change_not_allowed`, `workspace_credits_exhausted`, `agent_credit_limit_reached` | schema only |
| 404 | `object_not_found` | doesn't exist **or not shared with the integration** |
| 404 | `directory_not_found` | schema only |
| 409 | `conflict_error` | transaction collision (also File Upload storage downtime) |
| 429 | `rate_limited` | see 1.2 |
| 500 | `internal_server_error` | unexpected |
| 502 | `bad_gateway` | |
| 503 | `service_unavailable` | unavailable or response not built in time (write may have applied) |
| 503 | `database_connection_unavailable` | |
| 504 | `gateway_timeout` | |
| 529 | `service_overload` | temporarily overloaded |

Capability errors: endpoints return 403 when the integration lacks read/insert/update content
capability.

### 1.5 Size limits (request-limits page). Violations → 400 `validation_error`.
| What | Limit |
|---|---|
| rich text `text.content` | 2000 characters |
| rich text `text.link.url` | 2000 characters |
| rich text `equation.expression` | 1000 characters |
| any array of blocks or rich text objects | 100 elements |
| any URL | 2000 chars |
| any email / phone number | 200 chars |
| multi_select | 100 options |
| relation | 100 related pages |
| people | 100 users |
| whole payload | 1000 block elements **and** 500 KB |
- Limits are per request, not on what a property can hold; responses may contain more.
- "characters" — unit (UTF-16 code units vs code points vs bytes) **UNCLEAR**.
- Append children: max 100 children per request; "up to **two** levels of nesting in a single
  request" (patch-block-children). The request-limits page only says depth is limited without a
  number. The OpenAPI schema names (`WithSingleLevelOfChildren` / `WithoutChildren`) suggest
  top-level + one nested level — consistent with "two levels" if top level counts as one.
  Exact interpretation **UNCLEAR**; safest: children may have children, grandchildren may not.
- Create page `children`/`content`: max 100 blocks.
- Title / description arrays on databases & data sources: max 100 rich text items.
- Select / status options and status groups: max 100 each (data source schema).
- Data source query: max 10,000 results per query (see §6). `filter_properties`: max 100.

---------------------------------------------------------------------------------------
## 2. Versioning

- `Notion-Version` is a dated string. Named in docs: `2021-05-13`, `2021-08-16`,
  `2022-06-28`, `2025-09-03`, `2026-03-11` (latest at time of research). The versioning page
  gives no complete list.
- 2025-09-03 released 2025-08-26 ("separates databases (containers) from data sources (tables)").

### 2.1 2022-06-28 → 2025-09-03 (upgrade guide)
- `GET /v1/databases/{id}` no longer returns the schema; returns `data_sources: [{id, name}]`.
- New `GET /v1/data_sources/{id}` returns schema (`object: "data_source"`, `properties`,
  `parent` = database, `database_parent` = the database's parent).
- Rows are queried via data source query (replaces `POST /v1/databases/{id}/query`).
  Method: reference says **POST** `/v1/data_sources/{id}/query`; upgrade guide comment says
  `PATCH`. → contradiction; reference (POST) is authoritative.
- New create data source endpoint; `POST /v1/databases` takes schema under
  `initial_data_source.properties`; `PATCH /v1/databases/{id}` handles only `parent`, `title`,
  `is_inline`, `icon`, `cover`, `in_trash`; schema edits go to `PATCH /v1/data_sources/{id}`.
- Database and data source IDs are **not interchangeable**.
- Page parent in create: `{"type":"data_source_id","data_source_id":"..."}` (accepted on any
  version). Responses' page `parent` is `data_source_id` + convenience `database_id`.
- Relation: responses include both `database_id` and `data_source_id`; on 2025-09-03 requests
  must use **only** `data_source_id`.
- Database mentions in rich text still reference the database, not the data source.
- Search: with object filter, `"data_source"` replaces `"database"`. Results contain data source
  objects; one database can yield multiple results. Matching uses the **database** title.
- Webhooks: `database.content_updated`→`data_source.content_updated`,
  `database.schema_updated`→`data_source.schema_updated`; new `data_source.created|moved|
  deleted|undeleted`.
- Old version against a multi-source database: create-page-with-database-parent, read/write/query
  database, relation writes fail; retrieve → `validation_error`; search omits such databases.

### 2.2 2025-09-03 → 2026-03-11 (what we must NOT use on 2025-09-03, or must tolerate)
1. Append children: flat `"after": "<block_id>"` → `"position": {"type":"after_block",
   "after_block":{"id":"..."}}` (also `start`, `end`; default `end`).
2. `archived` removed in favour of `in_trash` everywhere (requests + responses).
   On 2025-09-03: block reference says `archived` "aliases `in_trash` before API version
   `2026-03-11`" → both fields present in responses; `archived` deprecated since April 2024.
3. Block type `transcription` renamed `meeting_notes`.
- Whether `position` is also accepted under 2025-09-03: **UNCLEAR** (the reference says "You
  cannot specify both `after` and `position`", implying some version accepts both; nothing
  states which). Use `after` on 2025-09-03.

---------------------------------------------------------------------------------------
## 3. Pagination

Request:
- `start_cursor` (string): omit on first request; then pass previous `next_cursor` unchanged.
- `page_size` (int): most endpoints default 100, accept 1–100.
- GET endpoints: query string. POST endpoints (search, data source query): JSON body.
Response (`object: "list"`):
```
{ "object": "list",
  "results": [ ... ],
  "next_cursor": "string" | null,
  "has_more": bool,
  "type": "block" | "page_or_data_source" | "property_item" | ...,
  "<type>": { ... },              // e.g. "block": {}, "property_item": {...}
  "request_status": {...} }       // query & search only, see §6
```
- Response "may contain fewer than `page_size`" results; loop on `has_more`, not count.
- `type` values seen: `block`, `page_or_data_source`, `property_item` (users/comments lists
  exist too; full enum not listed anywhere → **UNCLEAR**, deserialize as open enum).

| Endpoint | Method | Params in |
|---|---|---|
| `/v1/blocks/{id}/children` | GET | query |
| `/v1/pages/{id}/properties/{prop_id}` | GET | query |
| `/v1/users` | GET | query |
| `/v1/comments` | GET | query |
| `/v1/search` | POST | body |
| `/v1/data_sources/{id}/query` | POST | body |

---------------------------------------------------------------------------------------
## 4. Blocks

### 4.1 Endpoints
- `GET /v1/blocks/{block_id}` → block object (schema allows partial `{object,id}` or full).
  Page IDs are valid block IDs.
- `GET /v1/blocks/{block_id}/children?start_cursor=&page_size=` → list, `type: "block"`,
  `block: {}`. **First level only**; recurse where `has_children == true`. Page content =
  children of the page ID.
- `PATCH /v1/blocks/{block_id}/children` body:
  - `children`: [block] (required, max 100, ≤2 nesting levels per request)
  - `after`: block id string — insert after this sibling (2025-09-03 form; deprecated in ref)
  - `position` **[v2026]**: `{type:"start"} | {type:"end"} | {type:"after_block",after_block:{id}}`
  - `notifications`: `{mode: "default"|"silent"}` (version applicability **UNCLEAR**)
  - Response: list of **newly created first-level children** only (paginated list shape).
  - "Existing blocks cannot be moved using this endpoint."
  - Not creatable (inferred from request schema; docs don't list explicitly): `child_page`,
    `child_database` (use page/database create), `link_preview`, `meeting_notes`/`transcription`,
    `unsupported`. `template` creation deprecated since 2023-03-27.
  - `column_list` needs ≥2 columns; `table` needs ≥1 row (and rows must be given at create).
- `PATCH /v1/blocks/{block_id}` body: `{ "<type>": {...type fields}, "archived"|"in_trash": bool }`
  - "The update replaces the *entire* value for a given field"; omitted fields unchanged.
  - Children cannot be updated here. `child_page` → update page; `child_database` → update database.
  - Changing a block's `type`: not addressed in docs (**UNCLEAR**; assume unsupported).
  - Synced block content cannot be updated via API.
  - `table.table_width` cannot change after creation.
- `DELETE /v1/blocks/{block_id}` → sets `in_trash: true` (pages too); returns block object;
  restorable via update block / update page (`in_trash: false`).

### 4.2 Moving
- **Blocks cannot be moved** by the API ("Moving databases or other block types in the API is not
  currently supported" — move-page reference). Emulate with copy + delete.
- Pages: `POST /v1/pages/{page_id}/move` (added 2026-01-15), body
  `{"parent": {"type":"page_id","page_id":"..."} | {"type":"data_source_id","data_source_id":"..."}}`.
  Target must be a regular page (not a block, not a database); needs edit access to new parent.
  Works under 2025-09-03? Not stated (**UNCLEAR**, probably version-independent).
  Note patch-page reference still says "A page's `parent` cannot be changed" (via PATCH).

### 4.3 Block object (common)
```
object: "block"
id: uuid string
parent: {type:"page_id",page_id} | {type:"block_id",block_id} | {type:"data_source_id",...}
        | {type:"workspace",workspace:true} | agent_id (agent pages only)
type: string (open enum)
created_time, last_edited_time: ISO-8601 string
created_by, last_edited_by: {object:"user", id}
has_children: bool
archived: bool   // deprecated alias of in_trash; present on 2025-09-03, removed [v2026]
in_trash: bool
<type>: {...}    // type-specific payload keyed by type name
```

### 4.4 Block types and payloads
Enum in /reference/block: bookmark, breadcrumb, bulleted_list_item, callout, child_database,
child_page, column, column_list, divider, embed, equation, file, heading_1, heading_2,
heading_3, heading_4, image, link_preview, numbered_list_item, paragraph, pdf, quote,
synced_block, table, table_of_contents, table_row, template, to_do, toggle, transcription,
unsupported, video. Also documented but missing from that enum: audio, code, meeting_notes,
tab. `link_to_page` is **absent from /reference/block** but present in the append request
schema. → Deserialize `type` as an open enum with a catch-all.

`color` enum (blocks): default, gray, brown, orange, yellow, green, blue, purple, pink, red, and
each `<c>_background`. (Same set as rich-text annotation colors.)

| type | payload fields |
|---|---|
| paragraph | `rich_text`[], `color`, `children`? (create), `icon`? (only under a tab) |
| heading_1/2/3 (/4) | `rich_text`[], `color`, `is_toggleable` bool; children only if toggleable. heading_4 version availability **UNCLEAR** |
| bulleted_list_item | `rich_text`, `color`, `children`? |
| numbered_list_item | `rich_text`, `color`, `children`?, `list_start_index`? int, `list_format`? "numbers"/"letters"/"roman" (first item only) |
| to_do | `rich_text`, `checked`? bool, `color`, `children`? |
| toggle | `rich_text`, `color`, `children`? |
| quote | `rich_text`, `color`, `children`? |
| callout | `rich_text`, `icon` (emoji/custom_emoji/native icon/file, e.g. `{"emoji":"⭐"}`), `color`; listed as supporting children |
| code | `rich_text`, `caption`[], `language` (72-value enum incl. "plain text", "c++", "rust", "java/c/c++/c#") |
| divider, breadcrumb, column_list | `{}` |
| column | `width_ratio`? number 0..1 (ratios should sum to 1) |
| child_page | `title`: plain string (read-only via blocks API) |
| child_database | `title`: plain string |
| synced_block | `synced_from`: null (original; holds children) or `{type:"block_id", block_id}` |
| table | `table_width` int, `has_column_header` bool, `has_row_header` bool |
| table_row | `cells`: [[rich_text]] |
| link_to_page | `{type:"page_id",page_id}` / `{type:"database_id",database_id}` / `{type:"comment_id",comment_id}` |
| equation | `expression` (KaTeX string) |
| image/file/pdf/video/audio | `type`: "file"/"external"/"file_upload"; `file:{url,expiry_time}` (temporary URL, re-fetch after expiry) / `external:{url}` / `file_upload:{id}` (request only; responses say `file`); `caption`[]; `file` block also `name` |
| bookmark | `url`, `caption`[] |
| embed | `url` |
| link_preview | `url` (read-only) |
| table_of_contents | `color` |
| template | `rich_text`, `children` (creation deprecated) |
| tab | `{}`; only paragraph children (label in rich_text, optional icon, content in children) |
| meeting_notes / transcription | `title`[], `status`, `children:{summary_block_id,notes_block_id,transcript_block_id}`, `calendar_event`, `recording`, `is_full_page` |
| unsupported | `unsupported: {block_type: "<string>"}` informational (e.g. form, button, drive) |

Unsupported blocks appear in children listings as `type: "unsupported"`; they cannot be
recreated, so a round-trip client must preserve (not delete/recreate) them.

---------------------------------------------------------------------------------------
## 5. Pages

### 5.1 Page object
```
object: "page", id
created_time, last_edited_time: ISO-8601
created_by, last_edited_by: partial user {object:"user", id}
archived: bool (deprecated alias; removed [v2026])   in_trash: bool
is_locked: bool (patch-page mentions it; not in page object docs — UNCLEAR whether returned)
icon: null | emoji | external | file | file_upload | custom_emoji | native icon
cover: null | external | file | file_upload
parent: page_id | data_source_id (+database_id) | block_id | workspace | agent_id
properties: { "<Property Name>": { id, type, <type>: value } }   // keyed by name
url: string, public_url: string|null
```
- Under a page/workspace parent, the only property is `title`.
- **Title**: find the property whose `type == "title"` (key name varies, e.g. "Name");
  concatenate `title[].plain_text`. Page response title is capped (≤25 inline mentions) — use
  property item endpoint for full value.
- Content is not in the page object; use block children.

### 5.2 Endpoints
- `GET /v1/pages/{page_id}?filter_properties=<id>&filter_properties=<id>` (≤100; repeat param).
  Response limits: relation ≤25 refs (`has_more`), people >25 may be omitted,
  title/rich_text ≤25 inline mentions; formula/rollup may be partial.
- `GET /v1/pages/{page_id}/properties/{property_id}?start_cursor=&page_size=`
  - Single item: `{object:"property_item", id, type, <type>: value}` (most types; multi_select
    and files return one item whose value is an array).
  - Paginated (`title`, `rich_text`, `people`, `relation` — always a list, even 1 item; and
    `rollup`): `{object:"list", type:"property_item", results:[property_item...],
    property_item:{id, type, next_url, <type>:{}}, next_cursor, has_more}`.
  - Rollup: `property_item.rollup` has `type`, `function`; may be
    `{"type":"incomplete","incomplete":{},"function":"sum"}` until all pages read.
- `POST /v1/pages` body: `parent` (page_id | data_source_id | database_id | workspace — internal
  integrations must give page or data source), `properties`, `children`/`content` (≤100
  blocks), `markdown` (+`markdown_version`, `allow_async`; mutually exclusive with children),
  `icon`, `cover`, `template` (`none`|`default`|`template_id`, +`timezone`; no children with a
  template), `position` (`page_start`|`page_end`|`after_block`), `notifications`.
  Version availability of `markdown`/`position`/`template` under 2025-09-03: **UNCLEAR**.
  Formula, rollup, unique_id, created_*/last_edited_* and button cannot be set.
  Response 200 page (202 `async_task` only with `allow_async` + markdown).
- `PATCH /v1/pages/{page_id}` body: `properties` (partial; arrays replace whole value),
  `in_trash` (true = trash, false = restore), `archived` (2025-09-03 alias; the current schema
  shows `is_archived` — **UNCLEAR**), `icon`/`cover` (nullable), `is_locked` (UI lock only),
  `template`, `erase_content` (deletes all children, irreversible), `notifications`.
  Rollups cannot be updated; parent cannot change here (use move).

---------------------------------------------------------------------------------------
## 6. Databases & data sources

- `GET /v1/databases/{database_id}` → `object:"database"`, `id`, `title`[] , `description`[],
  `parent` (page_id | workspace | database_id | block_id), `is_inline`, `database_type`
  (string|null), `in_trash` (+`archived` alias on 2025-09-03), `is_locked`,
  `show_data_source_title`, `created_time`, `last_edited_time`,
  `data_sources: [{id, name, sync_metadata?}]` (≤100), `icon`, `cover`, `url`, `public_url`.
  (Some of these fields may be [v2026]-only additions; **UNCLEAR** which.)
- `GET /v1/data_sources/{data_source_id}` → `object:"data_source"`, `id`, `title`[],
  `description`[], `parent: {type:"database_id", database_id}` (or data_source_id for
  externally synced), `database_parent` (the database's parent), `is_inline`, `database_type`,
  `in_trash`, `created_time`, `last_edited_time`, `created_by`, `last_edited_by`,
  `properties: { "<name>": {id, name, description, type, <type>: config} }`, `icon`, `cover`,
  `url`, `public_url`, `sync_metadata?`. Relation-based properties omitted unless the related
  database is shared with the integration.
  The `properties` map is documented as "map of property ID to property config"; examples are
  keyed by name. **UNCLEAR**; treat keys as names and use inner `id`.
- `POST /v1/data_sources/{data_source_id}/query`
  - Query string: `filter_properties` (repeatable).
  - Body (`additionalProperties: false`): `filter`, `sorts`, `start_cursor`, `page_size` (1–100),
    `archive_status` ("active" default | "archived" | "all"), `is_archived` (deprecated),
    `result_type` ("page" | "data_source"). archive_status/result_type on 2025-09-03: **UNCLEAR**.
  - Response: `object:"list"`, `type:"page_or_data_source"`, `results`, `has_more`,
    `next_cursor`, `request_status: {type:"complete"|"incomplete",
    incomplete_reason?:"query_result_limit_reached"}`. Max 10,000 results per query (since
    2026-04-20): check `request_status` on every page.
  - 503 on timeouts: retry with smaller `page_size`.
- Filters: `{property:"<name|id>", <type>: {<condition>: value}}`; compound `{and:[..]}`/`{or:[..]}`
  ≤100 items per array, "Nesting is supported up to two levels deep". Timestamp filter:
  `{timestamp:"last_edited_time"|"created_time", last_edited_time:{on_or_after:"..."}}` — no
  `property` key (error if given). Date conditions: equals, before, after, on_or_before,
  on_or_after, is_empty, is_not_empty, past_/next_week|month|year, this_week. Datetime strings
  compared "with millisecond precision"; no TZ → UTC.
- Sorts: `[{property, direction} | {timestamp:"created_time"|"last_edited_time", direction}]`,
  direction "ascending"|"descending"; earlier entries take precedence. No documented max.

---------------------------------------------------------------------------------------
## 7. Search — `POST /v1/search`
Body: `query` (string, title match; omit = all), `filter: {property:"object",
value:"page"|"data_source"}` (+ `in_trash` bool in current ref), `sort: {direction,
timestamp:"last_edited_time"}` (current ref also `property:"relevance"`), `start_cursor`,
`page_size`. Query param `filter_properties`.
Response: list, `type:"page_or_data_source"`, results = page objects / data source objects,
`request_status`. Only objects shared with the integration; excludes duplicate linked databases.
Upgrade guide phrases the filter as `filter["type"] = "object"` but reference uses
`property: "object"` (use reference). Indexing delay / completeness: **UNCLEAR** (linked
"search limitations" page not reviewed). Don't use search for exhaustive sync.

---------------------------------------------------------------------------------------
## 8. Rich text object
```
{ "type": "text"|"mention"|"equation",
  "text": {"content": str, "link": {"url": str} | null},
  "mention": {"type": <sub>, <sub>: {...}},
  "equation": {"expression": str},
  "annotations": {"bold","italic","strikethrough","underline","code": bool, "color": enum},
  "plain_text": str, "href": str|null }
```
- Mention subtypes documented: `user {object:"user", id}`, `page {id}`, `database {id}`,
  `date {start, end}`, `link_preview {url}`, `template_mention {type:"template_mention_date",
  template_mention_date:"today"|"now"} | {type:"template_mention_user", template_mention_user:"me"}`.
  Other subtypes (e.g. link_mention, custom_emoji) are not documented → open enum.
- `plain_text`/`href`: returned in responses; docs don't say whether ignored on write (**UNCLEAR**).
- Limits from §1.5: content 2000, link url 2000, expression 1000, ≤100 items per array.
  Long text must be split across multiple rich text items.

---------------------------------------------------------------------------------------
## 9. Property values (page `properties[name]` = `{id, type, <type>: value}`)
Writable:
- `title`, `rich_text`: [rich text]; clear `[]`.
- `number`: number|null.
- `select`, `status`: `{id,name,color}`|null; write by id or name; status can't create options.
- `multi_select`: [{id,name,color}]; names can't contain commas; replaces whole array.
- `date`: `{start, end|null, time_zone|null}`|null.
- `people`: [{object:"user", id}] (reads may be partial users).
- `files`: [{name, type, external:{url} | file_upload:{id} | file:{url,expiry_time}}].
- `checkbox`: bool. `url`, `email`, `phone_number`: string|null.
- `relation`: [{id}] + `has_more` bool (read); write replaces list.
- `place`: `{lat, lon, name, address, aws_place_id, google_place_id}`|null (lat/lon required).
- `verification`: `{state, date, verified_by}` — wiki pages only; write state verified/unverified.
Read-only:
- `formula`: `{type:"string"|"number"|"boolean"|"date"|"unsupported", <type>: value}`.
- `rollup`: `{type:"number"|"date"|"array"|"unsupported"|("incomplete" in property items),
  function, <type>: value}`.
- `created_time`, `last_edited_time`: ISO string. `created_by`, `last_edited_by`: user.
- `unique_id`: `{number, prefix|null}`. `button`: `{}`.
Schema-side config: see /reference/property-object (number.format, select/status options,
status groups, formula.expression, relation {data_source_id, database_id, type
single_property|dual_property}, rollup {relation_property_*, rollup_property_*, function},
unique_id.prefix). Version availability of `place`, `verification` writes, heading_4: **UNCLEAR**.

---------------------------------------------------------------------------------------
## 10. Timestamps & conflict detection
- Format: ISO 8601 string, examples show milliseconds (`...T09:30:00.000Z`).
- Reference pages (block, page, property values, property item) say **nothing** about precision.
- Historical changelog, June 28 2021, titled "Last edited and created time properties are now
  rounded to the nearest minute"; body: "Starting July 1st, the `last_edited_time` and
  `created_time` properties will be rounded down to the closest minute" — applies to page,
  database and block objects. Title says "nearest", body says "rounded down" → treat as
  truncation to the minute (seconds/ms always `00.000`).
- Consequence: `last_edited_time` can't detect two edits within the same minute. A conflict
  check needs a content comparison/hash, not only timestamp equality.
- Filter comparisons on datetimes are done "with millisecond precision" against these
  minute-truncated values.
- The API has no ETag/If-Match or optimistic-concurrency mechanism documented (**not found**
  in any page reviewed); 409 `conflict_error` is only for server-side transaction collisions.
