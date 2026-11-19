//! App state, update and view. `update` only touches the store, the editor
//! buffer and the worker channel (its Task is just widget focus), so it is
//! testable without a window.

use std::collections::VecDeque;
use std::sync::Arc;

use iced::font::{Family, Style, Weight};
use iced::keyboard::{self, Key};
use iced::widget::text::Span;
use iced::widget::{
    button, column, container, mouse_area, operation, rich_text, row, scrollable, span, text,
    text_editor, text_input,
};
use iced::{event, Color, Element, Fill, Font, Subscription, Task};
use notion_core::markdown::from_markdown;
use notion_core::model::{plain_text, TargetKind};
use notion_core::model::{BlockKind, Parent, RichText, RichTextKind, TextBlock};
use notion_core::ops::OpStatus;
use notion_core::store::Store;
use notion_core::sync::{self, ChangeOutcome};
use tokio::sync::mpsc::UnboundedSender;

use crate::edit::{key_action, EditKey, Editing, KeyAction, EDITOR_ID};
use crate::keys::{self, Chord, Command, Keymap};
use crate::outline::{self, Marker, Row, Table};
use crate::slash::{self, Menu, MenuKey};
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
    /// Table cell clicked (table_row id, column): edit it if it is editable.
    FocusCell(String, usize),
    /// Link card clicked: hand the URL to xdg-open.
    OpenLink(String),
    /// child_database / database link clicked: tried like a page.
    OpenDatabase(String),
    EditorAction(text_editor::Action),
    /// A key the row editor hands to the app.
    EditKey(EditKey),
    /// ☐/☑ clicked.
    ToggleCheck(String),
    /// `/` menu entry chosen (index into the filtered list).
    SlashPick(usize),
    /// Any key press, seen window-wide: runs its global keymap command.
    Chord(Chord),
    /// A keymap command from the row editor (block type, mark, new block).
    Command(Command),
    /// Ctrl+K
    FocusSearch,
    /// Esc anywhere: back from the search box to the outline.
    SearchEscape,
}

const SEARCH_ID: &str = "search";

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
    /// Last database opened; its pull failing gets a friendlier error.
    pending_db: Option<String>,
    /// One entry per command sent and not yet answered (the worker answers
    /// each with one event, in order); true = Push.
    sent: VecDeque<bool>,
    /// The focused row, if any.
    editing: Option<Editing>,
    keys: Keymap,
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
            pending_db: None,
            sent: VecDeque::new(),
            editing: None,
            keys: Keymap::default(),
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
        // a commit ends the `/` menu; its query is kept as typed
        e.menu = None;
        let Some(rt) = e.changed() else {
            return Some(true);
        };
        // a push may have renamed a local-* block meanwhile
        let res = self.store.resolve_id(&e.block_id).and_then(|id| {
            if let Some((row, col)) = &e.cell {
                sync::local_edit_table_cell(&self.store, row, *col, rt)?;
            } else if e.equation {
                sync::local_edit_equation(&self.store, &id, &plain_text(&rt))?;
            } else {
                sync::local_edit_rich(&self.store, &id, rt)?;
            }
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

    /// The editable cell `col` of table row `row_id`: (table id, its text).
    fn cell(&self, row_id: &str, col: usize) -> Option<(&str, &[RichText])> {
        self.rows.iter().find_map(|r| match &r.marker {
            Marker::Table(t) if !r.locked => t
                .rows
                .iter()
                .find(|tr| tr.id == row_id)
                .and_then(|tr| tr.cells.get(col))
                .filter(|c| c.editable)
                .map(|c| (r.block_id.as_str(), c.rich_text.as_slice())),
            _ => None,
        })
    }

    /// The edited row, or cell, is still in the outline.
    fn edited_exists(&self) -> bool {
        match self.editing.as_ref().and_then(|e| e.cell.as_ref()) {
            // editable or not: a cell that turned read-only keeps its typed text
            Some((row, col)) => self.rows.iter().any(|r| match &r.marker {
                Marker::Table(t) => t
                    .rows
                    .iter()
                    .any(|tr| tr.id == *row && tr.cells.len() > *col),
                _ => false,
            }),
            None => self.editing_row().is_some(),
        }
    }

    /// Commit the current row and start editing a table cell, if it is editable.
    fn focus_cell(&mut self, row_id: &str, col: usize) -> Task<Message> {
        if !self.commit() {
            return Task::none();
        }
        let found = self
            .cell(row_id, col)
            .map(|(table, rt)| Editing::cell(table, row_id, col, rt));
        let Some(e) = found else {
            return Task::none();
        };
        self.editing = Some(e);
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
        if self.editing.is_some() && !self.edited_exists() {
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

impl State {
    /// Installs the user's keymap; `problems` go to the status bar.
    pub fn set_keys(&mut self, keys: Keymap, problems: Vec<String>) {
        self.keys = keys;
        if !problems.is_empty() {
            self.last_error = Some(problems.join("; "));
        }
    }
}

pub fn update(s: &mut State, msg: Message) -> Task<Message> {
    match msg {
        // a push is modal: the worker may already have sent the ops an edit
        // would rewrite
        Message::Focus(_)
        | Message::FocusCell(..)
        | Message::OpenDatabase(_)
        | Message::EditorAction(_)
        | Message::EditKey(_)
        | Message::ToggleCheck(_)
        | Message::SlashPick(_)
        | Message::Command(_)
        | Message::FocusSearch
        | Message::SearchEscape
        | Message::Open(_)
        | Message::PickResult(_)
            if s.pushing() => {}
        Message::Open(id) => {
            if !s.leave() {
                return Task::none();
            }
            s.pending_open = None;
            s.pending_db = None;
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
            // a failed commit keeps the editor; pushing now would lose the text
            if s.commit() {
                s.send(Cmd::Push);
            }
        }
        Message::SearchInput(q) => s.search = q,
        Message::SearchSubmit => {
            let q = s.search.trim().to_string();
            if !q.is_empty() {
                s.send(Cmd::Search(q));
            }
        }
        Message::OpenDatabase(id) => {
            s.pending_db = Some(id.clone());
            return update(s, Message::PickResult(id));
        }
        Message::OpenLink(url) => {
            if let Err(e) = open_url(&url) {
                s.last_error = Some(e);
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
        Message::ToggleCheck(id) => return toggle_check(s, &id),
        Message::FocusSearch => {
            s.commit();
            return Task::batch([
                operation::focus(SEARCH_ID),
                operation::select_all(SEARCH_ID),
            ]);
        }
        Message::SearchEscape => {
            return if s.editing.is_some() {
                operation::focus(EDITOR_ID)
            } else {
                iced::advanced::widget::operate(
                    iced::advanced::widget::operation::focusable::unfocus(),
                )
            };
        }
        Message::Focus(id) => return s.focus(&id),
        Message::FocusCell(row, col) => return s.focus_cell(&row, col),
        Message::EditorAction(a) => return on_editor_action(s, a),
        Message::SlashPick(i) => return slash_pick(s, i),
        // the message it maps to applies its own push rule
        Message::Chord(c) => {
            let msg = match s.keys.get(&c) {
                Some(Command::Push) => Message::Push,
                Some(Command::Pull) => Message::Pull { force: false },
                Some(Command::ForcePull) => Message::Pull { force: true },
                Some(Command::Search) => Message::FocusSearch,
                _ => return Task::none(),
            };
            return update(s, msg);
        }
        Message::Command(c) => return on_command(s, c),
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
                        s.pending_db = None;
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
                    let db = s.pending_open.is_some() && s.pending_open == s.pending_db;
                    s.pending_open = None;
                    s.pending_db = None;
                    // a push can fail after applying some ops
                    s.refresh();
                    s.last_error = Some(if db {
                        "databases not supported yet".into()
                    } else {
                        e.clone()
                    });
                }
            }
        }
    }
    Task::none()
}

/// Typing: opens the `/` menu, re-checks its query, fires Markdown shortcuts.
fn on_editor_action(s: &mut State, a: text_editor::Action) -> Task<Message> {
    let Some(e) = &mut s.editing else {
        return Task::none();
    };
    let typed = match &a {
        text_editor::Action::Edit(text_editor::Edit::Insert(c)) => Some(*c),
        _ => None,
    };
    e.content.perform(a);
    let (line, col) = e.pos();
    if e.menu.is_some() {
        e.menu_query();
    } else if typed == Some('/') && !e.code && e.cell.is_none() {
        e.menu = Some(Menu {
            line,
            slash: col - 1,
            selected: 0,
        });
    } else if typed.is_some() {
        // Notion only converts plain text blocks
        // ...without children: core refuses those, no error while typing
        let paragraph = s.editing_row().is_some_and(|r| {
            r.marker == Marker::None
                && s.store
                    .get_block(&r.block_id)
                    .ok()
                    .flatten()
                    .is_some_and(|b| !b.has_children)
                && s.store.children(&r.block_id).is_ok_and(|c| c.is_empty())
        });
        let text = s
            .editing
            .as_ref()
            .map(|e| e.content.text())
            .unwrap_or_default();
        if let Some((kind, rest)) = paragraph
            .then(|| slash::shortcut(&text, line, col))
            .flatten()
        {
            return convert(s, kind, &rest, (0, 0));
        }
    }
    Task::none()
}

fn slash_pick(s: &mut State, i: usize) -> Task<Message> {
    let Some(e) = &mut s.editing else {
        return Task::none();
    };
    let Some(q) = e.menu_query() else {
        return Task::none();
    };
    let Some(item) = slash::filter(&q).get(i).copied() else {
        return Task::none();
    };
    let m = e.menu.clone().expect("menu_query kept it open");
    let (_, col) = e.pos();
    let buf = m.strip(&e.content.text(), col);
    convert(s, item.kind, &buf, (m.line, m.slash))
}

/// A keymap command for the row editor.
fn on_command(s: &mut State, c: Command) -> Task<Message> {
    let Some(e) = &mut s.editing else {
        return Task::none();
    };
    e.menu = None;
    match c {
        // same path as a `/` pick, same refusals
        Command::Block(kind) => {
            let (buf, pos) = (e.content.text(), e.pos());
            convert(s, kind, &buf, pos)
        }
        // raw-text rows have no Markdown
        Command::Mark(m) if !e.code => {
            keys::wrap(&mut e.content, m);
            Task::none()
        }
        Command::NewBlock => on_edit_key(s, EditKey::Enter),
        _ => Task::none(),
    }
}

/// Converts the edited block to `kind` and continues editing it with `buf`
/// (cursor at `pos`). On refusal the buffer is left as typed.
fn convert(s: &mut State, kind: TargetKind, buf: &str, pos: (usize, usize)) -> Task<Message> {
    // a table cell is not a block
    let Some(id) = s
        .editing
        .as_ref()
        .filter(|e| e.cell.is_none())
        .map(|e| e.block_id.clone())
    else {
        return Task::none();
    };
    if kind == TargetKind::Divider && !buf.trim().is_empty() {
        return divider_after(s, buf, pos);
    }
    // code is raw text: drop the Markdown marks (an equation's buffer is TeX
    // already, `*` and `_` included)
    let (buf, pos) = if kind == TargetKind::Code {
        // ponytail: cursor to the start, mapping it through the stripped marks isn't worth it
        (plain_text(&from_markdown(buf)), (0, 0))
    } else {
        (buf.to_string(), pos)
    };
    let buf = buf.as_str();
    let res = s
        .store
        .resolve_id(&id)
        .and_then(|id| sync::local_change_type(&s.store, &id, kind).map(|o| (id, o)));
    let new = match res {
        Ok((_, ChangeOutcome::Replaced(new))) => new,
        Ok((id, ChangeOutcome::Kept)) => id,
        Err(err) => {
            s.last_error = Some(err.to_string());
            if let Some(e) = &mut s.editing {
                e.menu = None;
            }
            return Task::none();
        }
    };
    // the old id may be gone: don't let refresh discard the editor
    s.editing = None;
    s.refresh();
    let Some(row) = s.rows.iter().find(|r| r.block_id == new).cloned() else {
        return Task::none();
    };
    if !row.editable {
        // a divider has no text: continue in a new paragraph below it
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: Vec::new(),
            color: "default".into(),
        });
        return match sync::local_create_block(&s.store, &row.parent, Some(&row.block_id), para) {
            Ok(p) => {
                s.refresh();
                s.focus(&p)
            }
            Err(err) => {
                s.last_error = Some(err.to_string());
                Task::none()
            }
        };
    }
    let mut e = Editing::new(&row);
    e.set_buffer(buf, pos.0, pos.1);
    s.editing = Some(e);
    s.commit();
    operation::focus(EDITOR_ID)
}

/// Divider picked on a block with text: keep the block, put a divider after it.
fn divider_after(s: &mut State, buf: &str, pos: (usize, usize)) -> Task<Message> {
    if let Some(e) = &mut s.editing {
        e.set_buffer(buf, pos.0, pos.1);
    }
    if !s.commit() {
        return Task::none();
    }
    let Some(row) = s.editing_row().cloned() else {
        return Task::none();
    };
    match sync::local_create_block(
        &s.store,
        &row.parent,
        Some(&row.block_id),
        BlockKind::Divider,
    ) {
        Ok(_) => s.refresh(),
        Err(err) => s.last_error = Some(err.to_string()),
    }
    operation::focus(EDITOR_ID)
}

fn on_edit_key(s: &mut State, k: EditKey) -> Task<Message> {
    let Some(e) = &mut s.editing else {
        return Task::none();
    };
    if e.menu.is_some() {
        let n = e.menu_query().map_or(0, |q| slash::filter(&q).len());
        // menu_query closed it: the key is the editor's
        if let Some(m) = e.menu.as_mut().filter(|_| n > 0) {
            match slash::menu_key(k) {
                MenuKey::Prev => m.selected = (m.selected + n - 1) % n,
                MenuKey::Next => m.selected = (m.selected + 1) % n,
                MenuKey::Pick => {
                    let i = m.selected;
                    return slash_pick(s, i);
                }
                MenuKey::Close => e.menu = None,
                MenuKey::Ignore => {}
                MenuKey::Editor => {
                    e.content
                        .perform(text_editor::Action::Edit(text_editor::Edit::Backspace));
                    e.menu_query();
                }
            }
            return Task::none();
        }
    }
    // a table cell: only text editing; the row/block keys commit and leave
    if e.cell.is_some() {
        match key_action(k, e.caret()) {
            KeyAction::Pass(a) => e.content.perform(a),
            KeyAction::NewBlockAfter
            | KeyAction::Leave
            | KeyAction::Indent
            | KeyAction::Outdent => {
                s.leave();
            }
            _ => {}
        }
        return Task::none();
    }
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
            if !s.commit() {
                return Task::none();
            }
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
        KeyAction::Indent => return move_editing(s, outline::indent_target),
        KeyAction::Outdent => return move_editing(s, outline::outdent_target),
        KeyAction::ToggleCheck => return toggle_check(s, &id),
    }
    Task::none()
}

type Target = fn(&[Row], usize) -> Option<(String, Option<String>)>;

/// Tab / Shift+Tab: commit, then move the edited block to `target`. Focus
/// stays on it; its id only changes on push.
fn move_editing(s: &mut State, target: Target) -> Task<Message> {
    if !s.commit() {
        return Task::none();
    }
    let Some(id) = s.editing.as_ref().map(|e| e.block_id.clone()) else {
        return Task::none();
    };
    let Some(idx) = s.rows.iter().position(|r| r.block_id == id) else {
        return Task::none();
    };
    let Some((parent, after)) = target(&s.rows, idx) else {
        return Task::none();
    };
    // push would refuse it anyway (copy-then-delete can't recreate these)
    if let Some(m) = outline::unmovable_in_subtree(&s.rows, idx) {
        s.last_error = Some(format!("can't move: its subtree contains {}", m.label()));
        return Task::none();
    }
    if let Err(e) = sync::local_move_block(&s.store, &id, &parent, after.as_deref()) {
        s.last_error = Some(e.to_string());
    }
    s.refresh();
    operation::focus(EDITOR_ID)
}

/// Ctrl+Enter or a click on ☐/☑: commit, then flip the to-do. No-op otherwise.
fn toggle_check(s: &mut State, id: &str) -> Task<Message> {
    if !s.commit() {
        return Task::none();
    }
    // a synced block's to-do is locked
    let Some(Marker::Todo(checked)) = s
        .rows
        .iter()
        .find(|r| r.block_id == id && !r.locked)
        .map(|r| &r.marker)
    else {
        return Task::none();
    };
    let checked = *checked;
    let res = s
        .store
        .resolve_id(id)
        .and_then(|id| sync::local_set_checked(&s.store, &id, !checked));
    if let Err(e) = res {
        s.last_error = Some(e.to_string());
    }
    s.refresh();
    if s.editing.is_some() {
        operation::focus(EDITOR_ID)
    } else {
        Task::none()
    }
}

/// Every key press as a [`Message::Chord`] (the keymap is resolved in
/// `update`), plus a hard-wired Esc back to the outline. Sees events even when
/// a widget captured them (an Esc the editor also handles is harmless: it
/// refocuses or unfocuses).
fn shortcut(ev: iced::Event, _: event::Status, _: iced::window::Id) -> Option<Message> {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        text,
        repeat: false,
        ..
    }) = ev
    else {
        return None;
    };
    if key == Key::Named(keyboard::key::Named::Escape) && modifiers.is_empty() {
        return Some(Message::SearchEscape);
    }
    // plain typing: no message, no view rebuild
    if text.is_some() && !(modifiers.control() || modifiers.alt() || modifiers.logo()) {
        return None;
    }
    Chord::of(&key, physical_key, modifiers).map(Message::Chord)
}

pub fn subscription(_: &State) -> Subscription<Message> {
    event::listen_with(shortcut)
}

/// Only web links go to xdg-open (a file:// one could start a local
/// script or .desktop file).
fn allowed_url(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["http://", "https://"].iter().any(|p| l.starts_with(p))
}

/// Starts `xdg-open url` without waiting for it.
fn open_url(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    if !allowed_url(url) {
        return Err(format!("not opening {url}: only http and https links"));
    }
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("xdg-open: {e}"))?;
    // reap it so it doesn't linger as a zombie
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// What clicking a read-only row does, if anything.
fn click(r: &Row) -> Option<Message> {
    Some(match &r.marker {
        Marker::LinkCard { url, .. } => Message::OpenLink(url.clone()),
        Marker::LinkToPage {
            id,
            database: false,
            ..
        } => Message::PickResult(id.clone()),
        Marker::LinkToPage { id, .. } => Message::OpenDatabase(id.clone()),
        // a child page's / database's block id is its own id
        Marker::ChildPage(_) => Message::PickResult(r.block_id.clone()),
        Marker::ChildDatabase(_) => Message::OpenDatabase(r.block_id.clone()),
        _ => return None,
    })
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
    if let RichTextKind::Equation { expression } = &t.kind {
        // Unicode pretty-print in italic serif; the TeX source in monospace if it can't
        return match crate::tex::pretty_tex(expression) {
            Some(p) => span(p).font(Font {
                family: Family::Serif,
                style: Style::Italic,
                ..Font::DEFAULT
            }),
            None => span(expression.as_str()).font(Font::MONOSPACE),
        };
    }
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

fn editor<'a>(e: &'a Editing, keys: &'a Keymap, size: u32) -> Element<'a, Message> {
    text_editor(&e.content)
        .id(EDITOR_ID)
        .on_action(Message::EditorAction)
        .key_binding(move |kp| crate::edit::binding(kp, keys))
        .font(if e.code {
            Font::MONOSPACE
        } else {
            Font::DEFAULT
        })
        .size(size)
        .into()
}

/// A table as a grid; header row / column in bold, the edited cell as an editor.
fn table_view<'a>(
    t: &'a Table,
    editing: Option<&'a Editing>,
    keys: &'a Keymap,
) -> Element<'a, Message> {
    let rows = t.rows.iter().enumerate().map(|(i, tr)| {
        let cells = tr.cells.iter().enumerate().map(|(j, c)| {
            let bold = (t.column_header && i == 0) || (t.row_header && j == 0);
            let edited = editing.filter(|e| {
                e.cell
                    .as_ref()
                    .is_some_and(|(id, col)| *id == tr.id && *col == j)
            });
            let body: Element<_> = match edited {
                Some(e) => editor(e, keys, 15),
                None => {
                    let spans: Vec<Span<'_, ()>> = c
                        .rich_text
                        .iter()
                        .map(|rt| {
                            let mut sp = styled(rt);
                            if bold {
                                sp.font = Some(Font {
                                    weight: Weight::Bold,
                                    ..sp.font.unwrap_or_default()
                                });
                            }
                            sp
                        })
                        .collect();
                    let cell = container(rich_text(spans).size(15)).width(Fill);
                    if c.editable {
                        mouse_area(cell)
                            .on_press(Message::FocusCell(tr.id.clone(), j))
                            .into()
                    } else {
                        cell.into()
                    }
                }
            };
            container(body)
                .padding(4)
                .width(Fill)
                .style(container::bordered_box)
                .into()
        });
        row(cells).into()
    });
    column(rows).into()
}

fn row_view<'a>(
    r: &'a Row,
    editing: Option<&'a Editing>,
    keys: &'a Keymap,
) -> Element<'a, Message> {
    let size = match r.marker {
        Marker::Heading(1) => 28,
        Marker::Heading(2) => 22,
        Marker::Heading(3) => 18,
        Marker::Equation => 20,
        _ => 15,
    };
    let grey = matches!(
        r.marker,
        Marker::Unsupported(_)
            | Marker::Divider
            | Marker::Synced
            | Marker::ColumnList
            | Marker::Column
            | Marker::TableOfContents
            | Marker::Breadcrumb
    );
    let link = click(r).is_some();
    let indent = iced::Padding::ZERO.left(r.depth as f32 * 24.0);
    if let Marker::Table(t) = &r.marker {
        let editing = editing.filter(|e| e.block_id == r.block_id);
        return container(table_view(t, editing, keys))
            .padding(indent)
            .into();
    }
    let label = r.marker.label();
    if let Some(e) = editing.filter(|e| e.block_id == r.block_id) {
        let editor = editor(e, keys, size);
        let body: Element<_> = if label.is_empty() {
            editor
        } else {
            let marker = mouse_area(text(label).size(size))
                .on_press(Message::ToggleCheck(r.block_id.clone()));
            row![marker, editor].spacing(6).into()
        };
        let body: Element<_> = match e.menu.as_ref() {
            None => body,
            Some(m) => column![body, slash_menu(e, m)].spacing(4).into(),
        };
        return container(body).padding(indent).into();
    }
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    // to-do box is its own click target; it captures the press, so the
    // row's Focus doesn't fire too
    let todo = matches!(r.marker, Marker::Todo(_)).then(|| {
        mouse_area(text(label.clone()).size(size))
            .on_press(Message::ToggleCheck(r.block_id.clone()))
    });
    if todo.is_none() && !label.is_empty() {
        let sep = if matches!(r.marker, Marker::Code(_) | Marker::TableOfContents) {
            "\n"
        } else {
            " "
        };
        let color = if link {
            Some(LINK)
        } else {
            grey.then_some(GREY)
        };
        spans.push(span(format!("{label}{sep}")).color_maybe(color));
    }
    spans.extend(r.rich_text.iter().map(styled));
    let content: Element<_> = match todo {
        Some(t) => row![t, rich_text(spans).size(size)].spacing(6).into(),
        None => rich_text(spans).size(size).into(),
    };
    let mut body = container(content).padding(indent).width(Fill);
    if r.marker == Marker::Equation {
        body = body.center_x(Fill);
    }
    if r.editable {
        mouse_area(body)
            .on_press(Message::Focus(r.block_id.clone()))
            .into()
    } else if let Some(msg) = click(r) {
        mouse_area(body)
            .on_press(msg)
            .interaction(iced::mouse::Interaction::Pointer)
            .into()
    } else {
        body.into()
    }
}

/// The `/` menu, drawn in the outline right under the edited row.
fn slash_menu<'a>(e: &Editing, m: &Menu) -> Element<'a, Message> {
    let (line, col) = e.pos();
    let buf = e.content.text();
    let q = m.query(&buf, line, col).unwrap_or_default();
    let items = slash::filter(q).into_iter().enumerate().map(|(i, it)| {
        let style = if i == m.selected {
            button::primary
        } else {
            button::text
        };
        button(row![text(it.name).width(140), text(it.hint).size(12).color(GREY)].spacing(8))
            .on_press(Message::SlashPick(i))
            .style(style)
            .width(Fill)
            .into()
    });
    container(column(items).spacing(2))
        .style(container::bordered_box)
        .padding(4)
        .max_width(360)
        .into()
}

pub fn view(s: &State) -> Element<'_, Message> {
    let mut side = column![text_input("Search Notion… (Ctrl+K)", &s.search)
        .id(SEARCH_ID)
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
            scrollable(
                column(
                    s.rows
                        .iter()
                        .map(|r| row_view(r, s.editing.as_ref(), &s.keys))
                )
                .spacing(6)
            )
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

    fn depth(s: &State, id: &str) -> usize {
        s.rows.iter().find(|r| r.block_id == id).unwrap().depth
    }

    fn kinds(s: &State) -> Vec<String> {
        s.store
            .queued_ops()
            .unwrap()
            .iter()
            .map(|o| {
                serde_json::to_value(&o.kind).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn tab_and_shift_tab_move_the_block_and_keep_focus() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("c".into()));
        up(&mut s, Message::EditKey(EditKey::Outdent));
        assert_eq!(ids(&s), vec!["a", "c", "img", "m", "b"]);
        assert_eq!(depth(&s, "c"), 0);
        assert_eq!(editing(&s), Some("c"));
        up(&mut s, Message::EditKey(EditKey::Outdent));
        assert_eq!(depth(&s, "c"), 0, "top level: no-op");

        up(&mut s, Message::EditKey(EditKey::Indent));
        assert_eq!(depth(&s, "c"), 1);
        assert_eq!(s.rows[1].parent, "a");
        assert_eq!(editing(&s), Some("c"));
        assert!(
            kinds(&s).iter().all(|k| k == "move_block"),
            "{:?}",
            kinds(&s)
        );

        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::EditKey(EditKey::Indent));
        assert_eq!(depth(&s, "a"), 0, "first block: no previous sibling");
    }

    #[test]
    fn move_is_refused_for_unmovable_subtrees() {
        use crate::outline::tests::blk;
        for (t, payload) in [
            ("image", json!({ "type": "external" })),
            ("child_page", json!({ "title": "Sub" })),
        ] {
            let (mut s, _rx) = editing_state();
            s.store
                .replace_children("pg", "b", &[blk("u", "b", t, payload)])
                .unwrap();
            s.refresh();
            up(&mut s, Message::Focus("b".into()));
            up(&mut s, Message::EditKey(EditKey::Indent));
            assert_eq!(depth(&s, "b"), 0, "{t}");
            assert!(s.store.queued_ops().unwrap().is_empty());
            assert!(s.last_error.as_deref().unwrap().contains("can't move"));
        }
    }

    #[test]
    fn ctrl_enter_and_click_toggle_todos_only() {
        use crate::outline::tests::{blk, rt};
        let (mut s, _rx) = editing_state();
        s.store
            .replace_children(
                "pg",
                "b",
                &[blk(
                    "t",
                    "b",
                    "to_do",
                    json!({ "rich_text": rt("t"), "checked": false }),
                )],
            )
            .unwrap();
        s.refresh();
        let marker = |s: &State| {
            s.rows
                .iter()
                .find(|r| r.block_id == "t")
                .unwrap()
                .marker
                .clone()
        };

        up(&mut s, Message::Focus("t".into()));
        type_text(&mut s, "!");
        up(&mut s, Message::EditKey(EditKey::ToggleCheck));
        assert_eq!(marker(&s), Marker::Todo(true));
        assert_eq!(editing(&s), Some("t"));
        assert_eq!(
            kinds(&s),
            vec!["update_block"; 2],
            "commit, then the checkbox"
        );
        let t = s.store.get_block("t").unwrap().unwrap();
        assert_eq!(
            notion_core::model::plain_text(&t.kind.text().unwrap().rich_text),
            "t!"
        );

        up(&mut s, Message::EditKey(EditKey::Escape));
        up(&mut s, Message::ToggleCheck("t".into()));
        assert_eq!(marker(&s), Marker::Todo(false), "click on the box");

        let before = s.store.queued_ops().unwrap().len();
        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::EditKey(EditKey::ToggleCheck));
        up(&mut s, Message::ToggleCheck("a".into()));
        assert_eq!(s.store.queued_ops().unwrap().len(), before, "not a to-do");
    }

    #[test]
    fn failed_commit_blocks_push_and_new_block() {
        let (mut s, mut rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "x");
        s.store.delete_block("b").unwrap();
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert!(
            !ids(&s).iter().any(|i| i.starts_with("local-")),
            "(b) no block created"
        );

        up(&mut s, Message::Focus("a".into()));
        type_text(&mut s, "y");
        s.store.delete_block("a").unwrap();
        up(&mut s, Message::Push);
        assert!(rx.try_recv().is_err(), "(a) push not sent");

        // (a) Open and PickResult are ignored mid-push
        s.store.replace_children("pg", "pg", &[]).unwrap();
        up(&mut s, Message::Push);
        assert!(s.pushing());
        up(&mut s, Message::Open("other".into()));
        up(&mut s, Message::PickResult("other".into()));
        assert_eq!(s.current.as_deref(), Some("pg"));
        assert_eq!(rx.try_recv().unwrap(), Cmd::Push);
        assert!(rx.try_recv().is_err());
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
        let (mut s, mut rx) = state();
        s.current = Some("p".into());
        let mut k = |c, ctrl, shift| {
            if let Some(m) = shortcut(key(c, ctrl, shift), st, id) {
                up(&mut s, m);
            }
        };
        k("r", true, false);
        k("R", true, true);
        k("s", true, false);
        k("s", false, false);
        let mut got = Vec::new();
        while let Ok(c) = rx.try_recv() {
            got.push(c);
        }
        assert_eq!(
            got,
            [
                Cmd::Pull {
                    id: "p".into(),
                    force: false
                },
                Cmd::Pull {
                    id: "p".into(),
                    force: true
                },
                Cmd::Push
            ]
        );
        let mut held = key("s", true, false);
        if let iced::Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut held {
            *repeat = true;
        }
        assert!(shortcut(held, st, id).is_none(), "key repeat is ignored");
        let with_text = |c: &str, ctrl: bool| {
            let mut e = key(c, ctrl, false);
            if let iced::Event::Keyboard(keyboard::Event::KeyPressed { text, .. }) = &mut e {
                *text = Some(c.into());
            }
            shortcut(e, st, id)
        };
        assert!(
            with_text("a", false).is_none(),
            "plain typing sends nothing"
        );
        assert!(matches!(with_text("s", true), Some(Message::Chord(_))));
        let f5 = iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(keyboard::key::Named::F5),
            modified_key: Key::Named(keyboard::key::Named::F5),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        assert!(matches!(shortcut(f5, st, id), Some(Message::Chord(_))));
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

    fn marker(s: &State) -> Marker {
        s.editing_row().unwrap().marker.clone()
    }

    fn buffer(s: &State) -> String {
        s.editing.as_ref().unwrap().content.text()
    }

    fn menu_open(s: &State) -> bool {
        s.editing.as_ref().unwrap().menu.is_some()
    }

    #[test]
    fn slash_h1_on_a_fresh_block_keeps_it() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        let local = editing(&s).unwrap().to_string();
        type_text(&mut s, "/h1");
        assert!(menu_open(&s));
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_eq!(editing(&s), Some(local.as_str()));
        assert_eq!(marker(&s), Marker::Heading(1));
        assert_eq!(buffer(&s), "");
        assert!(!menu_open(&s));
    }

    #[test]
    fn slash_on_a_real_block_follows_the_replacement() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "/quo");
        // Down then Up wraps back to the only match
        up(&mut s, Message::EditKey(EditKey::Down));
        up(&mut s, Message::EditKey(EditKey::Up));
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert!(editing(&s).unwrap().starts_with("local-"));
        assert_eq!(marker(&s), Marker::Quote);
        assert_eq!(buffer(&s), "b");
        assert!(!ids(&s).contains(&"b"));
    }

    #[test]
    fn refused_change_keeps_the_text() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("a".into()));
        type_text(&mut s, "/h1");
        up(&mut s, Message::SlashPick(0));
        assert!(s.last_error.is_some());
        assert_eq!(editing(&s), Some("a"));
        assert_eq!(buffer(&s), "a/h1");
        assert_eq!(marker(&s), Marker::None);
        assert!(!menu_open(&s));
    }

    #[test]
    fn menu_closes_on_no_match_backspace_and_escape() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "/zzz");
        assert!(!menu_open(&s));
        // Enter is the editor's again: a new block
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_ne!(editing(&s), Some("b"));

        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "/h");
        up(&mut s, Message::EditKey(EditKey::Backspace));
        assert!(menu_open(&s));
        up(&mut s, Message::EditKey(EditKey::Backspace));
        assert!(!menu_open(&s));
        assert_eq!(buffer(&s), "b/zzz");

        type_text(&mut s, "/");
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert!(!menu_open(&s));
        assert_eq!(editing(&s), Some("b"));
    }

    #[test]
    fn markdown_shortcut_only_on_paragraphs() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        type_text(&mut s, "# ");
        assert_eq!(marker(&s), Marker::Heading(1));
        assert_eq!(buffer(&s), "");
        type_text(&mut s, "# ");
        assert_eq!(marker(&s), Marker::Heading(1));
        assert_eq!(buffer(&s), "# ");
    }

    #[test]
    fn markdown_shortcut_keeps_the_rest() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        let e = s.editing.as_mut().unwrap();
        e.set_buffer("b", 0, 0);
        type_text(&mut s, "-");
        type_text(&mut s, " ");
        assert_eq!(marker(&s), Marker::Bullet);
        assert_eq!(buffer(&s), "b");
    }

    #[test]
    fn divider_and_code_shortcuts() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        type_text(&mut s, "---");
        // continues in a fresh paragraph under the divider
        assert_eq!(marker(&s), Marker::None);
        let i = s
            .rows
            .iter()
            .position(|r| Some(r.block_id.as_str()) == editing(&s))
            .unwrap();
        assert_eq!(s.rows[i - 1].marker, Marker::Divider);
        type_text(&mut s, "```");
        assert!(matches!(marker(&s), Marker::Code(_)));
        assert_eq!(buffer(&s), "");
    }

    #[test]
    fn divider_on_a_block_with_text_keeps_it() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "/div");
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_eq!(editing(&s), Some("b"));
        assert_eq!(buffer(&s), "b");
        let i = s.rows.iter().position(|r| r.block_id == "b").unwrap();
        assert_eq!(s.rows[i + 1].marker, Marker::Divider);
        assert_eq!(plain_text(&s.rows[i].rich_text), "b");
    }

    #[test]
    fn code_conversion_drops_markdown_marks() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, " **x**/code");
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert!(matches!(marker(&s), Marker::Code(_)));
        assert_eq!(buffer(&s), "b x");
    }

    #[test]
    fn stale_menu_passes_the_key_on_and_commit_closes_it() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        // the `/` it pointed at is gone
        s.editing.as_mut().unwrap().menu = Some(Menu {
            line: 0,
            slash: 0,
            selected: 0,
        });
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_ne!(editing(&s), Some("b"), "Enter made a new block");

        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "/h1");
        up(&mut s, Message::Push);
        assert!(s.editing.as_ref().is_none_or(|e| e.menu.is_none()));
        let b = s.rows.iter().find(|r| r.block_id == "b").unwrap();
        assert_eq!(plain_text(&b.rich_text), "b/h1");
    }

    #[test]
    fn no_shortcut_on_a_block_with_children() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("a".into()));
        s.editing.as_mut().unwrap().set_buffer("a", 0, 0);
        type_text(&mut s, "# ");
        assert_eq!(s.last_error, None);
        assert_eq!(marker(&s), Marker::None);
        assert_eq!(buffer(&s), "# a");
    }

    fn expression(s: &State, id: &str) -> String {
        match s.store.get_block(id).unwrap().unwrap().kind {
            BlockKind::Equation { expression } => expression,
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn equation_blocks_via_menu_and_shortcut() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Enter));
        let local = editing(&s).unwrap().to_string();
        type_text(&mut s, "/katex");
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_eq!(marker(&s), Marker::Equation);
        assert!(s.editing.as_ref().unwrap().equation);
        // `/` is TeX here, not the menu
        type_text(&mut s, "a/b");
        assert!(!menu_open(&s));
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert_eq!(expression(&s, &local), "a/b");
        // reopening shows the bare expression
        up(&mut s, Message::Focus(local.clone()));
        assert_eq!(buffer(&s), "a/b");

        up(&mut s, Message::EditKey(EditKey::Enter));
        let next = editing(&s).unwrap().to_string();
        type_text(&mut s, "$$ ");
        assert_eq!(marker(&s), Marker::Equation);
        type_text(&mut s, "x^2");
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert_eq!(expression(&s, &next), "x^2");
    }

    #[test]
    fn inline_equations_edit_as_dollar_source() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, " $x^2$ **y**");
        up(&mut s, Message::EditKey(EditKey::Escape));
        let b = s.rows.iter().find(|r| r.block_id == "b").unwrap();
        assert!(b.editable);
        assert!(
            matches!(&b.rich_text[1].kind, RichTextKind::Equation { expression } if expression == "x^2")
        );
        up(&mut s, Message::Focus("b".into()));
        assert_eq!(buffer(&s), "b $x^2$ **y**");
    }

    /// A key press in the focused editor; true if the keymap or the app took it.
    fn press(s: &mut State, chord: &str, text: Option<&str>) -> bool {
        use iced::widget::text_editor::{Binding, KeyPress, Status};
        let (mods, k) = chord.rsplit_once('+').unwrap_or(("", chord));
        let mut m = keyboard::Modifiers::empty();
        m.set(keyboard::Modifiers::CTRL, mods.contains("ctrl"));
        m.set(keyboard::Modifiers::ALT, mods.contains("alt"));
        m.set(keyboard::Modifiers::SHIFT, mods.contains("shift"));
        let key = match k {
            "tab" => Key::Named(keyboard::key::Named::Tab),
            c => Key::Character(c.into()),
        };
        let kp = KeyPress {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            modifiers: m,
            text: text.map(Into::into),
            status: Status::Focused { is_hovered: false },
        };
        match crate::edit::binding(kp, &s.keys) {
            Some(Binding::Custom(msg)) => {
                up(s, msg);
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    #[test]
    fn keymap_block_commands_convert() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        // a real Ctrl+Alt+1 still carries text '1' (xkb's Control transform only affects @..~)
        assert!(press(&mut s, "ctrl+alt+1", Some("1")));
        assert_eq!(marker(&s), Marker::Heading(1));
        assert_eq!(buffer(&s), "b");
        assert!(editing(&s).unwrap().starts_with("local-"));
        // refused like a `/` pick: "a" has children
        up(&mut s, Message::Focus("a".into()));
        press(&mut s, "ctrl+alt+9", None);
        assert!(s.last_error.is_some());
        assert_eq!(marker(&s), Marker::None);
    }

    #[test]
    fn keymap_marks_wrap_the_selection() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        up(
            &mut s,
            Message::EditorAction(text_editor::Action::Select(text_editor::Motion::WordLeft)),
        );
        press(&mut s, "ctrl+b", None);
        assert_eq!(buffer(&s), "**b**");
        press(&mut s, "ctrl+e", None);
        type_text(&mut s, "x");
        assert_eq!(buffer(&s), "**b**`x`");
        up(&mut s, Message::EditKey(EditKey::Escape));
        let b = s.rows.iter().find(|r| r.block_id == "b").unwrap();
        assert!(b.rich_text[0].annotations.bold && b.rich_text[1].annotations.code);
    }

    #[test]
    fn keymap_routing_and_overrides() {
        let (mut s, mut rx) = editing_state();
        up(&mut s, Message::Focus("b".into()));
        // global commands are left to the window listener
        assert!(press(&mut s, "ctrl+s", None));
        assert!(rx.try_recv().is_err());
        // a plain letter is the editor's
        assert!(!press(&mut s, "q", Some("q")));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("keys.json"),
            r#"{"ctrl+alt+1": "quote", "ctrl+q": "nope"}"#,
        )
        .unwrap();
        let (k, p) = keys::load(dir.path());
        s.set_keys(k, p);
        assert!(s.last_error.as_deref().unwrap().contains("nope"));
        press(&mut s, "ctrl+alt+1", None);
        assert_eq!(marker(&s), Marker::Quote);

        // mid-push, keymap commands are ignored like typing
        s.sent.push_back(true);
        press(&mut s, "ctrl+alt+2", None);
        assert_eq!(marker(&s), Marker::Quote);
    }

    #[test]
    fn equation_conversion_keeps_raw_tex() {
        let (mut s, _rx) = editing_state();
        for tex in ["x_1 * y", "a*b*c"] {
            up(&mut s, Message::Focus("b".into()));
            up(&mut s, Message::EditKey(EditKey::Enter));
            let id = editing(&s).unwrap().to_string();
            type_text(&mut s, tex);
            type_text(&mut s, "/equation");
            up(&mut s, Message::EditKey(EditKey::Enter));
            assert_eq!(buffer(&s), tex);
            up(&mut s, Message::EditKey(EditKey::Escape));
            assert_eq!(expression(&s, &id), tex);
        }
    }

    /// Page "pg": s (synced: st to-do, sp, stbl table), card (image link),
    /// tbl (r1 = [a, b], r2 = [mention, c]), b.
    fn s3_state() -> (State, UnboundedReceiver<Cmd>) {
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
        let para = |id: &str, p: &str| blk(id, p, "paragraph", json!({ "rich_text": rt(id) }));
        let table = |id: &str, p: &str| {
            blk(
                id,
                p,
                "table",
                json!({ "table_width": 2, "has_column_header": true, "has_row_header": false }),
            )
        };
        let trow = |id: &str, p: &str, cells: serde_json::Value| {
            blk(id, p, "table_row", json!({ "cells": cells }))
        };
        let put = |s: &State, parent: &str, kids: &[notion_core::model::Block]| {
            s.store.replace_children("pg", parent, kids).unwrap()
        };
        put(
            &s,
            "pg",
            &[
                blk("s", "pg", "synced_block", json!({ "synced_from": null })),
                blk(
                    "card",
                    "pg",
                    "image",
                    json!({ "type": "external", "external": { "url": "https://x.example/a.png" }, "caption": [] }),
                ),
                table("tbl", "pg"),
                para("b", "pg"),
            ],
        );
        put(
            &s,
            "s",
            &[
                blk(
                    "st",
                    "s",
                    "to_do",
                    json!({ "rich_text": rt("st"), "checked": false }),
                ),
                para("sp", "s"),
                table("stbl", "s"),
            ],
        );
        put(&s, "stbl", &[trow("sr", "stbl", json!([rt("x"), rt("y")]))]);
        put(
            &s,
            "tbl",
            &[
                trow("r1", "tbl", json!([rt("a"), rt("b")])),
                trow("r2", "tbl", json!([mention, rt("c")])),
            ],
        );
        up(&mut s, Message::Open("pg".into()));
        (s, rx)
    }

    #[test]
    fn synced_and_link_rows_are_read_only() {
        let (mut s, _rx) = s3_state();
        for id in ["s", "st", "sp", "card", "tbl"] {
            up(&mut s, Message::Focus(id.into()));
            assert_eq!(editing(&s), None, "{id} focused");
            // without a focused row these do nothing
            up(&mut s, Message::EditKey(EditKey::Backspace));
            up(&mut s, Message::EditKey(EditKey::Indent));
            up(
                &mut s,
                Message::Command(Command::Block(TargetKind::Heading1)),
            );
            type_text(&mut s, "/x");
        }
        up(&mut s, Message::ToggleCheck("st".into()));
        up(&mut s, Message::FocusCell("sr".into(), 0));
        assert_eq!(editing(&s), None, "cell of a synced table");

        // from an editable neighbour: Up/Down skip them, Tab can't nest into them
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Up));
        assert_eq!(editing(&s), Some("b"), "nothing editable above");
        up(&mut s, Message::EditKey(EditKey::Indent));
        let b = s.rows.iter().find(|r| r.block_id == "b").unwrap();
        assert_eq!(b.depth, 0);
        assert!(s.store.queued_ops().unwrap().is_empty(), "{:?}", kinds(&s));
        let st = s.rows.iter().find(|r| r.block_id == "st").unwrap();
        assert_eq!(st.marker, Marker::Todo(false));
    }

    fn cell_text(s: &State, row: &str, col: usize) -> String {
        s.rows
            .iter()
            .find_map(|r| match &r.marker {
                Marker::Table(t) => t.rows.iter().find(|tr| tr.id == row),
                _ => None,
            })
            .map(|tr| plain_text(&tr.cells[col].rich_text))
            .unwrap()
    }

    #[test]
    fn table_cells_edit_and_commit() {
        let (mut s, _rx) = s3_state();
        up(&mut s, Message::FocusCell("r2".into(), 0));
        assert_eq!(editing(&s), None, "mention cell is read-only");

        up(&mut s, Message::FocusCell("r1".into(), 1));
        assert_eq!(editing(&s), Some("tbl"));
        // no `/` menu, no conversion, Backspace never deletes the table
        type_text(&mut s, "/");
        assert!(s.editing.as_ref().unwrap().menu.is_none());
        up(
            &mut s,
            Message::Command(Command::Block(TargetKind::Heading1)),
        );
        for _ in 0..3 {
            up(&mut s, Message::EditKey(EditKey::Backspace));
        }
        assert_eq!(s.editing.as_ref().unwrap().content.text(), "");
        type_text(&mut s, "**B**");
        assert!(s.store.queued_ops().unwrap().is_empty());
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert_eq!(editing(&s), None);
        assert_eq!(cell_text(&s, "r1", 1), "B");
        assert_eq!(kinds(&s), ["update_block"]);
        let notion_core::ops::OpKind::UpdateBlock { block_id, body } =
            &s.store.queued_ops().unwrap()[0].kind
        else {
            panic!()
        };
        assert_eq!(block_id, "r1");
        assert_eq!(body["table_row"]["cells"][0][0]["plain_text"], "a");
        assert_eq!(
            body["table_row"]["cells"][1][0]["annotations"]["bold"],
            true
        );

        // Enter and Tab commit too; clicking another cell commits the first
        for key in [EditKey::Enter, EditKey::Indent] {
            up(&mut s, Message::FocusCell("r1".into(), 0));
            type_text(&mut s, "!");
            up(&mut s, Message::EditKey(key));
            assert_eq!(editing(&s), None);
        }
        up(&mut s, Message::FocusCell("r2".into(), 1));
        type_text(&mut s, "?");
        up(&mut s, Message::FocusCell("r1".into(), 1));
        assert_eq!(cell_text(&s, "r1", 0), "a!!");
        assert_eq!(cell_text(&s, "r2", 1), "c?");
        assert_eq!(kinds(&s).len(), 4);
        assert_eq!(ids(&s), ["s", "st", "sp", "stbl", "card", "tbl", "b"]);

        // a pull makes the edited cell read-only (a mention): the text survives
        use crate::outline::tests::{blk, rt};
        let mention = json!([{ "type": "mention", "mention": { "type": "date", "date": { "start": "2025-01-01" } },
            "plain_text": "d", "annotations": {} }]);
        type_text(&mut s, "Z");
        let row = |c0: serde_json::Value| {
            blk(
                "r1",
                "tbl",
                "table_row",
                json!({ "cells": [c0, mention.clone()] }),
            )
        };
        s.store
            .replace_children("pg", "tbl", &[row(rt("a!!"))])
            .unwrap();
        s.refresh();
        assert_eq!(editing(&s), Some("tbl"), "editor kept");
        up(&mut s, Message::EditKey(EditKey::Escape));
        assert_eq!(cell_text(&s, "r1", 1), "BZ");

        // the edited row vanishes (a pull): the editor is dropped, not stuck
        up(&mut s, Message::FocusCell("r1".into(), 0));
        assert_eq!(editing(&s), Some("tbl"));
        s.store.replace_children("pg", "tbl", &[]).unwrap();
        s.refresh();
        assert_eq!(editing(&s), None);
        assert!(s.last_error.as_deref().unwrap().contains("gone"));
    }

    #[test]
    fn link_clicks() {
        let (mut s, mut rx) = s3_state();
        let card = s.rows.iter().find(|r| r.block_id == "card").unwrap();
        assert!(
            matches!(click(card), Some(Message::OpenLink(u)) if u == "https://x.example/a.png")
        );
        assert!(allowed_url("https://a") && allowed_url("HTTP://a"));
        for bad in [
            "file:///tmp/x.desktop",
            "javascript:alert(1)",
            "-x",
            "ftp://a",
        ] {
            assert!(!allowed_url(bad), "{bad}");
        }
        up(&mut s, Message::OpenLink("mailto:x@y".into()));
        assert!(s.last_error.as_deref().unwrap().contains("only http"));

        // a database: its pull fails with a friendly error
        up(&mut s, Message::OpenDatabase("db".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "db".into(),
                force: false
            }
        );
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("400 validation_error".into()))),
        );
        assert_eq!(s.last_error.as_deref(), Some("databases not supported yet"));
        // opening something else forgets the database
        up(&mut s, Message::OpenDatabase("db".into()));
        up(&mut s, Message::Open("pg".into()));
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("offline".into()))),
        );
        assert_eq!(s.last_error.as_deref(), Some("offline"));
        // a page pull failing keeps its own error
        up(&mut s, Message::PickResult("pg2".into()));
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("offline".into()))),
        );
        assert_eq!(s.last_error.as_deref(), Some("offline"));
    }
}
