//! Translation layer for Notion's private web API.
//!
//! The private API speaks `recordMap` records (blocks, collections, users,
//! discussions) authenticated by the browser's `token_v2` cookie. The rest of
//! this crate and the UI speak the public-API shapes in [`crate::model`]. This
//! module converts between the two and builds private transaction payloads.
//!
//! It is deliberately isolated from the HTTP client (`api.rs`): everything
//! here is pure data translation and can be unit-tested without a session.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use crate::model::{Block, Comment, DataSource, Page, SearchResult, User};
use crate::{Error, Result};

/// The private API endpoint prefix. `www.notion.so` is the canonical one;
/// `app.notion.com/api/v3` is the newer web-client base and can be selected
/// with `NOTION_PRIVATE_BASE`.
pub const DEFAULT_BASE: &str = "https://www.notion.so";

/// Whether `base` may receive the session cookie: the real site, or a
/// loopback `http://127.0.0.1:<port>` / `http://localhost:<port>` for tests.
pub fn safe_base(base: &str) -> bool {
    let base = base.trim_end_matches('/');
    if base == DEFAULT_BASE {
        return true;
    }
    reqwest::Url::parse(base).is_ok_and(|u| {
        u.scheme() == "http"
            && matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
            && u.port().is_some()
            && u.path() == "/"
            && u.username().is_empty()
    })
}

/// Whether the `file_token` cookie may be sent to `url`: a `notion.so` host
/// over https, or the (already validated) private base itself.
pub fn cookie_url_ok(url: &str, base: &str) -> bool {
    let (Ok(u), Ok(b)) = (reqwest::Url::parse(url), reqwest::Url::parse(base)) else {
        return false;
    };
    let notion = u.scheme() == "https"
        && u.host_str()
            .is_some_and(|h| h == "notion.so" || h.ends_with(".notion.so"));
    notion
        || (safe_base(base)
            && u.scheme() == b.scheme()
            && u.host_str() == b.host_str()
            && u.port_or_known_default() == b.port_or_known_default())
}

/// Escapes a key for use as one JSON-pointer segment.
fn ptr(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The browser-session credential and the workspace it is scoped to.
#[derive(Clone)]
pub struct PrivateAuth {
    pub token_v2: String,
    pub space_id: String,
    pub user_id: Option<String>,
    /// The browser's full `Cookie` header. Notion's file host needs the
    /// session cookies for private attachments; the API itself only needs
    /// `token_v2`.
    pub cookies: Option<String>,
}

impl std::fmt::Debug for PrivateAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateAuth")
            .field(
                "token_v2",
                &if self.token_v2.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("space_id", &self.space_id)
            .field("user_id", &self.user_id)
            .field(
                "cookies",
                &if self.cookies.as_deref().is_some_and(|c| !c.is_empty()) {
                    "<redacted>"
                } else {
                    ""
                },
            )
            .finish()
    }
}

/// A workspace from `getSpacesInitial` / `getSpaces`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub id: String,
    pub name: String,
}

/// Records collected from one or more private API responses.
#[derive(Clone, Default)]
pub struct Records {
    pub blocks: HashMap<String, Value>,
    pub collections: HashMap<String, Value>,
    pub views: HashMap<String, Value>,
    pub users: HashMap<String, Value>,
    pub discussions: HashMap<String, Value>,
    pub comments: HashMap<String, Value>,
    pub spaces: HashMap<String, Value>,
}

impl Records {
    /// Merge `{ "recordMap": { table: { id: { role, value } } } }` (or a bare
    /// record map).
    pub fn merge_response(&mut self, response: &Value) {
        let map = response.get("recordMap").unwrap_or(response);
        self.merge_map(map);
    }

    pub fn merge_map(&mut self, map: &Value) {
        for (table, entries) in map.as_object().into_iter().flatten() {
            self.merge_table(table, entries);
        }
    }

    pub fn merge_table(&mut self, table: &str, entries: &Value) {
        let Some(entries) = entries.as_object() else {
            return;
        };
        for (id, raw) in entries {
            let Some(record) = unwrap_record(raw) else {
                continue;
            };
            let target = match table {
                "block" => &mut self.blocks,
                "collection" => &mut self.collections,
                "collection_view" => &mut self.views,
                "notion_user" | "user" => &mut self.users,
                "discussion" => &mut self.discussions,
                "comment" => &mut self.comments,
                "space" => &mut self.spaces,
                _ => continue,
            };
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            target.insert(id, record.clone());
        }
    }

    pub fn block(&self, id: &str) -> Result<Block> {
        let rec = self
            .blocks
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("block {id} is not in the record map")))?;
        self.block_from_record(rec)
    }

    pub fn page(&self, id: &str) -> Result<Page> {
        let rec = self
            .blocks
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("page {id} is not in the record map")))?;
        self.page_from_record(rec)
    }

    pub fn data_source(&self, id: &str) -> Result<DataSource> {
        self.data_source_from_collection(id)
    }

    pub fn collection_title(&self, id: &str) -> Option<String> {
        let collection = self.collections.get(id)?;
        let title = collection.get("name").map(rich_text_to_official)?;
        Some(plain_official(&title))
    }

    pub fn collection_title_rich(&self, id: &str) -> Vec<Value> {
        self.collections
            .get(id)
            .and_then(|collection| collection.get("name"))
            .map(rich_text_to_official)
            .unwrap_or_default()
    }

    pub fn search_result(&self, id: &str) -> Result<SearchResult> {
        let rec = self
            .blocks
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("{id} is not in the record map")))?;
        match private_type(rec) {
            "page" => Ok(SearchResult::Page(self.page_from_record(rec)?)),
            "collection_view_page" | "collection_view" => {
                let collection = collection_id_of(rec).unwrap_or_else(|| id.to_string());
                Ok(SearchResult::DataSource(
                    self.data_source_from_collection(&collection)?,
                ))
            }
            _ => Err(Error::Invalid(format!("{id} is not a page or database"))),
        }
    }

    pub fn children(&self, id: &str) -> Result<Vec<Block>> {
        let rec = self
            .blocks
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("block {id} is not in the record map")))?;
        let mut out = Vec::new();
        for child in content_ids(rec) {
            if let Some(rec) = self.blocks.get(child) {
                out.push(self.block_from_record(rec)?);
            }
        }
        Ok(out)
    }

    pub fn users(&self) -> Vec<User> {
        self.users
            .values()
            .filter_map(private_user_to_official)
            .filter_map(|v| User::from_value(&v))
            .collect()
    }

    pub fn comments_for(&self, block_id: &str) -> Vec<Comment> {
        let mut out = Vec::new();
        for discussion in self.discussions.values() {
            if discussion.get("parent_id").and_then(Value::as_str) != Some(block_id) {
                continue;
            }
            let did = discussion
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            for cid in discussion
                .get("comments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if let Some(comment) = self.comments.get(cid) {
                    if let Ok(v) = self.comment_to_official(did, discussion, comment) {
                        if let Ok(c) = crate::model::comment_from_json(v) {
                            out.push(c);
                        }
                    }
                }
            }
        }
        out
    }

    /// Content ids referenced by a block but missing from the map. The client
    /// fetches these before translating a tree.
    pub fn missing_content(&self, root: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![root.to_string()];
        let mut missing = BTreeMap::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(rec) = self.blocks.get(&id) else {
                missing.insert(id, ());
                continue;
            };
            // Do not pull the contents of nested pages/databases here: they
            // are separate pulls and `block_tree` treats them as leaves.
            if matches!(
                private_type(rec),
                "page" | "collection_view_page" | "collection_view"
            ) && id != root
            {
                continue;
            }
            for child in content_ids(rec) {
                if !self.blocks.contains_key(child) {
                    missing.insert(child.to_string(), ());
                } else if !seen.contains(child) {
                    stack.push(child.to_string());
                }
            }
        }
        missing.into_keys().collect()
    }

    // -------------------------------------------------------- page/block

    fn page_from_record(&self, rec: &Value) -> Result<Page> {
        let v = self.page_json(rec)?;
        crate::model::page_from_json(v)
    }

    fn block_from_record(&self, rec: &Value) -> Result<Block> {
        let v = self.block_json(rec)?;
        crate::model::block_from_json(v)
    }

    fn page_json(&self, rec: &Value) -> Result<Value> {
        let id = record_id(rec)?;
        let typ = private_type(rec);
        let parent = self.official_parent(rec, true);
        let title = self.title_rich_text(rec);
        let mut properties = Map::new();
        properties.insert(
            "title".into(),
            json!({ "id": "title", "type": "title", "title": title }),
        );

        // A collection row has a schema; translate its other values too.
        if rec.get("parent_table").and_then(Value::as_str) == Some("collection") {
            if let Some(collection_id) = rec.get("parent_id").and_then(Value::as_str) {
                if let Some(collection) = self.collections.get(collection_id) {
                    if let Some(schema) = collection.get("schema").and_then(Value::as_object) {
                        for (key, entry) in schema {
                            if entry.get("type").and_then(Value::as_str) == Some("title") {
                                if let Some(value) = rec.pointer(&format!("/properties/{}", ptr(key))) {
                                    properties.insert(
                                        key.clone(),
                                        json!({ "id": key, "type": "title",
                                                "title": property_rich_text(value) }),
                                    );
                                }
                                continue;
                            }
                            if let Some(value) = rec.pointer(&format!("/properties/{}", ptr(key))) {
                                properties.insert(key.clone(), property_to_official(entry, value));
                            }
                        }
                    }
                }
            }
        } else if let Some(props) = rec.get("properties").and_then(Value::as_object) {
            // normal pages: keep extra rich-text-like properties, excluding title
            for (key, value) in props {
                if key == "title" || key.starts_with('_') {
                    continue;
                }
                if let Some(entry) = self.schema_entry(rec, key) {
                    properties.insert(key.clone(), property_to_official(&entry, value));
                } else if value.is_array() {
                    properties.insert(
                        key.clone(),
                        json!({ "id": key, "type": "rich_text",
                                "rich_text": property_rich_text(value) }),
                    );
                }
            }
        }

        let mut v = json!({
            "object": "page",
            "id": id,
            "created_time": timestamp(rec, "created_time"),
            "last_edited_time": timestamp(rec, "last_edited_time"),
            "archived": !alive(rec),
            "in_trash": !alive(rec),
            "url": page_url(&id),
            "parent": parent,
            "properties": properties,
            "last_edited_by": user_ref(rec.get("last_edited_by_id")),
            "is_locked": rec.pointer("/format/block_locked").and_then(Value::as_bool).unwrap_or(false),
        });
        if let Some(icon) = icon(rec) {
            v["icon"] = icon;
        }
        if let Some(cover) = cover(rec) {
            v["cover"] = cover;
        }
        // Keep the private type so the caller can recognise databases.
        v["_private_type"] = json!(typ);
        Ok(v)
    }

    fn block_json(&self, rec: &Value) -> Result<Value> {
        let id = record_id(rec)?;
        let ptype = private_type(rec);
        let official_type = official_type(ptype);
        let mut v = json!({
            "object": "block",
            "id": id,
            "type": official_type,
            "created_time": timestamp(rec, "created_time"),
            "last_edited_time": timestamp(rec, "last_edited_time"),
            "archived": !alive(rec),
            "in_trash": !alive(rec),
            "has_children": if matches!(ptype, "page" | "collection_view_page" | "collection_view") {
                false
            } else {
                !content_ids(rec).is_empty()
            },
            "parent": self.official_parent(rec, false),
            "last_edited_by": user_ref(rec.get("last_edited_by_id")),
        });

        let payload = match ptype {
            "page" => json!({ "title": plain_text(rec.pointer("/properties/title")) }),
            "collection_view_page" | "collection_view" => json!({
                "title": self.collection_name(rec).unwrap_or_else(|| "Database".into())
            }),
            "text" | "header" | "sub_header" | "sub_sub_header" | "header_4" | "bulleted_list"
            | "numbered_list" | "toggle" | "quote" | "callout" => {
                let mut payload = json!({
                    "rich_text": self.title_rich_text(rec),
                    "color": block_color(rec),
                    "is_toggleable": rec.pointer("/format/toggleable")
                        .and_then(Value::as_bool).unwrap_or(false),
                });
                if ptype == "callout" {
                    payload["icon"] = callout_icon(rec);
                }
                payload
            }
            "to_do" => json!({
                "rich_text": self.title_rich_text(rec),
                "color": block_color(rec),
                "checked": checked(rec),
            }),
            "code" => json!({
                "rich_text": self.title_rich_text(rec),
                "language": property_plain(rec.pointer("/properties/language"))
                    .or_else(|| rec.pointer("/format/code_language").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "plain text".into()),
            }),
            "equation" => json!({
                "expression": property_plain(rec.pointer("/properties/title"))
                    .unwrap_or_default(),
            }),
            "divider" => json!({}),
            "image" | "video" | "audio" | "file" | "pdf" => {
                let url = media_url(rec);
                let caption = rec
                    .pointer("/properties/caption")
                    .map(rich_text_to_official)
                    .unwrap_or_default();
                json!({
                    "type": "external",
                    "external": { "url": url },
                    "caption": caption,
                })
            }
            "bookmark" | "embed" | "tweet" | "figma" | "drive" | "maps" | "codepen"
            | "typeform" | "miro" => json!({
                "url": link_url(rec),
                "caption": rec.pointer("/properties/caption")
                    .map(rich_text_to_official)
                    .unwrap_or_default(),
            }),
            "table" => json!({
                "table_width": table_columns(self, rec).len(),
                "has_column_header": rec.pointer("/format/table_block_column_header")
                    .and_then(Value::as_bool).unwrap_or(false),
                "has_row_header": rec.pointer("/format/table_block_row_header")
                    .and_then(Value::as_bool).unwrap_or(false),
            }),
            "table_row" => json!({ "cells": table_cells(self, rec) }),
            "column_list" | "column" | "table_of_contents" | "breadcrumb" => json!({}),
            "alias" | "transclusion_container" | "transclusion_reference" => {
                json!({ "type": "block_id",
                        "block_id": rec.pointer("/format/alias_pointer/id")
                            .or_else(|| rec.pointer("/format/transclusion_reference_pointer/id"))
                            .and_then(Value::as_str).unwrap_or_default() })
            }
            _ => json!({}),
        };

        // The official payload lives under the mapped type, not the private name.
        v[official_type] = payload;
        v["_private_type"] = json!(ptype);
        Ok(v)
    }

    fn official_parent(&self, rec: &Value, page: bool) -> Value {
        let parent_id = rec
            .get("parent_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match rec.get("parent_table").and_then(Value::as_str) {
            Some("space") => json!({ "type": "workspace", "workspace": true }),
            Some("collection") => {
                let database = self
                    .blocks
                    .values()
                    .find(|b| collection_id_of(b).as_deref() == Some(parent_id))
                    .map(record_id)
                    .and_then(std::result::Result::ok)
                    .unwrap_or_else(|| parent_id.to_string());
                json!({ "type": "data_source_id", "data_source_id": parent_id,
                        "database_id": database })
            }
            Some("block") if page => json!({ "type": "page_id", "page_id": parent_id }),
            Some("block") => json!({ "type": "block_id", "block_id": parent_id }),
            _ => json!({ "type": "workspace", "workspace": true }),
        }
    }

    fn title_rich_text(&self, rec: &Value) -> Vec<Value> {
        rec.pointer("/properties/title")
            .map(rich_text_to_official)
            .unwrap_or_default()
    }

    fn schema_entry(&self, rec: &Value, key: &str) -> Option<Value> {
        let parent_id = rec.get("parent_id").and_then(Value::as_str)?;
        let collection = self.collections.get(parent_id)?;
        collection.pointer(&format!("/schema/{}", ptr(key))).cloned()
    }

    fn collection_name(&self, rec: &Value) -> Option<String> {
        let id = collection_id_of(rec)?;
        let collection = self.collections.get(&id)?;
        let name = collection.get("name").map(rich_text_to_official)?;
        Some(plain_official(&name))
    }

    fn data_source_from_collection(&self, id: &str) -> Result<DataSource> {
        let collection = self
            .collections
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("collection {id} is not in the record map")))?;
        let database = self
            .blocks
            .values()
            .find(|b| collection_id_of(b).as_deref() == Some(id))
            .map(record_id)
            .and_then(std::result::Result::ok)
            .unwrap_or_else(|| id.to_string());
        let title = collection
            .get("name")
            .map(rich_text_to_official)
            .unwrap_or_default();
        let schema = collection
            .get("schema")
            .and_then(Value::as_object)
            .map(|schema| {
                schema
                    .iter()
                    .map(|(key, entry)| (key.clone(), schema_to_official(key, entry)))
                    .collect::<Map<_, _>>()
            })
            .unwrap_or_default();
        let v = json!({
            "object": "data_source",
            "id": id,
            "created_time": timestamp(collection, "created_time"),
            "last_edited_time": timestamp(collection, "last_edited_time"),
            "parent": { "type": "database_id", "database_id": database },
            "title": title,
            "properties": schema,
            "database_parent": { "type": "database_id", "database_id": database },
        });
        crate::model::data_source_from_json(v)
    }

    pub fn comment_to_official(
        &self,
        discussion_id: &str,
        discussion: &Value,
        comment: &Value,
    ) -> Result<Value> {
        let parent_id = discussion
            .get("parent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("discussion has no parent".into()))?;
        let parent = if self
            .blocks
            .get(parent_id)
            .is_some_and(|b| matches!(private_type(b), "page" | "collection_view_page"))
        {
            json!({ "type": "page_id", "page_id": parent_id })
        } else {
            json!({ "type": "block_id", "block_id": parent_id })
        };
        Ok(json!({
            "object": "comment",
            "id": comment.get("id").and_then(Value::as_str).unwrap_or_default(),
            "discussion_id": discussion_id,
            "parent": parent,
            "created_time": timestamp(comment, "created_time"),
            "last_edited_time": timestamp(comment, "last_edited_time"),
            "created_by": { "object": "user",
                            "id": comment.get("created_by").and_then(Value::as_str).unwrap_or_default() },
            "rich_text": comment.get("text").map(rich_text_to_official).unwrap_or_default(),
            "resolved": discussion.get("resolved").and_then(Value::as_bool).unwrap_or(false),
        }))
    }
}

// ---------------------------------------------------------------- helpers

fn unwrap_record(raw: &Value) -> Option<&Value> {
    let mut v = raw;
    for _ in 0..3 {
        if v.get("role").is_some() || v.get("value").is_some() {
            if v.get("id").is_some() && v.get("value").is_none() {
                break;
            }
            let Some(inner) = v.get("value") else { break };
            if v.get("role").and_then(Value::as_str) == Some("none") {
                return None;
            }
            v = inner;
        } else {
            break;
        }
    }
    if v.get("role").and_then(Value::as_str) == Some("none") {
        return None;
    }
    v.get("id").is_some().then_some(v)
}

fn record_id(rec: &Value) -> Result<String> {
    rec.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid("record has no id".into()))
}

fn private_type(rec: &Value) -> &str {
    rec.get("type").and_then(Value::as_str).unwrap_or("")
}

fn alive(rec: &Value) -> bool {
    rec.get("alive").and_then(Value::as_bool).unwrap_or(true)
}

fn content_ids(rec: &Value) -> Vec<&str> {
    rec.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn collection_id_of(rec: &Value) -> Option<String> {
    rec.get("collection_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            rec.get("view_ids")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn timestamp(rec: &Value, field: &str) -> String {
    let ms = rec.get(field).and_then(Value::as_i64).unwrap_or(0);
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap())
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn page_url(id: &str) -> String {
    format!("https://www.notion.so/{}", id.replace('-', ""))
}

fn user_ref(id: Option<&Value>) -> Value {
    match id.and_then(Value::as_str) {
        Some(id) => json!({ "object": "user", "id": id }),
        None => Value::Null,
    }
}

fn block_color(rec: &Value) -> String {
    rec.pointer("/format/block_color")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string()
}

fn checked(rec: &Value) -> bool {
    rec.pointer("/properties/checked")
        .map(|v| plain_text(Some(v)).eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

fn icon(rec: &Value) -> Option<Value> {
    let icon = rec.pointer("/format/page_icon")?.as_str()?;
    Some(if icon.starts_with("http") || icon.starts_with('/') {
        json!({ "type": "external", "external": { "url": absolute(icon) } })
    } else {
        json!({ "type": "emoji", "emoji": icon })
    })
}

fn cover(rec: &Value) -> Option<Value> {
    let cover = rec.pointer("/format/page_cover")?.as_str()?;
    Some(json!({ "type": "external", "external": { "url": absolute(cover) } }))
}

fn callout_icon(rec: &Value) -> Value {
    let icon = rec
        .pointer("/format/page_icon")
        .and_then(Value::as_str)
        .unwrap_or("💡");
    json!({ "type": "emoji", "emoji": icon })
}

fn absolute(url: &str) -> String {
    if url.starts_with('/') {
        format!("https://www.notion.so{url}")
    } else {
        url.to_string()
    }
}

fn media_url(rec: &Value) -> String {
    rec.pointer("/format/display_source")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| property_plain(rec.pointer("/properties/source")))
        .unwrap_or_default()
}

fn link_url(rec: &Value) -> String {
    property_plain(rec.pointer("/properties/link"))
        .or_else(|| property_plain(rec.pointer("/properties/source")))
        .or_else(|| {
            rec.pointer("/format/display_source")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn table_columns(records: &Records, rec: &Value) -> Vec<String> {
    if let Some(order) = rec
        .pointer("/format/table_block_column_order")
        .and_then(Value::as_array)
    {
        return order
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    let _ = records;
    rec.get("properties")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn table_cells(records: &Records, rec: &Value) -> Vec<Value> {
    let columns = rec
        .get("parent_id")
        .and_then(Value::as_str)
        .and_then(|id| records.blocks.get(id))
        .map(|parent| table_columns(records, parent))
        .unwrap_or_else(|| table_columns(records, rec));
    columns
        .iter()
        .map(|col| {
            Value::Array(
                rec.pointer(&format!("/properties/{}", ptr(col)))
                    .map(rich_text_to_official)
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Translate one comment with its discussion, for `get_comment`.
pub fn comment_json(
    records: &Records,
    discussion_id: &str,
    discussion: &Value,
    comment: &Value,
) -> Result<Value> {
    records.comment_to_official(discussion_id, discussion, comment)
}
/// Official JSON for one private block record.
pub fn official_block(rec: &Value) -> Result<Value> {
    let mut records = Records::default();
    records.blocks.insert(record_id(rec)?, rec.clone());
    records.block_json(rec)
}

/// Official JSON for one private page record.
pub fn official_page(rec: &Value) -> Result<Value> {
    let mut records = Records::default();
    records.blocks.insert(record_id(rec)?, rec.clone());
    records.page_json(rec)
}
/// Private type -> official block type.
pub fn official_type(t: &str) -> &'static str {
    match t {
        "text" => "paragraph",
        "header" => "heading_1",
        "sub_header" => "heading_2",
        "sub_sub_header" => "heading_3",
        "header_4" => "heading_4",
        "bulleted_list" => "bulleted_list_item",
        "numbered_list" => "numbered_list_item",
        "to_do" => "to_do",
        "toggle" => "toggle",
        "quote" => "quote",
        "callout" => "callout",
        "code" => "code",
        "equation" => "equation",
        "divider" => "divider",
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        "file" => "file",
        "pdf" => "pdf",
        "bookmark" | "tweet" => "bookmark",
        "embed" | "figma" | "drive" | "maps" | "codepen" | "typeform" | "miro" => "embed",
        "table" => "table",
        "table_row" => "table_row",
        "column_list" => "column_list",
        "column" => "column",
        "table_of_contents" => "table_of_contents",
        "breadcrumb" => "breadcrumb",
        "alias" | "transclusion_reference" => "link_to_page",
        "transclusion_container" => "synced_block",
        "page" => "child_page",
        "collection_view" | "collection_view_page" => "child_database",
        _ => "unsupported",
    }
}

/// Official block type -> private type.
pub fn private_type_of(t: &str) -> &'static str {
    match t {
        "paragraph" => "text",
        "heading_1" => "header",
        "heading_2" => "sub_header",
        "heading_3" => "sub_sub_header",
        "heading_4" => "header_4",
        "bulleted_list_item" => "bulleted_list",
        "numbered_list_item" => "numbered_list",
        "to_do" => "to_do",
        "toggle" => "toggle",
        "quote" => "quote",
        "callout" => "callout",
        "code" => "code",
        "equation" => "equation",
        "divider" => "divider",
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        "file" => "file",
        "pdf" => "pdf",
        "bookmark" => "bookmark",
        "embed" => "embed",
        "table" => "table",
        "table_row" => "table_row",
        "column_list" => "column_list",
        "column" => "column",
        "table_of_contents" => "table_of_contents",
        "breadcrumb" => "breadcrumb",
        "link_to_page" => "alias",
        "synced_block" => "transclusion_container",
        "child_page" => "page",
        "child_database" => "collection_view_page",
        _ => "text",
    }
}
// ---------------------------------------------------------------- rich text

/// Private `properties.title` array -> official rich-text JSON array.
pub fn rich_text_to_official(v: &Value) -> Vec<Value> {
    let Some(runs) = v.as_array() else {
        return vec![official_text(
            v.as_str().unwrap_or_default(),
            Annotations::default(),
            None,
        )];
    };
    if runs.is_empty() {
        return Vec::new();
    }
    // A bare `[[...]]` may be one run's text array; detect a run layout.
    let mut out = Vec::new();
    for run in runs {
        match run {
            Value::String(s) => out.push(official_text(s, Annotations::default(), None)),
            Value::Array(parts) => {
                let text = parts.first().and_then(|v| v.as_str()).unwrap_or_default();
                let decorations = parts.get(1).and_then(Value::as_array);
                out.push(run_to_official(text, decorations));
            }
            _ => {}
        }
    }
    out
}

#[derive(Default, Clone)]
struct Annotations {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    code: bool,
    color: String,
}

fn run_to_official(text: &str, decorations: Option<&Vec<Value>>) -> Value {
    let mut ann = Annotations {
        color: "default".into(),
        ..Default::default()
    };
    let mut href = None;
    let mut mention = None;
    let mut equation = None;
    for d in decorations.into_iter().flatten() {
        let Some(d) = d.as_array() else { continue };
        let code = d.first().and_then(Value::as_str).unwrap_or_default();
        match code {
            "b" => ann.bold = true,
            "i" => ann.italic = true,
            "s" => ann.strike = true,
            "_" => ann.underline = true,
            "c" => ann.code = true,
            "h" => {
                if let Some(c) = d.get(1).and_then(Value::as_str) {
                    ann.color = c.to_string();
                }
            }
            "a" => href = d.get(1).and_then(Value::as_str).map(str::to_string),
            "u" => {
                mention = Some(json!({
                    "type": "user",
                    "user": { "object": "user",
                              "id": d.get(1).and_then(Value::as_str).unwrap_or_default() }
                }));
            }
            "p" => {
                mention = Some(json!({
                    "type": "page",
                    "page": { "id": d.get(1).and_then(Value::as_str).unwrap_or_default() }
                }));
            }
            "d" => {
                mention = Some(
                    json!({ "type": "date", "date": d.get(1).cloned().unwrap_or(Value::Null) }),
                );
            }
            "e" => {
                equation = Some(d.get(1).and_then(Value::as_str).unwrap_or(text).to_string());
            }
            _ => {}
        }
    }
    let plain = if text.is_empty() { "‣" } else { text };
    let mut v = if let Some(m) = mention {
        json!({ "type": "mention", "mention": m })
    } else if let Some(e) = equation {
        json!({ "type": "equation", "equation": { "expression": e } })
    } else {
        json!({
            "type": "text",
            "text": {
                "content": text,
                "link": href.as_ref().map(|u| json!({ "url": u })),
            },
        })
    };
    v["annotations"] = json!({
        "bold": ann.bold, "italic": ann.italic, "strikethrough": ann.strike,
        "underline": ann.underline, "code": ann.code, "color": ann.color,
    });
    v["plain_text"] = json!(plain);
    v["href"] = json!(href);
    v
}

fn official_text(text: &str, ann: Annotations, link: Option<String>) -> Value {
    json!({
        "type": "text",
        "text": { "content": text, "link": link.as_ref().map(|u| json!({ "url": u })) },
        "annotations": {
            "bold": ann.bold, "italic": ann.italic, "strikethrough": ann.strike,
            "underline": ann.underline, "code": ann.code, "color": ann.color,
        },
        "plain_text": text,
        "href": link,
    })
}

/// Official rich-text JSON array -> private text array.
pub fn rich_text_to_private(v: &Value) -> Value {
    let Some(runs) = v.as_array() else {
        return json!([[v.as_str().unwrap_or_default()]]);
    };
    let mut out = Vec::new();
    for run in runs {
        let kind = run.get("type").and_then(Value::as_str).unwrap_or("text");
        let plain = run
            .get("plain_text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut decorations: Vec<Value> = Vec::new();
        if let Some(a) = run.get("annotations").and_then(Value::as_object) {
            if a.get("bold").and_then(Value::as_bool).unwrap_or(false) {
                decorations.push(json!(["b"]));
            }
            if a.get("italic").and_then(Value::as_bool).unwrap_or(false) {
                decorations.push(json!(["i"]));
            }
            if a.get("strikethrough")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                decorations.push(json!(["s"]));
            }
            if a.get("underline").and_then(Value::as_bool).unwrap_or(false) {
                decorations.push(json!(["_"]));
            }
            if a.get("code").and_then(Value::as_bool).unwrap_or(false) {
                decorations.push(json!(["c"]));
            }
            if let Some(c) = a.get("color").and_then(Value::as_str) {
                if c != "default" {
                    decorations.push(json!(["h", c]));
                }
            }
        }
        if let Some(link) = run.pointer("/text/link/url").and_then(Value::as_str) {
            decorations.push(json!(["a", link]));
        }
        let text = match kind {
            "mention" => {
                let m = run.get("mention").cloned().unwrap_or(Value::Null);
                match m.get("type").and_then(Value::as_str) {
                    Some("user") => {
                        let id = m
                            .pointer("/user/id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        decorations.push(json!(["u", id]));
                        plain.to_string()
                    }
                    Some("page") => {
                        let id = m
                            .pointer("/page/id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        decorations.push(json!(["p", id]));
                        "‣".into()
                    }
                    Some("date") => {
                        decorations.push(json!([
                            "d",
                            private_date(m.get("date").unwrap_or(&Value::Null))
                        ]));
                        "‣".into()
                    }
                    _ => plain.to_string(),
                }
            }
            "equation" => {
                let e = run
                    .pointer("/equation/expression")
                    .and_then(Value::as_str)
                    .unwrap_or(plain);
                decorations.push(json!(["e", e]));
                e.to_string()
            }
            _ => run
                .pointer("/text/content")
                .and_then(Value::as_str)
                .unwrap_or(plain)
                .to_string(),
        };
        out.push(if decorations.is_empty() {
            json!([text])
        } else {
            json!([text, decorations])
        });
    }
    Value::Array(out)
}

fn private_date(v: &Value) -> Value {
    let start = v
        .get("start")
        .or_else(|| v.get("start_date"))
        .cloned()
        .unwrap_or(Value::Null);
    let end = v
        .get("end")
        .or_else(|| v.get("end_date"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({ "type": "date", "start_date": start, "end_date": end })
}

fn property_rich_text(v: &Value) -> Vec<Value> {
    rich_text_to_official(v)
}

fn display_name(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return (!s.trim().is_empty()).then(|| s.to_string());
    }
    let text = plain_text(Some(v));
    (!text.trim().is_empty()).then_some(text)
}
fn plain_text(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    let official = rich_text_to_official(v);
    plain_official(&official)
}

fn plain_official(v: &[Value]) -> String {
    v.iter()
        .filter_map(|r| r.get("plain_text").and_then(Value::as_str))
        .collect()
}

fn property_plain(v: Option<&Value>) -> Option<String> {
    let s = plain_text(v);
    (!s.is_empty()).then_some(s)
}

fn private_user_to_official(rec: &Value) -> Option<Value> {
    let id = rec.get("id").and_then(Value::as_str)?;
    let name = rec
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            let first = rec
                .get("given_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let last = rec
                .get("family_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let n = format!("{first} {last}").trim().to_string();
            (!n.is_empty()).then_some(n)
        });
    Some(json!({
        "object": "user",
        "id": id,
        "type": "person",
        "name": name,
        "avatar_url": rec.get("profile_photo").cloned().unwrap_or(Value::Null),
        "person": { "email": rec.get("email").cloned().unwrap_or(Value::Null) },
    }))
}

// ---------------------------------------------------------------- schemas

fn schema_to_official(key: &str, entry: &Value) -> Value {
    let name = entry.get("name").and_then(Value::as_str).unwrap_or(key);
    let typ = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("rich_text");
    let mut v = json!({ "id": key, "name": name, "type": typ });
    match typ {
        "select" | "multi_select" | "status" => {
            let options: Vec<Value> = entry
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|o| {
                    json!({
                        "id": o.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "name": o.get("value").or_else(|| o.get("name")).and_then(Value::as_str).unwrap_or_default(),
                        "color": o.get("color").and_then(Value::as_str).unwrap_or("default"),
                    })
                })
                .collect();
            v[typ] = json!({ "options": options });
        }
        "number" => v["number"] = json!({ "format": "number" }),
        "formula" => v["formula"] = json!({}),
        "rollup" => v["rollup"] = json!({}),
        "relation" => v["relation"] = json!({}),
        _ => v[typ] = json!({}),
    }
    v
}

fn property_to_official(entry: &Value, value: &Value) -> Value {
    let typ = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("rich_text");
    let text = plain_text(Some(value));
    let id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
    let base = json!({ "id": id, "type": typ });
    let mut v = base;
    match typ {
        "title" => v["title"] = json!(property_rich_text(value)),
        "rich_text" => v["rich_text"] = json!(property_rich_text(value)),
        "number" => v["number"] = json!(text.parse::<f64>().ok()),
        "select" => v["select"] = option_value(entry, &text).unwrap_or(Value::Null),
        "status" => v["status"] = option_value(entry, &text).unwrap_or(Value::Null),
        "multi_select" => {
            v["multi_select"] = json!(text
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| option_value(entry, s))
                .collect::<Vec<_>>());
        }
        "checkbox" => v["checkbox"] = json!(text.eq_ignore_ascii_case("yes") || text == "true"),
        "date" => v["date"] = private_date_to_official(value),
        "url" => v["url"] = json!(text),
        "email" => v["email"] = json!(text),
        "phone_number" => v["phone_number"] = json!(text),
        "people" => v["people"] = json!(user_mentions(value)),
        "relation" => {
            v["relation"] = json!(page_mentions(value)
                .into_iter()
                .map(|id| json!({"id": id}))
                .collect::<Vec<_>>())
        }
        "created_time" => v["created_time"] = json!(text),
        "last_edited_time" => v["last_edited_time"] = json!(text),
        "files" => v["files"] = json!([]),
        _ => v["rich_text"] = json!(property_rich_text(value)),
    }
    v
}

fn option_value(entry: &Value, name: &str) -> Option<Value> {
    let options = entry.get("options")?.as_array()?;
    options.iter().find_map(|o| {
        let value = o
            .get("value")
            .or_else(|| o.get("name"))
            .and_then(Value::as_str)?;
        (value == name).then(|| {
            json!({
                "id": o.get("id").and_then(Value::as_str).unwrap_or_default(),
                "name": value,
                "color": o.get("color").and_then(Value::as_str).unwrap_or("default"),
            })
        })
    })
}

fn private_date_to_official(value: &Value) -> Value {
    let mut out = json!({ "start": Value::Null, "end": Value::Null, "time_zone": Value::Null });
    for run in value.as_array().into_iter().flatten() {
        let Some(parts) = run.as_array() else {
            continue;
        };
        for d in parts.get(1).and_then(Value::as_array).into_iter().flatten() {
            let Some(d) = d.as_array() else { continue };
            if d.first().and_then(Value::as_str) == Some("d") {
                if let Some(date) = d.get(1) {
                    out["start"] = date
                        .get("start_date")
                        .or_else(|| date.get("start"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    out["end"] = date
                        .get("end_date")
                        .or_else(|| date.get("end"))
                        .cloned()
                        .unwrap_or(Value::Null);
                }
            }
        }
    }
    out
}

fn user_mentions(value: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for run in value.as_array().into_iter().flatten() {
        let Some(parts) = run.as_array() else {
            continue;
        };
        for d in parts.get(1).and_then(Value::as_array).into_iter().flatten() {
            let Some(d) = d.as_array() else { continue };
            if d.first().and_then(Value::as_str) == Some("u") {
                if let Some(id) = d.get(1).and_then(Value::as_str) {
                    out.push(json!({ "object": "user", "id": id }));
                }
            }
        }
    }
    out
}

fn page_mentions(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for run in value.as_array().into_iter().flatten() {
        let Some(parts) = run.as_array() else {
            continue;
        };
        for d in parts.get(1).and_then(Value::as_array).into_iter().flatten() {
            let Some(d) = d.as_array() else { continue };
            if d.first().and_then(Value::as_str) == Some("p") {
                if let Some(id) = d.get(1).and_then(Value::as_str) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- spaces

/// Log in with a browser cookie: fetch the user id and every workspace the
/// account can see. Tries the current bootstrap endpoint, then the legacy one.
pub async fn browser_spaces(token_v2: &str, base: &str) -> Result<(Option<String>, Vec<Space>)> {
    let token = token_v2
        .trim()
        .strip_prefix("token_v2=")
        .unwrap_or(token_v2.trim())
        .trim_matches('"')
        .to_string();
    if token.is_empty() {
        return Err(Error::Invalid("token_v2 is empty".into()));
    }
    let base = if safe_base(base) { base } else { DEFAULT_BASE };
    let http = reqwest::Client::new();
    // `getUserSpaces` lists every workspace the account belongs to, including
    // workspaces other people shared with it. The older bootstrap endpoints
    // only return the active workspace, which is why they are fallbacks.
    if let Ok(value) = space_post(&http, base, "getUserSpaces", &token).await {
        let (user, spaces) = spaces_from_response(&value);
        if !spaces.is_empty() {
            let user = match user {
                Some(user) => Some(user),
                None => match space_post(&http, base, "loadUserContent", &token).await {
                    Ok(value) => spaces_from_response(&value).0,
                    Err(_) => None,
                },
            };
            return Ok((user, spaces));
        }
    }
    for endpoint in ["getSpacesInitial", "getSpaces"] {
        if let Ok(value) = space_post(&http, base, endpoint, &token).await {
            let (user, spaces) = spaces_from_response(&value);
            if !spaces.is_empty() {
                return Ok((user, spaces));
            }
        }
    }
    // `loadUserContent` still tells us the user id and the active workspace.
    let value = space_post(&http, base, "loadUserContent", &token).await?;
    Ok(spaces_from_response(&value))
}

async fn space_post(
    http: &reqwest::Client,
    base: &str,
    endpoint: &str,
    token: &str,
) -> Result<Value> {
    let url = format!("{}/api/v3/{endpoint}", base.trim_end_matches('/'));
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
        )
        .header("Cookie", format!("token_v2={token}"))
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| Error::Http(e.without_url()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| Error::Http(e.without_url()))?;
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            status,
            code: "browser_login_failed".into(),
            message: "the browser login request was refused".into(),
        });
    }
    Ok(serde_json::from_str(&text)?)
}
/// Parse `getSpacesInitial` / `getSpaces` / `loadUserContent`.
pub fn spaces_from_response(v: &Value) -> (Option<String>, Vec<Space>) {
    let mut records = Records::default();
    records.merge_response(v);
    // The tables may be nested inside a per-user object rather than recordMap.
    for user in v
        .get("users")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(_, u)| u)
        .chain(v.as_object().into_iter().flatten().map(|(_, u)| u))
    {
        for table in ["space", "space_view", "notion_user"] {
            if let Some(entries) = user.get(table) {
                records.merge_table(table, entries);
            }
        }
    }
    let user_id = v
        .get("users")
        .and_then(Value::as_object)
        .and_then(|u| u.keys().next())
        .cloned()
        .or_else(|| {
            records
                .users
                .values()
                .find(|u| u.get("email").is_some())
                .and_then(|u| u.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let mut spaces: BTreeMap<String, String> = BTreeMap::new();
    // `getUserSpaces` returns a plain `spaces` array with every workspace the
    // account can enter, including ones whose full `space` record is not in
    // `loadUserContent`.
    for space in v
        .get("spaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = space.get("id").and_then(Value::as_str) {
            let name = display_name(space.get("name")).unwrap_or_else(|| id.to_string());
            spaces.insert(id.to_string(), name);
        }
    }
    for (id, space) in &records.spaces {
        let name = display_name(space.get("name")).unwrap_or_else(|| id.to_string());
        spaces.insert(id.clone(), name);
    }
    // space_view has spaceId/name; add names for spaces without a space record
    for view in records_views(v) {
        if let Some(id) = view
            .get("spaceId")
            .or_else(|| view.get("space_id"))
            .and_then(Value::as_str)
        {
            let name = display_name(view.get("name")).unwrap_or_else(|| id.to_string());
            spaces.entry(id.to_string()).or_insert(name);
        }
    }
    let spaces = spaces
        .into_iter()
        .map(|(id, name)| Space { id, name })
        .collect();
    (user_id, spaces)
}

fn records_views(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut walk = |entries: &Value| {
        let Some(obj) = entries.as_object() else {
            return;
        };
        for raw in obj.values() {
            if let Some(rec) = unwrap_record(raw) {
                if rec.get("spaceId").is_some() || rec.get("space_id").is_some() {
                    out.push(rec.clone());
                }
            }
        }
    };
    walk(v);
    for user in v
        .get("users")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(_, u)| u)
    {
        if let Some(views) = user.get("space_view") {
            walk(views);
        }
        if let Some(root) = user.get("user_root") {
            walk(root);
        }
    }
    out
}

// ---------------------------------------------------------------- writes

/// A new block/page id in the dashed form Notion uses.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// An official block body (`{type, <type>: payload}`) -> private block record.
pub fn block_record_from_official(
    id: &str,
    body: &Value,
    parent_id: &str,
    parent_table: &str,
    space_id: &str,
    user_id: Option<&str>,
) -> Result<Value> {
    // An update body has no "type" (sync strips it): infer it from the one
    // `{<type>: payload}` key so the block is never treated as a paragraph.
    let ty = body
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            body.as_object()?
                .keys()
                .map(String::as_str)
                .find(|k| *k == "paragraph" || private_type_of(k) != "text")
        })
        .unwrap_or("paragraph");
    let ptype = private_type_of(ty);
    let payload = body.get(ty).cloned().unwrap_or_else(|| json!({}));
    let now = now_ms();
    let mut props = Map::new();
    let mut format = Map::new();
    match ptype {
        "text" | "header" | "sub_header" | "sub_sub_header" | "header_4" | "bulleted_list"
        | "numbered_list" | "to_do" | "toggle" | "quote" | "callout" | "code" | "equation" => {
            // only fields present are written, so a partial update never wipes text
            if let Some(rt) = payload.get("rich_text") {
                props.insert("title".into(), rich_text_to_private(rt));
            }
            if let Some(color) = payload.get("color").and_then(Value::as_str) {
                format.insert("block_color".into(), json!(color));
            }
            if payload
                .get("is_toggleable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                format.insert("toggleable".into(), json!(true));
            }
            if let Some(checked) = payload.get("checked").and_then(Value::as_bool) {
                props.insert(
                    "checked".into(),
                    json!([[if checked { "Yes" } else { "No" }]]),
                );
            }
            if ptype == "code" {
                if let Some(lang) = payload.get("language").and_then(Value::as_str) {
                    props.insert("language".into(), json!([[lang]]));
                }
            }
            if ptype == "equation" {
                if let Some(e) = payload.get("expression").and_then(Value::as_str) {
                    props.insert("title".into(), json!([[e]]));
                }
            }
        }
        "image" | "video" | "audio" | "file" | "pdf" => {
            let url = payload
                .pointer("/external/url")
                .or_else(|| payload.pointer("/file/url"))
                .or_else(|| payload.pointer("/file_upload/id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !url.is_empty() {
                props.insert("source".into(), json!([[url]]));
                format.insert("display_source".into(), json!(url));
            }
            if let Some(caption) = payload.get("caption") {
                props.insert("caption".into(), rich_text_to_private(caption));
            }
        }
        "bookmark" | "embed" => {
            let url = payload
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !url.is_empty() {
                props.insert("link".into(), json!([[url]]));
            }
            if let Some(caption) = payload.get("caption") {
                props.insert("caption".into(), rich_text_to_private(caption));
            }
        }
        "table" | "table_row" => {
            return Err(Error::Invalid(
                "creating or editing tables is not supported by the browser backend yet".into(),
            ));
        }
        _ => {}
    }
    let mut rec = json!({
        "id": id,
        "version": 1,
        "type": ptype,
        "alive": true,
        "parent_id": parent_id,
        "parent_table": parent_table,
        "space_id": space_id,
        "content": [],
        "properties": props,
        "created_time": now,
        "last_edited_time": now,
        "created_by_id": user_id,
        "created_by_table": "notion_user",
        "last_edited_by_id": user_id,
        "last_edited_by_table": "notion_user",
    });
    if !format.is_empty() {
        rec["format"] = Value::Object(format);
    }
    Ok(rec)
}

/// Official property body -> private property value, using a collection schema.
/// Types that can't be written losslessly are an `Error::Invalid`.
pub fn property_to_private(entry: &Value, value: &Value) -> Result<Value> {
    let typ = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("rich_text");
    Ok(match typ {
        "title" => rich_text_to_private(value.get("title").unwrap_or(&json!([]))),
        "rich_text" => rich_text_to_private(value.get("rich_text").unwrap_or(&json!([]))),
        "number" => json!([[value.get("number").map(value_text).unwrap_or_default()]]),
        "select" => json!([[value
            .pointer("/select/name")
            .and_then(Value::as_str)
            .unwrap_or_default()]]),
        "status" => json!([[value
            .pointer("/status/name")
            .and_then(Value::as_str)
            .unwrap_or_default()]]),
        "multi_select" => {
            let names = value
                .get("multi_select")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| o.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>();
            // the reader splits the stored text on commas
            if let Some(n) = names.iter().find(|n| n.contains(',')) {
                return Err(Error::Invalid(format!(
                    "multi-select option `{n}` contains a comma, which the browser backend can't store"
                )));
            }
            json!([[names.join(",")]])
        }
        "checkbox" => json!([[
            if value
                .get("checkbox")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "Yes"
            } else {
                "No"
            }
        ]]),
        "date" => {
            let start = value
                .pointer("/date/start")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let end = value.pointer("/date/end").cloned().unwrap_or(Value::Null);
            json!([["‣", [["d", { "type": "date", "start_date": start, "end_date": end }]]]])
        }
        "url" => json!([[value.get("url").and_then(Value::as_str).unwrap_or_default()]]),
        "email" => json!([[value
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()]]),
        "phone_number" => json!([[value
            .get("phone_number")
            .and_then(Value::as_str)
            .unwrap_or_default()]]),
        "people" => {
            let mut runs = Vec::new();
            for p in value
                .get("people")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    runs.push(json!(["‣", [["u", id]]]));
                    runs.push(json!([","]));
                }
            }
            if runs.len() > 1 {
                runs.pop();
            }
            Value::Array(runs)
        }
        "relation" => {
            let mut runs = Vec::new();
            for p in value
                .get("relation")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = p.get("id").and_then(Value::as_str) {
                    runs.push(json!(["‣", [["p", id]]]));
                    runs.push(json!([","]));
                }
            }
            if runs.len() > 1 {
                runs.pop();
            }
            Value::Array(runs)
        }
        other => {
            return Err(Error::Invalid(format!(
                "the browser backend can't write `{other}` properties"
            )))
        }
    })
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Transaction wrapper for `saveTransactionsFanout`.
pub fn fanout(space_id: &str, operations: Vec<Value>) -> Value {
    let request = new_id();
    json!({
        "requestId": request,
        "transactions": [{
            "id": new_id(),
            "spaceId": space_id,
            "operations": operations,
        }],
    })
}

pub fn set_op(block_id: &str, space_id: &str, path: Value, value: Value) -> Value {
    json!({
        "pointer": { "id": block_id, "table": "block", "spaceId": space_id },
        "path": path,
        "command": "set",
        "args": value,
    })
}

pub fn set_op_table(table: &str, id: &str, space_id: &str, path: Value, value: Value) -> Value {
    json!({
        "pointer": { "id": id, "table": table, "spaceId": space_id },
        "path": path,
        "command": "set",
        "args": value,
    })
}

pub fn update_op_table(table: &str, id: &str, space_id: &str, path: Value, value: Value) -> Value {
    json!({
        "pointer": { "id": id, "table": table, "spaceId": space_id },
        "path": path,
        "command": "update",
        "args": value,
    })
}
pub fn update_op(block_id: &str, space_id: &str, path: Value, value: Value) -> Value {
    json!({
        "pointer": { "id": block_id, "table": "block", "spaceId": space_id },
        "path": path,
        "command": "update",
        "args": value,
    })
}

pub fn list_op(block_id: &str, space_id: &str, path: &str, command: &str, args: Value) -> Value {
    json!({
        "pointer": { "id": block_id, "table": "block", "spaceId": space_id },
        "path": [path],
        "command": command,
        "args": args,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn pointer_keys_are_escaped() {
        assert_eq!(ptr("a/b~c"), "a~1b~0c");
        let rec = json!({ "properties": { "a/b": 1, "a": { "b": 2 } } });
        assert_eq!(rec.pointer(&format!("/properties/{}", ptr("a/b"))), Some(&json!(1)));
    }

    #[test]
    fn bases_and_cookie_hosts_are_checked() {
        assert!(safe_base("https://www.notion.so/") && safe_base("http://127.0.0.1:8080"));
        assert!(safe_base("http://localhost:9"));
        for b in ["https://evil.example", "http://www.notion.so", "http://127.0.0.1", "https://www.notion.so.evil.com"] {
            assert!(!safe_base(b), "{b}");
        }
        let b = "http://127.0.0.1:8080";
        assert!(cookie_url_ok("https://file.notion.so/f/a.png?x=1", DEFAULT_BASE));
        assert!(cookie_url_ok("https://notion.so/a", DEFAULT_BASE));
        assert!(cookie_url_ok("http://127.0.0.1:8080/a.png", b));
        for u in ["https://s3.amazonaws.com/x", "https://evilnotion.so/x", "https://notion.so.evil.com/x", "http://file.notion.so/x", "http://127.0.0.1:9/x"] {
            assert!(!cookie_url_ok(u, b), "{u}");
        }
    }

    use super::*;

    fn rec(id: &str, ty: &str) -> Value {
        json!({
            "id": id, "type": ty, "alive": true,
            "parent_id": "space", "parent_table": "space",
            "properties": { "title": [["Hello", [["b"]]]] },
            "format": { "page_icon": "📄", "page_cover": "/images/c.png" },
            "created_time": 1700000000000_i64, "last_edited_time": 1700000000000_i64,
            "last_edited_by_id": "u1", "content": ["b1"],
        })
    }

    #[test]
    fn unwraps_both_record_forms() {
        let one = json!({ "role": "reader", "value": { "id": "x" } });
        let two =
            json!({ "role": "reader", "value": { "role": "reader", "value": { "id": "x" } } });
        assert_eq!(unwrap_record(&one).unwrap()["id"], "x");
        assert_eq!(unwrap_record(&two).unwrap()["id"], "x");
        assert!(unwrap_record(&json!({ "role": "none" })).is_none());
    }

    #[test]
    fn converts_private_rich_text() {
        let rt = json!([
            ["Hello ", [["b"]]],
            ["world", [["a", "https://x"], ["h", "red"]]],
            ["‣", [["u", "u1"]]],
            ["‣", [["p", "p1"]]],
            ["E=mc^2", [["e", "E=mc^2"]]]
        ]);
        let out = rich_text_to_official(&rt);
        assert_eq!(out[0]["annotations"]["bold"], true);
        assert_eq!(out[1]["text"]["link"]["url"], "https://x");
        assert_eq!(out[1]["annotations"]["color"], "red");
        assert_eq!(out[2]["type"], "mention");
        assert_eq!(out[2]["mention"]["user"]["id"], "u1");
        assert_eq!(out[3]["mention"]["page"]["id"], "p1");
        assert_eq!(out[4]["equation"]["expression"], "E=mc^2");
    }

    #[test]
    fn round_trips_official_rich_text() {
        let official = json!([
            { "type": "text", "text": { "content": "Bold", "link": null },
              "annotations": { "bold": true, "italic": false, "strikethrough": false,
                               "underline": false, "code": false, "color": "default" },
              "plain_text": "Bold", "href": null },
            { "type": "text", "text": { "content": " link", "link": { "url": "https://x" } },
              "annotations": { "bold": false, "italic": false, "strikethrough": false,
                               "underline": false, "code": false, "color": "red" },
              "plain_text": " link", "href": "https://x" }
        ]);
        let private = rich_text_to_private(&official);
        assert_eq!(private[0][0], "Bold");
        assert!(private[0][1].as_array().unwrap().contains(&json!(["b"])));
        assert!(private[1][1]
            .as_array()
            .unwrap()
            .contains(&json!(["a", "https://x"])));
        let back = rich_text_to_official(&private);
        assert_eq!(back[0]["plain_text"], "Bold");
        assert_eq!(back[1]["href"], "https://x");
    }

    #[test]
    fn translates_a_page_and_block() {
        let mut records = Records::default();
        records.blocks.insert("p1".into(), rec("p1", "page"));
        records.blocks.insert(
            "b1".into(),
            json!({
                "id": "b1", "type": "text", "alive": true,
                "parent_id": "p1", "parent_table": "block",
                "properties": { "title": [["Hi"]] },
                "created_time": 1700000000000_i64, "last_edited_time": 1700000000000_i64,
                "content": [],
            }),
        );
        let page = records.page("p1").unwrap();
        assert_eq!(page.title(), "Hello");
        assert!(matches!(page.parent, crate::model::Parent::Workspace));
        assert!(page.raw["cover"]["external"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/images/c.png"));
        let children = records.children("p1").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].kind.type_name(), "paragraph");
        assert_eq!(
            children[0].raw["paragraph"]["rich_text"][0]["plain_text"],
            "Hi"
        );
    }

    #[test]
    fn builds_a_create_record() {
        let body = json!({ "type": "paragraph",
                           "paragraph": { "rich_text": [ { "type": "text",
                             "text": { "content": "Hi", "link": null },
                             "annotations": { "bold": false, "italic": false,
                               "strikethrough": false, "underline": false,
                               "code": false, "color": "default" },
                             "plain_text": "Hi", "href": null } ],
                             "color": "default" } });
        let rec = block_record_from_official("b1", &body, "p1", "block", "s1", Some("u1")).unwrap();
        assert_eq!(rec["type"], "text");
        assert_eq!(rec["properties"]["title"], json!([["Hi"]]));
        assert_eq!(rec["parent_id"], "p1");
        assert_eq!(rec["space_id"], "s1");
    }

    #[test]
    fn lists_every_workspace_from_get_user_spaces() {
        let (user, spaces) = spaces_from_response(&json!({
            "spaces": [
                { "id": "s1", "name": "Nicolas's Notion" },
                { "id": "s2", "name": "Paul's Notion" },
            ]
        }));
        assert_eq!(user, None);
        assert_eq!(
            spaces,
            vec![
                Space {
                    id: "s1".into(),
                    name: "Nicolas's Notion".into()
                },
                Space {
                    id: "s2".into(),
                    name: "Paul's Notion".into()
                },
            ]
        );
    }

    #[test]
    fn partial_update_bodies_never_wipe_or_retype() {
        let body = json!({ "heading_2": { "color": "red" } });
        let rec = block_record_from_official("b", &body, "p", "block", "s", None).unwrap();
        assert_eq!(rec["type"], "sub_header");
        assert!(rec["properties"].get("title").is_none());
        let todo = json!({ "to_do": { "checked": false } });
        let rec = block_record_from_official("b", &todo, "p", "block", "s", None).unwrap();
        assert_eq!(rec["properties"]["checked"], json!([["No"]]));
    }

    #[test]
    fn lossy_property_writes_are_refused() {
        let formula = json!({ "type": "formula" });
        assert!(property_to_private(&formula, &json!({ "formula": {} })).is_err());
        let ms = json!({ "type": "multi_select" });
        let tags = |n: &str| json!({ "multi_select": [{ "name": "a" }, { "name": n }] });
        assert!(property_to_private(&ms, &tags("b,c")).is_err());
        assert_eq!(
            property_to_private(&ms, &tags("b")).unwrap(),
            json!([["a,b"]])
        );
    }

    #[test]
    fn property_values_round_trip_through_the_reader() {
        let opts = json!([{ "name": "a", "id": "1" }, { "name": "b", "id": "2" }]);
        let e = |t: &str| json!({ "id": "k", "type": t, "options": opts });
        let cases = [
            ("select", json!({ "select": { "name": "a" } })),
            ("multi_select", json!({ "multi_select": [{ "name": "a" }, { "name": "b" }] })),
            ("number", json!({ "number": 4.5 })),
            ("checkbox", json!({ "checkbox": true })),
            ("checkbox", json!({ "checkbox": false })),
            ("date", json!({ "date": { "start": "2024-01-02", "end": null } })),
        ];
        for (t, v) in cases {
            let stored = property_to_private(&e(t), &v).unwrap();
            let back = property_to_official(&e(t), &stored);
            match t {
                "select" => assert_eq!(back[t]["name"], "a"),
                "multi_select" => assert_eq!(back[t].as_array().unwrap().len(), 2),
                "number" => assert_eq!(back[t], json!(4.5)),
                "checkbox" => assert_eq!(back[t], v["checkbox"]),
                _ => assert_eq!(back[t]["start"], "2024-01-02"),
            }
        }
        let text = json!({ "id": "k", "type": "rich_text" });
        let stored = property_to_private(&text, &json!({ "rich_text": [] })).unwrap();
        assert_eq!(property_to_official(&text, &stored)["rich_text"], json!([]));
        for t in ["rollup", "formula", "files"] {
            assert!(property_to_private(&json!({ "type": t }), &json!({})).is_err(), "{t}");
        }
    }

    #[test]
    fn lookalike_hosts_are_rejected() {
        for u in ["https://notion.so.evil.com/", "https://evilnotion.so/", "http://www.notion.so/"] {
            assert!(!safe_base(u), "{u}");
            assert!(!cookie_url_ok(u, DEFAULT_BASE), "{u}");
        }
    }
}
