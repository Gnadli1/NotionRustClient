//! Comments (C1): a right-hand panel with the page's threads and the focused
//! row's, a reply box per thread and a box for a new comment. Comments are
//! written through core like block edits: cached at once, queued, pushed
//! later; their ops join the undo steps.
//!
//! Boxes are plain Markdown (`from_markdown`), so a mention is typed as its
//! `@[label](kind:id)` token; the E3 `@` menu is tied to the row editor.
//! Notion has no "has comments" flag on blocks and no page-wide listing, so a
//! block's threads are fetched when the panel shows that block (see
//! `sync::pull_page`); a row's 💬 count is what is cached.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align, Frame, Id, Key, Layout, Margin, Modifiers, RichText, ScrollArea, Sense, TextEdit,
    Ui,
};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, Comment, User};
use notion_core::ops::CommentTarget;
use notion_core::sync;

use crate::app::{Action, App, Grab};
use crate::keys;
use crate::md;
use crate::theme::Colors;
use crate::worker::Cmd;

pub(crate) struct Panel {
    /// The row whose threads show under the page's.
    pub block: Option<String>,
    /// The row to go on editing when Esc leaves the panel.
    back: Option<String>,
    /// The new-comment draft, and whether it goes to the page even with a row shown.
    pub new_text: String,
    pub on_page: bool,
    /// Reply drafts by discussion id.
    pub replies: HashMap<String, String>,
    /// The comment being edited and its text; `focus_edit` asks its box for the keyboard.
    pub edit: Option<(String, String)>,
    focus_edit: bool,
    /// Ask the new-comment box for the keyboard on the next frame.
    focus_new: bool,
    /// Cached from the store by `App::refresh`.
    pub page_comments: Vec<Comment>,
    pub block_comments: Vec<Comment>,
    /// Ids already fetched since the panel opened.
    asked: HashSet<String>,
    /// When the threads were last polled (see `App::poll_comments_at`).
    pub(crate) polled: Option<std::time::Instant>,
}

/// Comments grouped by thread, in order of each thread's first comment.
pub(crate) fn threads(cs: &[Comment]) -> Vec<Vec<&Comment>> {
    let mut out: Vec<Vec<&Comment>> = Vec::new();
    for c in cs {
        match out
            .iter_mut()
            .find(|t| t[0].discussion_id == c.discussion_id)
        {
            Some(t) => t.push(c),
            None => out.push(vec![c]),
        }
    }
    out
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
pub(crate) fn ago(then: i64, now: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..45 => "just now".into(),
        45..5400 => format!("{} min ago", (s + 30) / 60),
        5400..129_600 => format!("{} h ago", (s + 1800) / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// Who wrote `c`: "you" for this integration's own, else the cached name
/// (the id's first 8 characters if the user isn't cached).
fn author(users: &[User], mine: bool, c: &Comment) -> String {
    if mine {
        "you".into()
    } else {
        crate::people::name_of(users, &c.created_by)
    }
}

/// Whether `c` was written by this integration (or is still unpushed): the
/// only comments Notion lets it edit or delete. `created_by` is the
/// integration's bot user, learned from `GET /v1/users/me`.
fn mine(me: Option<&str>, c: &Comment) -> bool {
    c.id.starts_with("local-") || me == Some(c.created_by.as_str())
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn box_id(kind: &str, key: &str) -> Id {
    Id::unique(("comment-box", kind, key))
}

impl App {
    /// Ctrl+Shift+M: opens the panel on the edited row (and the keyboard in
    /// its new-comment box), or closes it.
    pub(crate) fn toggle_comments(&mut self) {
        if self.comments.is_some() {
            self.close_comments(true);
        } else {
            let block = self
                .focus
                .as_ref()
                .filter(|f| f.cell.is_none())
                .map(|f| f.block_id.clone());
            self.open_comments(block);
        }
    }

    /// Opens the panel, on `block` if given. A 💬 click comes here too.
    pub(crate) fn open_comments(&mut self, block: Option<String>) {
        let back = self
            .focus
            .as_ref()
            .filter(|f| f.cell.is_none())
            .map(|f| f.block_id.clone());
        if !self.commit() {
            return;
        }
        let keep = self.comments.take();
        self.comments = Some(Panel {
            block: block.clone(),
            back,
            new_text: keep
                .as_ref()
                .map(|p| p.new_text.clone())
                .unwrap_or_default(),
            on_page: false,
            replies: keep.map(|p| p.replies).unwrap_or_default(),
            edit: None,
            focus_edit: false,
            focus_new: true,
            page_comments: Vec::new(),
            block_comments: Vec::new(),
            asked: HashSet::new(),
            polled: None,
        });
        // the page's threads may be stale too: one request each
        if let Some(p) = self.current.clone() {
            self.fetch_comments(&p);
        }
        if let Some(b) = block {
            self.fetch_comments(&b);
        }
        if self.me.is_none() && !self.me_asked && !self.offline() {
            self.me_asked = true;
            self.send(Cmd::Me);
        }
        self.refresh();
    }

    pub(crate) fn close_comments(&mut self, back: bool) {
        let Some(p) = self.comments.take() else {
            return;
        };
        if back {
            self.edit_row(p.back.as_deref());
        }
    }

    /// Esc in a box: the keyboard goes back to the page, to the row the
    /// panel was opened from if it is still there.
    pub(crate) fn comments_back(&mut self) {
        let back = self.comments.as_mut().and_then(|p| p.back.take());
        self.edit_row(back.as_deref());
    }

    fn edit_row(&mut self, id: Option<&str>) {
        let found = id
            .and_then(|id| {
                self.rows
                    .iter()
                    .position(|r| r.block_id == id && r.editable)
            })
            .filter(|&i| self.shown[i]);
        if let Some(i) = found {
            self.start_edit(i, Grab::End);
        }
    }

    pub(crate) fn offline(&self) -> bool {
        self.last_error
            .as_deref()
            .is_some_and(|e| e.starts_with("offline"))
    }

    /// Asks the worker for the threads of a page or block, once per panel
    /// opening. Unpushed `local-*` blocks have none on Notion yet.
    fn fetch_comments(&mut self, id: &str) {
        if id.starts_with("local-") || self.offline() {
            return;
        }
        if self
            .comments
            .as_mut()
            .is_some_and(|p| p.asked.insert(id.to_string()))
        {
            self.send(Cmd::PullComments { id: id.to_string() });
        }
    }

    /// The panel follows the edited row. Clicking a box ends the row's edit
    /// but the panel keeps its row.
    pub(crate) fn comments_follow_focus(&mut self) {
        let Some(id) = self
            .focus
            .as_ref()
            .filter(|f| f.cell.is_none())
            .map(|f| f.block_id.clone())
        else {
            return;
        };
        if self
            .comments
            .as_ref()
            .is_some_and(|p| p.block.as_ref() != Some(&id))
        {
            if let Some(p) = &mut self.comments {
                p.block = Some(id.clone());
                p.on_page = false;
            }
            self.fetch_comments(&id);
            self.load_comments();
        }
    }

    /// Reloads the panel's lists and the rows' counts from the store.
    pub(crate) fn load_comments(&mut self) {
        let res = (|| -> notion_core::Result<()> {
            self.comment_counts = match &self.current {
                Some(page) => self.store.comment_counts(page)?,
                None => HashMap::new(),
            };
            if let Some(p) = &mut self.comments {
                p.page_comments = match &self.current {
                    Some(page) => self.store.comments_for(page)?,
                    None => Vec::new(),
                };
                p.block_comments = match &p.block {
                    Some(b) => self.store.comments_for(b)?,
                    None => Vec::new(),
                };
            }
            Ok(())
        })();
        if let Err(e) = res {
            self.last_error = Some(e.to_string());
        }
    }

    /// A box's Ctrl+Enter: queues a comment. The draft stays if that fails.
    pub(crate) fn send_comment(&mut self, target: CommentTarget, text: String) {
        let rich = from_markdown(&text);
        if plain_text(&rich).trim().is_empty() && rich.iter().all(|r| r.href.is_none()) {
            return;
        }
        let res = self.step(|s| sync::local_add_comment(&s.store, target.clone(), rich));
        match res {
            Ok(_) => {
                if let Some(p) = &mut self.comments {
                    match &target {
                        CommentTarget::Discussion(d) => {
                            p.replies.remove(d);
                        }
                        _ => p.new_text.clear(),
                    }
                }
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
        self.refresh();
    }

    /// Saves an edited comment (Ctrl+Enter in its box). Paused while a push
    /// runs: the worker may hold the op an edit would rewrite.
    pub(crate) fn save_comment(&mut self, id: String, text: String) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let rich = from_markdown(&text);
        if plain_text(&rich).trim().is_empty() {
            return;
        }
        match self.step(|s| sync::local_edit_comment(&s.store, &id, rich)) {
            Ok(_) => {
                if let Some(p) = &mut self.comments {
                    p.edit = None;
                }
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
        self.refresh();
    }

    pub(crate) fn delete_comment(&mut self, id: String) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        if let Err(e) = self.step(|s| sync::local_delete_comment(&s.store, &id)) {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
    }

    /// The right-hand panel; the caller shows it only while it is open.
    pub(crate) fn comments_ui(&mut self, ui: &mut Ui, c: &Colors) -> Vec<Action> {
        let Some(mut p) = self.comments.take() else {
            return Vec::new();
        };
        let me = self.me.clone();
        let users = &self.users;
        let label = p.block.as_ref().map(|b| {
            let text = self
                .rows
                .iter()
                .find(|r| &r.block_id == b)
                .map(|r| plain_text(&r.rich_text))
                .unwrap_or_default();
            if text.trim().is_empty() {
                "this block".to_string()
            } else {
                text.chars().take(30).collect()
            }
        });
        let title = crate::view::untitled(&self.title).to_string();
        let mut acts = Vec::new();
        let now = now_secs();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Comments").strong().size(15.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    acts.push(Action::CloseComments);
                }
            });
        });
        ui.separator();
        // the new-comment box sits at the bottom, outside the scroll area
        egui::Panel::bottom("comment-new").show(ui, |ui| {
            ui.add_space(4.0);
            if let Some(l) = &label {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("New comment on")
                            .size(12.0)
                            .color(c.secondary),
                    );
                    ui.selectable_value(&mut p.on_page, false, l.as_str());
                    ui.selectable_value(&mut p.on_page, true, "page");
                });
            } else {
                ui.label(
                    RichText::new("New comment on the page")
                        .size(12.0)
                        .color(c.secondary),
                );
            }
            let target = match (&p.block, p.on_page) {
                (Some(b), false) => CommentTarget::Block(b.clone()),
                _ => CommentTarget::Page(self.current.clone().unwrap_or_default()),
            };
            let id = box_id("new", "");
            if std::mem::take(&mut p.focus_new) {
                ui.memory_mut(|m| m.request_focus(id));
            }
            if let Some(a) = draft_box(
                ui,
                id,
                &mut p.new_text,
                "Write a comment…  (Ctrl+Enter sends, Esc leaves)",
            ) {
                acts.push(match a {
                    Draft::Send => Action::CommentSend(target, p.new_text.clone()),
                    Draft::Leave => Action::CommentsBack,
                });
            }
            ui.add_space(4.0);
        });
        let Panel {
            page_comments,
            block_comments,
            replies,
            edit,
            focus_edit,
            ..
        } = &mut p;
        ScrollArea::vertical()
            .id_salt("comments")
            .auto_shrink(false)
            .show(ui, |ui| {
                let mut section = |ui: &mut Ui, head: String, cs: &[Comment]| {
                    ui.label(RichText::new(head).size(12.0).color(c.secondary));
                    if cs.is_empty() {
                        ui.label(RichText::new("No comments yet.").color(c.secondary));
                    }
                    for t in threads(cs) {
                        Frame::new()
                            .stroke(egui::Stroke::new(1.0, c.border))
                            .corner_radius(6.0)
                            .inner_margin(Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for cm in &t {
                                    let mine = mine(me.as_deref(), cm);
                                    comment_ui(
                                        ui,
                                        c,
                                        (cm, mine),
                                        (users, now),
                                        (&mut *edit, &mut *focus_edit),
                                        &mut acts,
                                    );
                                }
                                let disc = &t[0].discussion_id;
                                let draft = replies.entry(disc.clone()).or_default();
                                if let Some(a) =
                                    draft_box(ui, box_id("reply", disc), draft, "Reply…")
                                {
                                    acts.push(match a {
                                        Draft::Send => Action::CommentSend(
                                            CommentTarget::Discussion(disc.clone()),
                                            draft.clone(),
                                        ),
                                        Draft::Leave => Action::CommentsBack,
                                    });
                                }
                            });
                        ui.add_space(6.0);
                    }
                };
                section(ui, format!("Page: {title}"), page_comments);
                if let Some(l) = &label {
                    ui.add_space(8.0);
                    section(ui, format!("Block: {l}"), block_comments);
                }
            });
        self.comments = Some(p);
        acts
    }
}

enum Draft {
    Send,
    Leave,
}

/// A multi-line box: Ctrl+Enter sends, Esc leaves. Both are taken before the
/// TextEdit runs, which would type a newline / drop focus itself.
fn draft_box(ui: &mut Ui, id: Id, text: &mut String, hint: &str) -> Option<Draft> {
    let ctx = ui.ctx().clone();
    let focused = ctx.memory(|m| m.has_focus(id));
    let mut out = None;
    if focused {
        let (send, esc) = ctx.input_mut(|i| {
            (
                keys::take_exact(&mut i.events, Modifiers::CTRL, Key::Enter).0 > 0,
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        out = if send {
            Some(Draft::Send)
        } else if esc {
            ctx.memory_mut(|m| m.surrender_focus(id));
            Some(Draft::Leave)
        } else {
            None
        };
    }
    ui.add(
        TextEdit::multiline(text)
            .id(id)
            .hint_text(hint)
            .desired_rows(1)
            .desired_width(f32::INFINITY)
            // else egui drops the focus on Esc before this code sees the key
            .event_filter(egui::EventFilter {
                tab: false,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            }),
    );
    out
}

/// One comment: author and age, its text (or the edit box), Edit / Delete
/// for the integration's own.
fn comment_ui(
    ui: &mut Ui,
    c: &Colors,
    (cm, mine): (&Comment, bool),
    (users, now): (&[User], i64),
    (edit, focus_edit): (&mut Option<(String, String)>, &mut bool),
    acts: &mut Vec<Action>,
) {
    let local = cm.id.starts_with("local-");
    let who = author(users, mine, cm);
    let when = if local {
        "queued".to_string()
    } else {
        ago(cm.created_time.timestamp(), now)
    };
    ui.horizontal(|ui| {
        // our own keep one colour, whether pushed (bot id) or still local
        let by = if mine { "you" } else { cm.created_by.as_str() };
        crate::people::avatar(ui, by, &who, 18.0);
        ui.label(
            RichText::new(format!("{who} · {when}"))
                .size(12.0)
                .color(c.secondary),
        );
        if mine {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Delete").clicked() {
                    acts.push(Action::CommentDelete(cm.id.clone()));
                }
                if ui.small_button("Edit").clicked() {
                    *edit = Some((cm.id.clone(), to_markdown(&cm.rich_text)));
                    *focus_edit = true;
                }
            });
        }
    });
    match edit {
        Some((id, text)) if *id == cm.id => {
            let box_ = box_id("edit", &cm.id);
            if std::mem::take(focus_edit) {
                ui.memory_mut(|m| m.request_focus(box_));
            }
            match draft_box(ui, box_, text, "Edit…") {
                Some(Draft::Send) => acts.push(Action::CommentSave(id.clone(), text.clone())),
                Some(Draft::Leave) => *edit = None,
                None => {}
            }
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    if let Some((id, text)) = edit.as_ref() {
                        acts.push(Action::CommentSave(id.clone(), text.clone()));
                    }
                }
                if ui.button("Cancel").clicked() {
                    *edit = None;
                }
            });
        }
        _ => {
            ui.add(egui::Label::new(md::rich_job(&cm.rich_text, 14.0, false, users)).wrap());
        }
    }
    ui.add_space(4.0);
}

/// The 💬 count at the right of a row; the row's rect is `row`. Returns
/// true when clicked.
pub(crate) fn badge(ui: &mut Ui, c: &Colors, row: egui::Rect, id: Id, n: usize) -> bool {
    let size = egui::vec2(48.0, 20.0);
    let r = egui::Rect::from_min_size(egui::pos2(row.right() - size.x, row.top()), size);
    let resp = ui.interact(r, id, Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 4.0, c.hover);
    }
    ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        format!("💬 {n}"),
        egui::FontId::proportional(12.0),
        c.secondary,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, PointerButton};
    use notion_core::ops::OpKind;
    use serde_json::json;

    use crate::testkit::{key, key_mod, Win};

    const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::NONE
    };

    fn cm_on(id: &str, disc: &str, parent: &str, by: &str, text: &str) -> Comment {
        let t = "2025-01-01T00:00:00.000Z";
        let ptype = if parent == "p" { "page_id" } else { "block_id" };
        serde_json::from_value(json!({
            "object": "comment", "id": id, "discussion_id": disc,
            "parent": {"type": ptype, ptype: parent},
            "created_time": t, "last_edited_time": t,
            "created_by": {"object": "user", "id": by},
            "rich_text": [{"type": "text", "text": {"content": text}, "plain_text": text, "annotations": {}}],
        }))
        .unwrap()
    }

    fn cm(id: &str, disc: &str) -> Comment {
        cm_on(id, disc, "p", "u", "")
    }

    fn open_panel(w: &mut Win) {
        w.press(Key::M, CTRL_SHIFT);
        w.frames(vec![]);
        assert!(w.app.comments.is_some());
    }

    fn ops(w: &Win) -> Vec<OpKind> {
        w.app
            .store
            .queued_ops()
            .unwrap()
            .into_iter()
            .map(|o| o.kind)
            .collect()
    }

    #[test]
    fn threads_group_by_discussion_in_first_seen_order() {
        let cs = [cm("1", "a"), cm("2", "b"), cm("3", "a")];
        let ids: Vec<Vec<&str>> = threads(&cs)
            .iter()
            .map(|t| t.iter().map(|c| c.id.as_str()).collect())
            .collect();
        assert_eq!(ids, [vec!["1", "3"], vec!["2"]]);
    }

    #[test]
    fn relative_time() {
        assert_eq!(ago(100, 110), "just now");
        assert_eq!(ago(100, 100 + 300), "5 min ago");
        assert_eq!(ago(0, 3 * 3600), "3 h ago");
        assert_eq!(ago(0, 3 * 86_400 + 5), "3 d ago");
        assert_eq!(ago(500, 100), "just now", "a clock skewed into the future");
    }

    #[test]
    fn only_the_integrations_own_and_unpushed_comments_are_mine() {
        assert!(mine(Some("bot"), &cm_on("c", "d", "p", "bot", "")));
        assert!(!mine(Some("bot"), &cm_on("c", "d", "p", "someone", "")));
        assert!(
            !mine(None, &cm_on("c", "d", "p", "bot", "")),
            "bot id not known yet"
        );
        assert!(mine(None, &cm_on("local-1", "d", "p", "local", "")));
    }

    #[test]
    fn ctrl_shift_m_opens_and_closes_the_panel_and_fetches_threads() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.cmds();
        open_panel(&mut w);
        assert_eq!(
            w.cmds(),
            [Cmd::PullComments { id: "p".into() }, Cmd::Me],
            "the page's threads and the bot id, once"
        );
        w.press(Key::M, CTRL_SHIFT);
        assert!(w.app.comments.is_none(), "again closes it");
        open_panel(&mut w);
        assert_eq!(
            w.cmds(),
            [Cmd::PullComments { id: "p".into() }],
            "Me is asked once"
        );
    }

    #[test]
    fn ctrl_shift_m_default_binding_does_not_collide() {
        let k = crate::keys::Keymap::default();
        assert_eq!(
            k.get(&crate::keys::Chord::parse("ctrl+shift+m").unwrap()),
            Some(crate::keys::Command::Comments)
        );
    }

    #[test]
    fn typing_then_ctrl_enter_queues_a_page_comment() {
        let mut w = Win::with_texts(&["a"]);
        open_panel(&mut w);
        w.type_text("hello **world**");
        assert_eq!(w.app.comments.as_ref().unwrap().new_text, "hello **world**");
        w.send(key_mod(Key::Enter, Modifiers::CTRL));
        let got = ops(&w);
        assert_eq!(got.len(), 1, "{got:?}");
        let OpKind::CreateComment {
            parent, rich_text, ..
        } = &got[0]
        else {
            panic!("{got:?}")
        };
        assert_eq!(*parent, CommentTarget::Page("p".into()));
        assert_eq!(
            rich_text[1]["annotations"]["bold"], true,
            "Markdown became rich text"
        );
        assert_eq!(w.app.counts[0], 1, "the status bar counts it");
        assert!(
            w.app.comments.as_ref().unwrap().new_text.is_empty(),
            "the draft is cleared"
        );
        let shown = &w.app.comments.as_ref().unwrap().page_comments;
        assert_eq!(shown.len(), 1);
        assert!(shown[0].id.starts_with("local-"));
        // an empty box sends nothing
        w.send(key_mod(Key::Enter, Modifiers::CTRL));
        assert_eq!(ops(&w).len(), 1);
    }

    #[test]
    fn the_panel_follows_the_edited_row_and_a_comment_goes_to_that_block() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(1);
        w.cmds();
        w.press(Key::M, CTRL_SHIFT);
        w.frames(vec![]);
        let p = w.app.comments.as_ref().unwrap();
        assert_eq!(p.block.as_deref(), Some("p-1"), "opened on the edited row");
        assert!(w.app.focus.is_none(), "the row's edit was committed");
        assert!(w.cmds().contains(&Cmd::PullComments { id: "p-1".into() }));
        w.type_text("on b");
        w.send(key_mod(Key::Enter, Modifiers::CTRL));
        let got = ops(&w);
        let OpKind::CreateComment { parent, .. } = &got[0] else {
            panic!("{got:?}")
        };
        assert_eq!(*parent, CommentTarget::Block("p-1".into()));
        assert!(
            w.app.queued_ids.contains("p-1"),
            "the row gets its sync dot"
        );
        assert_eq!(w.app.comment_counts["p-1"], 1);

        // focusing another row moves the panel there (and fetches its threads)
        w.edit(0);
        assert_eq!(
            w.app.comments.as_ref().unwrap().block.as_deref(),
            Some("p-0")
        );
        assert!(w.cmds().contains(&Cmd::PullComments { id: "p-0".into() }));
    }

    #[test]
    fn esc_leaves_the_box_and_returns_to_the_row() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        open_panel(&mut w);
        w.type_text("draft");
        w.send(key(Key::Escape));
        assert_eq!(
            w.app.focus.as_ref().map(|f| f.block_id.as_str()),
            Some("p-0")
        );
        assert_eq!(
            w.app.comments.as_ref().unwrap().new_text,
            "draft",
            "the draft is kept"
        );
        assert!(ops(&w).is_empty());
    }

    #[test]
    fn a_reply_goes_to_its_own_discussion() {
        let mut w = Win::with_texts(&["a"]);
        w.app
            .store
            .put_comment(&cm_on("c1", "d1", "p", "u", "first"))
            .unwrap();
        w.app
            .store
            .put_comment(&cm_on("c2", "d2", "p", "u", "second"))
            .unwrap();
        w.app.refresh();
        open_panel(&mut w);
        let id = box_id("reply", "d2");
        w.ctx.memory_mut(|m| m.request_focus(id));
        w.frames(vec![]);
        w.type_text("re");
        w.send(key_mod(Key::Enter, Modifiers::CTRL));
        let got = ops(&w);
        assert_eq!(got.len(), 1, "{got:?}");
        let OpKind::CreateComment { parent, .. } = &got[0] else {
            panic!("{got:?}")
        };
        assert_eq!(*parent, CommentTarget::Discussion("d2".into()));
        let c = w.app.store.comments_for("p").unwrap();
        let reply = c.iter().find(|c| c.id.starts_with("local-")).unwrap();
        assert_eq!(
            (
                reply.discussion_id.as_str(),
                plain_text(&reply.rich_text).as_str()
            ),
            ("d2", "re")
        );
        assert!(
            w.app
                .comments
                .as_ref()
                .unwrap()
                .replies
                .get("d2")
                .is_none_or(String::is_empty),
            "its draft is cleared"
        );
    }

    #[test]
    fn row_badge_counts_cached_comments_and_a_click_opens_the_panel_on_the_row() {
        let mut w = Win::with_texts(&["a", "b"]);
        let store = &w.app.store;
        store
            .put_comment(&cm_on("c1", "d1", "p-1", "u", "x"))
            .unwrap();
        store
            .put_comment(&cm_on("c2", "d1", "p-1", "u", "y"))
            .unwrap();
        w.app.refresh();
        w.frames(vec![]);
        assert_eq!(w.app.comment_counts.get("p-1"), Some(&2));
        assert_eq!(w.app.comment_counts.get("p-0"), None);
        let id = Id::unique(("comments-badge", "p-1"));
        let r = w.ctx.read_response(id).expect("the badge was drawn").rect;
        assert!(
            w.ctx
                .read_response(Id::unique(("comments-badge", "p-0")))
                .is_none(),
            "no badge without comments"
        );
        let pos = r.center();
        let ev = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        w.frames(vec![Event::PointerMoved(pos)]);
        w.frames(vec![ev(true)]);
        w.frames(vec![ev(false)]);
        w.frames(vec![]);
        assert_eq!(
            w.app.comments.as_ref().and_then(|p| p.block.as_deref()),
            Some("p-1")
        );
        assert_eq!(w.app.comments.as_ref().unwrap().block_comments.len(), 2);
        assert!(w.cmds().contains(&Cmd::PullComments { id: "p-1".into() }));
    }

    #[test]
    fn edit_and_delete_queue_ops_and_undo_takes_them_back() {
        let mut w = Win::with_texts(&["a"]);
        w.app.me = Some("bot".into());
        w.app
            .store
            .put_comment(&cm_on("c1", "d1", "p", "bot", "old"))
            .unwrap();
        w.app
            .store
            .put_comment(&cm_on("c2", "d2", "p", "bot", "gone"))
            .unwrap();
        w.app.refresh();
        open_panel(&mut w);
        w.app.sent.clear(); // the fetches are answered; undo waits for them
        w.app.apply(Action::CommentSave("c1".into(), "new".into()));
        w.app.apply(Action::CommentDelete("c2".into()));
        let got = ops(&w);
        assert!(
            matches!(&got[0], OpKind::UpdateComment { comment_id, .. } if comment_id == "c1"),
            "{got:?}"
        );
        assert!(
            matches!(&got[1], OpKind::DeleteComment { comment_id } if comment_id == "c2"),
            "{got:?}"
        );
        assert_eq!(w.app.counts[0], 2);
        let shown = &w.app.comments.as_ref().unwrap().page_comments;
        assert_eq!(shown.len(), 1);
        assert_eq!(plain_text(&shown[0].rich_text), "new");

        w.press(Key::Z, Modifiers::CTRL);
        w.press(Key::Z, Modifiers::CTRL);
        assert!(ops(&w).is_empty(), "undone");
        assert_eq!(w.app.comments.as_ref().unwrap().page_comments.len(), 2);

        // refused while a push runs
        w.app.sent.push_back(Cmd::Push);
        w.app.apply(Action::CommentDelete("c2".into()));
        assert!(ops(&w).is_empty());
        assert!(w
            .app
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("paused")));
    }

    #[test]
    fn failed_comment_ops_are_listed_and_discard_drops_the_unpushed_comment() {
        let mut w = Win::with_texts(&["a"]);
        open_panel(&mut w);
        w.app.apply(Action::CommentSend(
            CommentTarget::Block("p-0".into()),
            "oops".into(),
        ));
        let op = w.app.store.queued_ops().unwrap()[0].id;
        w.app
            .store
            .mark_op(
                op,
                notion_core::ops::OpStatus::Failed,
                Some("403 restricted_resource"),
            )
            .unwrap();
        w.app.refresh();
        assert_eq!(w.app.counts, [0, 0, 1]);
        let p = &w.app.problems[0];
        assert_eq!(
            (
                p.kind,
                p.label.as_str(),
                p.block.as_str(),
                p.page.as_deref()
            ),
            ("comment", "oops", "p-0", Some("p"))
        );
        assert!(w.app.problem_ids.contains("p-0"));
        w.cmds();
        w.app.apply(Action::Discard(op));
        assert!(
            w.app.store.comments_for("p-0").unwrap().is_empty(),
            "no ghost comment"
        );
        assert!(w.app.problems.is_empty());
        assert!(w.cmds().contains(&Cmd::Pull {
            id: "p".into(),
            force: true
        }));
    }

    #[test]
    fn worker_events_update_the_bot_id_and_reload() {
        let mut w = Win::with_texts(&["a"]);
        open_panel(&mut w);
        w.cmds();
        w.app
            .store
            .put_comment(&cm_on("c1", "d1", "p", "bot", "x"))
            .unwrap();
        w.answer(crate::worker::Event::CommentsPulled);
        w.answer(crate::worker::Event::Me("bot".into()));
        assert_eq!(w.app.me.as_deref(), Some("bot"));
        assert_eq!(w.app.comments.as_ref().unwrap().page_comments.len(), 1);
    }
}
