//! Serde types for Notion API objects (API version 2025-09-03).
//!
//! Parsing is lenient: unknown fields are ignored, unknown or malformed block
//! types / property values become `Unsupported`, and every top-level object
//! keeps its full JSON in `raw` so writes and the cache stay lossless.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};

use crate::{Error, Result};

/// Serialize via `to_json`, deserialize via `from_value`.
macro_rules! serde_via_value {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                self.to_json().serialize(s)
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                Self::from_value(Value::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    )*};
}

serde_via_value!(
    RichText,
    Block,
    Parent,
    Page,
    PropertyValue,
    DataSource,
    SearchResult
);

fn str_field(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("missing string field `{k}`")))
}

fn time_field(v: &Value, k: &str) -> Result<DateTime<Utc>> {
    let s = str_field(v, k)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::Invalid(format!("bad timestamp `{k}`: {e}")))
}

fn in_trash(v: &Value) -> bool {
    v.get("in_trash")
        .and_then(Value::as_bool)
        .or_else(|| v.get("archived").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Write `in_trash` (+ `archived` if present) and `last_edited_time` into `out`,
/// only where they differ from what `out` already says (keeps exact round trips).
fn write_common(out: &mut Value, trashed: bool, last_edited: DateTime<Utc>) {
    if !out.is_object() {
        *out = json!({});
    }
    if in_trash(out) != trashed {
        out["in_trash"] = json!(trashed);
        if out.get("archived").is_some() {
            out["archived"] = json!(trashed);
        }
    }
    if time_field(out, "last_edited_time").ok() != Some(last_edited) {
        out["last_edited_time"] =
            json!(last_edited.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
}

fn default_color() -> String {
    "default".into()
}

// ---------------------------------------------------------------- rich text

#[derive(Debug, Clone, PartialEq)]
pub struct RichText {
    pub plain_text: String,
    pub href: Option<String>,
    pub annotations: Annotations,
    pub kind: RichTextKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RichTextKind {
    Text {
        content: String,
        link: Option<String>,
    },
    Mention(Value),
    Equation {
        expression: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Annotations {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
    pub code: bool,
    pub color: String,
}

impl Default for Annotations {
    fn default() -> Self {
        Annotations {
            bold: false,
            italic: false,
            strikethrough: false,
            underline: false,
            code: false,
            color: default_color(),
        }
    }
}

impl RichText {
    pub fn plain(text: &str) -> Self {
        RichText {
            plain_text: text.into(),
            href: None,
            annotations: Annotations::default(),
            kind: RichTextKind::Text {
                content: text.into(),
                link: None,
            },
        }
    }

    fn from_value(v: Value) -> Result<Self> {
        #[derive(Deserialize)]
        struct Link {
            url: String,
        }
        #[derive(Deserialize)]
        struct Text {
            content: String,
            link: Option<Link>,
        }
        #[derive(Deserialize)]
        struct Equation {
            expression: String,
        }
        #[derive(Deserialize)]
        struct In {
            #[serde(rename = "type")]
            kind: String,
            text: Option<Text>,
            mention: Option<Value>,
            equation: Option<Equation>,
            #[serde(default)]
            annotations: Annotations,
            #[serde(default)]
            plain_text: String,
            href: Option<String>,
        }
        let i: In = serde_json::from_value(v)?;
        let kind = match (i.kind.as_str(), i.text, i.mention, i.equation) {
            ("text", Some(t), _, _) => RichTextKind::Text {
                content: t.content,
                link: t.link.map(|l| l.url),
            },
            ("mention", _, Some(m), _) => RichTextKind::Mention(m),
            ("equation", _, _, Some(e)) => RichTextKind::Equation {
                expression: e.expression,
            },
            (t, ..) => return Err(Error::Invalid(format!("unsupported rich text `{t}`"))),
        };
        Ok(RichText {
            plain_text: i.plain_text,
            href: i.href,
            annotations: i.annotations,
            kind,
        })
    }

    fn to_json(&self) -> Value {
        let mut v = match &self.kind {
            RichTextKind::Text { content, link } => json!({
                "type": "text",
                "text": { "content": content, "link": link.as_ref().map(|u| json!({ "url": u })) },
            }),
            RichTextKind::Mention(m) => json!({ "type": "mention", "mention": m }),
            RichTextKind::Equation { expression } => {
                json!({ "type": "equation", "equation": { "expression": expression } })
            }
        };
        v["annotations"] = json!(self.annotations);
        v["plain_text"] = json!(self.plain_text);
        v["href"] = json!(self.href);
        v
    }
}

/// Concatenated `plain_text` of a rich text array.
pub fn plain_text(rt: &[RichText]) -> String {
    rt.iter().map(|r| r.plain_text.as_str()).collect()
}

// ---------------------------------------------------------------- parent

#[derive(Debug, Clone, PartialEq)]
pub enum Parent {
    Page(String),
    Block(String),
    Database(String),
    DataSource {
        id: String,
        database_id: String,
    },
    Workspace,
    /// Any other parent type (e.g. `agent_id`); the raw parent JSON.
    Other(Value),
}

impl Parent {
    pub fn kind_str(&self) -> &str {
        match self {
            Parent::Page(_) => "page_id",
            Parent::Block(_) => "block_id",
            Parent::Database(_) => "database_id",
            Parent::DataSource { .. } => "data_source_id",
            Parent::Workspace => "workspace",
            Parent::Other(raw) => raw["type"].as_str().unwrap_or("unknown"),
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Parent::Page(id) | Parent::Block(id) | Parent::Database(id) => Some(id),
            Parent::DataSource { id, .. } => Some(id),
            Parent::Workspace | Parent::Other(_) => None,
        }
    }

    fn from_value(v: Value) -> Result<Self> {
        let t = str_field(&v, "type")?;
        Ok(match t.as_str() {
            "page_id" => Parent::Page(str_field(&v, "page_id")?),
            "block_id" => Parent::Block(str_field(&v, "block_id")?),
            "database_id" => Parent::Database(str_field(&v, "database_id")?),
            "data_source_id" => Parent::DataSource {
                id: str_field(&v, "data_source_id")?,
                database_id: str_field(&v, "database_id")?,
            },
            "workspace" => Parent::Workspace,
            _ => Parent::Other(v),
        })
    }

    fn to_json(&self) -> Value {
        match self {
            Parent::Workspace => json!({ "type": "workspace", "workspace": true }),
            Parent::Other(raw) => raw.clone(),
            Parent::DataSource { id, database_id } => json!({
                "type": "data_source_id", "data_source_id": id, "database_id": database_id,
            }),
            p => {
                let k = p.kind_str();
                json!({ "type": k, k: p.id() })
            }
        }
    }
}

// ---------------------------------------------------------------- blocks

/// Serialize emits `raw` with these struct fields written back: `kind` (payload
/// merged into `raw[type]`, `type` updated), `in_trash` (+ `archived` if present)
/// and `last_edited_time`. `id`, `parent`, `has_children`, `created_time` are not.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: String,
    pub parent: Parent,
    pub has_children: bool,
    pub in_trash: bool,
    pub created_time: DateTime<Utc>,
    pub last_edited_time: DateTime<Utc>,
    pub kind: BlockKind,
    /// Full JSON object as received.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Paragraph(TextBlock),
    Heading1(Heading),
    Heading2(Heading),
    Heading3(Heading),
    BulletedListItem(TextBlock),
    NumberedListItem(TextBlock),
    ToDo {
        text: TextBlock,
        checked: bool,
    },
    Toggle(TextBlock),
    Quote(TextBlock),
    Callout(TextBlock),
    Code {
        text: TextBlock,
        language: String,
    },
    Divider,
    ChildPage {
        title: String,
    },
    ChildDatabase {
        title: String,
    },
    /// Every other type, incl. API type "unsupported"; `raw` is the full block object.
    Unsupported {
        raw: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub rich_text: Vec<RichText>,
    #[serde(default = "default_color")]
    pub color: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub text: TextBlock,
    pub is_toggleable: bool,
}

impl BlockKind {
    pub fn type_name(&self) -> &str {
        match self {
            BlockKind::Paragraph(_) => "paragraph",
            BlockKind::Heading1(_) => "heading_1",
            BlockKind::Heading2(_) => "heading_2",
            BlockKind::Heading3(_) => "heading_3",
            BlockKind::BulletedListItem(_) => "bulleted_list_item",
            BlockKind::NumberedListItem(_) => "numbered_list_item",
            BlockKind::ToDo { .. } => "to_do",
            BlockKind::Toggle(_) => "toggle",
            BlockKind::Quote(_) => "quote",
            BlockKind::Callout(_) => "callout",
            BlockKind::Code { .. } => "code",
            BlockKind::Divider => "divider",
            BlockKind::ChildPage { .. } => "child_page",
            BlockKind::ChildDatabase { .. } => "child_database",
            BlockKind::Unsupported { raw } => raw["type"].as_str().unwrap_or("unsupported"),
        }
    }

    pub fn text(&self) -> Option<&TextBlock> {
        match self {
            BlockKind::Paragraph(t)
            | BlockKind::BulletedListItem(t)
            | BlockKind::NumberedListItem(t)
            | BlockKind::Toggle(t)
            | BlockKind::Quote(t)
            | BlockKind::Callout(t)
            | BlockKind::ToDo { text: t, .. }
            | BlockKind::Code { text: t, .. } => Some(t),
            BlockKind::Heading1(h) | BlockKind::Heading2(h) | BlockKind::Heading3(h) => {
                Some(&h.text)
            }
            _ => None,
        }
    }

    pub fn set_text(&mut self, rt: Vec<RichText>) -> Result<()> {
        let t = match self {
            BlockKind::Paragraph(t)
            | BlockKind::BulletedListItem(t)
            | BlockKind::NumberedListItem(t)
            | BlockKind::Toggle(t)
            | BlockKind::Quote(t)
            | BlockKind::Callout(t)
            | BlockKind::ToDo { text: t, .. }
            | BlockKind::Code { text: t, .. } => t,
            BlockKind::Heading1(h) | BlockKind::Heading2(h) | BlockKind::Heading3(h) => &mut h.text,
            other => {
                return Err(Error::Invalid(format!(
                    "block type `{}` has no text",
                    other.type_name()
                )))
            }
        };
        t.rich_text = rt;
        Ok(())
    }

    pub fn to_request_json(&self) -> Result<Value> {
        match self {
            BlockKind::Unsupported { .. }
            | BlockKind::ChildPage { .. }
            | BlockKind::ChildDatabase { .. } => Err(Error::Invalid(format!(
                "block type `{}` cannot be written",
                self.type_name()
            ))),
            _ => {
                let t = self.type_name();
                Ok(json!({ "type": t, t: self.payload() }))
            }
        }
    }

    /// The modelled payload fields (`block[type]`); `Null` for Unsupported.
    fn payload(&self) -> Value {
        let text = |t: &TextBlock| json!(t);
        let heading = |h: &Heading| {
            let mut v = text(&h.text);
            v["is_toggleable"] = json!(h.is_toggleable);
            v
        };
        match self {
            BlockKind::Paragraph(t)
            | BlockKind::BulletedListItem(t)
            | BlockKind::NumberedListItem(t)
            | BlockKind::Toggle(t)
            | BlockKind::Quote(t)
            | BlockKind::Callout(t) => text(t),
            BlockKind::Heading1(h) | BlockKind::Heading2(h) | BlockKind::Heading3(h) => heading(h),
            BlockKind::ToDo { text: t, checked } => {
                let mut v = text(t);
                v["checked"] = json!(checked);
                v
            }
            // code blocks carry no color
            BlockKind::Code { text: t, language } => {
                json!({ "rich_text": t.rich_text, "language": language })
            }
            BlockKind::Divider => json!({}),
            BlockKind::ChildPage { title } | BlockKind::ChildDatabase { title } => {
                json!({ "title": title })
            }
            BlockKind::Unsupported { .. } => Value::Null,
        }
    }

    /// Parse a known type's payload; `None` for unknown types or bad payloads.
    fn parse(t: &str, p: &Value) -> Option<Self> {
        let text = || serde_json::from_value::<TextBlock>(p.clone()).ok();
        let heading = || {
            Some(Heading {
                text: text()?,
                is_toggleable: p["is_toggleable"].as_bool().unwrap_or(false),
            })
        };
        let title = || p["title"].as_str().map(str::to_string);
        Some(match t {
            "paragraph" => BlockKind::Paragraph(text()?),
            "heading_1" => BlockKind::Heading1(heading()?),
            "heading_2" => BlockKind::Heading2(heading()?),
            "heading_3" => BlockKind::Heading3(heading()?),
            "bulleted_list_item" => BlockKind::BulletedListItem(text()?),
            "numbered_list_item" => BlockKind::NumberedListItem(text()?),
            "to_do" => BlockKind::ToDo {
                text: text()?,
                checked: p["checked"].as_bool().unwrap_or(false),
            },
            "toggle" => BlockKind::Toggle(text()?),
            "quote" => BlockKind::Quote(text()?),
            "callout" => BlockKind::Callout(text()?),
            "code" => BlockKind::Code {
                text: text()?,
                language: p["language"].as_str()?.to_string(),
            },
            "divider" => BlockKind::Divider,
            "child_page" => BlockKind::ChildPage { title: title()? },
            "child_database" => BlockKind::ChildDatabase { title: title()? },
            _ => return None,
        })
    }
}

impl Block {
    fn from_value(v: Value) -> Result<Self> {
        let t = str_field(&v, "type")?;
        let kind = BlockKind::parse(&t, &v[t.as_str()])
            .unwrap_or_else(|| BlockKind::Unsupported { raw: v.clone() });
        Ok(Block {
            id: str_field(&v, "id")?,
            parent: Parent::from_value(v.get("parent").cloned().unwrap_or(Value::Null))?,
            has_children: v["has_children"].as_bool().unwrap_or(false),
            in_trash: in_trash(&v),
            created_time: time_field(&v, "created_time")?,
            last_edited_time: time_field(&v, "last_edited_time")?,
            kind,
            raw: v,
        })
    }

    fn to_json(&self) -> Value {
        let mut out = match &self.kind {
            BlockKind::Unsupported { raw } => raw.clone(),
            kind => {
                let t = kind.type_name();
                let mut out = self.raw.clone();
                if !out.is_object() {
                    out = json!({});
                }
                if let Some(old) = out["type"].as_str().filter(|old| *old != t) {
                    let old = old.to_string();
                    out.as_object_mut().unwrap().remove(&old);
                }
                out["type"] = json!(t);
                if !out[t].is_object() {
                    out[t] = json!({});
                }
                if let (Some(dst), Value::Object(src)) = (out[t].as_object_mut(), kind.payload()) {
                    dst.extend(src);
                }
                out
            }
        };
        write_common(&mut out, self.in_trash, self.last_edited_time);
        out
    }
}

// ---------------------------------------------------------------- pages

/// Serialize emits `raw` with these struct fields written back: `properties`
/// (the map replaces `raw.properties`), `in_trash` (+ `archived` if present) and
/// `last_edited_time`. `id`, `parent`, `url`, `created_time` are not.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub id: String,
    pub parent: Parent,
    pub created_time: DateTime<Utc>,
    pub last_edited_time: DateTime<Utc>,
    pub in_trash: bool,
    pub url: String,
    pub properties: BTreeMap<String, PropertyValue>,
    pub raw: Value,
}

impl Page {
    /// Plain text of the property whose type is "title"; "" if none.
    pub fn title(&self) -> String {
        self.properties
            .values()
            .find_map(|p| match p {
                PropertyValue::Title(rt) => Some(plain_text(rt)),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn from_value(v: Value) -> Result<Self> {
        let properties = v["properties"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, p)| (k.clone(), PropertyValue::parse(p.clone())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Page {
            id: str_field(&v, "id")?,
            parent: Parent::from_value(v.get("parent").cloned().unwrap_or(Value::Null))?,
            created_time: time_field(&v, "created_time")?,
            last_edited_time: time_field(&v, "last_edited_time")?,
            in_trash: in_trash(&v),
            url: v["url"].as_str().unwrap_or_default().to_string(),
            properties,
            raw: v,
        })
    }

    /// `raw` with `properties` rebuilt from the map. Unchanged values keep their
    /// raw JSON (lossless); changed ones get `type` + `<type>` replaced, keeping `id` etc.
    fn to_json(&self) -> Value {
        let mut out = self.raw.clone();
        if !out.is_object() {
            out = json!({});
        }
        let props: Map<String, Value> = self
            .properties
            .iter()
            .map(|(name, pv)| {
                let orig = &self.raw["properties"][name];
                if PropertyValue::parse(orig.clone()) == *pv {
                    return (name.clone(), orig.clone());
                }
                let new = pv.to_json();
                if matches!(pv, PropertyValue::Unsupported { .. }) {
                    return (name.clone(), new);
                }
                let mut obj = orig.as_object().cloned().unwrap_or_default();
                if let Some(old) = orig["type"].as_str() {
                    obj.remove(old);
                }
                if let Value::Object(n) = new {
                    obj.extend(n);
                }
                (name.clone(), Value::Object(obj))
            })
            .collect();
        out["properties"] = Value::Object(props);
        write_common(&mut out, self.in_trash, self.last_edited_time);
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Title(Vec<RichText>),
    RichText(Vec<RichText>),
    Number(Option<f64>),
    Select(Option<SelectOption>),
    MultiSelect(Vec<SelectOption>),
    Status(Option<SelectOption>),
    Date(Option<DateValue>),
    Checkbox(bool),
    Url(Option<String>),
    Email(Option<String>),
    PhoneNumber(Option<String>),
    Relation(Vec<String>),
    Formula(Value),
    Rollup(Value),
    CreatedTime(DateTime<Utc>),
    LastEditedTime(DateTime<Utc>),
    /// Any other type, or anything failing to parse; `raw` is the full property value object.
    Unsupported {
        raw: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectOption {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// Dates stay strings: they may be date-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DateValue {
    pub start: String,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

impl PropertyValue {
    fn from_value(v: Value) -> Result<Self> {
        Ok(Self::parse(v))
    }

    fn parse(v: Value) -> Self {
        Self::try_parse(&v).unwrap_or(PropertyValue::Unsupported { raw: v })
    }

    fn try_parse(v: &Value) -> Option<Self> {
        fn de<T: serde::de::DeserializeOwned>(p: &Value) -> Option<T> {
            serde_json::from_value(p.clone()).ok()
        }
        let t = v["type"].as_str()?;
        let p = &v[t];
        Some(match t {
            "title" => PropertyValue::Title(de(p)?),
            "rich_text" => PropertyValue::RichText(de(p)?),
            "number" => PropertyValue::Number(de(p)?),
            "select" => PropertyValue::Select(de(p)?),
            "multi_select" => PropertyValue::MultiSelect(de(p)?),
            "status" => PropertyValue::Status(de(p)?),
            "date" => PropertyValue::Date(de(p)?),
            "checkbox" => PropertyValue::Checkbox(p.as_bool()?),
            "url" => PropertyValue::Url(de(p)?),
            "email" => PropertyValue::Email(de(p)?),
            "phone_number" => PropertyValue::PhoneNumber(de(p)?),
            "relation" => PropertyValue::Relation(
                p.as_array()?
                    .iter()
                    .map(|r| r["id"].as_str().map(str::to_string))
                    .collect::<Option<_>>()?,
            ),
            "formula" => PropertyValue::Formula(p.clone()),
            "rollup" => PropertyValue::Rollup(p.clone()),
            "created_time" => PropertyValue::CreatedTime(de(p)?),
            "last_edited_time" => PropertyValue::LastEditedTime(de(p)?),
            _ => return None,
        })
    }

    fn type_name(&self) -> &str {
        match self {
            PropertyValue::Title(_) => "title",
            PropertyValue::RichText(_) => "rich_text",
            PropertyValue::Number(_) => "number",
            PropertyValue::Select(_) => "select",
            PropertyValue::MultiSelect(_) => "multi_select",
            PropertyValue::Status(_) => "status",
            PropertyValue::Date(_) => "date",
            PropertyValue::Checkbox(_) => "checkbox",
            PropertyValue::Url(_) => "url",
            PropertyValue::Email(_) => "email",
            PropertyValue::PhoneNumber(_) => "phone_number",
            PropertyValue::Relation(_) => "relation",
            PropertyValue::Formula(_) => "formula",
            PropertyValue::Rollup(_) => "rollup",
            PropertyValue::CreatedTime(_) => "created_time",
            PropertyValue::LastEditedTime(_) => "last_edited_time",
            PropertyValue::Unsupported { raw } => raw["type"].as_str().unwrap_or("unsupported"),
        }
    }

    /// `{"type": t, t: value}`; Unsupported → its raw.
    fn to_json(&self) -> Value {
        let value = match self {
            PropertyValue::Title(rt) | PropertyValue::RichText(rt) => json!(rt),
            PropertyValue::Number(n) => json!(n),
            PropertyValue::Select(o) | PropertyValue::Status(o) => json!(o),
            PropertyValue::MultiSelect(o) => json!(o),
            PropertyValue::Date(d) => json!(d),
            PropertyValue::Checkbox(b) => json!(b),
            PropertyValue::Url(s) | PropertyValue::Email(s) | PropertyValue::PhoneNumber(s) => {
                json!(s)
            }
            PropertyValue::Relation(ids) => ids.iter().map(|id| json!({ "id": id })).collect(),
            PropertyValue::Formula(v) | PropertyValue::Rollup(v) => v.clone(),
            PropertyValue::CreatedTime(t) | PropertyValue::LastEditedTime(t) => json!(t),
            PropertyValue::Unsupported { raw } => return raw.clone(),
        };
        let t = self.type_name();
        json!({ "type": t, t: value })
    }
}

// ---------------------------------------------------------------- data sources & search

#[derive(Debug, Clone, PartialEq)]
pub struct DataSource {
    pub id: String,
    pub database_id: String,
    pub title: String,
    pub last_edited_time: DateTime<Utc>,
    pub properties: Value,
    pub raw: Value,
}

impl DataSource {
    fn from_value(v: Value) -> Result<Self> {
        let title: Vec<RichText> =
            serde_json::from_value(v.get("title").cloned().unwrap_or(json!([])))?;
        Ok(DataSource {
            id: str_field(&v, "id")?,
            database_id: v["parent"]["database_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            title: plain_text(&title),
            last_edited_time: time_field(&v, "last_edited_time")?,
            properties: v.get("properties").cloned().unwrap_or_else(|| json!({})),
            raw: v,
        })
    }

    fn to_json(&self) -> Value {
        self.raw.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchResult {
    Page(Page),
    DataSource(DataSource),
}

impl SearchResult {
    fn from_value(v: Value) -> Result<Self> {
        match v["object"].as_str() {
            Some("page") => Ok(SearchResult::Page(Page::from_value(v)?)),
            Some("data_source") => Ok(SearchResult::DataSource(DataSource::from_value(v)?)),
            other => Err(Error::Invalid(format!(
                "unexpected search result object {other:?}"
            ))),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            SearchResult::Page(p) => p.to_json(),
            SearchResult::DataSource(d) => d.to_json(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "2025-01-01T09:30:00.000Z";

    fn rt(s: &str) -> Value {
        json!({
            "type": "text",
            "text": { "content": s, "link": null },
            "annotations": { "bold": false, "italic": false, "strikethrough": false,
                             "underline": false, "code": false, "color": "default" },
            "plain_text": s,
            "href": null
        })
    }

    fn block_json(t: &str, payload: Value) -> Value {
        json!({
            "object": "block",
            "id": "b1",
            "parent": { "type": "page_id", "page_id": "p1" },
            "created_time": T,
            "last_edited_time": T,
            "created_by": { "object": "user", "id": "u1" },
            "has_children": false,
            "archived": false,
            "in_trash": false,
            "type": t,
            t: payload
        })
    }

    fn parse_block(v: Value) -> Block {
        serde_json::from_value(v).unwrap()
    }

    fn text_of(b: &Block) -> String {
        plain_text(&b.kind.text().unwrap().rich_text)
    }

    #[test]
    fn typed_block_kinds_parse() {
        let tp = |s: &str| json!({ "rich_text": [rt(s)], "color": "default" });
        for (t, s) in [
            ("paragraph", "para"),
            ("bulleted_list_item", "bul"),
            ("numbered_list_item", "num"),
            ("toggle", "tog"),
            ("quote", "quo"),
            ("callout", "cal"),
        ] {
            let mut p = tp(s);
            if t == "callout" {
                p["icon"] = json!({ "emoji": "⭐" });
            }
            let b = parse_block(block_json(t, p));
            assert_eq!(b.kind.type_name(), t);
            assert_eq!(text_of(&b), s);
            assert!(!matches!(b.kind, BlockKind::Unsupported { .. }));
        }

        for (t, tog) in [
            ("heading_1", true),
            ("heading_2", false),
            ("heading_3", true),
        ] {
            let mut p = tp(t);
            p["is_toggleable"] = json!(tog);
            let b = parse_block(block_json(t, p));
            let h = match &b.kind {
                BlockKind::Heading1(h) | BlockKind::Heading2(h) | BlockKind::Heading3(h) => h,
                k => panic!("{k:?}"),
            };
            assert_eq!(b.kind.type_name(), t);
            assert_eq!(h.is_toggleable, tog);
            assert_eq!(plain_text(&h.text.rich_text), t);
        }

        let mut p = tp("todo");
        p["checked"] = json!(true);
        let b = parse_block(block_json("to_do", p));
        assert!(matches!(&b.kind, BlockKind::ToDo { checked: true, .. }));
        assert_eq!(text_of(&b), "todo");

        let b = parse_block(block_json(
            "code",
            json!({ "rich_text": [rt("fn main() {}")], "caption": [], "language": "rust" }),
        ));
        assert!(matches!(&b.kind, BlockKind::Code { language, .. } if language == "rust"));
        assert_eq!(text_of(&b), "fn main() {}");

        assert_eq!(
            parse_block(block_json("divider", json!({}))).kind,
            BlockKind::Divider
        );
        assert_eq!(
            parse_block(block_json("child_page", json!({ "title": "Sub" }))).kind,
            BlockKind::ChildPage {
                title: "Sub".into()
            }
        );
        assert_eq!(
            parse_block(block_json("child_database", json!({ "title": "DB" }))).kind,
            BlockKind::ChildDatabase { title: "DB".into() }
        );

        // known type with a bad payload → Unsupported, block still parses
        let v = block_json("paragraph", json!({ "rich_text": "nope" }));
        assert_eq!(
            parse_block(v.clone()).kind,
            BlockKind::Unsupported { raw: v }
        );
    }

    #[test]
    fn unsupported_blocks_round_trip_exactly() {
        for v in [
            block_json(
                "some_future_block",
                json!({ "foo": [1, 2, { "bar": null }], "baz": "q" }),
            ),
            block_json("unsupported", json!({ "block_type": "form" })),
        ] {
            let b = parse_block(v.clone());
            assert!(matches!(&b.kind, BlockKind::Unsupported { raw } if *raw == v));
            assert_eq!(b.kind.type_name(), v["type"].as_str().unwrap());
            assert_eq!(serde_json::to_value(&b).unwrap(), v);
            assert!(b.kind.to_request_json().is_err());
            assert!(b.kind.clone().set_text(vec![]).is_err());
        }
    }

    #[test]
    fn set_text_round_trip_keeps_unmodelled_fields() {
        let v = block_json(
            "paragraph",
            json!({ "rich_text": [rt("old")], "color": "blue", "icon": { "emoji": "x" } }),
        );
        let mut b = parse_block(v);
        b.kind.set_text(vec![RichText::plain("x")]).unwrap();
        let out = serde_json::to_value(&b).unwrap();
        let b2 = parse_block(out.clone());
        assert_eq!(text_of(&b2), "x");
        assert_eq!(b2.kind.text().unwrap().color, "blue");
        assert_eq!(out["paragraph"]["icon"], json!({ "emoji": "x" }));
        assert_eq!(out["created_by"]["id"], "u1");
    }

    #[test]
    fn to_do_request_json() {
        let b = parse_block(block_json(
            "to_do",
            json!({ "rich_text": [rt("task")], "checked": true, "color": "red" }),
        ));
        assert_eq!(
            b.kind.to_request_json().unwrap(),
            json!({ "type": "to_do", "to_do": { "rich_text": [rt("task")], "checked": true, "color": "red" } })
        );
        let cp = BlockKind::ChildPage { title: "x".into() };
        assert!(cp.to_request_json().is_err());
    }

    fn page_json() -> Value {
        json!({
            "object": "page",
            "id": "pg1",
            "created_time": T,
            "last_edited_time": "2025-01-02T10:00:00.000Z",
            "created_by": { "object": "user", "id": "u1" },
            "icon": { "type": "emoji", "emoji": "📄" },
            "cover": null,
            "parent": { "type": "data_source_id", "data_source_id": "ds1", "database_id": "db1" },
            "archived": false,
            "in_trash": false,
            "url": "https://www.notion.so/pg1",
            "public_url": null,
            "properties": {
                "Task": { "id": "title", "type": "title", "title": [rt("Hello "), rt("world")] },
                "Notes": { "id": "a", "type": "rich_text", "rich_text": [rt("n")] },
                "Count": { "id": "b", "type": "number", "number": 3 },
                "Empty": { "id": "b2", "type": "number", "number": null },
                "Kind": { "id": "c", "type": "select", "select": { "id": "s1", "name": "A", "color": "red" } },
                "Tags": { "id": "d", "type": "multi_select", "multi_select": [
                    { "id": "t1", "name": "x", "color": "blue" }, { "id": "t2", "name": "y", "color": "gray" } ] },
                "State": { "id": "e", "type": "status", "status": { "id": "st", "name": "Done", "color": "green" } },
                "Due": { "id": "f", "type": "date", "date": { "start": "2025-02-01", "end": null, "time_zone": null } },
                "Ok": { "id": "g", "type": "checkbox", "checkbox": true },
                "Link": { "id": "h", "type": "url", "url": "https://x.y" },
                "Mail": { "id": "i", "type": "email", "email": null },
                "Phone": { "id": "j", "type": "phone_number", "phone_number": "+43 1" },
                "Rel": { "id": "k", "type": "relation", "relation": [{ "id": "r1" }, { "id": "r2" }], "has_more": false },
                "F": { "id": "l", "type": "formula", "formula": { "type": "number", "number": 6 } },
                "R": { "id": "m", "type": "rollup", "rollup": { "type": "number", "number": 1, "function": "sum" } },
                "Created": { "id": "n", "type": "created_time", "created_time": T },
                "Edited": { "id": "o", "type": "last_edited_time", "last_edited_time": T },
                "Who": { "id": "p", "type": "people", "people": [{ "object": "user", "id": "u1" }] }
            }
        })
    }

    #[test]
    fn page_properties_and_round_trip() {
        let v = page_json();
        let page: Page = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(
            page.parent,
            Parent::DataSource {
                id: "ds1".into(),
                database_id: "db1".into()
            }
        );
        assert_eq!(page.parent.kind_str(), "data_source_id");
        assert_eq!(page.title(), "Hello world");
        let p = &page.properties;
        let t: DateTime<Utc> = T.parse().unwrap();
        assert!(matches!(&p["Notes"], PropertyValue::RichText(r) if plain_text(r) == "n"));
        assert_eq!(p["Count"], PropertyValue::Number(Some(3.0)));
        assert_eq!(p["Empty"], PropertyValue::Number(None));
        assert!(matches!(&p["Kind"], PropertyValue::Select(Some(o)) if o.name == "A"));
        assert!(matches!(&p["Tags"], PropertyValue::MultiSelect(o) if o.len() == 2));
        assert!(matches!(&p["State"], PropertyValue::Status(Some(o)) if o.name == "Done"));
        assert!(
            matches!(&p["Due"], PropertyValue::Date(Some(d)) if d.start == "2025-02-01" && d.end.is_none())
        );
        assert_eq!(p["Ok"], PropertyValue::Checkbox(true));
        assert_eq!(p["Link"], PropertyValue::Url(Some("https://x.y".into())));
        assert_eq!(p["Mail"], PropertyValue::Email(None));
        assert_eq!(p["Phone"], PropertyValue::PhoneNumber(Some("+43 1".into())));
        assert_eq!(
            p["Rel"],
            PropertyValue::Relation(vec!["r1".into(), "r2".into()])
        );
        assert!(matches!(&p["F"], PropertyValue::Formula(f) if f["number"] == 6));
        assert!(matches!(&p["R"], PropertyValue::Rollup(r) if r["function"] == "sum"));
        assert_eq!(p["Created"], PropertyValue::CreatedTime(t));
        assert_eq!(p["Edited"], PropertyValue::LastEditedTime(t));
        assert_eq!(
            p["Who"],
            PropertyValue::Unsupported {
                raw: v["properties"]["Who"].clone()
            }
        );

        // unchanged round-trip is exact
        let out = serde_json::to_value(&page).unwrap();
        assert_eq!(out, v);
        let back: Page = serde_json::from_value(out).unwrap();
        assert_eq!(back, page);

        // a local edit shows up and keeps the property id
        let mut edited = page.clone();
        edited
            .properties
            .insert("Count".into(), PropertyValue::Number(Some(7.0)));
        let out = serde_json::to_value(&edited).unwrap();
        assert_eq!(out["properties"]["Count"]["id"], "b");
        let back: Page = serde_json::from_value(out).unwrap();
        assert_eq!(back.properties["Count"], PropertyValue::Number(Some(7.0)));
    }

    #[test]
    fn title_with_no_title_property_is_empty() {
        let mut v = page_json();
        v["properties"] = json!({});
        let page: Page = serde_json::from_value(v).unwrap();
        assert_eq!(page.title(), "");
    }

    #[test]
    fn search_results_and_archived_fallback() {
        let mut pv = page_json();
        pv.as_object_mut().unwrap().remove("in_trash");
        pv["archived"] = json!(true);
        let r: SearchResult = serde_json::from_value(pv).unwrap();
        assert!(matches!(&r, SearchResult::Page(p) if p.in_trash));

        let ds = json!({
            "object": "data_source",
            "id": "ds1",
            "title": [rt("My "), rt("DB")],
            "parent": { "type": "database_id", "database_id": "db1" },
            "created_time": T,
            "last_edited_time": T,
            "properties": { "Name": { "id": "title", "name": "Name", "type": "title", "title": {} } }
        });
        let r: SearchResult = serde_json::from_value(ds.clone()).unwrap();
        let SearchResult::DataSource(d) = &r else {
            panic!("{r:?}")
        };
        assert_eq!(d.title, "My DB");
        assert_eq!(d.database_id, "db1");
        assert_eq!(d.properties["Name"]["type"], "title");
        assert_eq!(serde_json::to_value(&r).unwrap(), ds);

        assert!(
            serde_json::from_value::<SearchResult>(json!({ "object": "database", "id": "x" }))
                .is_err()
        );
    }

    #[test]
    fn unknown_parent_round_trips() {
        let parent = json!({ "type": "agent_id", "agent_id": "a1" });
        let mut bv = block_json(
            "paragraph",
            json!({ "rich_text": [rt("x")], "color": "default" }),
        );
        bv["parent"] = parent.clone();
        let b = parse_block(bv.clone());
        assert_eq!(b.parent, Parent::Other(parent.clone()));
        assert_eq!(b.parent.kind_str(), "agent_id");
        assert_eq!(b.parent.id(), None);
        assert_eq!(serde_json::to_value(&b).unwrap(), bv);

        let mut pv = page_json();
        pv["parent"] = parent.clone();
        let page: Page = serde_json::from_value(pv.clone()).unwrap();
        assert_eq!(page.parent, Parent::Other(parent));
        assert_eq!(serde_json::to_value(&page).unwrap(), pv);
    }

    #[test]
    fn struct_field_edits_are_serialized() {
        let mut v = block_json(
            "paragraph",
            json!({ "rich_text": [rt("p")], "color": "default" }),
        );
        v.as_object_mut().unwrap().remove("in_trash");
        v["archived"] = json!(true);
        let mut b = parse_block(v);
        assert!(b.in_trash, "archived fallback on a block");

        let t: DateTime<Utc> = "2025-03-04T05:06:00.000Z".parse().unwrap();
        b.in_trash = false;
        b.last_edited_time = t;
        b.kind = BlockKind::Quote(b.kind.text().unwrap().clone());
        let out = serde_json::to_value(&b).unwrap();
        assert_eq!(out["in_trash"], false);
        assert_eq!(out["archived"], false);
        assert_eq!(out["last_edited_time"], "2025-03-04T05:06:00.000Z");
        assert_eq!(out["type"], "quote");
        assert!(out.get("paragraph").is_none());
        let b2 = parse_block(out);
        assert!(matches!(&b2.kind, BlockKind::Quote(tb) if plain_text(&tb.rich_text) == "p"));
        assert_eq!(b2.last_edited_time, t);

        let mut page: Page = serde_json::from_value(page_json()).unwrap();
        page.in_trash = true;
        page.last_edited_time = t;
        let out = serde_json::to_value(&page).unwrap();
        assert_eq!(
            (&out["in_trash"], &out["archived"]),
            (&json!(true), &json!(true))
        );
        assert_eq!(out["last_edited_time"], "2025-03-04T05:06:00.000Z");
    }

    #[test]
    fn unknown_rich_text_type_makes_block_unsupported() {
        let v = block_json(
            "paragraph",
            json!({ "rich_text": [rt("ok"), { "type": "future_rt", "future_rt": {}, "plain_text": "?" }],
                    "color": "default" }),
        );
        let b = parse_block(v.clone());
        assert_eq!(b.kind, BlockKind::Unsupported { raw: v.clone() });
        assert_eq!(serde_json::to_value(&b).unwrap(), v);
    }

    #[test]
    fn rich_text_serialization() {
        let mut r = RichText::plain("a");
        r.kind = RichTextKind::Text {
            content: "a".into(),
            link: Some("https://l".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["text"]["link"], json!({ "url": "https://l" }));
        assert_eq!(serde_json::from_value::<RichText>(v).unwrap(), r);

        for v in [
            json!({ "type": "mention", "mention": { "type": "page", "page": { "id": "p" } },
                    "annotations": Annotations::default(), "plain_text": "P", "href": "https://p" }),
            json!({ "type": "equation", "equation": { "expression": "e=mc^2" },
                    "annotations": Annotations::default(), "plain_text": "e=mc^2", "href": null }),
        ] {
            let r: RichText = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(&r).unwrap(), v);
        }
        assert_eq!(
            Parent::Workspace.to_json(),
            json!({ "type": "workspace", "workspace": true })
        );
        assert_eq!(
            Parent::Block("x".into()).to_json(),
            json!({ "type": "block_id", "block_id": "x" })
        );
    }
}
