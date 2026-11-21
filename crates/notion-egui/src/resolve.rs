//! Conflicts you can resolve (C4): a window with your text next to Notion's,
//! the word differences marked, an editable merge below (yours to start
//! with), and Keep theirs / Keep mine / Use merged. Tab moves between the
//! controls, Ctrl+Enter is Use merged and Esc closes it (both taken in
//! `App::shortcuts`).
//!
//! Notion's version is fetched when the panel opens (`Cmd::Fetch`); Keep
//! mine and Use merged are one `Cmd::Resolve` each, which fetches again and
//! re-queues the op against what it finds. Keep theirs is Discard.

use eframe::egui::{
    self, text::LayoutJob, Align2, Button, Color32, Context, Id, Label, RichText, ScrollArea, Stroke,
    TextEdit, TextFormat, Window,
};
use notion_core::sync::Fetched;

use crate::app::{Action, App};
use crate::theme::Colors;
use crate::worker::Cmd;

pub(crate) struct Panel {
    pub op: i64,
    /// Like the list's entry: what it is, its text, why it conflicted.
    title: String,
    error: String,
    /// Notion's version, once it has come.
    pub fetched: Option<Fetched>,
    /// Why it did not come.
    pub error_fetching: Option<String>,
    /// The merged text; starts as yours.
    pub merged: String,
    /// The merged box's id: the panel's keys count only while it (or no
    /// text box) has the keyboard.
    pub merged_id: Id,
    /// The word differences of the two texts, worked out once.
    segs: Vec<(Seg, String)>,
    focus_merged: bool,
}

impl Panel {
    pub fn got(&mut self, what: Fetched) {
        if let Fetched::Text { mine, theirs } = &what {
            self.segs = word_diff(mine, theirs);
            self.merged = mine.clone();
            self.focus_merged = true;
        }
        self.fetched = Some(what);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    Same,
    /// Only in your text (an insertion).
    Mine,
    /// Only in Notion's (a deletion).
    Theirs,
}

/// Words and the whitespace between them, so the pieces join back exactly.
fn tokens(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let (mut start, mut ws) = (0, None);
    for (i, ch) in s.char_indices() {
        let now = ch.is_whitespace();
        if ws.is_some_and(|w| w != now) {
            v.push(&s[start..i]);
            start = i;
        }
        ws = Some(now);
    }
    if start < s.len() {
        v.push(&s[start..]);
    }
    v
}

/// Above this many table cells the diff is not worked out: each side is
/// marked whole instead.
const MAX_CELLS: usize = 1_000_000;

/// The word-level differences from Notion's text to yours, by longest common
/// subsequence of what is left between the common start and end, adjacent
/// pieces of one kind joined.
pub fn word_diff(mine: &str, theirs: &str) -> Vec<(Seg, String)> {
    let (a, b) = (tokens(mine), tokens(theirs));
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<(Seg, String)> = Vec::new();
    let mut put = |seg, t: &str| match out.last_mut() {
        Some((s, x)) if *s == seg => x.push_str(t),
        _ => out.push((seg, t.to_string())),
    };
    a[..pre].iter().for_each(|t| put(Seg::Same, t));
    if (am.len() + 1) * (bm.len() + 1) > MAX_CELLS {
        bm.iter().for_each(|t| put(Seg::Theirs, t));
        am.iter().for_each(|t| put(Seg::Mine, t));
    } else {
        // l[i * w + j]: the common length of am[i..] and bm[j..]
        let w = bm.len() + 1;
        let mut l = vec![0u32; (am.len() + 1) * w];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                l[i * w + j] = if am[i] == bm[j] {
                    l[(i + 1) * w + j + 1] + 1
                } else {
                    l[(i + 1) * w + j].max(l[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                put(Seg::Same, am[i]);
                i += 1;
                j += 1;
            } else if j < bm.len() && (i == am.len() || l[i * w + j + 1] >= l[(i + 1) * w + j]) {
                put(Seg::Theirs, bm[j]);
                j += 1;
            } else {
                put(Seg::Mine, am[i]);
                i += 1;
            }
        }
    }
    a[a.len() - suf..].iter().for_each(|t| put(Seg::Same, t));
    out
}

/// One side of the diff: what is common, and its own pieces marked.
fn side(segs: &[(Seg, String)], own: Seg, mark: Color32, c: &Colors) -> LayoutJob {
    let mut job = LayoutJob::default();
    for (seg, t) in segs.iter().filter(|(s, _)| *s != other(own)) {
        let marked = *seg == own;
        job.append(
            t,
            0.0,
            TextFormat {
                color: c.text,
                background: if marked { mark } else { Color32::TRANSPARENT },
                underline: if marked { Stroke::new(1.0, c.text) } else { Stroke::NONE },
                ..Default::default()
            },
        );
    }
    job
}

fn other(s: Seg) -> Seg {
    if s == Seg::Mine {
        Seg::Theirs
    } else {
        Seg::Mine
    }
}

impl App {
    /// Opens the panel on a conflict (not a failed op) and asks for Notion's
    /// version.
    pub(crate) fn open_conflict(&mut self, op: i64) {
        let Some(p) = self.problems.iter().find(|p| p.op == op && p.conflict) else {
            return;
        };
        let (title, error) = (format!("{} · {}", p.kind, p.label), p.error.clone());
        if !self.commit() {
            return;
        }
        self.problems_open = false;
        self.conflict = Some(Panel {
            op,
            title,
            error,
            fetched: None,
            error_fetching: None,
            merged: String::new(),
            merged_id: Id::unique("conflict-merged"),
            segs: Vec::new(),
            focus_merged: false,
        });
        self.send(Cmd::Fetch { op });
    }

    /// Keep mine (`merged` None) or Use merged. Not while a push runs, nor
    /// when Notion no longer has the object.
    pub(crate) fn resolve(&mut self, op: i64, merged: Option<String>) {
        let gone = matches!(self.conflict.as_ref().and_then(|p| p.fetched.as_ref()), Some(Fetched::Gone));
        if gone || self.sent.iter().any(|c| matches!(c, Cmd::Resolve { .. })) {
            return;
        }
        if self.pushing() {
            self.last_error = Some("resolving is paused while a push runs".into());
            return;
        }
        // what the panel showed of Notion's text: it must still be that
        let seen = match self.conflict.as_ref().and_then(|p| p.fetched.as_ref()) {
            Some(Fetched::Text { theirs, .. }) => Some(theirs.clone()),
            _ => None,
        };
        self.send(Cmd::Resolve { op, merged, seen });
    }

    /// Use merged (also Ctrl+Enter): only for a text conflict.
    pub(crate) fn merge_conflict(&mut self, op: i64) {
        if let Some(p) = &self.conflict {
            if matches!(p.fetched, Some(Fetched::Text { .. })) {
                if p.merged.trim().is_empty() {
                    self.last_error = Some("the merged text is empty".into());
                    return;
                }
                let merged = p.merged.clone();
                self.resolve(op, Some(merged));
            }
        }
    }

    /// The window, while the panel is open.
    pub(crate) fn conflict_ui(&mut self, ctx: &Context, c: &Colors) -> Vec<Action> {
        let Some(mut p) = self.conflict.take() else {
            return Vec::new();
        };
        let mut acts = Vec::new();
        let busy = self.sent.iter().any(|c| matches!(c, Cmd::Resolve { .. }));
        Window::new("Resolve conflict")
            .id(Id::unique("conflict-panel"))
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(680.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(&p.title).strong());
                ui.label(RichText::new(&p.error).size(12.0).color(c.error));
                match &p.fetched {
                    None => {
                        let msg = p.error_fetching.as_deref().map_or("Asking Notion for its version…".into(), |e| format!("Could not get Notion's version: {e}"));
                        ui.label(RichText::new(msg).color(c.secondary));
                    }
                    Some(Fetched::Gone) => {
                        ui.label(RichText::new("Notion no longer has this (deleted or in the trash): only Keep theirs is left.").color(c.error));
                    }
                    Some(Fetched::Other) => {
                        ui.label(RichText::new("Not a text edit: there is nothing to compare.").color(c.secondary));
                    }
                    Some(Fetched::Text { .. }) => {
                        let green = Color32::from_rgba_unmultiplied(46, 160, 67, 70);
                        let red = Color32::from_rgba_unmultiplied(224, 62, 62, 70);
                        ui.columns(2, |cols| {
                            for (col, name, own, mark) in [(0, "Yours", Seg::Mine, green), (1, "Notion's", Seg::Theirs, red)] {
                                let ui = &mut cols[col];
                                ui.label(RichText::new(name).size(12.0).color(c.secondary));
                                ScrollArea::vertical().id_salt(name).max_height(160.0).show(ui, |ui| {
                                    ui.add(Label::new(side(&p.segs, own, mark, c)).selectable(true));
                                });
                            }
                        });
                        ui.label(RichText::new("Merged").size(12.0).color(c.secondary));
                        let id = p.merged_id;
                        if std::mem::take(&mut p.focus_merged) {
                            ctx.memory_mut(|m| m.request_focus(id));
                        }
                        ui.add(TextEdit::multiline(&mut p.merged).id(id).desired_rows(4).desired_width(f32::INFINITY));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("Keep theirs").clicked() {
                        acts.push(Action::Discard(p.op));
                    }
                    let can = matches!(p.fetched, Some(Fetched::Other | Fetched::Text { .. })) && !busy;
                    if p.fetched != Some(Fetched::Gone) && ui.add_enabled(can, Button::new("Keep mine")).clicked() {
                        acts.push(Action::KeepMine(p.op));
                    }
                    if matches!(p.fetched, Some(Fetched::Text { .. }))
                        && ui.add_enabled(!busy, Button::new("Use merged (Ctrl+Enter)")).clicked()
                    {
                        acts.push(Action::Merge(p.op));
                    }
                });
                ui.label(RichText::new("Tab moves between the controls · Esc closes without changing anything").size(11.0).color(c.secondary));
            });
        self.conflict = Some(p);
        acts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;
    use crate::worker::Event as WEvent;
    use eframe::egui::{self, Key, Modifiers};
    use notion_core::ops::OpStatus;
    use notion_core::sync;

    fn d(mine: &str, theirs: &str) -> Vec<(Seg, String)> {
        word_diff(mine, theirs)
    }
    fn s(seg: Seg, t: &str) -> (Seg, String) {
        (seg, t.to_string())
    }

    #[test]
    fn word_diff_marks_insertions_and_deletions() {
        use Seg::*;
        assert_eq!(d("a b c", "a b c"), [s(Same, "a b c")]);
        assert_eq!(d("a b c", "a c"), [s(Same, "a "), s(Mine, "b "), s(Same, "c")]);
        assert_eq!(d("a c", "a b c"), [s(Same, "a "), s(Theirs, "b "), s(Same, "c")]);
        assert_eq!(
            d("the quick fox", "the slow fox"),
            [s(Same, "the "), s(Theirs, "slow"), s(Mine, "quick"), s(Same, " fox")]
        );
        assert_eq!(d("", "x y"), [s(Theirs, "x y")]);
        assert_eq!(d("x y", ""), [s(Mine, "x y")]);
        assert!(d("", "").is_empty());
    }

    #[test]
    fn word_diff_pieces_rebuild_both_texts() {
        let (m, t) = ("one  two\nthree four", "one two three\n five four six");
        let segs = d(m, t);
        let join = |skip: Seg| segs.iter().filter(|(s, _)| *s != skip).map(|(_, x)| x.as_str()).collect::<String>();
        assert_eq!(join(Seg::Theirs), m);
        assert_eq!(join(Seg::Mine), t);
    }

    fn conflict_win() -> (Win, i64) {
        let mut w = Win::with_texts(&["a", "b"]);
        let op = sync::local_edit_text(&w.app.store, "p-1", "my text").unwrap();
        w.app.store.mark_op(op, OpStatus::Conflict, Some("remote edited")).unwrap();
        w.app.refresh();
        (w, op)
    }

    fn text(mine: &str, theirs: &str) -> Fetched {
        Fetched::Text { mine: mine.into(), theirs: theirs.into() }
    }

    /// The list opens the panel (Enter on the selected entry), which asks for
    /// Notion's version and shows both texts and three buttons.
    #[test]
    fn the_panel_opens_from_the_list() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::ToggleProblems);
        w.frames(vec![]);
        w.frames(vec![]); // an area is drawn from its second frame
        assert!(w.shown.contains("Resolve…") && w.shown.contains("Keep theirs"), "{}", w.shown);
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.conflict.is_some() && !w.app.problems_open);
        assert_eq!(w.cmds(), [Cmd::Fetch { op }]);
        w.frames(vec![]);
        assert!(w.shown.contains("Asking Notion") && w.shown.contains("Keep theirs"));
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });
        w.frames(vec![]);
        for t in ["Yours", "Notion's", "Keep theirs", "Keep mine", "Use merged", "Merged"] {
            assert!(w.shown.contains(t), "{t}: {}", w.shown);
        }
        assert_eq!(w.app.conflict.as_ref().unwrap().merged, "my text", "starts as yours");

        // the Resolve… button opens it too, and only for a conflict
        w.app.conflict = None;
        w.app.apply(Action::Resolve(op));
        assert_eq!(w.cmds(), [Cmd::Fetch { op }]);
        w.app.conflict = None;
        w.app.apply(Action::Resolve(op + 100));
        assert!(w.app.conflict.is_none() && w.cmds().is_empty());
    }

    #[test]
    fn the_three_actions_send_their_commands() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });

        // Keep mine
        w.app.apply(Action::KeepMine(op));
        assert_eq!(w.cmds(), [Cmd::Resolve { op, merged: None, seen: Some("their text".into()) }]);
        w.app.apply(Action::KeepMine(op));
        assert!(w.cmds().is_empty(), "one at a time");
        w.answer(WEvent::Error("boom".into()));
        assert!(w.app.sent.is_empty(), "an error answers it too");
        assert!(w.app.conflict.is_some() && w.app.last_error.as_deref() == Some("boom"));

        // Use merged: the button's action and Ctrl+Enter
        w.app.conflict.as_mut().unwrap().merged = "my and their text".into();
        w.app.apply(Action::Merge(op));
        let merged = Some("my and their text".to_string());
        assert_eq!(w.cmds(), [Cmd::Resolve { op, merged: merged.clone(), seen: Some("their text".into()) }]);
        w.answer(WEvent::Error("boom".into()));
        w.press(Key::Enter, Modifiers::CTRL);
        assert_eq!(w.cmds(), [Cmd::Resolve { op, merged, seen: Some("their text".into()) }]);
        w.answer(WEvent::Resolved(op));
        assert!(w.app.conflict.is_none() && w.app.sent.is_empty());
        assert!(w.app.info.as_deref().unwrap().contains("re-queued"));

        // Keep theirs: the op goes and the page is pulled
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.app.apply(Action::Discard(op));
        assert!(w.app.conflict.is_none());
        assert_eq!(w.cmds(), [Cmd::Pull { id: "p".into(), force: true }]);
    }

    #[test]
    fn esc_closes_the_panel_and_changes_nothing() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });
        w.frames(vec![]);
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.conflict.is_none());
        assert!(w.cmds().is_empty());
        assert_eq!(w.app.store.ops_by_status(OpStatus::Conflict).unwrap().len(), 1);
        assert_eq!(w.app.store.queued_ops().unwrap().len(), 0);
        // the late answer of the fetch is ignored
        w.answer(WEvent::Fetched { op, what: text("x", "y") });
        assert!(w.app.conflict.is_none());
    }

    #[test]
    fn a_gone_object_offers_only_keep_theirs() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: Fetched::Gone });
        w.frames(vec![]);
        assert!(w.shown.contains("Keep theirs") && w.shown.contains("no longer has"), "{}", w.shown);
        assert!(!w.shown.contains("Keep mine") && !w.shown.contains("Use merged"), "{}", w.shown);
        w.app.apply(Action::KeepMine(op));
        w.press(Key::Enter, Modifiers::CTRL);
        assert!(w.cmds().is_empty());
    }

    #[test]
    fn a_failed_fetch_is_answered_and_shown() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.answer(WEvent::Error("offline: NOTION_TOKEN not set".into()));
        assert!(w.app.sent.is_empty());
        w.frames(vec![]);
        assert!(w.shown.contains("Could not get Notion's version"), "{}", w.shown);
    }

    #[test]
    fn up_down_select_in_the_list() {
        let (mut w, op) = conflict_win();
        let op2 = sync::local_edit_text(&w.app.store, "p-0", "x").unwrap();
        w.app.store.mark_op(op2, OpStatus::Failed, Some("400")).unwrap();
        w.app.refresh();
        w.app.apply(Action::ToggleProblems);
        w.frames(vec![]);
        w.press(Key::ArrowDown, Modifiers::NONE);
        assert_eq!(w.app.problem_sel, 1);
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.conflict.is_none(), "a failed op has no panel");
        w.press(Key::ArrowUp, Modifiers::NONE);
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.conflict.as_ref().map(|p| p.op), Some(op));
    }

    #[test]
    fn word_diff_trims_the_common_ends_and_gives_up_on_huge_inputs() {
        use Seg::*;
        assert_eq!(
            d("start a b end", "start x end"),
            [s(Same, "start "), s(Theirs, "x"), s(Mine, "a b"), s(Same, " end")]
        );
        // far over the table cap: each side is marked whole, the ends stay common
        let words = |p: &str| (0..1500).map(|i| format!("{p}{i}")).collect::<Vec<_>>().join(" ");
        let (m, t) = (format!("head {} tail", words("m")), format!("head {} tail", words("t")));
        let segs = d(&m, &t);
        assert_eq!(segs, [s(Same, "head "), s(Theirs, &words("t")), s(Mine, &words("m")), s(Same, " tail")]);
    }

    #[test]
    fn notion_changing_again_refetches_and_a_late_error_is_not_the_next_panels() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });
        w.app.apply(Action::KeepMine(op));
        w.cmds();
        w.answer(WEvent::Error("Notion changed again: review it".into()));
        assert_eq!(w.cmds(), [Cmd::Fetch { op }], "shows the new text");
        assert!(w.app.conflict.as_ref().unwrap().fetched.is_none());
        w.answer(WEvent::Fetched { op, what: text("my text", "their newer text") });
        assert!(w.app.sent.is_empty());

        // A's fetch fails late, after B's panel opened
        let b = sync::local_edit_text(&w.app.store, "p-0", "other").unwrap();
        w.app.store.mark_op(b, OpStatus::Conflict, Some("x")).unwrap();
        w.app.refresh();
        w.app.conflict = None;
        w.app.apply(Action::Resolve(op));
        w.app.conflict = None; // closed (Esc) before the answer
        w.app.apply(Action::Resolve(b));
        w.cmds();
        w.answer(WEvent::Error("500".into())); // A's
        assert_eq!(w.app.conflict.as_ref().map(|p| (p.op, p.error_fetching.clone())), Some((b, None)));
        w.answer(WEvent::Error("500".into())); // B's own
        assert!(w.app.conflict.as_ref().unwrap().error_fetching.is_some());
    }

    #[test]
    fn an_empty_merge_is_refused() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });
        w.app.conflict.as_mut().unwrap().merged = "  ".into();
        w.app.apply(Action::Merge(op));
        assert!(w.cmds().is_empty());
        assert!(w.app.last_error.as_deref().unwrap().contains("empty"));
    }

    /// Ctrl+Enter belongs to the comment draft while it has the keyboard.
    #[test]
    fn ctrl_enter_in_the_comment_draft_sends_the_comment_not_the_merge() {
        let (mut w, op) = conflict_win();
        w.app.apply(Action::Resolve(op));
        w.cmds();
        w.answer(WEvent::Fetched { op, what: text("my text", "their text") });
        w.frames(vec![]);
        w.app.open_comments(None);
        w.frames(vec![]);
        w.ctx.memory_mut(|m| m.request_focus(egui::Id::unique(("comment-box", "new", ""))));
        w.frames(vec![]);
        w.type_text("hello");
        w.cmds();
        w.send(key_mod(Key::Enter, Modifiers::CTRL));
        assert!(
            w.app.store.queued_ops().unwrap().iter().any(|o| matches!(o.kind, notion_core::ops::OpKind::CreateComment { .. })),
            "the comment was queued"
        );
        assert!(w.cmds().iter().all(|c| !matches!(c, Cmd::Resolve { .. })), "no merge");
        assert!(w.app.conflict.is_some());
    }
}
