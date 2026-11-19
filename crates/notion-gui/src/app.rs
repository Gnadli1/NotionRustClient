//! App state, update and view. `update` only touches the store, the editor
//! buffer and the worker channel (its Task is just widget focus), so it is
//! testable without a window.

use std::collections::VecDeque;
use std::sync::Arc;

use iced::font::{Style, Weight};
use iced::keyboard::{self, Key};
use iced::widget::text::Span;
use iced::widget::{
    button, column, container, mouse_area, operation, rich_text, row, scrollable, span, text,
    text_editor, text_input,
};
use iced::{event, Color, Element, Fill, Font, Subscription, Task};
use notion_core::model::{BlockKind, Parent, RichText, TextBlock};
use notion_core::ops::OpStatus;
use notion_core::store::Store;
use notion_core::sync;
use tokio::sync::mpsc::UnboundedSender;

use crate::edit::{key_action, EditKey, Editing, KeyAction, EDITOR_ID};
use crate::outline::{self, Marker, Row};
use crate::worker::{Cmd, Event};

#[derive(Debug, Clone)]
pub enum Message {
    /// Show a cached page (no network).
    Open(String),
    Pull {
        force: bool,
    },
    Push,
    SearchInput(String),
    SearchSubmit,
    /// Search result chosen: pull it, then open it.
    PickResult(String),
    Worker(Arc<Event>),
    /// The worker's event stream ended (thread exited or panicked).
    WorkerGone,
    /// Row clicked: edit it if it is editable.
    Focus(String),
    EditorAction(text_editor::Action),
    /// A key the row editor hands to the app.
    EditKey(EditKey),
}

pub struct State {
    store: Store,
    cmds: UnboundedSender<Cmd>,
    pages: Vec<(String, String)>,
    current: Option<String>,
    title: String,
    rows: Vec<Row>,
    /// queued, conflict, failed
    counts: [usize; 3],
    last_error: Option<String>,
    search: String,
    results: Vec<(String, String)>,
    /// Picked search result waiting for its pull.
    pending_open: Option<String>,
    /// One entry per command sent and not yet answered (the worker answers
    /// each with one event, in order); true = Push.
    sent: VecDeque<bool>,
    /// The focused row, if any.
    editing: Option<Editing>,
}

impl State {
    pub fn new(store: Store, cmds: UnboundedSender<Cmd>) -> Self {
        let mut s = State {
            store,
            cmds,
            pages: Vec::new(),
            current: None,
            title: String::new(),
            rows: Vec::new(),
            counts: [0; 3],
            last_error: None,
            search: String::new(),
            results: Vec::new(),
            pending_open: None,
            sent: VecDeque::new(),
            editing: None,
        };
        s.refresh();
        s
    }

    fn editing_row(&self) -> Option<&Row> {
        let e = self.editing.as_ref()?;
        self.rows.iter().find(|r| r.block_id == e.block_id)
    }

    /// Writes the editor's text to the store if it changed, then refreshes.
    /// False if the write failed (the editor is kept).
    fn commit(&mut self) -> bool {
        let wrote = self.write_edit();
        // Some(true) = nothing to write; the rows are current
        if wrote != Some(true) {
            self.refresh();
        }
        wrote.is_some()
    }

    /// Some(changed?) on success, None on a failed write. No refresh.
    fn write_edit(&mut self) -> Option<bool> {
        let Some(e) = &mut self.editing else {
            return Some(true);
        };
        let Some(rt) = e.changed() else {
            return Some(true);
        };
        // a push may have renamed a local-* block meanwhile
        let res = self.store.resolve_id(&e.block_id).and_then(|id| {
            sync::local_edit_rich(&self.store, &id, rt)?;
            e.block_id = id;
            Ok(())
        });
        match res {
            Ok(()) => {
                e.original = e.content.text();
                Some(false)
            }
            Err(err) => {
                self.last_error = Some(err.to_string());
                None
            }
        }
    }

    /// Commit and stop editing; keeps the editor if the commit failed.
    fn leave(&mut self) -> bool {
        let ok = self.commit();
        if ok {
            self.editing = None;
        }
        ok
    }

    /// Commit the current row and start editing `id`, if it is editable.
    fn focus(&mut self, id: &str) -> Task<Message> {
        if !self.commit() {
            return Task::none();
        }
        let Some(row) = self.rows.iter().find(|r| r.block_id == id && r.editable) else {
            return Task::none();
        };
        self.editing = Some(Editing::new(row));
        operation::focus(EDITOR_ID)
    }

    fn send(&mut self, cmd: Cmd) {
        let push = cmd == Cmd::Push;
        match self.cmds.send(cmd) {
            Ok(()) => self.sent.push_back(push),
            Err(_) => self.last_error = Some("sync worker stopped".into()),
        }
    }

    /// A push is queued or running: editing is paused, since the worker may
    /// already have sent the queued ops an edit would rewrite.
    fn pushing(&self) -> bool {
        self.sent.contains(&true)
    }

    /// Reloads sidebar, outline and op counts from the store.
    fn refresh(&mut self) {
        if let Err(e) = self.try_refresh() {
            self.last_error = Some(e.to_string());
        }
        // a push may have renamed the edited local-* block
        if let Some(e) = &mut self.editing {
            match self.store.resolve_id(&e.block_id) {
                Ok(id) => e.block_id = id,
                Err(err) => self.last_error = Some(err.to_string()),
            }
        }
        if self.editing.is_some() && self.editing_row().is_none() {
            self.editing = None;
            self.last_error = Some("the edited block is gone; edit discarded".into());
        }
    }

    fn try_refresh(&mut self) -> notion_core::Result<()> {
        // ponytail: one get_page per cached page; fine for hundreds, add a
        // Store query if the cache grows to thousands
        self.pages.clear();
        for (id, title) in self.store.pages()? {
            let row = self
                .store
                .get_page(&id)?
                .is_some_and(|p| matches!(p.parent, Parent::DataSource { .. }));
            if !row {
                self.pages.push((id, title));
            }
        }
        if let Some(id) = &self.current {
            self.title = self
                .store
                .get_page(id)?
                .map_or_else(|| "(not cached)".into(), |p| untitled(&p.title()).into());
            self.rows = outline::flatten(&self.store, id)?;
        }
        self.counts = [
            self.store.queued_ops()?.len(),
            self.store.ops_by_status(OpStatus::Conflict)?.len(),
            self.store.ops_by_status(OpStatus::Failed)?.len(),
        ];
        Ok(())
    }
}

/// Closing the window with a row open must not lose its text.
impl Drop for State {
    fn drop(&mut self) {
        // mid-push the text was committed at Ctrl+S and edits are paused
        if std::thread::panicking() || self.pushing() {
            return;
        }
        self.write_edit();
    }
}

pub fn update(s: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::Open(id) => {
            if !s.leave() {
                return Task::none();
            }
            s.pending_open = None;
            s.current = Some(id);
            s.refresh();
        }
        Message::Pull { force } => {
            // commit first: a pull keeps blocks with queued edits
            s.commit();
            if let Some(id) = s.current.clone() {
                s.send(Cmd::Pull { id, force });
            }
        }
        Message::Push => {
            s.commit();
            s.send(Cmd::Push);
        }
        Message::SearchInput(q) => s.search = q,
        Message::SearchSubmit => {
            let q = s.search.trim().to_string();
            if !q.is_empty() {
                s.send(Cmd::Search(q));
            }
        }
        Message::PickResult(id) => {
            if matches!(s.store.get_page(&id), Ok(Some(_))) {
                if !s.leave() {
                    return Task::none();
                }
                s.current = Some(id.clone());
                s.refresh();
            }
            s.pending_open = Some(id.clone());
            s.send(Cmd::Pull { id, force: false });
        }
        Message::Focus(_) | Message::EditorAction(_) | Message::EditKey(_) if s.pushing() => {}
        Message::Focus(id) => return s.focus(&id),
        Message::EditorAction(a) => {
            if let Some(e) = &mut s.editing {
                e.content.perform(a);
            }
        }
        Message::EditKey(k) => return on_edit_key(s, k),
        Message::WorkerGone => {
            s.sent.clear();
            s.last_error = Some("sync worker stopped".into());
        }
        Message::Worker(ev) => {
            s.sent.pop_front();
            match &*ev {
                Event::Pulled { id, .. } => {
                    if s.pending_open.as_ref() == Some(id) && s.leave() {
                        s.pending_open = None;
                        s.current = Some(id.clone());
                    }
                    s.last_error = None;
                    // the editor keeps its own buffer; only the other rows change
                    s.refresh();
                    if s.editing.is_some() {
                        // its place in the tree may have moved
                        return operation::focus(EDITOR_ID);
                    }
                }
                Event::Pushed(_) => {
                    // ids of pushed local blocks are gone now: drop the focus
                    s.leave();
                    s.last_error = None;
                    s.refresh();
                    // force: the page's edit time may match the cache, and the
                    // re-pull is what turns local ids into real ones
                    if let Some(id) = s.current.clone() {
                        s.send(Cmd::Pull { id, force: true });
                    }
                }
                Event::SearchResults(r) => s.results = r.clone(),
                Event::Error(e) => {
                    s.pending_open = None;
                    // a push can fail after applying some ops
                    s.refresh();
                    s.last_error = Some(e.clone());
                }
            }
        }
    }
    Task::none()
}

fn on_edit_key(s: &mut State, k: EditKey) -> Task<Message> {
    let Some(e) = &mut s.editing else {
        return Task::none();
    };
    let id = e.block_id.clone();
    let Some(idx) = s.rows.iter().position(|r| r.block_id == id) else {
        return Task::none();
    };
    let editable = |r: &&Row| r.editable;
    match key_action(k, e.caret()) {
        KeyAction::Pass(a) => e.content.perform(a),
        KeyAction::FocusPrev => {
            if let Some(r) = s.rows[..idx].iter().rev().find(editable) {
                return s.focus(&r.block_id.clone());
            }
        }
        KeyAction::FocusNext => {
            if let Some(r) = s.rows[idx + 1..].iter().find(editable) {
                return s.focus(&r.block_id.clone());
            }
        }
        KeyAction::NewBlockAfter => {
            s.commit();
            // commit may have resolved the id and rebuilt the rows
            let Some(row) = s.editing_row().cloned() else {
                return Task::none();
            };
            let para = BlockKind::Paragraph(TextBlock {
                rich_text: Vec::new(),
                color: "default".into(),
            });
            let ids = s
                .store
                .resolve_id(&row.parent)
                .and_then(|p| Ok((p, s.store.resolve_id(&row.block_id)?)));
            match ids.and_then(|(parent, after)| {
                sync::local_create_block(&s.store, &parent, Some(&after), para)
            }) {
                Ok(new) => {
                    s.refresh();
                    return s.focus(&new);
                }
                Err(err) => s.last_error = Some(err.to_string()),
            }
        }
        KeyAction::DeleteRow => {
            // never take a subtree with it
            if s.rows
                .get(idx + 1)
                .is_some_and(|r| r.depth > s.rows[idx].depth)
            {
                return Task::none();
            }
            let prev = s.rows[..idx]
                .iter()
                .rev()
                .find(editable)
                .map(|r| r.block_id.clone());
            s.editing = None;
            let res = s
                .store
                .resolve_id(&id)
                .and_then(|id| sync::local_delete_block(&s.store, &id));
            if let Err(err) = res {
                s.last_error = Some(err.to_string());
            }
            s.refresh();
            if let Some(p) = prev {
                return s.focus(&p);
            }
        }
        KeyAction::Leave => {
            s.leave();
        }
    }
    Task::none()
}

/// Ctrl+R pull, Ctrl+Shift+R force pull, Ctrl+S push. Sees events even when a
/// widget (the search box) captured them.
fn shortcut(ev: iced::Event, _: event::Status, _: iced::window::Id) -> Option<Message> {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modifiers,
        repeat: false,
        ..
    }) = ev
    else {
        return None;
    };
    let Key::Character(c) = key.as_ref() else {
        return None;
    };
    if !modifiers.command() {
        return None;
    }
    if c.eq_ignore_ascii_case("r") {
        Some(Message::Pull {
            force: modifiers.shift(),
        })
    } else if c.eq_ignore_ascii_case("s") {
        Some(Message::Push)
    } else {
        None
    }
}

pub fn subscription(_: &State) -> Subscription<Message> {
    event::listen_with(shortcut)
}

fn untitled(t: &str) -> &str {
    if t.is_empty() {
        "Untitled"
    } else {
        t
    }
}

const GREY: Color = Color::from_rgb(0.55, 0.55, 0.55);
const LINK: Color = Color::from_rgb(0.25, 0.45, 0.9);

fn styled(t: &RichText) -> Span<'_, ()> {
    let a = &t.annotations;
    let mut font = if a.code {
        Font::MONOSPACE
    } else {
        Font::DEFAULT
    };
    if a.bold {
        font.weight = Weight::Bold;
    }
    if a.italic {
        font.style = Style::Italic;
    }
    span(t.plain_text.as_str())
        .font(font)
        .strikethrough(a.strikethrough)
        .underline(a.underline || t.href.is_some())
        .color_maybe(t.href.as_ref().map(|_| LINK))
}

fn row_view<'a>(r: &'a Row, editing: Option<&'a Editing>) -> Element<'a, Message> {
    let size = match r.marker {
        Marker::Heading(1) => 28,
        Marker::Heading(2) => 22,
        Marker::Heading(3) => 18,
        _ => 15,
    };
    let grey = matches!(
        r.marker,
        Marker::Unsupported(_) | Marker::ChildPage(_) | Marker::ChildDatabase(_) | Marker::Divider
    );
    let indent = iced::Padding::ZERO.left(r.depth as f32 * 24.0);
    let label = r.marker.label();
    if let Some(e) = editing.filter(|e| e.block_id == r.block_id) {
        let editor = text_editor(&e.content)
            .id(EDITOR_ID)
            .on_action(Message::EditorAction)
            .key_binding(crate::edit::binding)
            .font(if e.code {
                Font::MONOSPACE
            } else {
                Font::DEFAULT
            })
            .size(size);
        let body: Element<_> = if label.is_empty() {
            editor.into()
        } else {
            row![text(label).size(size), editor].spacing(6).into()
        };
        return container(body).padding(indent).into();
    }
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    if !label.is_empty() {
        let sep = if matches!(r.marker, Marker::Code(_)) {
            "\n"
        } else {
            " "
        };
        spans.push(span(format!("{label}{sep}")).color_maybe(grey.then_some(GREY)));
    }
    spans.extend(r.rich_text.iter().map(styled));
    let body = container(rich_text(spans).size(size))
        .padding(indent)
        .width(Fill);
    if r.editable {
        mouse_area(body)
            .on_press(Message::Focus(r.block_id.clone()))
            .into()
    } else {
        body.into()
    }
}

pub fn view(s: &State) -> Element<'_, Message> {
    let mut side = column![text_input("Search Notion…", &s.search)
        .on_input(Message::SearchInput)
        .on_submit(Message::SearchSubmit)]
    .spacing(4)
    .width(240);
    for (id, title) in &s.results {
        side = side.push(
            button(text(untitled(title)))
                .on_press(Message::PickResult(id.clone()))
                .style(button::text),
        );
    }
    if !s.results.is_empty() {
        side = side.push(text("Cached").color(GREY));
    }
    for (id, title) in &s.pages {
        let style = if s.current.as_ref() == Some(id) {
            button::secondary
        } else {
            button::text
        };
        side = side.push(
            button(text(untitled(title)))
                .on_press(Message::Open(id.clone()))
                .style(style),
        );
    }

    let main: Element<_> = match &s.current {
        None => text("Pick a page on the left, or search.")
            .color(GREY)
            .into(),
        Some(_) => column![
            text(&s.title).size(32),
            scrollable(column(s.rows.iter().map(|r| row_view(r, s.editing.as_ref()))).spacing(6))
                .height(Fill)
        ]
        .spacing(12)
        .into(),
    };

    let [queued, conflicts, failed] = s.counts;
    let mut status = format!("queued {queued} · conflicts {conflicts} · failed {failed}");
    if s.pushing() {
        status.push_str(" · pushing…");
    } else if !s.sent.is_empty() {
        status.push_str(" · syncing…");
    }
    if let Some(e) = &s.last_error {
        status.push_str(&format!(" · error: {e}"));
    }

    column![
        row![scrollable(side), container(main).width(Fill).height(Fill)]
            .spacing(16)
            .height(Fill),
        text(status).size(13).color(GREY),
    ]
    .padding(12)
    .spacing(8)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notion_core::model::Page;
    use serde_json::json;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn page(id: &str, parent: serde_json::Value) -> Page {
        let t = "2025-01-01T00:00:00.000Z";
        serde_json::from_value(json!({
            "object": "page", "id": id, "parent": parent, "created_time": t,
            "last_edited_time": t, "in_trash": false, "url": "https://notion.so/x",
            "properties": { "Name": { "id": "title", "type": "title", "title": [
                { "type": "text", "text": { "content": id }, "plain_text": id, "annotations": {} }] } },
        }))
        .unwrap()
    }

    #[test]
    fn sidebar_skips_data_source_rows() {
        let (mut s, _rx) = state();
        s.store
            .upsert_page(&page(
                "a",
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        s.store
            .upsert_page(&page(
                "row",
                json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" }),
            ))
            .unwrap();
        s.refresh();
        assert_eq!(s.pages, vec![("a".into(), "a".into())]);
    }

    #[test]
    fn pending_open_is_dropped_and_worker_loss_shown() {
        let (mut s, _rx) = state();
        up(&mut s, Message::PickResult("p1".into()));
        up(&mut s, Message::Open("p2".into()));
        let pulled = || {
            Message::Worker(Arc::new(Event::Pulled {
                id: "p1".into(),
                report: Default::default(),
            }))
        };
        up(&mut s, pulled());
        assert_eq!(s.current.as_deref(), Some("p2"), "user's choice wins");

        up(&mut s, Message::PickResult("p1".into()));
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("offline".into()))),
        );
        up(&mut s, pulled());
        assert_eq!(s.current.as_deref(), Some("p2"), "failed pick is forgotten");

        up(&mut s, Message::Pull { force: false });
        up(&mut s, Message::WorkerGone);
        assert_eq!(s.sent.len(), 0);
        assert_eq!(s.last_error.as_deref(), Some("sync worker stopped"));
    }

    fn state() -> (State, UnboundedReceiver<Cmd>) {
        let (tx, rx) = unbounded_channel();
        (State::new(Store::open_in_memory().unwrap(), tx), rx)
    }

    /// Page "pg" open with rows: a (child c), img, m (mention), b.
    fn editing_state() -> (State, UnboundedReceiver<Cmd>) {
        use crate::outline::tests::{blk, rt};
        let (mut s, rx) = state();
        s.store
            .upsert_page(&page(
                "pg",
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        let mention = json!([{ "type": "mention", "mention": { "type": "date", "date": { "start": "2025-01-01" } },
            "plain_text": "d", "annotations": {} }]);
        let para =
            |id: &str, parent: &str| blk(id, parent, "paragraph", json!({ "rich_text": rt(id) }));
        s.store
            .replace_children(
                "pg",
                "pg",
                &[
                    para("a", "pg"),
                    blk("img", "pg", "image", json!({ "type": "external" })),
                    blk("m", "pg", "paragraph", json!({ "rich_text": mention })),
                    para("b", "pg"),
                ],
            )
            .unwrap();
        s.store
            .replace_children("pg", "a", &[para("c", "a")])
            .unwrap();
        up(&mut s, Message::Open("pg".into()));
        (s, rx)
    }

    fn up(s: &mut State, m: Message) {
        let _ = update(s, m);
    }

    fn type_text(s: &mut State, t: &str) {
        for c in t.chars() {
            up(
                s,
                Message::EditorAction(text_editor::Action::Edit(text_editor::Edit::Insert(c))),
            );
        }
    }

    fn editing(s: &State) -> Option<&str> {
        s.editing.as_ref().map(|e| e.block_id.as_str())
    }

    fn ids(s: &State) -> Vec<&str> {
        s.rows.iter().map(|r| r.block_id.as_str()).collect()
    }

    #[test]
    fn pushed_drops_focus_and_commits_to_the_real_id() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        let local = editing(&s).unwrap().to_string();
        type_text(&mut s, "new");
        // what a push does to a created block
        s.store.rename_block(&local, "real").unwrap();
        s.store.map_id(&local, "real").unwrap();
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Pushed(Default::default()))),
        );
        assert_eq!(editing(&s), None);
        let row = s.rows.iter().find(|r| r.block_id == "real").unwrap();
        assert_eq!(notion_core::model::plain_text(&row.rich_text), "new");
    }

    #[test]
    fn code_rows_commit_raw_text() {
        use crate::outline::tests::{blk, rt};
        let (mut s, _rx) = editing_state();
        s.store
            .replace_children(
                "pg",
                "b",
                &[blk(
                    "code",
                    "b",
                    "code",
                    json!({ "rich_text": rt("x"), "language": "rust" }),
                )],
            )
            .unwrap();
        s.refresh();
        up(&mut s, Message::Focus("code".into()));
        assert!(s.editing.as_ref().unwrap().code);
        type_text(&mut s, " *y*");
        up(&mut s, Message::EditKey(EditKey::Escape));
        let row = s.rows.iter().find(|r| r.block_id == "code").unwrap();
        assert_eq!(row.rich_text.len(), 1);
        assert_eq!(row.rich_text[0].plain_text, "x *y*");
        assert!(!row.rich_text[0].annotations.italic);
    }

    #[test]
    fn push_is_modal() {
        let (mut s, mut rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "1");
        up(&mut s, Message::Pull { force: false });
        up(&mut s, Message::Push);
        assert_eq!(s.store.queued_ops().unwrap().len(), 1, "Ctrl+S committed");
        assert!(s.pushing());
        type_text(&mut s, "2");
        up(&mut s, Message::EditKey(EditKey::Enter));
        up(&mut s, Message::Focus("a".into()));
        assert_eq!(editing(&s), Some("b"));
        assert_eq!(
            s.editing.as_ref().unwrap().content.text(),
            "b1",
            "ignored mid-push"
        );
        assert_eq!(ids(&s).len(), 5, "no block created");

        // the pull's error answers the pull, not the push
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("offline".into()))),
        );
        assert!(s.pushing());
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Pushed(Default::default()))),
        );
        assert!(!s.pushing());
        assert_eq!(editing(&s), None);
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "pg".into(),
                force: false
            }
        );
        assert_eq!(rx.try_recv().unwrap(), Cmd::Push);
    }

    #[test]
    fn refresh_follows_a_renamed_block() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        let local = editing(&s).unwrap().to_string();
        type_text(&mut s, "hi");
        s.store.rename_block(&local, "real").unwrap();
        s.store.map_id(&local, "real").unwrap();
        // e.g. a push that failed on a later op
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("boom".into()))),
        );
        assert_eq!(editing(&s), Some("real"));
        assert_eq!(s.editing.as_ref().unwrap().content.text(), "hi");
    }

    #[test]
    fn failed_commit_keeps_the_editor() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "x");
        s.store.delete_block("b").unwrap();
        assert_eq!(s.write_edit(), None);
        assert!(s.editing.is_some());
        assert!(s.last_error.is_some());
    }

    #[test]
    fn non_editable_rows_refuse_focus() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("img".into()));
        assert_eq!(editing(&s), None);
        up(&mut s, Message::Focus("m".into()));
        assert_eq!(editing(&s), None, "mention is read-only");
        up(&mut s, Message::Focus("a".into()));
        assert_eq!(editing(&s), Some("a"));
        assert_eq!(s.editing.as_ref().unwrap().content.text(), "a");
    }

    #[test]
    fn commits_only_changed_text() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert_eq!(editing(&s), None);
        assert!(s.store.queued_ops().unwrap().is_empty(), "unchanged: no op");

        up(&mut s, Message::Focus("a".into()));
        type_text(&mut s, " **x**");
        // moving to another row commits
        up(&mut s, Message::Focus("b".into()));
        assert_eq!(s.store.queued_ops().unwrap().len(), 1);
        assert_eq!(s.counts[0], 1);
        let rt = &s.rows[0].rich_text;
        assert_eq!(notion_core::model::plain_text(rt), "a x");
        assert!(rt[1].annotations.bold);
    }

    #[test]
    fn enter_creates_sibling_after_and_backspace_deletes_it() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("c".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        let new = editing(&s).unwrap().to_string();
        assert!(new.starts_with("local-"));
        assert_eq!(ids(&s), vec!["a", "c", new.as_str(), "img", "m", "b"]);
        assert_eq!(s.rows[2].parent, "a", "same parent as c");
        assert_eq!(s.rows[2].depth, 1);

        up(&mut s, Message::EditKey(EditKey::Backspace));
        assert_eq!(ids(&s), vec!["a", "c", "img", "m", "b"]);
        assert_eq!(editing(&s), Some("c"), "focus moves to the previous row");
        assert!(
            s.store.queued_ops().unwrap().is_empty(),
            "unpushed create dropped"
        );
    }

    #[test]
    fn backspace_on_empty_row_with_children_keeps_it() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::EditKey(EditKey::Backspace)); // "a" -> ""
        assert_eq!(s.editing.as_ref().unwrap().content.text(), "");
        up(&mut s, Message::EditKey(EditKey::Backspace));
        assert_eq!(ids(&s)[..2], ["a", "c"]);
    }

    #[test]
    fn pull_keeps_editor_text() {
        let (mut s, mut rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "z");
        let pulled = Event::Pulled {
            id: "pg".into(),
            report: Default::default(),
        };
        up(&mut s, Message::Worker(Arc::new(pulled)));
        assert_eq!(s.editing.as_ref().unwrap().content.text(), "bz");

        // Ctrl+R commits before the pull goes out
        up(&mut s, Message::Pull { force: false });
        assert_eq!(s.store.queued_ops().unwrap().len(), 1);
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "pg".into(),
                force: false
            }
        );
        assert_eq!(editing(&s), Some("b"), "still editing");
    }

    #[test]
    fn up_down_move_between_editable_rows() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("c".into()));
        up(&mut s, Message::EditKey(EditKey::Down));
        assert_eq!(editing(&s), Some("b"), "skips img and the mention");
        up(&mut s, Message::EditKey(EditKey::Down));
        assert_eq!(editing(&s), Some("b"), "last row: stay");
        up(&mut s, Message::EditKey(EditKey::Up));
        assert_eq!(editing(&s), Some("c"));
    }

    fn key(c: &str, ctrl: bool, shift: bool) -> iced::Event {
        let mut m = keyboard::Modifiers::empty();
        m.set(keyboard::Modifiers::CTRL, ctrl);
        m.set(keyboard::Modifiers::SHIFT, shift);
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character(c.into()),
            modified_key: Key::Character(c.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: m,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn shortcuts() {
        let id = iced::window::Id::unique();
        let st = event::Status::Captured;
        let k = |c, ctrl, shift| shortcut(key(c, ctrl, shift), st, id);
        assert!(matches!(
            k("r", true, false),
            Some(Message::Pull { force: false })
        ));
        assert!(matches!(
            k("R", true, true),
            Some(Message::Pull { force: true })
        ));
        assert!(matches!(k("s", true, false), Some(Message::Push)));
        assert!(k("s", false, false).is_none());
        let mut held = key("s", true, false);
        if let iced::Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut held {
            *repeat = true;
        }
        assert!(shortcut(held, st, id).is_none(), "key repeat is ignored");
    }

    #[test]
    fn push_then_repull_and_pick_result() {
        let (mut s, mut rx) = state();
        up(&mut s, Message::Pull { force: false });
        assert!(rx.try_recv().is_err(), "no page open: nothing to pull");

        up(&mut s, Message::PickResult("p1".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "p1".into(),
                force: false
            }
        );
        assert_eq!(s.current, None, "not cached yet");
        let pulled = Event::Pulled {
            id: "p1".into(),
            report: Default::default(),
        };
        up(&mut s, Message::Worker(Arc::new(pulled)));
        assert_eq!(s.current.as_deref(), Some("p1"));

        up(&mut s, Message::Push);
        assert_eq!(rx.try_recv().unwrap(), Cmd::Push);
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Pushed(Default::default()))),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "p1".into(),
                force: true
            }
        );
        assert_eq!(s.sent.len(), 1, "the re-pull is out");

        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("boom".into()))),
        );
        assert_eq!(s.last_error.as_deref(), Some("boom"));
        assert_eq!(s.sent.len(), 0);
    }
}
