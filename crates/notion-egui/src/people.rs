//! People (C2): names and initials avatars for the cached workspace users,
//! and the "edited by" lines. Avatar *images* are not downloaded (F7 does
//! image loading); the user's `avatar_url` is cached but unused here.

use eframe::egui::{self, Color32, FontId, Sense, Ui};
use notion_core::model::User;

use crate::comments::ago;

/// Notion's nine text colours, dark enough for white initials.
const PALETTE: [Color32; 9] = [
    Color32::from_rgb(0x78, 0x77, 0x74), // gray
    Color32::from_rgb(0x9F, 0x6B, 0x53), // brown
    Color32::from_rgb(0xD9, 0x73, 0x0D), // orange
    Color32::from_rgb(0xCB, 0x91, 0x2F), // yellow
    Color32::from_rgb(0x44, 0x83, 0x61), // green
    Color32::from_rgb(0x33, 0x7E, 0xA9), // blue
    Color32::from_rgb(0x90, 0x65, 0xB0), // purple
    Color32::from_rgb(0xC1, 0x4C, 0x8A), // pink
    Color32::from_rgb(0xD4, 0x4C, 0x47), // red
];

/// The cached user `id`, if any.
pub fn find<'a>(users: &'a [User], id: &str) -> Option<&'a User> {
    users.iter().find(|u| u.id == id)
}

/// The cached name of `id`, else its first 8 characters.
pub fn name_of(users: &[User], id: &str) -> String {
    match find(users, id).filter(|u| !u.name.is_empty()) {
        Some(u) => u.name.clone(),
        None => id.chars().take(8).collect(),
    }
}

/// The avatar's circle colour: FNV-1a over the id, so it is the same on every
/// run and Rust release (`DefaultHasher` promises neither).
pub fn avatar_color(id: &str) -> Color32 {
    let h = id.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    PALETTE[h as usize % PALETTE.len()]
}

/// Up to two capitals: first letters of the first two words. `?` for no name.
pub fn initials(name: &str) -> String {
    let s: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if s.is_empty() {
        "?".into()
    } else {
        s
    }
}

/// A round avatar for user `id` shown as `name` (see [`name_of`]).
pub fn avatar(ui: &mut Ui, id: &str, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, avatar_color(id));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials(name),
        FontId::proportional(size * 0.45),
        Color32::WHITE,
    );
}

/// Page header: "Edited by Ada · 5 min ago" (`who` None: no editor on record).
pub fn edited_header(who: Option<&str>, then: i64, now: i64) -> String {
    match who {
        Some(w) => format!("Edited by {w} · {}", ago(then, now)),
        None => format!("Edited {}", ago(then, now)),
    }
}

/// Block tooltip: "Edited by Ada, 5 min ago".
pub fn edited_tip(who: Option<&str>, then: i64, now: i64) -> String {
    match who {
        Some(w) => format!("Edited by {w}, {}", ago(then, now)),
        None => format!("Edited {}", ago(then, now)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Id, Key, Modifiers};
    use notion_core::model::{Block, Page};
    use serde_json::json;

    use crate::testkit::Win;

    pub fn user(id: &str, name: &str) -> User {
        User::from_value(&json!({ "object": "user", "id": id, "type": "person", "name": name })).unwrap()
    }

    #[test]
    fn names_fall_back_to_the_id_prefix() {
        let us = [user("abcdef0123456789", "Ada Lovelace"), user("nameless-user-id", "")];
        assert_eq!(name_of(&us, "abcdef0123456789"), "Ada Lovelace");
        assert_eq!(name_of(&us, "nameless-user-id"), "nameless");
        assert_eq!(name_of(&us, "zzzzzzzzzzzz"), "zzzzzzzz");
    }

    #[test]
    fn avatar_colour_is_stable_per_id() {
        // pinned: a change of hash or palette reshuffles every user's colour
        assert_eq!(avatar_color("u1"), avatar_color("u1"));
        assert_eq!(avatar_color("u1"), PALETTE[0]);
        assert_eq!(avatar_color("u2"), PALETTE[8]);
        assert_eq!(avatar_color("bot"), PALETTE[5]);
        let distinct: std::collections::HashSet<_> =
            (0..40).map(|i| avatar_color(&format!("user-{i}"))).collect();
        assert!(distinct.len() >= 5, "ids spread over the palette: {}", distinct.len());
    }

    #[test]
    fn initials_are_one_or_two_capitals() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("bob"), "B");
        assert_eq!(initials("  ada   b  c "), "AB");
        assert_eq!(initials("Émile Zola"), "ÉZ");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn edited_lines() {
        assert_eq!(edited_header(Some("Ada"), 0, 300), "Edited by Ada · 5 min ago");
        assert_eq!(edited_tip(Some("Ada"), 0, 300), "Edited by Ada, 5 min ago");
        assert_eq!(edited_header(None, 0, 3 * 3600), "Edited 3 h ago");
    }

    /// Long ago, so the lines end in "N d ago".
    fn now_iso() -> String {
        "2025-01-01T00:00:00.000Z".into()
    }

    /// Window on page "p" (blocks p-0, p-1) edited by user `by`, who is cached as Ada.
    fn edited_by_ada(by: &str) -> Win {
        let mut w = Win::with_texts(&["a", "b"]);
        let store = &w.app.store;
        store.put_users(&[user("u1", "Ada")]).unwrap();
        let mut p = serde_json::to_value(store.get_page("p").unwrap().unwrap()).unwrap();
        p["last_edited_by"] = json!({ "object": "user", "id": by });
        p["last_edited_time"] = json!(now_iso());
        store.upsert_page(&serde_json::from_value::<Page>(p).unwrap()).unwrap();
        let mut b = serde_json::to_value(store.get_block("p-0").unwrap().unwrap()).unwrap();
        b["last_edited_by"] = json!({ "object": "user", "id": by });
        b["last_edited_time"] = json!(now_iso());
        store.replace_children("p", "p", &[
            serde_json::from_value::<Block>(b).unwrap(),
            store.get_block("p-1").unwrap().unwrap(),
        ]).unwrap();
        w.app.refresh();
        w.frames(vec![]);
        w
    }

    #[test]
    fn the_page_header_names_the_last_editor() {
        let mut w = edited_by_ada("u1");
        assert!(w.shown.contains("Edited by Ada · ") && w.shown.contains(" d ago"), "{}", w.shown);
        let (then, now) = (w.app.page_edit.as_ref().unwrap().1, 300);
        assert_eq!(
            w.app.page_edited(then + now).unwrap().1,
            "Edited by Ada · 5 min ago"
        );
        // an editor that isn't cached shows the id's first 8 characters
        let w = edited_by_ada("0123456789abcdef");
        assert!(w.shown.contains("Edited by 01234567 · "), "{}", w.shown);
        // no editor on record: just the time
        let mut w = edited_by_ada("u1");
        let mut p = serde_json::to_value(w.app.store.get_page("p").unwrap().unwrap()).unwrap();
        p.as_object_mut().unwrap().remove("last_edited_by");
        w.app.store.upsert_page(&serde_json::from_value::<Page>(p).unwrap()).unwrap();
        w.app.refresh();
        w.frames(vec![]);
        assert!(w.shown.contains("Edited ") && !w.shown.contains("Edited by"), "{}", w.shown);
    }

    #[test]
    fn a_block_tooltip_names_the_editor_and_unknown_ids_show_their_prefix() {
        let w = edited_by_ada("u1");
        let then = w.app.store.get_block("p-0").unwrap().unwrap().last_edited_time.timestamp();
        assert_eq!(w.app.block_edited_tip("p-0", then + 3 * 3600).unwrap(), "Edited by Ada, 3 h ago");
        let w = edited_by_ada("0123456789abcdef");
        assert!(w.app.block_edited_tip("p-0", crate::comments::now_secs()).unwrap().starts_with("Edited by 01234567, "));
        // a block without a recorded editor, and an unpushed one
        let t = w.app.block_edited_tip("p-1", crate::comments::now_secs()).unwrap();
        assert!(t.starts_with("Edited ") && !t.contains(" by "), "{t}");
        assert_eq!(w.app.block_edited_tip("local-1", 0), None);
    }

    #[test]
    fn hovering_a_row_shows_the_edited_by_tooltip() {
        let mut w = edited_by_ada("u1");
        // the tooltip comes at once, not after egui's delay
        w.ctx.all_styles_mut(|s| s.interaction.tooltip_delay = 0.0);
        let id = Id::unique(("row-edited", "p-0"));
        // the hover area only exists while the pointer is inside the row: find it
        let mut pos = None;
        for y in (60..600).step_by(10) {
            let p = egui::pos2(630.0, y as f32);
            w.frames(vec![Event::PointerMoved(p)]);
            if w.ctx.read_response(id).is_some() {
                pos = Some(p);
                break;
            }
        }
        let pos = pos.expect("the row has its hover area");
        // arrive afresh: a pointer swept across rows has used up egui's tooltip
        w.frames(vec![Event::PointerGone]);
        w.frames(vec![Event::PointerMoved(pos)]);
        w.frames(vec![]);
        assert!(w.ctx.read_response(id).is_some_and(|r| r.hovered()));
        assert!(w.shown.contains("Edited by Ada, "), "{}", w.shown);
        // not while the row is being edited
        w.edit(0);
        w.frames(vec![Event::PointerMoved(pos)]);
        w.frames(vec![]);
        assert!(!w.shown.contains("Edited by Ada, "), "{}", w.shown);
    }

    fn comment_by(by: &str) -> notion_core::model::Comment {
        let t = "2025-01-01T00:00:00.000Z";
        serde_json::from_value(json!({
            "object": "comment", "id": "c1", "discussion_id": "d1",
            "parent": {"type": "page_id", "page_id": "p"},
            "created_time": t, "last_edited_time": t,
            "created_by": {"object": "user", "id": by},
            "rich_text": [{"type": "text", "text": {"content": "hi"}, "plain_text": "hi", "annotations": {}}],
        }))
        .unwrap()
    }

    #[test]
    fn a_comment_shows_its_authors_name_and_you_for_the_bot() {
        let mut w = Win::with_texts(&["a"]);
        w.app.store.put_users(&[user("u1", "Ada Lovelace")]).unwrap();
        w.app.store.put_comment(&comment_by("u1")).unwrap();
        w.app.refresh();
        w.press(Key::M, Modifiers::CTRL | Modifiers::SHIFT);
        assert!(w.shown.contains("Ada Lovelace · "), "{}", w.shown);
        assert!(!w.shown.contains("u1 ·"), "the id is gone: {}", w.shown);
        // not cached: the id's first 8 characters, as in C1
        let mut c = comment_by("0123456789abcdef");
        c.id = "c2".into();
        w.app.store.put_comment(&c).unwrap();
        // our own: "you"
        let mut c = comment_by("bot");
        c.id = "c3".into();
        w.app.store.put_comment(&c).unwrap();
        w.app.me = Some("bot".into());
        w.app.refresh();
        w.frames(vec![]);
        assert!(w.shown.contains("01234567 · "), "{}", w.shown);
        assert!(w.shown.contains("you · "), "{}", w.shown);
    }
}
