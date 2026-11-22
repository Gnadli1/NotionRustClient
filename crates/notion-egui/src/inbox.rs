//! C5: the Updates inbox, a modal list in the style of the Trash view. It
//! shows what `notion_core::activity` recorded: new comments, mentions of you
//! and pages others edited. Up/Down select, Enter opens the page (and the
//! comment panel for a comment), Esc closes, Ctrl+Shift+A marks all read.

use eframe::egui::{self, Align2, Context, Id, Key, Modal, Modifiers, RichText, Vec2};
use notion_core::activity::{Entry, COMMENT, MENTION};

use crate::app::App;
use crate::keys::take_exact;
use crate::theme::Colors;

pub(crate) const NO_PERSON: &str = "Set NOTION_USER_ID to your Notion user id to enable Updates";

/// The open view: the list is loaded once on open and after read changes.
pub(crate) struct Inbox {
    pub sel: usize,
    /// Newest first, with the page's title resolved.
    list: Vec<(Entry, String)>,
}

fn kind_label(k: &str) -> &'static str {
    match k {
        COMMENT => "💬 comment",
        MENTION => "@ mention",
        _ => "✎ edited",
    }
}

impl App {
    /// Reloads the unread count the sidebar shows, and the open list.
    pub(crate) fn refresh_unread(&mut self) {
        self.unread = self.store.unread_count().unwrap_or(0);
        self.no_person = self.store.person().ok().flatten().is_none();
        if let Some(sel) = self.inbox.as_ref().map(|i| i.sel) {
            self.inbox = Some(self.load_inbox(sel));
        }
    }

    fn load_inbox(&self, sel: usize) -> Inbox {
        let mut list = self.store.activity().unwrap_or_default();
        list.truncate(200);
        let list = list
            .into_iter()
            .map(|e| {
                let t = self.store.get_page(&e.page_id).ok().flatten().map_or_else(String::new, |p| p.title());
                let t = crate::view::untitled(&t).to_string();
                (e, t)
            })
            .collect();
        Inbox { sel, list }
    }

    pub(crate) fn toggle_inbox(&mut self) {
        if self.inbox.take().is_none() && self.commit() {
            self.refresh_unread();
            self.inbox = Some(self.load_inbox(0));
        }
    }

    pub(crate) fn mark_all_read(&mut self) {
        if let Err(e) = self.store.mark_all_read() {
            self.last_error = Some(e.to_string());
        }
        self.refresh_unread();
    }

    /// The open page is being looked at: its entries are read. A failure is
    /// only reported, it must not stop the page from loading.
    pub(crate) fn mark_open_page_read(&mut self) {
        let Some(id) = self.current.clone() else { return };
        if let Err(e) = self.store.mark_page_read(&id) {
            self.last_error = Some(e.to_string());
        }
        self.refresh_unread();
    }

    /// Opens the entry's page, and for a comment the panel on what it hangs off.
    pub(crate) fn open_entry(&mut self, e: &Entry) {
        let _ = self.store.mark_entry_read(&e.kind, &e.target);
        self.inbox = None;
        self.open_or_pull(&e.page_id);
        if e.kind != notion_core::activity::PAGE && self.current.as_deref() == Some(e.page_id.as_str()) {
            self.open_comments((e.parent_id != e.page_id).then(|| e.parent_id.clone()));
        }
        self.refresh_unread();
    }

    /// The inbox's keys, taken before any widget sees them.
    pub(crate) fn inbox_keys(&mut self, ctx: &Context) {
        let Some(ib) = self.inbox.as_mut() else { return };
        let (esc, u, d, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                // one Enter per press: a held key must not open entry after entry
                take_exact(&mut i.events, Modifiers::NONE, Key::Enter).0 > 0,
            )
        });
        if esc {
            self.inbox = None;
            return;
        }
        ib.sel = (ib.sel as isize + d as isize - u as isize).clamp(0, ib.list.len().max(1) as isize - 1) as usize;
        if let Some(e) = ib.list.get(ib.sel).map(|(e, _)| e.clone()).filter(|_| enter) {
            self.open_entry(&e);
        }
    }

    pub(crate) fn inbox_ui(&mut self, ctx: &Context, c: &Colors) {
        let Some(ib) = self.inbox.as_ref() else { return };
        let (sel, list, hint) = (ib.sel, &ib.list, self.no_person);
        let now = crate::comments::now_secs();
        let (mut clicked, mut all) = (None, false);
        let id = Id::unique("inbox");
        let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 100.0));
        let modal = Modal::new(id).area(area).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(
                RichText::new("Updates: Up/Down, Enter opens, Ctrl+Shift+A marks all read, Esc closes")
                    .small()
                    .color(c.secondary),
            );
            if hint {
                ui.label(RichText::new(NO_PERSON).small().color(c.secondary));
            } else if list.is_empty() {
                ui.label("Nothing new yet: comments and edits by others show up here as they are pulled.");
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for (i, (e, page)) in list.iter().enumerate() {
                    let who = crate::people::name_of(&self.users, &e.author);
                    let mut text = format!(
                        "{} {} · {who} · {page} · {}",
                        if e.unread { "●" } else { "○" },
                        kind_label(&e.kind),
                        crate::comments::ago(e.at.timestamp(), now),
                    );
                    if !e.summary.is_empty() {
                        text.push_str(&format!("\n    {}", e.summary.replace('\n', " ")));
                    }
                    let rt = if e.unread { RichText::new(text).strong() } else { RichText::new(text).color(c.secondary) };
                    if ui.selectable_label(i == sel, rt).clicked() {
                        clicked = Some(e.clone());
                    }
                }
            });
            all = ui.button("Mark all read").clicked();
        });
        if all {
            self.mark_all_read();
        }
        if let Some(e) = clicked {
            self.open_entry(&e);
        } else if modal.backdrop_response.clicked() {
            self.inbox = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Win;
    use notion_core::model::Comment;
    use serde_json::json;

    const CS: Modifiers = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };

    fn comment(id: &str, by: &str) -> Comment {
        serde_json::from_value(json!({
            "object": "comment", "id": id, "discussion_id": "d", "parent": {"type": "page_id", "page_id": "p"},
            "created_time": "2099-01-01T00:00:00.000Z", "last_edited_time": "2099-01-01T00:00:00.000Z",
            "created_by": {"object": "user", "id": by},
            "rich_text": [{"type": "text", "text": {"content": "hi there"}, "plain_text": "hi there", "annotations": {}}],
        }))
        .unwrap()
    }

    #[test]
    fn the_inbox_lists_updates_enter_opens_and_ctrl_shift_a_marks_all_read() {
        let mut w = Win::with_texts(&["a"]);
        w.app.store.set_identity("bot", None, Some("me")).unwrap();
        w.app.store.note_comments("p", &[comment("c1", "u1"), comment("c2", "u1"), comment("mine", "me")]).unwrap();
        w.app.refresh();
        assert_eq!(w.app.unread, 0, "the open page's entries are read as it is viewed");
        w.app.store.note_comments("p", &[comment("c3", "u1")]).unwrap();
        w.app.refresh_unread();
        assert_eq!(w.app.unread, 1);
        w.press(Key::J, CS);
        w.frames(vec![]);
        assert!(w.app.inbox.is_some());
        assert!(w.shown.contains("Updates") && w.shown.contains("hi there"), "{}", w.shown);
        w.press(Key::A, CS);
        assert_eq!(w.app.unread, 0);
        assert_eq!(w.app.store.activity().unwrap().len(), 3, "read entries stay listed");
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.inbox.is_none());
        assert!(w.app.comments.is_some(), "a comment entry opens the thread panel");
        // Esc closes the view; the same chord reopens it
        w.press(Key::J, CS);
        assert!(w.app.inbox.is_some());
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.inbox.is_none());
    }

    #[test]
    fn an_open_inbox_keeps_the_keys_from_the_selected_block_and_never_reads_while_unfocused() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.app.store.set_identity("bot", None, Some("me")).unwrap();
        w.app.store.note_comments("p", &[comment("c1", "u1")]).unwrap();
        w.app.refresh_unread();
        assert_eq!(w.app.unread, 1);
        w.app.was_focused = false;
        w.app.refresh();
        assert_eq!(w.app.unread, 1, "an unfocused window doesn't read the page");
        w.app.mark_open_page_read();
        assert_eq!(w.app.unread, 0);
        w.app.store.note_comments("p", &[comment("c2", "u1")]).unwrap();
        w.app.sel = Some(("p-0".into(), "p-0".into()));
        w.press(Key::J, CS);
        assert!(w.app.inbox.is_some());
        let n = w.app.rows.len();
        w.press(Key::Delete, Modifiers::NONE);
        assert_eq!(w.app.rows.len(), n, "Delete didn't reach the page");
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.inbox.is_none(), "the inbox took Enter");
        assert_eq!(w.app.rows.len(), n);
    }

    #[test]
    fn without_a_person_identity_the_hint_shows_and_nothing_is_recorded() {
        let mut w = Win::with_texts(&["a"]);
        w.app.store.set_identity("bot", None, None).unwrap();
        w.app.store.note_comments("p", &[comment("c1", "u1")]).unwrap();
        w.app.refresh_unread();
        assert!(w.app.no_person && w.app.unread == 0);
        w.press(Key::J, CS);
        w.frames(vec![]);
        assert!(w.shown.contains(NO_PERSON), "{}", w.shown);
    }
}
