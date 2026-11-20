//! Flattened, read-only outline of a cached page. No iced types here.

use std::collections::HashSet;
use std::ops::Range;

use notion_core::markdown::is_markdown_editable;
use notion_core::model::{plain_text, BlockKind, RichText, RichTextKind};
use notion_core::store::Store;
use notion_core::Result;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Marker {
    None,
    Bullet,
    /// Running number among consecutive numbered siblings.
    Number(usize),
    Todo(bool),
    Toggle,
    /// 1..=3
    Heading(u8),
    Quote,
    /// The icon emoji (💡 if it has none).
    Callout(String),
    Code(String),
    Divider,
    Equation,
    ChildPage(String),
    ChildDatabase(String),
    /// bookmark, embed, link_preview, file, pdf, video, image, audio: opens
    /// `url` with xdg-open.
    LinkCard {
        kind: String,
        url: String,
        label: String,
    },
    /// link_to_page; `title` is the cached title, "" if not cached.
    LinkToPage {
        id: String,
        title: String,
        database: bool,
    },
    /// synced_block: it and its subtree are locked.
    Synced,
    ColumnList,
    Column,
    TableOfContents,
    Breadcrumb,
    /// A table and its rows, as one outline row.
    Table(Table),
    /// Any other block type, shown as a grey `[type]`.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub column_header: bool,
    pub row_header: bool,
    pub rows: Vec<TableRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub id: String,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub rich_text: Vec<RichText>,
    pub editable: bool,
}

impl Marker {
    /// Text shown before the row's content ("" for none).
    pub fn label(&self) -> String {
        match self {
            Marker::None | Marker::Heading(_) | Marker::Equation => String::new(),
            Marker::Bullet => "•".into(),
            Marker::Number(n) => format!("{n}."),
            Marker::Todo(false) => "☐".into(),
            Marker::Todo(true) => "☑".into(),
            Marker::Toggle => "▸".into(),
            Marker::Quote => "│".into(),
            Marker::Callout(icon) => icon.clone(),
            Marker::Code(lang) => format!("```{lang}"),
            Marker::Divider => "───".into(),
            Marker::ChildPage(t) => format!("[page: {t}]"),
            Marker::ChildDatabase(t) => format!("[database: {t}]"),
            Marker::LinkCard { kind, label, .. } => format!("[{kind}] {label}"),
            Marker::LinkToPage {
                title, database, ..
            } => {
                let what = if *database { "database" } else { "page" };
                if title.is_empty() {
                    format!("→ [{what}]")
                } else {
                    format!("→ {title}")
                }
            }
            Marker::Synced => "⟳ synced".into(),
            Marker::ColumnList => "[columns]".into(),
            Marker::Column => "[column]".into(),
            Marker::TableOfContents => "[table of contents]".into(),
            Marker::Breadcrumb => "[breadcrumb]".into(),
            Marker::Table(_) => String::new(),
            Marker::Unsupported(t) => format!("[{t}]"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub block_id: String,
    /// Page or block the row sits under.
    pub parent: String,
    pub depth: usize,
    pub marker: Marker,
    pub rich_text: Vec<RichText>,
    /// Text block whose rich text survives a Markdown round trip.
    pub editable: bool,
    /// Inside a synced block: nothing may queue an op on it (to-do clicks
    /// included).
    pub locked: bool,
    /// Toggle block or toggleable heading: its children can be collapsed.
    pub toggle: bool,
    /// Bullets / numbers: how many same-kind list items it is nested in.
    pub level: usize,
}

/// Notion's bullet for a nesting level: • ◦ ▪, cycling.
pub fn bullet(level: usize) -> &'static str {
    ["•", "◦", "▪"][level % 3]
}

/// Notion's number label for a nesting level: 1. a. i., cycling.
pub fn number(n: usize, level: usize) -> String {
    match level % 3 {
        0 => format!("{n}."),
        1 => {
            // a..z, aa, ab, … (CSS lower-alpha)
            let (mut n, mut s) = (n, String::new());
            while n > 0 {
                n -= 1;
                s.insert(0, (b'a' + (n % 26) as u8) as char);
                n /= 26;
            }
            format!("{s}.")
        }
        _ => {
            let mut n = n;
            let mut s = String::new();
            for (v, r) in [
                (1000, "m"),
                (900, "cm"),
                (500, "d"),
                (400, "cd"),
                (100, "c"),
                (90, "xc"),
                (50, "l"),
                (40, "xl"),
                (10, "x"),
                (9, "ix"),
                (5, "v"),
                (4, "iv"),
                (1, "i"),
            ] {
                while n >= v {
                    s.push_str(r);
                    n -= v;
                }
            }
            format!("{s}.")
        }
    }
}

/// Per row: is it shown? Toggles are collapsed unless their id is in `open`;
/// a collapsed toggle hides its whole subtree.
pub fn shown(rows: &[Row], open: &HashSet<String>) -> Vec<bool> {
    // rows deeper than this are inside a collapsed toggle
    let mut hide_below = usize::MAX;
    rows.iter()
        .map(|r| {
            if r.depth <= hide_below {
                hide_below = usize::MAX;
            } else {
                return false;
            }
            if r.toggle && !open.contains(&r.block_id) {
                hide_below = r.depth;
            }
            true
        })
        .collect()
}

/// Ids of `rows[idx]`'s ancestors, nearest first.
pub fn ancestors(rows: &[Row], idx: usize) -> Vec<&str> {
    let mut d = rows[idx].depth;
    let mut out = Vec::new();
    for r in rows[..idx].iter().rev() {
        if r.depth < d {
            out.push(r.block_id.as_str());
            d = r.depth;
        }
    }
    out
}

/// The row ranges of a column_list's columns (each column's subtree, without
/// its own marker row).
pub fn columns(rows: &[Row], idx: usize) -> Vec<Range<usize>> {
    let d = rows[idx].depth;
    let end = idx + 1 + subtree(rows, idx).len();
    (idx + 1..end)
        .filter(|&i| rows[i].depth == d + 1)
        .map(|i| i + 1..i + 1 + subtree(rows, i).len())
        .collect()
}

/// Blocks Notion lets hold children (headings only when toggleable, which a
/// Row doesn't know, so they're left out).
pub fn can_have_children(m: &Marker) -> bool {
    matches!(
        m,
        Marker::None
            | Marker::Bullet
            | Marker::Number(_)
            | Marker::Todo(_)
            | Marker::Toggle
            | Marker::Quote
            | Marker::Callout(_)
    )
}

/// The rows of `rows[idx]`'s subtree (its descendants, in order).
pub fn subtree(rows: &[Row], idx: usize) -> &[Row] {
    let d = rows[idx].depth;
    let end = rows[idx + 1..]
        .iter()
        .position(|r| r.depth <= d)
        .map_or(rows.len(), |n| idx + 1 + n);
    &rows[idx + 1..end]
}

/// Tab: (new parent = previous sibling, after = its last child or None).
pub fn indent_target(rows: &[Row], idx: usize) -> Option<(String, Option<String>)> {
    let d = rows[idx].depth;
    let j = rows[..idx].iter().rposition(|r| r.depth <= d)?;
    let prev = &rows[j];
    // a heading that already has children is toggleable
    let has_kids = rows.get(j + 1).is_some_and(|r| r.depth > d);
    let holds =
        can_have_children(&prev.marker) || (matches!(prev.marker, Marker::Heading(_)) && has_kids);
    if prev.depth < d || !holds {
        return None; // first child, or a sibling that can't hold children
    }
    let last = rows[j + 1..idx]
        .iter()
        .rev()
        .find(|r| r.depth == d + 1)
        .map(|r| r.block_id.clone());
    Some((prev.block_id.clone(), last))
}

/// Shift+Tab: (new parent = grandparent, after = parent). None at top level
/// and in a column (the block would land in the column_list).
pub fn outdent_target(rows: &[Row], idx: usize) -> Option<(String, Option<String>)> {
    let d = rows[idx].depth.checked_sub(1)?;
    let parent = &rows[..idx].iter().rev().find(|r| r.depth == d)?;
    if parent.marker == Marker::Column {
        return None;
    }
    Some((parent.parent.clone(), Some(parent.block_id.clone())))
}

/// First block in the subtree that push's copy-then-delete can't recreate
/// (anything not modelled in core).
pub fn unmovable_in_subtree(rows: &[Row], idx: usize) -> Option<&Marker> {
    subtree(rows, idx).iter().map(|r| &r.marker).find(|m| {
        !matches!(
            m,
            Marker::None
                | Marker::Bullet
                | Marker::Number(_)
                | Marker::Todo(_)
                | Marker::Toggle
                | Marker::Heading(_)
                | Marker::Quote
                | Marker::Callout(_)
                | Marker::Code(_)
                | Marker::Divider
                | Marker::Equation
        )
    })
}

/// Every cached block under `page_id`, depth-first in order. Child pages and
/// databases are leaves.
pub fn flatten(store: &Store, page_id: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    walk(store, page_id, 0, false, [0, 0], &mut rows)?;
    // a table of contents lists the page's headings
    let toc: String = rows
        .iter()
        .filter_map(|r| match r.marker {
            Marker::Heading(n) => Some(format!(
                "{}{}\n",
                "  ".repeat(n as usize - 1),
                plain_text(&r.rich_text)
            )),
            _ => None,
        })
        .collect();
    for r in rows
        .iter_mut()
        .filter(|r| r.marker == Marker::TableOfContents)
    {
        r.rich_text = vec![RichText::plain(toc.trim_end())];
    }
    Ok(rows)
}

/// The marker for a block core keeps as raw JSON (`raw` = the full block).
fn raw_marker(store: &Store, raw: &Value, locked: bool) -> Result<Marker> {
    let t = raw["type"].as_str().unwrap_or("unsupported");
    let p = &raw[t];
    Ok(match t {
        "bookmark" | "embed" | "link_preview" | "file" | "pdf" | "video" | "image" | "audio" => {
            // `url` directly, or under the file's source (external / file)
            let src = p["type"].as_str().unwrap_or_default();
            let url = p["url"].as_str().or(p[src]["url"].as_str());
            let Some(url) = url.filter(|u| !u.is_empty()) else {
                return Ok(Marker::Unsupported(t.into()));
            };
            let caption = p["caption"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|r| r["plain_text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default();
            let label = [p["name"].as_str().unwrap_or_default(), &caption, url]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or(url)
                .to_string();
            Marker::LinkCard {
                kind: t.into(),
                url: url.into(),
                label,
            }
        }
        "link_to_page" => {
            let database = p["type"] == "database_id";
            let Some(id) = p[if database { "database_id" } else { "page_id" }].as_str() else {
                return Ok(Marker::Unsupported(t.into()));
            };
            let title = store.get_page(id)?.map(|p| p.title()).unwrap_or_default();
            Marker::LinkToPage {
                id: id.into(),
                title,
                database,
            }
        }
        "synced_block" => Marker::Synced,
        "column_list" => Marker::ColumnList,
        "column" => Marker::Column,
        "table_of_contents" => Marker::TableOfContents,
        "breadcrumb" => Marker::Breadcrumb,
        "table" => {
            let mut rows = Vec::new();
            for b in store.children(raw["id"].as_str().unwrap_or_default())? {
                let BlockKind::Unsupported { raw } = &b.kind else {
                    continue;
                };
                // a cell that doesn't parse is shown empty and read-only
                let cells = raw["table_row"]["cells"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(
                        |c| match serde_json::from_value::<Vec<RichText>>(c.clone()) {
                            Ok(rt) => Cell {
                                editable: !locked && is_markdown_editable(&rt),
                                rich_text: rt,
                            },
                            Err(_) => Cell {
                                rich_text: Vec::new(),
                                editable: false,
                            },
                        },
                    )
                    .collect();
                rows.push(TableRow {
                    id: b.id.clone(),
                    cells,
                });
            }
            Marker::Table(Table {
                column_header: p["has_column_header"].as_bool().unwrap_or(false),
                row_header: p["has_row_header"].as_bool().unwrap_or(false),
                rows,
            })
        }
        _ => Marker::Unsupported(t.into()),
    })
}

/// `locked`: under a synced block. `lists`: how many bulleted / numbered
/// list items the parent is directly nested in (reset by any other block).
fn walk(
    store: &Store,
    parent: &str,
    depth: usize,
    locked: bool,
    lists: [usize; 2],
    rows: &mut Vec<Row>,
) -> Result<()> {
    let mut number = 0;
    for b in store.children(parent)? {
        number = match b.kind {
            BlockKind::NumberedListItem(_) => number + 1,
            _ => 0,
        };
        let marker = match &b.kind {
            BlockKind::Paragraph(_) => Marker::None,
            BlockKind::Heading1(_) => Marker::Heading(1),
            BlockKind::Heading2(_) => Marker::Heading(2),
            BlockKind::Heading3(_) => Marker::Heading(3),
            BlockKind::BulletedListItem(_) => Marker::Bullet,
            BlockKind::NumberedListItem(_) => Marker::Number(number),
            BlockKind::ToDo { checked, .. } => Marker::Todo(*checked),
            BlockKind::Toggle(_) => Marker::Toggle,
            BlockKind::Quote(_) => Marker::Quote,
            BlockKind::Callout(_) => Marker::Callout(
                b.raw["callout"]["icon"]["emoji"]
                    .as_str()
                    .unwrap_or("💡")
                    .into(),
            ),
            BlockKind::Code { language, .. } => Marker::Code(language.clone()),
            BlockKind::Divider => Marker::Divider,
            BlockKind::Equation { .. } => Marker::Equation,
            BlockKind::ChildPage { title } => Marker::ChildPage(title.clone()),
            BlockKind::ChildDatabase { title } => Marker::ChildDatabase(title.clone()),
            BlockKind::Unsupported { raw } => raw_marker(store, raw, locked)?,
        };
        let (rich_text, editable) = match &b.kind {
            // shown and edited as one equation run
            BlockKind::Equation { expression } => (
                vec![RichText {
                    plain_text: expression.clone(),
                    href: None,
                    annotations: Default::default(),
                    kind: RichTextKind::Equation {
                        expression: expression.clone(),
                    },
                }],
                true,
            ),
            k => {
                let rt = k.text().map(|t| t.rich_text.clone()).unwrap_or_default();
                let ok = k.text().is_some() && is_markdown_editable(&rt);
                (rt, ok)
            }
        };
        // a table's rows live in its marker
        let leaf = matches!(
            b.kind,
            BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
        ) || matches!(marker, Marker::Table(_));
        let locked = locked || marker == Marker::Synced;
        let toggle = match &b.kind {
            BlockKind::Toggle(_) => true,
            BlockKind::Heading1(h) | BlockKind::Heading2(h) | BlockKind::Heading3(h) => {
                h.is_toggleable
            }
            _ => false,
        };
        let (level, kids) = match marker {
            Marker::Bullet => (lists[0], [lists[0] + 1, 0]),
            Marker::Number(_) => (lists[1], [0, lists[1] + 1]),
            _ => (0, [0, 0]),
        };
        rows.push(Row {
            block_id: b.id.clone(),
            parent: parent.to_string(),
            depth,
            marker,
            rich_text,
            editable: editable && !locked,
            locked,
            toggle,
            level,
        });
        if !leaf {
            walk(store, &b.id, depth + 1, locked, kids, rows)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use notion_core::model::Block;
    use serde_json::{json, Value};

    const PID: &str = "p";
    const T: &str = "2025-01-01T00:00:00.000Z";

    pub(crate) fn rt(s: &str) -> Value {
        json!([{ "type": "text", "text": { "content": s }, "plain_text": s, "annotations": {} }])
    }

    pub(crate) fn blk(id: &str, parent: &str, t: &str, payload: Value) -> Block {
        serde_json::from_value(json!({
            "object": "block", "id": id, "parent": { "type": "block_id", "block_id": parent },
            "type": t, t: payload, "has_children": false, "in_trash": false,
            "created_time": T, "last_edited_time": T,
        }))
        .unwrap()
    }

    fn num(id: &str, parent: &str) -> Block {
        blk(
            id,
            parent,
            "numbered_list_item",
            json!({ "rich_text": rt(id) }),
        )
    }

    fn r(id: &str, parent: &str, depth: usize, marker: Marker) -> Row {
        Row {
            toggle: marker == Marker::Toggle,
            block_id: id.into(),
            parent: parent.into(),
            depth,
            marker,
            rich_text: Vec::new(),
            editable: true,
            locked: false,
            level: 0,
        }
    }

    #[test]
    fn indent_and_outdent_targets() {
        // a, a/a1, a/a1/x, b, c(divider), d, d/d1, d/d2
        let rows = vec![
            r("a", PID, 0, Marker::None),
            r("a1", "a", 1, Marker::Bullet),
            r("x", "a1", 2, Marker::None),
            r("b", PID, 0, Marker::None),
            r("c", PID, 0, Marker::Divider),
            r("d", PID, 0, Marker::Toggle),
            r("d1", "d", 1, Marker::None),
            r("d2", "d", 1, Marker::None),
        ];
        let some = |p: &str, a: Option<&str>| Some((p.to_string(), a.map(str::to_string)));
        assert_eq!(indent_target(&rows, 0), None, "no previous sibling");
        assert_eq!(indent_target(&rows, 1), None, "first child");
        assert_eq!(
            indent_target(&rows, 3),
            some("a", Some("a1")),
            "after last child"
        );
        assert_eq!(indent_target(&rows, 5), None, "divider can't hold children");
        assert_eq!(
            indent_target(&rows, 7),
            some("d1", None),
            "sibling without children"
        );
        assert_eq!(outdent_target(&rows, 0), None, "top level");
        assert_eq!(outdent_target(&rows, 2), some("a", Some("a1")), "nested");
        assert_eq!(outdent_target(&rows, 7), some(PID, Some("d")));
        assert_eq!(subtree(&rows, 0).len(), 2);

        let h = |kids: bool| {
            let mut v = vec![r("h", PID, 0, Marker::Heading(2))];
            if kids {
                v.push(r("h1", "h", 1, Marker::None));
            }
            v.push(r("n", PID, 0, Marker::None));
            v
        };
        assert_eq!(indent_target(&h(false), 1), None, "plain heading");
        assert_eq!(
            indent_target(&h(true), 2),
            some("h", Some("h1")),
            "toggleable heading"
        );
        assert_eq!(subtree(&rows, 7).len(), 0);
    }

    #[test]
    fn flattens_with_depth_numbering_and_markers() {
        let store = Store::open_in_memory().unwrap();
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
            "plain_text": "2025-01-01", "annotations": {} }]);
        store
            .replace_children(
                PID,
                PID,
                &[
                    num("n1", PID),
                    num("n2", PID),
                    blk(
                        "b",
                        PID,
                        "bulleted_list_item",
                        json!({ "rich_text": rt("b") }),
                    ),
                    num("n3", PID),
                    blk("m", PID, "paragraph", json!({ "rich_text": mention })),
                    blk("img", PID, "image", json!({ "type": "external" })),
                    blk("cp", PID, "child_page", json!({ "title": "Sub" })),
                ],
            )
            .unwrap();
        store
            .replace_children(
                PID,
                "n2",
                &[
                    num("c1", "n2"),
                    blk(
                        "t",
                        "n2",
                        "to_do",
                        json!({ "rich_text": rt("t"), "checked": true }),
                    ),
                ],
            )
            .unwrap();
        // under a child page: must not be descended into
        store
            .replace_children(PID, "cp", &[num("hidden", "cp")])
            .unwrap();

        let rows = flatten(&store, PID).unwrap();
        let got: Vec<(&str, usize, Marker)> = rows
            .iter()
            .map(|r| (r.block_id.as_str(), r.depth, r.marker.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("n1", 0, Marker::Number(1)),
                ("n2", 0, Marker::Number(2)),
                ("c1", 1, Marker::Number(1)),
                ("t", 1, Marker::Todo(true)),
                ("b", 0, Marker::Bullet),
                ("n3", 0, Marker::Number(1)),
                ("m", 0, Marker::None),
                ("img", 0, Marker::Unsupported("image".into())),
                ("cp", 0, Marker::ChildPage("Sub".into())),
            ]
        );
        let editable = |id: &str| rows.iter().find(|r| r.block_id == id).unwrap().editable;
        assert!(editable("n1"));
        assert!(!editable("m"), "mention is read-only");
        assert!(!editable("img"));
        assert!(!editable("cp"));
        assert_eq!(rows[7].marker.label(), "[image]");
        assert_eq!(rows[3].marker.label(), "☑");
    }

    /// Caches `kids` under `parent` and returns the flattened page.
    fn page_of(store: &Store, parent: &str, kids: &[Block]) -> Vec<Row> {
        store.replace_children(PID, parent, kids).unwrap();
        flatten(store, PID).unwrap()
    }

    fn one(t: &str, payload: Value) -> Row {
        let store = Store::open_in_memory().unwrap();
        let rows = page_of(&store, PID, &[blk("x", PID, t, payload)]);
        assert_eq!(rows.len(), 1);
        rows.into_iter().next().unwrap()
    }

    fn card(kind: &str, url: &str, label: &str) -> Marker {
        Marker::LinkCard {
            kind: kind.into(),
            url: url.into(),
            label: label.into(),
        }
    }

    #[test]
    fn link_cards() {
        let cap = |s: &str| json!({ "caption": rt(s) });
        let mut bm = cap("");
        bm["url"] = json!("https://a.example");
        assert_eq!(
            one("bookmark", bm).marker,
            card("bookmark", "https://a.example", "https://a.example"),
            "label falls back to the url"
        );
        let mut em = cap("Slides");
        em["url"] = json!("https://e.example");
        assert_eq!(
            one("embed", em).marker,
            card("embed", "https://e.example", "Slides")
        );
        assert_eq!(
            one(
                "image",
                json!({ "type": "external", "external": { "url": "https://i.example/x.png" }, "caption": [] })
            )
            .marker,
            card("image", "https://i.example/x.png", "https://i.example/x.png")
        );
        assert_eq!(
            one(
                "image",
                json!({ "type": "file", "file": { "url": "https://s3.example/y.png", "expiry_time": "2025-01-01T01:00:00.000Z" },
                    "caption": rt("Diagram") })
            )
            .marker,
            card("image", "https://s3.example/y.png", "Diagram")
        );
        let pdf = one(
            "pdf",
            json!({ "type": "file", "file": { "url": "https://s3.example/a.pdf" }, "caption": rt("cap"), "name": "notes.pdf" }),
        );
        assert_eq!(
            pdf.marker,
            card("pdf", "https://s3.example/a.pdf", "notes.pdf"),
            "name before caption"
        );
        assert!(!pdf.editable);
        assert_eq!(pdf.marker.label(), "[pdf] notes.pdf");
        assert_eq!(
            one(
                "video",
                json!({ "type": "external", "external": { "url": "https://v.example/v" }, "caption": [] })
            )
            .marker,
            card("video", "https://v.example/v", "https://v.example/v")
        );
        // no url (e.g. a file upload): stays a placeholder
        assert_eq!(
            one(
                "image",
                json!({ "type": "file_upload", "file_upload": { "id": "u" } })
            )
            .marker,
            Marker::Unsupported("image".into())
        );
    }

    #[test]
    fn page_and_database_links() {
        let store = Store::open_in_memory().unwrap();
        let rows = page_of(
            &store,
            PID,
            &[
                blk(
                    "l1",
                    PID,
                    "link_to_page",
                    json!({ "type": "page_id", "page_id": "other" }),
                ),
                blk(
                    "l2",
                    PID,
                    "link_to_page",
                    json!({ "type": "database_id", "database_id": "db" }),
                ),
                blk("cd", PID, "child_database", json!({ "title": "Grades" })),
            ],
        );
        let ms: Vec<_> = rows.iter().map(|r| r.marker.clone()).collect();
        assert_eq!(
            ms,
            vec![
                Marker::LinkToPage {
                    id: "other".into(),
                    title: String::new(),
                    database: false
                },
                Marker::LinkToPage {
                    id: "db".into(),
                    title: String::new(),
                    database: true
                },
                Marker::ChildDatabase("Grades".into()),
            ]
        );
        assert_eq!(rows[0].marker.label(), "→ [page]");
        assert!(rows.iter().all(|r| !r.editable));
    }

    #[test]
    fn synced_block_and_its_subtree_are_locked() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_children(
                PID,
                PID,
                &[
                    blk("s", PID, "synced_block", json!({ "synced_from": null })),
                    blk(
                        "dup",
                        PID,
                        "synced_block",
                        json!({ "synced_from": { "type": "block_id", "block_id": "s" } }),
                    ),
                    blk(
                        "after",
                        PID,
                        "paragraph",
                        json!({ "rich_text": rt("after") }),
                    ),
                ],
            )
            .unwrap();
        let todo = json!({ "rich_text": rt("t"), "checked": false });
        store
            .replace_children(
                PID,
                "s",
                &[
                    blk("t", "s", "to_do", todo.clone()),
                    blk("tg", "s", "toggle", json!({ "rich_text": rt("tg") })),
                ],
            )
            .unwrap();
        store
            .replace_children(
                PID,
                "tg",
                &[blk(
                    "deep",
                    "tg",
                    "paragraph",
                    json!({ "rich_text": rt("d") }),
                )],
            )
            .unwrap();
        store
            .replace_children(PID, "dup", &[blk("t2", "dup", "to_do", todo)])
            .unwrap();
        let rows2 = flatten(&store, PID).unwrap();
        let got: Vec<(&str, usize, bool, bool)> = rows2
            .iter()
            .map(|r| (r.block_id.as_str(), r.depth, r.editable, r.locked))
            .collect();
        assert_eq!(
            got,
            vec![
                ("s", 0, false, true),
                ("t", 1, false, true),
                ("tg", 1, false, true),
                ("deep", 2, false, true),
                ("dup", 0, false, true),
                ("t2", 1, false, true),
                ("after", 0, true, false),
            ]
        );
        assert_eq!(rows2[0].marker, Marker::Synced);
        assert_eq!(rows2[0].marker.label(), "⟳ synced");
        assert_eq!(rows2[1].marker, Marker::Todo(false));
    }

    #[test]
    fn columns_nest_editable_children() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_children(PID, PID, &[blk("cl", PID, "column_list", json!({}))])
            .unwrap();
        store
            .replace_children(
                PID,
                "cl",
                &[
                    blk("c1", "cl", "column", json!({})),
                    blk("c2", "cl", "column", json!({})),
                ],
            )
            .unwrap();
        store
            .replace_children(
                PID,
                "c1",
                &[blk("a", "c1", "paragraph", json!({ "rich_text": rt("a") }))],
            )
            .unwrap();
        let rows = page_of(
            &store,
            "c2",
            &[blk(
                "b",
                "c2",
                "bulleted_list_item",
                json!({ "rich_text": rt("b") }),
            )],
        );
        let got: Vec<(&str, usize, Marker, bool)> = rows
            .iter()
            .map(|r| (r.block_id.as_str(), r.depth, r.marker.clone(), r.editable))
            .collect();
        assert_eq!(
            got,
            vec![
                ("cl", 0, Marker::ColumnList, false),
                ("c1", 1, Marker::Column, false),
                ("a", 2, Marker::None, true),
                ("c2", 1, Marker::Column, false),
                ("b", 2, Marker::Bullet, true),
            ]
        );
        assert_eq!(outdent_target(&rows, 2), None, "out of a column: refused");
        assert_eq!(indent_target(&rows, 4), None);
        assert!(unmovable_in_subtree(&rows, 0).is_some());
    }

    #[test]
    fn toc_and_breadcrumb() {
        let store = Store::open_in_memory().unwrap();
        let rows = page_of(
            &store,
            PID,
            &[
                blk("bc", PID, "breadcrumb", json!({})),
                blk(
                    "toc",
                    PID,
                    "table_of_contents",
                    json!({ "color": "default" }),
                ),
                blk("h1", PID, "heading_1", json!({ "rich_text": rt("Intro") })),
                blk("h2", PID, "heading_2", json!({ "rich_text": rt("Part") })),
            ],
        );
        assert_eq!(rows[0].marker, Marker::Breadcrumb);
        assert_eq!(rows[0].marker.label(), "[breadcrumb]");
        assert_eq!(rows[1].marker, Marker::TableOfContents);
        assert_eq!(plain_text(&rows[1].rich_text), "Intro\n  Part");
        assert!(!rows[0].editable && !rows[1].editable);
    }

    fn trow(id: &str, cells: Value) -> Block {
        blk(id, "tbl", "table_row", json!({ "cells": cells }))
    }

    #[test]
    fn table_is_one_row() {
        let store = Store::open_in_memory().unwrap();
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
            "plain_text": "d", "annotations": {} }]);
        store
            .replace_children(
                PID,
                PID,
                &[
                    blk(
                        "tbl",
                        PID,
                        "table",
                        json!({ "table_width": 2, "has_column_header": true, "has_row_header": false }),
                    ),
                    blk("after", PID, "paragraph", json!({ "rich_text": rt("x") })),
                ],
            )
            .unwrap();
        let rows = page_of(
            &store,
            "tbl",
            &[
                trow("r1", json!([rt("Name"), rt("Grade")])),
                trow("r2", json!([rt("Anna"), mention])),
            ],
        );
        assert_eq!(
            rows.iter().map(|r| r.block_id.as_str()).collect::<Vec<_>>(),
            ["tbl", "after"],
            "no table_row rows"
        );
        let Marker::Table(t) = &rows[0].marker else {
            panic!("{:?}", rows[0].marker)
        };
        assert!(t.column_header && !t.row_header);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1].id, "r2");
        assert_eq!(plain_text(&t.rows[0].cells[1].rich_text), "Grade");
        let editable: Vec<Vec<bool>> = t
            .rows
            .iter()
            .map(|r| r.cells.iter().map(|c| c.editable).collect())
            .collect();
        assert_eq!(
            editable,
            vec![vec![true, true], vec![true, false]],
            "mention"
        );
        assert!(!rows[0].editable);
    }

    #[test]
    fn table_in_synced_block_is_read_only() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_children(
                PID,
                PID,
                &[blk(
                    "s",
                    PID,
                    "synced_block",
                    json!({ "synced_from": null }),
                )],
            )
            .unwrap();
        store
            .replace_children(
                PID,
                "s",
                &[blk(
                    "tbl",
                    "s",
                    "table",
                    json!({ "table_width": 1, "has_column_header": false, "has_row_header": true }),
                )],
            )
            .unwrap();
        let rows = page_of(&store, "tbl", &[trow("r1", json!([rt("a")]))]);
        let Marker::Table(t) = &rows[1].marker else {
            panic!()
        };
        assert!(t.row_header);
        assert!(!t.rows[0].cells[0].editable);
        assert!(rows[1].locked);
    }

    #[test]
    fn unknown_type_stays_a_placeholder() {
        let r = one("ai_block", json!({}));
        assert_eq!(r.marker, Marker::Unsupported("ai_block".into()));
        assert_eq!(r.marker.label(), "[ai_block]");
        assert!(!r.editable);
        let r = one("unsupported", json!({}));
        assert_eq!(r.marker, Marker::Unsupported("unsupported".into()));
    }

    #[test]
    fn toggles_collapse_their_subtree() {
        // t, t/x, t/u (toggle), t/u/y, t/z, after, h (toggleable heading), h/k
        let mut h = r("h", PID, 0, Marker::Heading(1));
        h.toggle = true;
        let rows = vec![
            r("t", PID, 0, Marker::Toggle),
            r("x", "t", 1, Marker::None),
            r("u", "t", 1, Marker::Toggle),
            r("y", "u", 2, Marker::None),
            r("z", "t", 1, Marker::None),
            r("after", PID, 0, Marker::None),
            h,
            r("k", "h", 1, Marker::None),
        ];
        let open = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let vis = |o: &[&str]| -> Vec<&str> {
            let sh = shown(&rows, &open(o));
            rows.iter()
                .zip(sh)
                .filter(|(_, s)| *s)
                .map(|(r, _)| r.block_id.as_str())
                .collect()
        };
        assert_eq!(vis(&[]), ["t", "after", "h"], "all start collapsed");
        assert_eq!(vis(&["u"]), ["t", "after", "h"], "inner open, outer shut");
        assert_eq!(vis(&["t"]), ["t", "x", "u", "z", "after", "h"]);
        assert_eq!(
            vis(&["t", "u", "h"]),
            ["t", "x", "u", "y", "z", "after", "h", "k"]
        );
        // revealing y means opening u and t
        assert_eq!(ancestors(&rows, 3), ["u", "t"]);
        assert_eq!(
            vis(&ancestors(&rows, 3)),
            ["t", "x", "u", "y", "z", "after", "h"]
        );
        assert!(ancestors(&rows, 5).is_empty());
    }

    #[test]
    fn list_labels_by_depth() {
        assert_eq!(
            [bullet(0), bullet(1), bullet(2), bullet(3)],
            ["•", "◦", "▪", "•"]
        );
        assert_eq!(number(3, 0), "3.");
        assert_eq!(number(1, 1), "a.");
        assert_eq!(number(26, 1), "z.");
        assert_eq!(number(28, 1), "ab.");
        assert_eq!(number(4, 2), "iv.");
        assert_eq!(number(14, 2), "xiv.");
        assert_eq!(number(2, 3), "2.");

        // levels count same-kind list ancestors; other blocks reset them
        let store = Store::open_in_memory().unwrap();
        let b =
            |id: &str, p: &str| blk(id, p, "bulleted_list_item", json!({ "rich_text": rt(id) }));
        store
            .replace_children(PID, PID, &[b("b0", PID), num("n0", PID)])
            .unwrap();
        store.replace_children(PID, "b0", &[b("b1", "b0")]).unwrap();
        store
            .replace_children(PID, "b1", &[b("b2", "b1"), num("n1", "b1")])
            .unwrap();
        store
            .replace_children(PID, "n0", &[num("n01", "n0")])
            .unwrap();
        store
            .replace_children(
                PID,
                "n01",
                &[blk("t", "n01", "toggle", json!({ "rich_text": rt("t") }))],
            )
            .unwrap();
        let rows = page_of(&store, "t", &[b("tb", "t")]);
        let got: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| (r.block_id.as_str(), r.level))
            .collect();
        assert_eq!(
            got,
            [
                ("b0", 0),
                ("b1", 1),
                ("b2", 2),
                ("n1", 0),
                ("n0", 0),
                ("n01", 1),
                ("t", 0),
                ("tb", 0)
            ]
        );
        assert!(rows.iter().find(|r| r.block_id == "t").unwrap().toggle);
    }

    #[test]
    fn callout_icon_and_toggleable_headings() {
        let c = one(
            "callout",
            json!({ "rich_text": rt("c"), "icon": { "type": "emoji", "emoji": "⭐" } }),
        );
        assert_eq!(c.marker, Marker::Callout("⭐".into()));
        let c = one("callout", json!({ "rich_text": rt("c"), "icon": null }));
        assert_eq!(c.marker, Marker::Callout("💡".into()));
        assert!(
            one(
                "heading_2",
                json!({ "rich_text": rt("h"), "is_toggleable": true })
            )
            .toggle
        );
        assert!(!one("heading_2", json!({ "rich_text": rt("h") })).toggle);
        assert!(!c.toggle);
    }

    #[test]
    fn column_ranges() {
        // before, cl, c1, a, a/a1, c2, b, after
        let rows = vec![
            r("before", PID, 0, Marker::None),
            r("cl", PID, 0, Marker::ColumnList),
            r("c1", "cl", 1, Marker::Column),
            r("a", "c1", 2, Marker::None),
            r("a1", "a", 3, Marker::None),
            r("c2", "cl", 1, Marker::Column),
            r("b", "c2", 2, Marker::None),
            r("after", PID, 0, Marker::None),
        ];
        assert_eq!(columns(&rows, 1), [3..5, 6..7]);
        // an empty column is an empty range
        let rows = &rows[..3];
        assert_eq!(columns(rows, 1), vec![(3..3)]);
    }
}
