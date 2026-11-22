//! Property values of database rows, on the raw JSON Notion sends (nothing is
//! lost): the text of a cell, a sort number, and the value an edit writes.

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde_json::{json, Value};

use crate::model::RichText;
use crate::{Error, Result};

/// The property types a row cell can be edited as (the API writes these).
pub fn editable(kind: &str) -> bool {
    matches!(
        kind,
        "title" | "rich_text" | "number" | "select" | "multi_select" | "status" | "date"
            | "checkbox" | "url" | "email" | "phone_number" | "people"
    )
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        n.to_string()
    }
}

/// `start` or `start → end`.
fn date_text(d: &Value) -> String {
    match d["end"].as_str() {
        Some(e) => format!("{} → {e}", s(&d["start"])),
        None => s(&d["start"]),
    }
}

/// What a cell shows. `who` names a user id. "" for an empty or unknown value.
pub fn cell_text(p: &Value, who: &dyn Fn(&str) -> String) -> String {
    let t = p["type"].as_str().unwrap_or_default();
    let v = &p[t];
    let each = |f: &dyn Fn(&Value) -> String| {
        let items = v.as_array().map(Vec::as_slice).unwrap_or_default();
        items.iter().map(f).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ")
    };
    match t {
        "title" | "rich_text" => v.as_array().map_or(String::new(), |a| {
            a.iter().map(|r| r["plain_text"].as_str().or(r["text"]["content"].as_str()).unwrap_or_default()).collect()
        }),
        "number" => v.as_f64().map(num).unwrap_or_default(),
        "select" | "status" => s(&v["name"]),
        "multi_select" => each(&|o| s(&o["name"])),
        "date" => date_text(v),
        "checkbox" | "boolean" => (if v.as_bool() == Some(true) { "☑" } else { "☐" }).into(),
        "url" | "email" | "phone_number" | "created_time" | "last_edited_time" | "string" => s(v),
        "people" => each(&|u| who(&s(&u["id"]))),
        "created_by" | "last_edited_by" => who(&s(&v["id"])),
        "relation" => match v.as_array().map_or(0, Vec::len) {
            0 => String::new(),
            n => format!("{n} linked"),
        },
        "files" => each(&|f| s(&f["name"])),
        "unique_id" => match v["prefix"].as_str() {
            Some(p) => format!("{p}-{}", v["number"]),
            None => v["number"].to_string(),
        },
        "formula" => cell_text(v, who),
        "rollup" if v["type"] == "array" => {
            let items = v["array"].as_array().map(Vec::as_slice).unwrap_or_default();
            items.iter().map(|i| cell_text(i, who)).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ")
        }
        "rollup" => cell_text(v, who),
        "verification" => s(&v["state"]),
        _ => String::new(),
    }
}

/// The number a column of numbers sorts by.
pub fn sort_num(p: &Value) -> Option<f64> {
    match p["type"].as_str()? {
        "number" => p["number"].as_f64(),
        t @ ("formula" | "rollup") if p[t]["type"] == "number" => p[t]["number"].as_f64(),
        _ => None,
    }
}

/// The text an editor starts with; what [`encode`] reads back.
pub fn edit_text(p: &Value) -> String {
    let t = p["type"].as_str().unwrap_or_default();
    let v = &p[t];
    let join = |f: &dyn Fn(&Value) -> String| {
        v.as_array().map(Vec::as_slice).unwrap_or_default().iter().map(f).collect::<Vec<_>>().join(",")
    };
    match t {
        "multi_select" => join(&|o| s(&o["name"])),
        "people" => join(&|u| s(&u["id"])),
        "date" => match v["end"].as_str() {
            Some(e) => format!("{}/{e}", s(&v["start"])),
            None => s(&v["start"]),
        },
        "checkbox" => v.as_bool().unwrap_or(false).to_string(),
        _ => cell_text(p, &|id| id.to_string()),
    }
}

pub fn parse_number(t: &str) -> Result<Option<f64>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    // "1,5" is a decimal comma; "1,000" or "1,234.5" is ambiguous
    if t.matches(',').count() > 1 || (t.contains(',') && t.contains('.')) || t.split_once(',').is_some_and(|(_, f)| f.len() == 3 && f.bytes().all(|b| b.is_ascii_digit())) {
        return Err(Error::Invalid(format!("`{t}` is not a number")));
    }
    match t.replace(',', ".").parse::<f64>() {
        Ok(n) if n.is_finite() => Ok(Some(n)),
        _ => Err(Error::Invalid(format!("`{t}` is not a number"))),
    }
}

fn dated(d: &str) -> bool {
    (d.len() == 10 && NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
        || DateTime::parse_from_rfc3339(d).is_ok()
        || ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"].iter().any(|f| NaiveDateTime::parse_from_str(d, f).is_ok())
}

/// `YYYY-MM-DD` (or a datetime), optionally `start/end` or `start → end`.
pub fn parse_date(t: &str) -> Result<Option<(String, Option<String>)>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let (a, b) = match t.split_once(['/', '→']) {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (t, None),
    };
    if !dated(a) || b.is_some_and(|b| !dated(b)) {
        return Err(Error::Invalid("a date is YYYY-MM-DD, or start/end".into()));
    }
    if b.is_some_and(|b| b.len() == a.len() && b < a) {
        return Err(Error::Invalid("the end is before the start".into()));
    }
    Ok(Some((a.to_string(), b.map(str::to_string))))
}

pub fn parse_url(t: &str) -> Result<Option<String>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let ok = reqwest::Url::parse(t).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some());
    if !ok || t.len() > 2000 {
        return Err(Error::Invalid("a URL starts with http:// or https://".into()));
    }
    Ok(Some(t.to_string()))
}

pub fn parse_email(t: &str) -> Result<Option<String>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let ok = t.len() <= 200
        && !t.contains(char::is_whitespace)
        && t.split_once('@').is_some_and(|(l, d)| {
            !l.is_empty() && !d.contains('@') && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
        });
    if ok { Ok(Some(t.to_string())) } else { Err(Error::Invalid("not an email address".into())) }
}

pub fn parse_phone(t: &str) -> Result<Option<String>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let ok = t.len() <= 200
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || "+-()./ x".contains(c));
    if ok { Ok(Some(t.to_string())) } else { Err(Error::Invalid("not a phone number".into())) }
}

fn list(t: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in t.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// The schema's option named `name` (id and colour), else just the name.
fn option(schema: &Value, kind: &str, name: &str) -> Value {
    let found = schema[kind]["options"].as_array().and_then(|o| o.iter().find(|o| o["name"] == name));
    found.cloned().unwrap_or_else(|| json!({ "name": name }))
}

/// The edit `text` of the property `schema` describes, as (the property value
/// to send, the property as the cache holds it). `text`: select and status a
/// name, multi-select names and people user ids joined by `,`, a checkbox
/// `true`/`false`, a date as [`parse_date`]. `prev` is the current value (a
/// date keeps its time zone).
pub fn encode(schema: &Value, text: &str, prev: &Value) -> Result<(Value, Value)> {
    let kind = schema["type"].as_str().unwrap_or_default();
    let name = |n: &str| json!({ "name": n });
    let (sent, cached) = match kind {
        "title" | "rich_text" => {
            let cs: Vec<char> = text.chars().collect();
            let runs: Vec<String> = cs.chunks(2000).map(|c| c.iter().collect()).collect();
            (
                json!(runs.iter().map(|r| json!({ "type": "text", "text": { "content": r } })).collect::<Vec<_>>()),
                json!(runs.iter().map(|r| RichText::plain(r)).collect::<Vec<_>>()),
            )
        }
        "number" => {
            let n = parse_number(text)?.map(|n| if n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { json!(n) });
            (json!(n), json!(n))
        }
        "select" | "status" => match text.trim() {
            "" if kind == "select" => (Value::Null, Value::Null),
            "" => return Err(Error::Invalid("a status can't be empty".into())),
            n => {
                let o = option(schema, kind, n);
                if kind == "status" && o.get("id").is_none() {
                    return Err(Error::Invalid(format!("`{n}` is not one of the status options")));
                }
                (name(n), o)
            }
        },
        "multi_select" => {
            let ns = list(text);
            (json!(ns.iter().map(|n| name(n)).collect::<Vec<_>>()), json!(ns.iter().map(|n| option(schema, kind, n)).collect::<Vec<_>>()))
        }
        "date" => match parse_date(text)? {
            None => (Value::Null, Value::Null),
            Some((start, end)) => {
                // a time zone only goes with a datetime that has no offset
                let naive = start.contains('T') && DateTime::parse_from_rfc3339(&start).is_err();
                let tz = if naive { prev["date"]["time_zone"].clone() } else { Value::Null };
                let d = json!({ "start": start, "end": end, "time_zone": tz });
                (d.clone(), d)
            }
        },
        "checkbox" => (json!(text.trim() == "true"), json!(text.trim() == "true")),
        "url" | "email" | "phone_number" => {
            let v = match kind {
                "url" => parse_url(text)?,
                "email" => parse_email(text)?,
                _ => parse_phone(text)?,
            };
            (json!(v), json!(v))
        }
        "people" => {
            let us: Vec<Value> = list(text).iter().map(|id| json!({ "object": "user", "id": id })).collect();
            (json!(us), json!(us))
        }
        k => return Err(Error::Invalid(format!("a {k} property can't be edited"))),
    };
    Ok((json!({ kind: sent }), json!({ "id": schema["id"], "type": kind, kind: cached })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(id: &str) -> String {
        format!("user {id}")
    }

    #[test]
    fn cell_text_of_every_type() {
        let t = |p: Value| cell_text(&p, &who);
        assert_eq!(t(json!({ "type": "title", "title": [{ "plain_text": "Ab" }, { "plain_text": "c" }] })), "Abc");
        assert_eq!(t(json!({ "type": "number", "number": 3.0 })), "3");
        assert_eq!(t(json!({ "type": "number", "number": 2.5 })), "2.5");
        assert_eq!(t(json!({ "type": "number", "number": null })), "");
        assert_eq!(t(json!({ "type": "select", "select": { "name": "A" } })), "A");
        assert_eq!(t(json!({ "type": "select", "select": null })), "");
        assert_eq!(t(json!({ "type": "multi_select", "multi_select": [{ "name": "a" }, { "name": "b" }] })), "a, b");
        assert_eq!(t(json!({ "type": "date", "date": { "start": "2025-01-01", "end": "2025-01-03" } })), "2025-01-01 → 2025-01-03");
        assert_eq!(t(json!({ "type": "date", "date": null })), "");
        assert_eq!(t(json!({ "type": "checkbox", "checkbox": true })), "☑");
        assert_eq!(t(json!({ "type": "people", "people": [{ "id": "u1" }, { "id": "u2" }] })), "user u1, user u2");
        assert_eq!(t(json!({ "type": "last_edited_by", "last_edited_by": { "id": "u1" } })), "user u1");
        assert_eq!(t(json!({ "type": "relation", "relation": [{ "id": "a" }] })), "1 linked");
        assert_eq!(t(json!({ "type": "files", "files": [{ "name": "a.pdf" }] })), "a.pdf");
        assert_eq!(t(json!({ "type": "unique_id", "unique_id": { "prefix": "T", "number": 4 } })), "T-4");
        assert_eq!(t(json!({ "type": "formula", "formula": { "type": "string", "string": "x" } })), "x");
        assert_eq!(t(json!({ "type": "formula", "formula": { "type": "number", "number": 7 } })), "7");
        let roll = json!({ "type": "rollup", "rollup": { "type": "array", "array": [
            { "type": "number", "number": 1 }, { "type": "title", "title": [{ "plain_text": "x" }] }] } });
        assert_eq!(t(roll), "1, x");
        assert_eq!(t(json!({ "type": "button", "button": {} })), "");
        assert_eq!(sort_num(&json!({ "type": "formula", "formula": { "type": "number", "number": 7 } })), Some(7.0));
        assert_eq!(sort_num(&json!({ "type": "select", "select": null })), None);
    }

    #[test]
    fn parsers_accept_and_refuse() {
        assert_eq!(parse_number(" 1,5 ").unwrap(), Some(1.5));
        assert_eq!(parse_number("").unwrap(), None);
        assert!(parse_number("abc").is_err() && parse_number("inf").is_err());
        assert_eq!(parse_date("2025-02-03").unwrap(), Some(("2025-02-03".into(), None)));
        assert_eq!(parse_date("2025-02-03 → 2025-02-05").unwrap(), Some(("2025-02-03".into(), Some("2025-02-05".into()))));
        assert_eq!(parse_date("2025-02-03/2025-02-05").unwrap().unwrap().1.as_deref(), Some("2025-02-05"));
        assert!(parse_date("2025-02-30").is_err() && parse_date("3.2.2025").is_err());
        assert!(parse_date("2025-02-05/2025-02-03").is_err());
        assert!(parse_date("2025-02-03T09:30").is_ok() && parse_date("2025-02-03T09:30:00.000+01:00").is_ok());
        assert_eq!(parse_url("https://a.at/x").unwrap().as_deref(), Some("https://a.at/x"));
        assert!(parse_url("ftp://a.at").is_err() && parse_url("a.at").is_err() && parse_url("javascript:alert(1)").is_err());
        assert!(parse_email("a@b.at").is_ok() && parse_email("a@b").is_err() && parse_email("a b@c.at").is_err() && parse_email("@c.at").is_err());
        assert!(parse_phone("+43 (1) 234-56").is_ok() && parse_phone("call me").is_err() && parse_phone("+-").is_err());
    }

    fn schema(kind: &str) -> Value {
        json!({ "id": "pid", "name": "P", "type": kind,
            "select": { "options": [{ "id": "o1", "name": "A", "color": "red" }] },
            "status": { "options": [{ "id": "s1", "name": "Done", "color": "green" }] },
            "multi_select": { "options": [{ "id": "m1", "name": "x", "color": "blue" }] } })
    }

    #[test]
    fn parser_edge_cases_and_empty_input_clears() {
        assert_eq!(parse_number("-2,5").unwrap(), Some(-2.5));
        assert_eq!(parse_number("  ").unwrap(), None);
        assert!(parse_number("1,000").is_err() && parse_number("1,5").is_ok());
        assert!(parse_number("1,234.5").is_err() && parse_number("nan").is_err() && parse_number("1,2,3").is_err());
        assert!(parse_date("2025-01-01/").is_err() && parse_date("/2025-01-01").is_err());
        assert!(parse_date("2025-01-01T10:00/2025-01-01T09:00").is_err());
        assert_eq!(parse_date(" 2025-01-01 / 2025-01-01 ").unwrap().unwrap().1.as_deref(), Some("2025-01-01"));
        assert!(parse_url("https://").is_err() && parse_url("http://a b.at").is_err());
        assert!(parse_email("a@@b.at").is_err() && parse_email("a@.at").is_err() && parse_email("a@b.").is_err());
        for k in ["url", "email", "phone_number", "number", "select", "date"] {
            let (sent, _) = encode(&schema(k), "  ", &Value::Null).unwrap();
            assert_eq!(sent, json!({ k: null }), "{k}");
        }
        for k in ["multi_select", "people", "title"] {
            assert_eq!(encode(&schema(k), "", &Value::Null).unwrap().0, json!({ k: [] }), "{k}");
        }
        assert!(encode(&schema("status"), "", &Value::Null).is_err());
        assert!(!editable("formula") && !editable("rollup") && !editable("relation") && editable("people"));
    }

    #[test]
    fn encode_builds_each_property_body() {
        let sent = |kind: &str, text: &str| encode(&schema(kind), text, &Value::Null).unwrap().0;
        assert_eq!(sent("title", "Hi"), json!({ "title": [{ "type": "text", "text": { "content": "Hi" } }] }));
        assert_eq!(sent("rich_text", ""), json!({ "rich_text": [] }));
        assert_eq!(sent("number", "3"), json!({ "number": 3 }));
        assert_eq!(sent("number", "2,5"), json!({ "number": 2.5 }));
        assert_eq!(sent("number", ""), json!({ "number": null }));
        assert_eq!(sent("select", "A"), json!({ "select": { "name": "A" } }));
        assert_eq!(sent("select", ""), json!({ "select": null }));
        assert_eq!(sent("status", "Done"), json!({ "status": { "name": "Done" } }));
        assert_eq!(sent("multi_select", "x, y,x"), json!({ "multi_select": [{ "name": "x" }, { "name": "y" }] }));
        assert_eq!(sent("date", "2025-01-01/2025-01-02"), json!({ "date": { "start": "2025-01-01", "end": "2025-01-02", "time_zone": null } }));
        assert_eq!(sent("date", ""), json!({ "date": null }));
        assert_eq!(sent("checkbox", "true"), json!({ "checkbox": true }));
        assert_eq!(sent("checkbox", "false"), json!({ "checkbox": false }));
        assert_eq!(sent("url", "https://a.at"), json!({ "url": "https://a.at" }));
        assert_eq!(sent("email", "a@b.at"), json!({ "email": "a@b.at" }));
        assert_eq!(sent("phone_number", ""), json!({ "phone_number": null }));
        assert_eq!(sent("people", "u1,u2"), json!({ "people": [{ "object": "user", "id": "u1" }, { "object": "user", "id": "u2" }] }));
        // a status can't be made up; a select can (Notion adds the option)
        assert!(encode(&schema("status"), "Nope", &Value::Null).is_err());
        assert!(encode(&schema("select"), "New", &Value::Null).is_ok());
        for k in ["formula", "rollup", "relation", "created_time", "files", "unique_id", "button", "verification"] {
            assert!(encode(&schema(k), "x", &Value::Null).is_err(), "{k}");
        }
        assert!(encode(&schema("number"), "x", &Value::Null).is_err());
        // 2001 chars are two runs
        let long = "a".repeat(2001);
        assert_eq!(sent("rich_text", &long)["rich_text"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn encode_caches_what_notion_would_send_back() {
        let c = |kind: &str, text: &str| encode(&schema(kind), text, &Value::Null).unwrap().1;
        assert_eq!(c("select", "A"), json!({ "id": "pid", "type": "select", "select": { "id": "o1", "name": "A", "color": "red" } }));
        assert_eq!(c("multi_select", "x,y")["multi_select"], json!([{ "id": "m1", "name": "x", "color": "blue" }, { "name": "y" }]));
        // the cached value reads back as the text that made it
        for (k, t) in [("number", "4"), ("select", "A"), ("multi_select", "x,y"), ("date", "2025-01-01/2025-01-02"),
            ("checkbox", "true"), ("url", "https://a.at"), ("people", "u1,u2"), ("title", "Hello")] {
            assert_eq!(edit_text(&c(k, t)), t, "{k}");
        }
        // a date keeps its time zone
        let prev = json!({ "type": "date", "date": { "start": "2025-01-01T09:00:00", "time_zone": "Europe/Vienna" } });
        let (sent, _) = encode(&schema("date"), "2025-01-02T10:00:00", &prev).unwrap();
        assert_eq!(sent["date"]["time_zone"], "Europe/Vienna");
        let (sent, _) = encode(&schema("date"), "2025-01-02T10:00:00+01:00", &prev).unwrap();
        assert_eq!(sent["date"]["time_zone"], Value::Null);
    }
}
