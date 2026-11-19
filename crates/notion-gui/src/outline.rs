//! Flattened, read-only outline of a cached page. No iced types here.

use notion_core::markdown::is_markdown_editable;
use notion_core::model::{BlockKind, RichText};
use notion_core::store::Store;
use notion_core::Result;

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
    Callout,
    Code(String),
    Divider,
    ChildPage(String),
    ChildDatabase(String),
    /// Any other block type, shown as a grey `[type]`.
    Unsupported(String),
}

impl Marker {
    /// Text shown before the row's content ("" for none).
    pub fn label(&self) -> String {
        match self {
            Marker::None | Marker::Heading(_) => String::new(),
            Marker::Bullet => "•".into(),
            Marker::Number(n) => format!("{n}."),
            Marker::Todo(false) => "☐".into(),
            Marker::Todo(true) => "☑".into(),
            Marker::Toggle => "▸".into(),
            Marker::Quote => "│".into(),
            Marker::Callout => "!".into(),
            Marker::Code(lang) => format!("```{lang}"),
            Marker::Divider => "───".into(),
            Marker::ChildPage(t) => format!("[page: {t}]"),
            Marker::ChildDatabase(t) => format!("[database: {t}]"),
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
            | Marker::Callout
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

/// Shift+Tab: (new parent = grandparent, after = parent). None at top level.
pub fn outdent_target(rows: &[Row], idx: usize) -> Option<(String, Option<String>)> {
    let d = rows[idx].depth.checked_sub(1)?;
    let parent = &rows[..idx].iter().rev().find(|r| r.depth == d)?;
    Some((parent.parent.clone(), Some(parent.block_id.clone())))
}

/// First block in the subtree that push's copy-then-delete can't recreate.
pub fn unmovable_in_subtree(rows: &[Row], idx: usize) -> Option<&Marker> {
    subtree(rows, idx).iter().map(|r| &r.marker).find(|m| {
        matches!(
            m,
            Marker::Unsupported(_) | Marker::ChildPage(_) | Marker::ChildDatabase(_)
        )
    })
}

/// Every cached block under `page_id`, depth-first in order. Child pages and
/// databases are leaves.
pub fn flatten(store: &Store, page_id: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    walk(store, page_id, 0, &mut rows)?;
    Ok(rows)
}

fn walk(store: &Store, parent: &str, depth: usize, rows: &mut Vec<Row>) -> Result<()> {
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
            BlockKind::Callout(_) => Marker::Callout,
            BlockKind::Code { language, .. } => Marker::Code(language.clone()),
            BlockKind::Divider => Marker::Divider,
            BlockKind::ChildPage { title } => Marker::ChildPage(title.clone()),
            BlockKind::ChildDatabase { title } => Marker::ChildDatabase(title.clone()),
            BlockKind::Unsupported { .. } => Marker::Unsupported(b.kind.type_name().into()),
        };
        let rich_text = b
            .kind
            .text()
            .map(|t| t.rich_text.clone())
            .unwrap_or_default();
        let editable = b.kind.text().is_some() && is_markdown_editable(&rich_text);
        let leaf = matches!(
            b.kind,
            BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
        );
        rows.push(Row {
            block_id: b.id.clone(),
            parent: parent.to_string(),
            depth,
            marker,
            rich_text,
            editable,
        });
        if !leaf {
            walk(store, &b.id, depth + 1, rows)?;
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
            block_id: id.into(),
            parent: parent.into(),
            depth,
            marker,
            rich_text: Vec::new(),
            editable: true,
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
        let mention = json!([{ "type": "mention", "mention": { "type": "date", "date": { "start": "2025-01-01" } },
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
}
