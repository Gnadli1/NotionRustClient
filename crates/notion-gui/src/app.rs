//! App state and update (rendering is in `view`). `update` only touches the store, the editor
//! buffer and the worker channel (its Task is just widget focus), so it is
//! testable without a window.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key};
use iced::widget::{operation, text_editor};
use iced::{event, Subscription, Task};
use notion_core::markdown::from_markdown;
use notion_core::model::{plain_text, TargetKind};
use notion_core::model::{Block, BlockKind, PropertyValue, RichText, RichTextKind, TextBlock};
use notion_core::ops::{Op, OpKind, OpStatus};
use notion_core::store::{PageEntry, Store};
use notion_core::sync::{self, ChangeOutcome};
use tokio::sync::mpsc::UnboundedSender;

use crate::edit::{key_action, EditKey, Editing, KeyAction, EDITOR_ID};
use crate::keys::{self, Chord, Command, Keymap};
use crate::outline::{self, Marker, Row};
use crate::session::{self, Session};
use crate::slash::{self, Menu, MenuKey};
use crate::tabs::{self, Tab};
use crate::view::untitled;
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
    /// ▸/▾ clicked: expand or collapse a toggle (view only, no op).
    ToggleOpen(String),
    /// `/` menu entry chosen (index into the filtered list).
    SlashPick(usize),
    /// Any key press, seen window-wide: runs its global keymap command.
    Chord(Chord),
    /// A keymap command from the row editor (block type, mark, new block).
    Command(Command),
    /// Ctrl+K or the sidebar's Search row: opens the search overlay.
    FocusSearch,
    /// Esc anywhere, or a click on the overlay's backdrop: closes the search
    /// overlay, back to the outline.
    SearchEscape,
    /// Sidebar page clicked: opened if cached, else pulled first.
    SidebarOpen(String),
    /// Sidebar ▸/▾ clicked: expand or collapse that page's children.
    SidebarExpand(String),
    /// Ctrl+\: show or hide the sidebar.
    ToggleSidebar,
    /// System light/dark mode, at startup and on change.
    ThemeChanged(iced::theme::Mode),
    /// Ctrl+T: an empty tab, with the search overlay over it.
    NewTab,
    /// Ctrl+W, middle-click or ×: close tab `i`.
    CloseTab(usize),
    /// A tab clicked, or Ctrl+Tab / Ctrl+Shift+Tab: make tab `i` active.
    SwitchTab(usize),
    /// The page scrolled to this vertical offset.
    Scrolled(f32),
    /// A link in a row's text clicked: a Notion page opens here, the rest
    /// goes to xdg-open.
    LinkClick(String),
    /// The page title clicked: edit it in place.
    EditTitle,
    TitleInput(String),
    /// Enter in the title, or a click beside it: commit the title.
    TitleCommit,
    /// The 5-minute timer or the window gaining focus.
    AutoPull,
    /// The status bar's conflict/failed count: show or hide their list.
    ToggleProblems,
    /// A problem op clicked (op id): open its page and block.
    OpenProblem(i64),
    /// Drop a conflict/failed op (op id) and force-pull its page.
    Discard(i64),
    /// Ctrl+Z: the editor's uncommitted typing, else the last local edit.
    Undo,
    Redo,
    /// The window's new size.
    Resized(f32, f32),
}

/// A conflict or failed op, for the status bar's list.
#[derive(Debug, Clone)]
pub(crate) struct Problem {
    pub op: i64,
    pub kind: &'static str,
    /// The block to show and focus: a create's own block while cached.
    pub block: String,
    pub page: Option<String>,
    /// The block's text, or the page's title, or the id.
    pub label: String,
    pub error: String,
    /// A create's unpushed `local-*` block.
    local: Option<String>,
}

/// Auto-pulls at most this often, however the focus flaps.
const AUTO_PULL_GAP: Duration = Duration::from_secs(30);
/// The auto-pull timer.
pub const AUTO_PULL_EVERY: Duration = Duration::from_secs(300);

pub(crate) const SEARCH_ID: &str = "search";
/// The page's scrollable, scrolled back on a tab switch.
pub(crate) const PAGE_SCROLL_ID: &str = "page";
pub(crate) const TITLE_ID: &str = "title";
/// Undo steps kept.
const UNDO_MAX: usize = 100;

pub struct State {
    store: Store,
    cmds: UnboundedSender<Cmd>,
    /// Sidebar pages; [`crate::sidebar`] nests them.
    pub(crate) tree: Vec<PageEntry>,
    /// Expanded sidebar pages.
    pub(crate) tree_open: HashSet<String>,
    pub(crate) sidebar: bool,
    /// The search overlay is open.
    pub(crate) searching: bool,
    pub(crate) current: Option<String>,
    pub(crate) title: String,
    pub(crate) rows: Vec<Row>,
    /// queued, conflict, failed
    pub(crate) counts: [usize; 3],
    pub(crate) last_error: Option<String>,
    pub(crate) search: String,
    pub(crate) results: Vec<(String, String)>,
    /// Picked search result waiting for its pull.
    pending_open: Option<String>,
    /// Last database opened; its pull failing gets a friendlier error.
    pending_db: Option<String>,
    /// One entry per command sent and not yet answered (the worker answers
    /// each with one event, in order); true = Push.
    pub(crate) sent: VecDeque<bool>,
    /// The focused row, if any.
    pub(crate) editing: Option<Editing>,
    pub(crate) keys: Keymap,
    /// System light/dark mode; `None` (no preference) renders light.
    pub(crate) mode: iced::theme::Mode,
    /// Expanded toggles; every other toggle is collapsed (Notion's default).
    pub(crate) open: HashSet<String>,
    /// The page's scroll offset.
    pub(crate) scroll: f32,
    /// Open tabs; `tabs[active]` is stale, `current`/`open`/`scroll` are its
    /// live state.
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// The title being edited in place.
    pub(crate) title_edit: Option<String>,
    /// The title has no mentions or equations (plain text round-trips).
    pub(crate) title_editable: bool,
    last_auto: Option<Instant>,
    /// The command in flight is an auto-pull.
    auto_pulling: bool,
    /// Resolved targets of queued ops, and of conflict/failed ones.
    pub(crate) queued_ids: HashSet<String>,
    pub(crate) problem_ids: HashSet<String>,
    pub(crate) problems: Vec<Problem>,
    pub(crate) problems_open: bool,
    /// Local edits to undo / redo, newest last: (page, change).
    pub(crate) undo: Vec<(String, sync::Change)>,
    pub(crate) redo: Vec<(String, sync::Change)>,
    /// Inside [`State::step`]: nested steps join the outer one.
    stepping: bool,
    /// Recently opened pages, most recent first.
    pub(crate) recent: Vec<String>,
    /// The highlighted entry of the overlay's recent list.
    pub(crate) recent_sel: usize,
    /// Where session.json is written on exit; None (tests) writes nothing.
    session_dir: Option<PathBuf>,
    pub(crate) window: (f32, f32),
}

impl State {
    pub fn new(store: Store, cmds: UnboundedSender<Cmd>) -> Self {
        let mut s = State {
            store,
            cmds,
            tree: Vec::new(),
            tree_open: HashSet::new(),
            sidebar: true,
            searching: false,
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
            mode: iced::theme::Mode::None,
            open: HashSet::new(),
            scroll: 0.0,
            tabs: vec![Tab::default()],
            active: 0,
            title_edit: None,
            title_editable: false,
            last_auto: None,
            auto_pulling: false,
            queued_ids: HashSet::new(),
            problem_ids: HashSet::new(),
            problems: Vec::new(),
            problems_open: false,
            undo: Vec::new(),
            redo: Vec::new(),
            stepping: false,
            recent: Vec::new(),
            recent_sel: 0,
            session_dir: None,
            window: Session::default().window,
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
        let dirty = self.title_edit.is_some()
            || self
                .editing
                .as_ref()
                .is_some_and(|e| e.content.text() != e.original);
        if dirty {
            self.step(Self::commit_now)
        } else {
            self.commit_now()
        }
    }

    fn commit_now(&mut self) -> bool {
        let titled = self.write_title();
        let wrote = self.write_edit();
        // Some(true) = nothing to write; the rows are current
        if wrote != Some(true) || titled {
            self.refresh();
        }
        wrote.is_some()
    }

    /// Ends title editing, writing the title if it changed. True if the
    /// store may have changed. A failed write drops the edit (shown as an
    /// error) rather than block every commit point.
    fn write_title(&mut self) -> bool {
        let Some(new) = self.title_edit.take() else {
            return false;
        };
        let Some(id) = self.current.clone() else {
            return false;
        };
        let old = self.store.get_page(&id).ok().flatten().map(|p| p.title());
        if old.as_deref() == Some(new.as_str()) {
            return false;
        }
        if let Err(e) = sync::local_set_title(&self.store, &id, &new) {
            self.last_error = Some(format!("title not saved: {e}"));
        }
        true
    }

    /// Runs `f`, which may edit the current page locally, as one undo step.
    // ponytail: snapshots the whole page + op table per step and keeps only the
    // changed rows; diff per block if huge pages make edits lag
    fn step<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let Some(page) = self.current.clone().filter(|_| !self.stepping) else {
            return f(self);
        };
        let cp = sync::checkpoint(&self.store, &[&page]);
        self.stepping = true;
        let r = f(self);
        self.stepping = false;
        match cp.and_then(|cp| cp.changes(&self.store)) {
            Ok(Some(c)) => {
                self.undo.push((page, c));
                if self.undo.len() > UNDO_MAX {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
            Ok(None) => {}
            Err(e) => self.last_error = Some(format!("undo: {e}")),
        }
        r
    }

    /// Nothing is being edited, searched or sent, and a page is open.
    fn idle(&self) -> bool {
        self.current.is_some()
            && self.sent.is_empty()
            && self.editing.is_none()
            && self.title_edit.is_none()
            && !self.searching
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
        self.reveal_edited();
        operation::focus(EDITOR_ID)
    }

    /// Expands the toggles the edited row is hidden in.
    fn reveal_edited(&mut self) {
        let Some(e) = &self.editing else { return };
        let Some(idx) = self.rows.iter().position(|r| r.block_id == e.block_id) else {
            return;
        };
        for id in outline::ancestors(&self.rows, idx) {
            if self.rows.iter().any(|r| r.block_id == id && r.toggle) {
                self.open.insert(id.to_string());
            }
        }
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
    pub(crate) fn pushing(&self) -> bool {
        self.sent.contains(&true)
    }

    /// Reloads sidebar, outline and op counts from the store.
    fn refresh(&mut self) {
        if let Some(id) = &self.current {
            session::touch(&mut self.recent, id);
        }
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
        // a push renames local-* toggles: keep them expanded
        let open = std::mem::take(&mut self.open);
        self.open = open
            .into_iter()
            .map(|id| self.store.resolve_id(&id).unwrap_or(id))
            .collect();
        // e.g. Tab moved it into a collapsed toggle
        self.reveal_edited();
    }

    fn try_refresh(&mut self) -> notion_core::Result<()> {
        self.tree = self.store.page_tree()?;
        self.title_editable = false;
        if let Some(id) = &self.current {
            let page = self.store.get_page(id)?;
            self.title = page
                .as_ref()
                .map_or_else(|| "(not cached)".into(), |p| untitled(&p.title()).into());
            // mentions and equations don't survive a plain-text edit
            self.title_editable = page.is_some_and(|p| {
                p.properties.values().any(|v| match v {
                    PropertyValue::Title(rt) => rt
                        .iter()
                        .all(|r| matches!(r.kind, RichTextKind::Text { .. })),
                    _ => false,
                })
            });
            self.rows = outline::flatten(&self.store, id)?;
        } else {
            // an empty tab
            self.title.clear();
            self.rows.clear();
        }
        let queued = self.store.queued_ops()?;
        let conflict = self.store.ops_by_status(OpStatus::Conflict)?;
        let failed = self.store.ops_by_status(OpStatus::Failed)?;
        self.counts = [queued.len(), conflict.len(), failed.len()];
        // a create marks its own block, not its parent
        self.queued_ids = queued
            .iter()
            .map(|o| {
                self.store
                    .resolve_id(sync::op_block(&o.kind).unwrap_or(o.kind.target_id()))
            })
            .collect::<notion_core::Result<_>>()?;
        self.problems = conflict
            .iter()
            .chain(&failed)
            .map(|o| problem(&self.store, o))
            .collect::<notion_core::Result<_>>()?;
        self.problem_ids = self.problems.iter().map(|p| p.block.clone()).collect();
        if self.problems.is_empty() {
            self.problems_open = false;
        }
        Ok(())
    }
}

/// A conflict/failed op's list entry: what it is, where, and why.
fn problem(store: &Store, op: &Op) -> notion_core::Result<Problem> {
    let target = store.resolve_id(op.kind.target_id())?;
    let (kind, local) = match &op.kind {
        OpKind::CreateBlock { block, .. } => {
            ("create", block["id"].as_str().map(|id| id.to_string()))
        }
        OpKind::UpdateBlock { .. } => ("edit", None),
        OpKind::DeleteBlock { .. } => ("delete", None),
        OpKind::MoveBlock { .. } => ("move", None),
        OpKind::UpdatePageProperties { .. } => ("title", None),
        OpKind::CreateComment { .. } => ("comment", None),
        OpKind::UpdateComment { .. } => ("edit comment", None),
        OpKind::DeleteComment { .. } => ("delete comment", None),
    };
    let shown = match &local {
        Some(l) if store.get_block(l)?.is_some() => l.clone(),
        _ => target.clone(),
    };
    // a deleted block is gone from the cache: its base snapshot remains
    let block = match store.get_block(&shown)? {
        Some(b) => Some(b),
        None => op
            .base_raw
            .clone()
            .and_then(|v| serde_json::from_value::<Block>(v).ok()),
    };
    let page = store.get_page(&shown)?;
    let page_id = match (&page, &block) {
        (Some(p), _) => Some(p.id.clone()),
        (None, Some(b)) => match store.block_page(&b.id)? {
            Some(p) => Some(p),
            // one hop up from the snapshot
            None => match &b.parent {
                notion_core::model::Parent::Page(p) => Some(p.clone()),
                notion_core::model::Parent::Block(p) => store.block_page(p)?,
                _ => None,
            },
        },
        (None, None) => None,
    };
    let text = block
        .as_ref()
        .and_then(|b| b.kind.text().map(|t| plain_text(&t.rich_text)))
        .or_else(|| page.map(|p| p.title()))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| shown.clone());
    Ok(Problem {
        op: op.id,
        kind,
        block: shown,
        page: page_id,
        label: text.chars().take(60).collect(),
        error: op.error.clone().unwrap_or_default(),
        local,
    })
}

/// Closing the window with a row open must not lose its text.
impl Drop for State {
    fn drop(&mut self) {
        if let Some(dir) = &self.session_dir {
            if let Err(e) = session::save(dir, &self.session()) {
                eprintln!("cannot write the session: {e}");
            }
        }
        // mid-push the text was committed at Ctrl+S and edits are paused
        if std::thread::panicking() || self.pushing() {
            return;
        }
        self.write_title();
        self.write_edit();
    }
}

impl State {
    /// Expands the sidebar pages above the current one.
    fn reveal_current(&mut self) {
        let Some(id) = &self.current else { return };
        let up: Vec<String> = crate::sidebar::ancestors(&self.tree, id)
            .into_iter()
            .map(String::from)
            .collect();
        self.tree_open.extend(up);
    }

    /// Closes the search overlay; Esc then goes back to the edited row.
    fn close_search(&mut self) -> Task<Message> {
        self.searching = false;
        // mid-push the editor is paused: don't hand it the focus
        if self.editing.is_some() && !self.pushing() {
            operation::focus(EDITOR_ID)
        } else {
            iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::unfocus())
        }
    }

    /// Commits the edit, parks the active tab and loads tab `to`. A failed
    /// commit keeps the editor and the tab.
    fn switch_tab(&mut self, to: usize) -> Task<Message> {
        if to == self.active || to >= self.tabs.len() || !self.leave() {
            return Task::none();
        }
        let live = Tab {
            page: self.current.clone(),
            scroll: self.scroll,
            open: std::mem::take(&mut self.open),
            title: self.title.clone(),
        };
        let next = tabs::switch(&mut self.tabs, self.active, to, live);
        self.active = to;
        self.load_tab(next)
    }

    /// Makes `t` the live state (editing has ended) and scrolls back to it.
    fn load_tab(&mut self, t: Tab) -> Task<Message> {
        // a late pull must not open its page in this tab
        self.pending_open = None;
        self.pending_db = None;
        // Ctrl+W / Ctrl+Tab with the overlay open: close it with the tab it
        // was over (NewTab reopens it)
        self.searching = false;
        self.current = t.page;
        self.open = t.open;
        self.scroll = t.scroll;
        self.refresh();
        self.reveal_current();
        // e.g. a restored tab: pulled like any uncached page
        if let Some(id) = self.current.clone().filter(|id| !self.cached(id)) {
            self.pending_open = Some(id.clone());
            self.send(Cmd::Pull { id, force: false });
        }
        operation::scroll_to(
            PAGE_SCROLL_ID,
            operation::AbsoluteOffset {
                x: None,
                y: Some(t.scroll),
            },
        )
    }

    fn cached(&self, id: &str) -> bool {
        matches!(self.store.get_page(id), Ok(Some(_)))
    }

    /// The state to write to session.json.
    fn session(&self) -> Session {
        let mut tabs = self.tabs.clone();
        tabs[self.active] = Tab {
            page: self.current.clone(),
            scroll: self.scroll,
            open: self.open.clone(),
            title: self.title.clone(),
        };
        Session {
            tabs,
            active: self.active,
            sidebar: self.sidebar,
            expanded: self.tree_open.clone(),
            recent: self.recent.clone(),
            window: self.window,
        }
    }

    /// Restores a saved session (from [`session::load`]) and saves it to
    /// `dir` on exit; `problem` goes to the status bar.
    pub fn restore_session(
        &mut self,
        dir: PathBuf,
        s: Session,
        problem: Option<String>,
    ) -> Task<Message> {
        self.session_dir = Some(dir);
        if let Some(p) = problem {
            self.last_error = Some(match self.last_error.take() {
                Some(e) => format!("{e}; {p}"),
                None => p,
            });
        }
        self.tabs = s.tabs;
        self.active = s.active;
        self.sidebar = s.sidebar;
        self.tree_open = s.expanded;
        self.recent = s.recent;
        self.window = s.window;
        let t = self.tabs[self.active].clone();
        self.load_tab(t)
    }

    /// Cached recent pages for the overlay: (id, icon, title).
    pub(crate) fn recent_pages(&self) -> Vec<(String, String, String)> {
        self.recent
            .iter()
            .filter_map(|id| {
                let p = self.store.get_page(id).ok()??;
                let icon = match p.raw["icon"]["emoji"].as_str() {
                    Some(e) => e.to_string(),
                    None => "📄".into(),
                };
                Some((p.id.clone(), icon, p.title()))
            })
            .collect()
    }

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
        // view only: allowed mid-push
        Message::SidebarExpand(id) => {
            if !s.tree_open.remove(&id) {
                s.tree_open.insert(id);
            }
            return Task::none();
        }
        Message::ToggleSidebar => {
            s.sidebar = !s.sidebar;
            return Task::none();
        }
        Message::Scrolled(y) => {
            s.scroll = y;
            return Task::none();
        }
        Message::Resized(w, h) => {
            s.window = (w, h);
            return Task::none();
        }
        Message::ToggleProblems => {
            s.problems_open = !s.problems_open && !s.problems.is_empty();
            return Task::none();
        }
        Message::AutoPull => {
            let now = Instant::now();
            let offline = s
                .last_error
                .as_deref()
                .is_some_and(|e| e.starts_with("offline"));
            let due = !offline && auto_pull_due(s.last_auto.map(|t| now - t), s.idle());
            if let Some(id) = s.current.clone().filter(|_| due) {
                s.last_auto = Some(now);
                s.send(Cmd::Pull { id, force: false });
                s.auto_pulling = !s.sent.is_empty();
            }
            return Task::none();
        }
        // the overlay must not stick open during a push
        Message::SearchEscape if s.searching => return s.close_search(),
        Message::SearchEscape if s.problems_open => {
            s.problems_open = false;
            return Task::none();
        }
        // a push is modal: the worker may already have sent the ops an edit
        // would rewrite
        Message::Focus(_)
        | Message::FocusCell(..)
        | Message::OpenDatabase(_)
        | Message::EditorAction(_)
        | Message::EditKey(_)
        | Message::ToggleCheck(_)
        | Message::ToggleOpen(_)
        | Message::SlashPick(_)
        | Message::Command(_)
        | Message::FocusSearch
        | Message::SearchEscape
        | Message::Open(_)
        | Message::PickResult(_)
        | Message::SidebarOpen(_)
        | Message::NewTab
        | Message::CloseTab(_)
        | Message::SwitchTab(_)
        | Message::EditTitle
        | Message::TitleInput(_)
        | Message::TitleCommit
        | Message::OpenProblem(_)
        | Message::Discard(_)
            if s.pushing() => {}
        // a pull in flight writes rows it read before our undo
        Message::Undo | Message::Redo if !s.sent.is_empty() => {}
        Message::Undo => return undo(s, true),
        Message::Redo => return undo(s, false),
        Message::LinkClick(url) => return update(s, link_target(&url)),
        Message::EditTitle => {
            if !s.title_editable || s.title_edit.is_some() || !s.leave() {
                return Task::none();
            }
            let Some(p) = s
                .current
                .as_ref()
                .and_then(|id| s.store.get_page(id).ok().flatten())
            else {
                return Task::none();
            };
            s.title_edit = Some(p.title());
            return Task::batch([
                operation::focus(TITLE_ID),
                operation::move_cursor_to_end(TITLE_ID),
            ]);
        }
        Message::TitleInput(t) => {
            if let Some(e) = &mut s.title_edit {
                *e = t;
            }
        }
        Message::TitleCommit => {
            if s.title_edit.is_some() {
                s.commit();
                return iced::advanced::widget::operate(
                    iced::advanced::widget::operation::focusable::unfocus(),
                );
            }
        }
        Message::OpenProblem(op) => {
            let Some(p) = s.problems.iter().find(|p| p.op == op).cloned() else {
                return Task::none();
            };
            s.problems_open = false;
            let Some(page) = p.page else {
                return Task::none();
            };
            let open = update(s, Message::SidebarOpen(page.clone()));
            // where feasible: an editable block already shown gets the focus
            if s.current.as_deref() == Some(page.as_str())
                && s.rows.iter().any(|r| r.block_id == p.block && r.editable)
            {
                return Task::batch([open, s.focus(&p.block)]);
            }
            return open;
        }
        Message::Discard(op) => {
            let Some(p) = s.problems.iter().find(|p| p.op == op).cloned() else {
                return Task::none();
            };
            if !s.commit() {
                return Task::none();
            }
            // undo steps may hold the discarded ops
            s.undo.clear();
            s.redo.clear();
            let res = s.store.complete_op(op).and_then(|()| {
                let Some(l) = p.local.as_deref().filter(|l| l.starts_with("local-")) else {
                    return Ok(());
                };
                // ops on the never-created block go too
                // ponytail: direct dependents only; ops on its children or
                // placed after it stay (they fail on push and are listed then)
                for o in s
                    .store
                    .queued_ops()?
                    .into_iter()
                    .chain(s.store.ops_by_status(OpStatus::Failed)?)
                {
                    if sync::op_block(&o.kind) == Some(l) || o.kind.target_id() == l {
                        s.store.complete_op(o.id)?;
                    }
                }
                // pulls keep local-* blocks: drop it from the cache too
                if s.store.get_block(l)?.is_some() {
                    s.store.delete_block(l)?;
                }
                Ok(())
            });
            if let Err(e) = res {
                s.last_error = Some(e.to_string());
            }
            s.refresh();
            // the remote version wins
            if let Some(page) = p.page {
                s.send(Cmd::Pull {
                    id: page,
                    force: true,
                });
            }
        }
        Message::NewTab => {
            if !s.leave() {
                return Task::none();
            }
            s.tabs.push(Tab::default());
            let to = s.tabs.len() - 1;
            let scroll = s.switch_tab(to);
            return Task::batch([scroll, update(s, Message::FocusSearch)]);
        }
        Message::CloseTab(i) if i < s.tabs.len() => {
            if i != s.active {
                s.active = tabs::close(&mut s.tabs, s.active, i);
                return Task::none();
            }
            if !s.leave() {
                return Task::none();
            }
            s.active = tabs::close(&mut s.tabs, s.active, i);
            let t = s.tabs[s.active].clone();
            return s.load_tab(t);
        }
        Message::CloseTab(_) => {}
        Message::SwitchTab(i) => return s.switch_tab(i),
        Message::SidebarOpen(id) => {
            if s.tree.iter().any(|e| e.database && e.id == id) {
                s.last_error = Some("databases not supported yet".into());
                return Task::none();
            }
            let cached = matches!(s.store.get_page(&id), Ok(Some(_)));
            return update(
                s,
                if cached {
                    Message::Open(id)
                } else {
                    Message::PickResult(id)
                },
            );
        }
        Message::Open(id) => {
            if !s.leave() {
                return Task::none();
            }
            s.pending_open = None;
            s.pending_db = None;
            s.current = Some(id);
            s.refresh();
            s.reveal_current();
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
            } else if let Some((id, ..)) = s.recent_pages().into_iter().nth(s.recent_sel) {
                return update(s, Message::PickResult(id));
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
            s.searching = false;
            if matches!(s.store.get_page(&id), Ok(Some(_))) {
                if !s.leave() {
                    return Task::none();
                }
                s.current = Some(id.clone());
                s.refresh();
                s.reveal_current();
            }
            s.pending_open = Some(id.clone());
            s.send(Cmd::Pull { id, force: false });
        }
        Message::ToggleCheck(id) => return toggle_check(s, &id),
        Message::ToggleOpen(id) => {
            if !s.open.contains(&id) {
                s.open.insert(id);
                return Task::none();
            }
            // collapsing over the edited row: commit and leave it first
            // (before removing, or refresh would re-open it)
            let inside = s.editing_row().and_then(|r| {
                let i = s.rows.iter().position(|x| x.block_id == r.block_id)?;
                Some(outline::ancestors(&s.rows, i).contains(&id.as_str()))
            });
            if inside == Some(true) && !s.leave() {
                return Task::none();
            }
            s.open.remove(&id);
        }
        Message::FocusSearch => {
            s.commit();
            s.searching = true;
            s.recent_sel = 0;
            return Task::batch([
                operation::focus(SEARCH_ID),
                operation::select_all(SEARCH_ID),
            ]);
        }
        Message::SearchEscape => {
            if s.title_edit.is_some() {
                return update(s, Message::TitleCommit);
            }
            return if s.editing.is_some() {
                operation::focus(EDITOR_ID)
            } else {
                iced::advanced::widget::operate(
                    iced::advanced::widget::operation::focusable::unfocus(),
                )
            };
        }
        Message::ThemeChanged(m) => s.mode = m,
        Message::Focus(id) => return s.focus(&id),
        Message::FocusCell(row, col) => return s.focus_cell(&row, col),
        Message::EditorAction(a) => return on_editor_action(s, a),
        Message::SlashPick(i) => return slash_pick(s, i),
        // the overlay's recent list (empty query)
        Message::Chord(c)
            if s.searching
                && s.search.trim().is_empty()
                && [Chord::parse("up"), Chord::parse("down")].contains(&Some(c.clone())) =>
        {
            let n = s.recent_pages().len();
            if n > 0 {
                let step = if Some(c) == Chord::parse("down") {
                    1
                } else {
                    n - 1
                };
                s.recent_sel = (s.recent_sel + step) % n;
            }
        }
        // the message it maps to applies its own push rule
        Message::Chord(c) => {
            let msg = match s.keys.get(&c) {
                Some(Command::Push) => Message::Push,
                Some(Command::Pull) => Message::Pull { force: false },
                Some(Command::ForcePull) => Message::Pull { force: true },
                Some(Command::Search) => Message::FocusSearch,
                Some(Command::ToggleSidebar) => Message::ToggleSidebar,
                Some(Command::NewTab) => Message::NewTab,
                Some(Command::CloseTab) => Message::CloseTab(s.active),
                Some(Command::NextTab) => {
                    Message::SwitchTab(tabs::cycle(s.tabs.len(), s.active, 1))
                }
                Some(Command::PrevTab) => {
                    Message::SwitchTab(tabs::cycle(s.tabs.len(), s.active, -1))
                }
                Some(Command::Undo) => Message::Undo,
                Some(Command::Redo) => Message::Redo,
                _ => return Task::none(),
            };
            return update(s, msg);
        }
        Message::Command(c) => return on_command(s, c),
        Message::EditKey(k) => return on_edit_key(s, k),
        Message::WorkerGone => {
            s.sent.clear();
            s.auto_pulling = false;
            s.last_error = Some("sync worker stopped".into());
        }
        Message::Worker(ev) => {
            // pushed ops can't be undone, nor edits a pull rewrote under them
            let pulled = matches!(&*ev, Event::Pulled { report, .. } if !report.skipped);
            if s.sent.pop_front() == Some(true) || pulled {
                s.undo.clear();
                s.redo.clear();
            }
            // auto-pulls only start with nothing in flight: this answers it
            if std::mem::take(&mut s.auto_pulling) {
                // quiet: no error shown or cleared, pending opens kept
                s.refresh();
                if s.editing.is_some() && !s.searching {
                    return operation::focus(EDITOR_ID);
                }
                return Task::none();
            }
            match &*ev {
                Event::Pulled { id, .. } => {
                    let opened = s.pending_open.as_ref() == Some(id) && s.leave();
                    if opened {
                        s.pending_open = None;
                        s.pending_db = None;
                        s.current = Some(id.clone());
                    }
                    s.last_error = None;
                    // the editor keeps its own buffer; only the other rows change
                    s.refresh();
                    if opened {
                        s.reveal_current();
                    }
                    if s.editing.is_some() && !s.searching {
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
    s.step(|s| convert_now(s, kind, buf, pos))
}

fn convert_now(s: &mut State, kind: TargetKind, buf: &str, pos: (usize, usize)) -> Task<Message> {
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
    s.step(|s| divider_after_now(s, buf, pos))
}

fn divider_after_now(s: &mut State, buf: &str, pos: (usize, usize)) -> Task<Message> {
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
    // the keys that may create, delete or move a block (one undo step each)
    let edits = match k {
        EditKey::Enter | EditKey::Indent | EditKey::Outdent | EditKey::ToggleCheck => true,
        EditKey::Backspace => s.editing.as_ref().is_some_and(|e| e.caret().empty),
        _ => false,
    };
    if edits {
        s.step(|s| edit_key(s, k))
    } else {
        edit_key(s, k)
    }
}

fn edit_key(s: &mut State, k: EditKey) -> Task<Message> {
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
    // editable and not inside a collapsed toggle
    let shown = outline::shown(&s.rows, &s.open);
    let rows = &s.rows;
    let target = |i: &usize| rows[*i].editable && shown[*i];
    match key_action(k, e.caret()) {
        KeyAction::Pass(a) => e.content.perform(a),
        KeyAction::FocusPrev => {
            if let Some(i) = (0..idx).rev().find(target) {
                return s.focus(&s.rows[i].block_id.clone());
            }
        }
        KeyAction::FocusNext => {
            if let Some(i) = (idx + 1..s.rows.len()).find(target) {
                return s.focus(&s.rows[i].block_id.clone());
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
            let prev = (0..idx)
                .rev()
                .find(target)
                .map(|i| s.rows[i].block_id.clone());
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

/// Undo (`back`) or redo of the last local edit (uncommitted typing is
/// committed first), refused once its op was completed or marked.
fn undo(s: &mut State, back: bool) -> Task<Message> {
    if s.searching {
        return Task::none();
    }
    // uncommitted typing (row or title) becomes its own step first, so this
    // undoes exactly it and redo brings it back
    if !s.commit() {
        return Task::none();
    }
    let stack = if back { &mut s.undo } else { &mut s.redo };
    let Some((page, c)) = stack.pop() else {
        return Task::none();
    };
    // just committed: the buffer is clean
    let edited = s
        .editing
        .take()
        .filter(|e| e.cell.is_none())
        .map(|e| e.block_id);
    s.title_edit = None;
    match sync::restore(&s.store, &c) {
        Ok(true) => {
            let inv = (page.clone(), c.inverse());
            if back {
                s.redo.push(inv);
            } else {
                s.undo.push(inv);
            }
            if s.current.as_deref() != Some(page.as_str()) {
                s.pending_open = None;
                s.pending_db = None;
                s.current = Some(page);
            }
        }
        res => {
            s.undo.clear();
            s.redo.clear();
            s.last_error = Some(match res {
                Err(e) => format!("undo: {e}"),
                _ => "can't undo: pushed or changed since".into(),
            });
        }
    }
    s.refresh();
    s.reveal_current();
    match edited {
        Some(id) if s.rows.iter().any(|r| r.block_id == id && r.editable) => s.focus(&id),
        _ => Task::none(),
    }
}

/// Ctrl+Enter or a click on ☐/☑: commit, then flip the to-do. No-op otherwise.
fn toggle_check(s: &mut State, id: &str) -> Task<Message> {
    s.step(|s| toggle_check_now(s, id))
}

fn toggle_check_now(s: &mut State, id: &str) -> Task<Message> {
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
    match ev {
        iced::Event::Window(iced::window::Event::Focused) => return Some(Message::AutoPull),
        iced::Event::Window(iced::window::Event::Resized(size)) => {
            return Some(Message::Resized(size.width, size.height))
        }
        _ => {}
    }
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
    Subscription::batch([
        event::listen_with(shortcut),
        iced::system::theme_changes().map(Message::ThemeChanged),
    ])
}

impl State {
    /// "Page title — notion-gui", or just the crate name with no page open.
    pub fn window_title(&self) -> String {
        let app = env!("CARGO_PKG_NAME");
        match self.current {
            Some(_) => format!("{} — {app}", untitled(&self.title)),
            None => app.to_owned(),
        }
    }
}

pub fn theme(s: &State) -> iced::Theme {
    crate::view::theme::theme(s.mode)
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

/// Whether an auto-pull may run: idle (see [`State::idle`]) and none in the
/// last [`AUTO_PULL_GAP`] (`since_last`: time since the last one, if any).
fn auto_pull_due(since_last: Option<Duration>, idle: bool) -> bool {
    idle && since_last.is_none_or(|d| d >= AUTO_PULL_GAP)
}

/// The page id (8-4-4-4-12) a Notion link points at: a notion.so /
/// www.notion.so / notion.com / www.notion.com / *.notion.site URL, or a relative `/…` path, whose last
/// segment ends in a 32-hex id (dashed or not; `?…#…` ignored).
pub(crate) fn notion_page_id(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let path = if url.starts_with('/') {
        url
    } else {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let (host, path) = rest.split_at(rest.find('/')?);
        let host = host.to_ascii_lowercase();
        let hosts = ["notion.so", "www.notion.so", "notion.com", "www.notion.com"];
        if !(hosts.contains(&host.as_str()) || host.ends_with(".notion.site")) {
            return None;
        }
        path
    };
    let last: String = path
        .rsplit('/')
        .next()?
        .chars()
        .filter(|&c| c != '-')
        .collect();
    let h = last
        .get(last.len().checked_sub(32)?..)?
        .to_ascii_lowercase();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// A clicked link: a Notion page opens in the app, anything else via xdg-open.
pub(crate) fn link_target(url: &str) -> Message {
    match notion_page_id(url) {
        Some(id) => Message::SidebarOpen(id),
        None => Message::OpenLink(url.to_string()),
    }
}

/// What clicking a read-only row does, if anything.
pub(crate) fn click(r: &Row) -> Option<Message> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use notion_core::model::{Page, RichTextKind};
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
        let ids: Vec<&str> = s.tree.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a"]);
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
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
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

    #[test]
    fn collapsed_toggles_hide_rows_from_up_down() {
        use crate::outline::tests::{blk, rt};
        let (mut s, _rx) = editing_state();
        // a, a/c, img, m, b, then tg with child in, then z
        let para = |id: &str, p: &str| blk(id, p, "paragraph", json!({ "rich_text": rt(id) }));
        let mut kids = s.store.children("pg").unwrap();
        kids.push(blk("tg", "pg", "toggle", json!({ "rich_text": rt("tg") })));
        kids.push(para("z", "pg"));
        s.store.replace_children("pg", "pg", &kids).unwrap();
        s.store
            .replace_children("pg", "tg", &[para("in", "tg")])
            .unwrap();
        s.refresh();

        up(&mut s, Message::Focus("tg".into()));
        up(&mut s, Message::EditKey(EditKey::Down));
        assert_eq!(editing(&s), Some("z"), "collapsed: skips its child");
        up(&mut s, Message::EditKey(EditKey::Up));
        assert_eq!(editing(&s), Some("tg"));

        up(&mut s, Message::ToggleOpen("tg".into()));
        assert!(s.open.contains("tg"));
        up(&mut s, Message::EditKey(EditKey::Down));
        assert_eq!(editing(&s), Some("in"), "expanded: child reachable");

        // collapsing over the edited row commits and leaves it
        type_text(&mut s, "!");
        up(&mut s, Message::ToggleOpen("tg".into()));
        assert!(!s.open.contains("tg"));
        assert_eq!(editing(&s), None);
        assert_eq!(kinds(&s), ["update_block"], "only the text commit");

        // focusing a hidden row expands its ancestors
        up(&mut s, Message::Focus("in".into()));
        assert!(s.open.contains("tg"));
        up(&mut s, Message::EditKey(EditKey::Escape));
        up(&mut s, Message::ToggleOpen("tg".into()));
        up(&mut s, Message::ToggleOpen("tg".into()));
        assert!(s.open.contains("tg"));
        assert_eq!(kinds(&s), ["update_block"], "toggling queues no op");
    }

    #[test]
    fn styled_blocks_render_without_panicking() {
        use crate::outline::tests::{blk, rt};
        let (mut s, _rx) = editing_state();
        let tb = |id: &str, p: &str, t: &str| blk(id, p, t, json!({ "rich_text": rt(id) }));
        let put = |s: &State, p: &str, kids: &[notion_core::model::Block]| {
            s.store.replace_children("pg", p, kids).unwrap()
        };
        put(
            &s,
            "pg",
            &[
                tb("q", "pg", "quote"),
                blk(
                    "co",
                    "pg",
                    "callout",
                    json!({ "rich_text": rt("co"), "icon": { "emoji": "⭐" } }),
                ),
                blk(
                    "code",
                    "pg",
                    "code",
                    json!({ "rich_text": rt("x"), "language": "rust" }),
                ),
                blk("d", "pg", "divider", json!({})),
                blk(
                    "h",
                    "pg",
                    "heading_2",
                    json!({ "rich_text": rt("h"), "is_toggleable": true }),
                ),
                blk("cl", "pg", "column_list", json!({})),
                blk(
                    "t",
                    "pg",
                    "to_do",
                    json!({ "rich_text": rt("t"), "checked": true }),
                ),
            ],
        );
        put(&s, "h", &[tb("hb", "h", "bulleted_list_item")]);
        put(&s, "hb", &[tb("hbb", "hb", "bulleted_list_item")]);
        put(
            &s,
            "cl",
            &[
                blk("c1", "cl", "column", json!({})),
                blk("c2", "cl", "column", json!({})),
            ],
        );
        put(&s, "c1", &[tb("tg", "c1", "toggle")]);
        put(&s, "tg", &[tb("n", "tg", "numbered_list_item")]);
        put(&s, "c2", &[tb("p2", "c2", "paragraph")]);
        s.refresh();
        let _ = crate::view::view(&s);
        up(&mut s, Message::Focus("hbb".into()));
        up(&mut s, Message::Focus("n".into()));
        assert!(s.open.contains("h") && s.open.contains("tg"));
        let _ = crate::view::view(&s);
        up(&mut s, Message::Focus("code".into()));
        let _ = crate::view::view(&s);
    }

    fn key(c: &str, ctrl: bool, shift: bool) -> iced::Event {
        let mut m = keyboard::Modifiers::empty();
        m.set(keyboard::Modifiers::CTRL, ctrl);
        m.set(keyboard::Modifiers::SHIFT, shift);
        let k = match c {
            "tab" => Key::Named(keyboard::key::Named::Tab),
            "up" => Key::Named(keyboard::key::Named::ArrowUp),
            "down" => Key::Named(keyboard::key::Named::ArrowDown),
            c => Key::Character(c.into()),
        };
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: k.clone(),
            modified_key: k,
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
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
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
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
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

    /// "par" (workspace) with a cached child "kid", which has an uncached
    /// child_page block "unc".
    fn tree_state() -> (State, UnboundedReceiver<Cmd>) {
        use crate::outline::tests::blk;
        let (mut s, rx) = state();
        s.store
            .upsert_page(&page(
                "par",
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        s.store
            .upsert_page(&page("kid", json!({ "type": "page_id", "page_id": "par" })))
            .unwrap();
        s.store
            .replace_children(
                "kid",
                "kid",
                &[blk("unc", "kid", "child_page", json!({ "title": "Unc" }))],
            )
            .unwrap();
        s.refresh();
        (s, rx)
    }

    fn visible(s: &State) -> Vec<&str> {
        crate::sidebar::rows(&s.tree, &s.tree_open)
            .into_iter()
            .map(|r| r.entry.id.as_str())
            .collect()
    }

    #[test]
    fn sidebar_arrow_expands_without_opening() {
        let (mut s, mut rx) = tree_state();
        assert_eq!(visible(&s), ["par"]);
        up(&mut s, Message::SidebarExpand("par".into()));
        assert_eq!(visible(&s), ["par", "kid"]);
        assert_eq!(s.current, None, "not opened");
        assert!(rx.try_recv().is_err(), "nothing sent");
        assert!(s.store.queued_ops().unwrap().is_empty(), "no op queued");
        up(&mut s, Message::SidebarExpand("par".into()));
        assert_eq!(visible(&s), ["par"]);
        let _ = crate::view::view(&s);
    }

    #[test]
    fn sidebar_open_reveals_and_pulls_uncached() {
        let (mut s, mut rx) = tree_state();
        // opening a nested page expands its ancestors
        s.current = Some("unc".into());
        s.reveal_current();
        assert_eq!(visible(&s), ["par", "kid", "unc"]);
        s.current = None;
        s.tree_open.clear();

        up(&mut s, Message::SidebarOpen("kid".into()));
        assert_eq!(s.current.as_deref(), Some("kid"));
        assert!(rx.try_recv().is_err(), "cached: no network");
        assert_eq!(visible(&s), ["par", "kid"], "ancestors only");

        // a database entry: an error, no pull
        let db = notion_core::store::PageEntry { id: "db".into(), database: true, ..s.tree[0].clone() };
        s.tree.push(db);
        up(&mut s, Message::SidebarOpen("db".into()));
        assert_eq!(s.last_error.as_deref(), Some("databases not supported yet"));
        assert!(rx.try_recv().is_err());

        up(&mut s, Message::SidebarOpen("unc".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "unc".into(),
                force: false
            }
        );
        assert_eq!(s.current.as_deref(), Some("kid"), "until it's pulled");
        let _ = crate::view::view(&s);
    }

    #[test]
    fn ctrl_sharp_s_toggles_the_sidebar_on_german_layouts() {
        let id = iced::window::Id::unique();
        let (mut s, _rx) = state();
        let m = shortcut(key("ß", true, false), event::Status::Ignored, id).unwrap();
        up(&mut s, m);
        assert!(!s.sidebar);
    }

    #[test]
    fn ctrl_backslash_toggles_the_sidebar() {
        let id = iced::window::Id::unique();
        let (mut s, _rx) = state();
        assert!(s.sidebar);
        let m = shortcut(key("\\", true, false), event::Status::Ignored, id).unwrap();
        up(&mut s, m.clone());
        assert!(!s.sidebar);
        let _ = crate::view::view(&s);
        up(&mut s, m);
        assert!(s.sidebar);
    }

    #[test]
    fn search_overlay_opens_closes_and_picks() {
        let id = iced::window::Id::unique();
        let (mut s, mut rx) = tree_state();
        let ctrl_k = shortcut(key("k", true, false), event::Status::Ignored, id).unwrap();
        up(&mut s, ctrl_k.clone());
        assert!(s.searching);
        let _ = crate::view::view(&s);
        let esc = iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(keyboard::key::Named::Escape),
            modified_key: Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        up(&mut s, shortcut(esc, event::Status::Captured, id).unwrap());
        assert!(!s.searching);

        // search, then pick a result
        up(&mut s, Message::FocusSearch);
        up(&mut s, Message::SearchInput("ki".into()));
        up(&mut s, Message::SearchSubmit);
        assert_eq!(rx.try_recv().unwrap(), Cmd::Search("ki".into()));
        up(
            &mut s,
            Message::Worker(Arc::new(Event::SearchResults(vec![(
                "kid".into(),
                "kid".into(),
            )]))),
        );
        let _ = crate::view::view(&s);
        up(&mut s, Message::PickResult("kid".into()));
        assert!(!s.searching);
        assert_eq!(s.current.as_deref(), Some("kid"));

        // the backdrop closes it even mid-push
        up(&mut s, Message::FocusSearch);
        s.sent.push_back(true);
        up(&mut s, Message::SearchEscape);
        assert!(!s.searching);
    }

    fn chord(s: &mut State, c: &str, ctrl: bool, shift: bool) {
        let id = iced::window::Id::unique();
        let m = shortcut(key(c, ctrl, shift), event::Status::Captured, id).unwrap();
        up(s, m);
    }

    fn tab_pages(s: &State) -> Vec<Option<&str>> {
        (0..s.tabs.len())
            .map(|i| {
                if i == s.active {
                    s.current.as_deref()
                } else {
                    s.tabs[i].page.as_deref()
                }
            })
            .collect()
    }

    #[test]
    fn ctrl_t_opens_a_tab_with_the_overlay_and_a_pick_fills_it() {
        let (mut s, _rx) = tree_state();
        up(&mut s, Message::Open("par".into()));
        chord(&mut s, "t", true, false);
        assert_eq!((s.tabs.len(), s.active), (2, 1));
        assert!(s.searching);
        assert_eq!(s.current, None);
        assert!(s.rows.is_empty() && s.title.is_empty(), "old rows cleared");
        let _ = crate::view::view(&s);
        up(&mut s, Message::PickResult("kid".into()));
        assert!(!s.searching);
        assert_eq!(tab_pages(&s), [Some("par"), Some("kid")]);
        assert_eq!(s.tabs[0].title, "par", "parked with its title");
        // the sidebar opens in the active tab
        up(&mut s, Message::SidebarOpen("par".into()));
        assert_eq!(tab_pages(&s), [Some("par"), Some("par")]);
        up(&mut s, Message::Open("kid".into()));

        // Ctrl+W with the overlay open closes it too
        chord(&mut s, "t", true, false);
        chord(&mut s, "w", true, false);
        assert!(!s.searching);
        assert_eq!(tab_pages(&s), [Some("par"), Some("kid")]);

        // closing the overlay without a pick keeps the empty tab
        chord(&mut s, "t", true, false);
        up(&mut s, Message::SearchEscape);
        assert!(!s.searching);
        assert_eq!(tab_pages(&s), [Some("par"), Some("kid"), None]);
        assert_eq!(s.window_title(), env!("CARGO_PKG_NAME"));
        let _ = crate::view::view(&s);
    }

    #[test]
    fn ctrl_w_and_middle_click_close_tabs() {
        let (mut s, _rx) = tree_state();
        up(&mut s, Message::Open("par".into()));
        up(&mut s, Message::NewTab);
        up(&mut s, Message::PickResult("kid".into()));
        up(&mut s, Message::NewTab);
        up(&mut s, Message::SearchEscape);
        // middle-click on a background tab left of the active one
        up(&mut s, Message::CloseTab(0));
        assert_eq!(tab_pages(&s), [Some("kid"), None]);
        assert_eq!(s.active, 1);
        // Ctrl+W on the rightmost: the left one becomes active
        chord(&mut s, "w", true, false);
        assert_eq!(tab_pages(&s), [Some("kid")]);
        assert_eq!(s.current.as_deref(), Some("kid"));
        assert_eq!(s.title, "kid");
        // the last tab: one empty tab, no quit
        chord(&mut s, "w", true, false);
        assert_eq!(tab_pages(&s), [None]);
        assert!(s.rows.is_empty());
        up(&mut s, Message::CloseTab(5));
        assert_eq!(s.tabs.len(), 1, "out of range: ignored");
        let _ = crate::view::view(&s);
    }

    #[test]
    fn ctrl_tab_cycles_while_editing_and_tab_still_indents() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::NewTab);
        up(&mut s, Message::SearchEscape);
        up(&mut s, Message::SwitchTab(0));
        up(&mut s, Message::Focus("b".into()));
        // the editor leaves Ctrl+Tab to the window listener: no indent, no op
        assert!(press(&mut s, "ctrl+tab", None));
        assert!(press(&mut s, "ctrl+shift+tab", None));
        assert_eq!(depth(&s, "b"), 0);
        assert_eq!(editing(&s), Some("b"));
        chord(&mut s, "tab", true, false);
        assert_eq!(s.active, 1);
        assert_eq!(editing(&s), None);
        assert!(
            s.store.queued_ops().unwrap().is_empty(),
            "nothing to commit"
        );
        // wraps both ways
        chord(&mut s, "tab", true, false);
        assert_eq!(s.active, 0);
        chord(&mut s, "tab", true, true);
        assert_eq!(s.active, 1);
        chord(&mut s, "tab", true, true);
        assert_eq!(s.active, 0);
        assert_eq!(s.current.as_deref(), Some("pg"));

        // plain Tab still indents (under "m")
        up(&mut s, Message::Focus("b".into()));
        assert!(press(&mut s, "tab", None));
        assert_eq!(depth(&s, "b"), 1);
        assert_eq!(kinds(&s), ["move_block"]);
    }

    #[test]
    fn a_tab_switch_commits_and_restores_per_tab_state() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::ToggleOpen("a".into()));
        up(&mut s, Message::Scrolled(120.0));
        up(&mut s, Message::NewTab);
        up(&mut s, Message::PickResult("pg".into()));
        assert!(s.open.is_empty(), "a new tab starts collapsed");
        assert_eq!(s.scroll, 0.0);
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "x");
        up(&mut s, Message::SwitchTab(0));
        assert_eq!(editing(&s), None, "left unfocused");
        assert_eq!(kinds(&s), ["update_block"], "the edit was committed");
        let b = s.rows.iter().find(|r| r.block_id == "b").unwrap();
        assert_eq!(plain_text(&b.rich_text), "bx");
        assert!(s.open.contains("a"));
        assert_eq!(s.scroll, 120.0);
        up(&mut s, Message::SwitchTab(1));
        assert!(s.open.is_empty());
        assert_eq!(s.scroll, 0.0);
    }

    #[test]
    fn tabs_are_modal_during_a_push() {
        let (mut s, _rx) = editing_state();
        up(&mut s, Message::NewTab);
        up(&mut s, Message::SearchEscape);
        s.sent.push_back(true);
        for m in [Message::NewTab, Message::CloseTab(1), Message::SwitchTab(0)] {
            up(&mut s, m);
        }
        chord(&mut s, "tab", true, false);
        chord(&mut s, "w", true, false);
        assert_eq!((s.tabs.len(), s.active), (2, 1));
    }

    #[test]
    fn breadcrumb_lists_the_ancestors() {
        let (mut s, _rx) = tree_state();
        let ids = |s: &State, id: &str| -> Vec<String> {
            crate::sidebar::trail(&s.tree, id)
                .iter()
                .map(|e| e.id.clone())
                .collect()
        };
        assert_eq!(ids(&s, "unc"), ["par", "kid"]);
        assert_eq!(ids(&s, "kid"), ["par"]);
        assert!(ids(&s, "par").is_empty());
        up(&mut s, Message::Open("kid".into()));
        let _ = crate::view::view(&s);
    }

    const PID: &str = "0123abcd-0123-4567-89ab-0123456789ab";

    #[test]
    fn notion_urls_parse_to_page_ids() {
        let hex = "0123abcd0123456789ab0123456789ab";
        for url in [
            format!("https://www.notion.so/{hex}"),
            format!("https://notion.so/{PID}"),
            format!("https://www.notion.so/team/My-Page-{hex}?pvs=4"),
            format!("http://WWW.Notion.so/My-Page-{}#abc", hex.to_uppercase()),
            format!("https://acme.notion.site/Page-{hex}"),
            format!("https://www.notion.com/Page-{hex}"),
            format!("https://notion.com/{PID}?pvs=4"),
            format!("/{hex}"),
            format!("/My-Page-{PID}?v=1#x"),
        ] {
            assert_eq!(notion_page_id(&url).as_deref(), Some(PID), "{url}");
        }
        for url in [
            format!("https://example.com/{hex}"),
            format!("https://notion.so.evil.com/{hex}"),
            format!("https://fakenotion.site/{hex}"),
            format!("https://notion.com.evil.com/{hex}"),
            "https://www.notion.so/no-id-here".into(),
            format!("https://www.notion.so/{hex}/sub"),
            format!("file:///{hex}"),
            format!("https://www.notion.so/x?p={hex}"),
            "/short-abc".into(),
        ] {
            assert_eq!(notion_page_id(&url), None, "{url}");
        }
    }

    #[test]
    fn link_clicks_route_in_app_or_to_xdg_open() {
        assert!(
            matches!(link_target(&format!("https://www.notion.so/P-{}", PID.replace('-', ""))),
                Message::SidebarOpen(id) if id == PID)
        );
        assert!(
            matches!(link_target("https://example.com/a"), Message::OpenLink(u) if u == "https://example.com/a")
        );
        // an uncached Notion page is pulled, then opened here
        let (mut s, mut rx) = editing_state();
        up(&mut s, Message::LinkClick(format!("/{PID}")));
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: PID.into(),
                force: false
            }
        );
        // a cached one opens right away
        s.store
            .upsert_page(&page(
                PID,
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        up(
            &mut s,
            Message::LinkClick(format!("https://acme.notion.site/X-{PID}")),
        );
        assert_eq!(s.current.as_deref(), Some(PID));
        // (web links go to xdg-open: not clicked here, a test would start a browser)
    }

    #[test]
    fn title_edits_commit_and_queue_the_op() {
        let (mut s, _rx) = editing_state();
        assert!(s.title_editable);
        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::EditTitle);
        assert_eq!(editing(&s), None, "the row editor left");
        assert_eq!(s.title_edit.as_deref(), Some("pg"));
        up(&mut s, Message::TitleInput("New".into()));
        up(&mut s, Message::TitleCommit);
        assert_eq!(s.title_edit, None);
        assert_eq!(s.title, "New");
        assert_eq!(s.tree[0].title, "New");
        assert_eq!(kinds(&s), ["update_page_properties"]);
        // Esc commits too, into the same op
        up(&mut s, Message::EditTitle);
        up(&mut s, Message::TitleInput("Newer".into()));
        up(&mut s, Message::SearchEscape);
        assert_eq!((s.title_edit.as_deref(), s.title.as_str()), (None, "Newer"));
        assert_eq!(kinds(&s), ["update_page_properties"]);
        // unchanged: nothing queued; a click on a row commits
        up(&mut s, Message::EditTitle);
        up(&mut s, Message::Focus("b".into()));
        assert_eq!(s.title_edit, None);
        assert_eq!(editing(&s), Some("b"));
        assert_eq!(kinds(&s), ["update_page_properties"]);

        // a title with a mention is read-only
        let mut p = s.store.get_page("pg").unwrap().unwrap();
        p.properties.insert(
            "Name".into(),
            PropertyValue::Title(vec![serde_json::from_value(json!({ "type": "mention",
                "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
                "plain_text": "d", "annotations": {} }))
            .unwrap()]),
        );
        s.store.upsert_page(&p).unwrap();
        s.refresh();
        assert!(!s.title_editable);
        up(&mut s, Message::EditTitle);
        assert_eq!(s.title_edit, None);
    }

    #[test]
    fn auto_pull_waits_for_idle_and_rate_limits() {
        let secs = |n| Some(Duration::from_secs(n));
        assert!(auto_pull_due(None, true));
        assert!(auto_pull_due(secs(30), true));
        assert!(!auto_pull_due(secs(29), true), "focus flapping");
        assert!(!auto_pull_due(None, false));

        let (mut s, mut rx) = editing_state();
        let pulls =
            |rx: &mut UnboundedReceiver<Cmd>| std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert!(s.idle());
        up(&mut s, Message::Focus("a".into()));
        up(&mut s, Message::AutoPull);
        up(&mut s, Message::EditKey(EditKey::Escape));
        up(&mut s, Message::EditTitle);
        up(&mut s, Message::AutoPull);
        up(&mut s, Message::TitleCommit);
        up(&mut s, Message::FocusSearch);
        up(&mut s, Message::AutoPull);
        up(&mut s, Message::SearchEscape);
        assert_eq!(pulls(&mut rx), 0, "editing a row or the title, searching");
        up(&mut s, Message::Push);
        up(&mut s, Message::AutoPull);
        assert_eq!(rx.try_recv().unwrap(), Cmd::Push);
        assert_eq!(pulls(&mut rx), 0, "pushing");
        up(&mut s, Message::WorkerGone);
        s.last_error = None;
        up(&mut s, Message::AutoPull);
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "pg".into(),
                force: false
            }
        );
        // its answer is quiet: no error shown, a pending open kept
        s.pending_open = Some("other".into());
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Error("500 boom".into()))),
        );
        assert_eq!(s.last_error, None);
        assert_eq!(s.pending_open.as_deref(), Some("other"));
        assert!(s.sent.is_empty() && !s.auto_pulling);
        up(&mut s, Message::AutoPull);
        assert_eq!(pulls(&mut rx), 0, "within 30 s of the last");
        // offline: none at all
        s.last_auto = None;
        s.last_error = Some("offline: NOTION_TOKEN not set".into());
        up(&mut s, Message::AutoPull);
        assert_eq!(pulls(&mut rx), 0, "offline");
        // no page open
        let (mut s, mut rx) = state();
        up(&mut s, Message::AutoPull);
        assert_eq!(pulls(&mut rx), 0);
        // window focus maps to it
        assert!(matches!(
            shortcut(
                iced::Event::Window(iced::window::Event::Focused),
                event::Status::Ignored,
                iced::window::Id::unique()
            ),
            Some(Message::AutoPull)
        ));
    }

    #[test]
    fn sync_dots_problem_list_and_discard() {
        let (mut s, mut rx) = editing_state();
        sync::local_edit_text(&s.store, "a", "x").unwrap();
        let bad = sync::local_edit_text(&s.store, "b", "y").unwrap();
        s.store
            .mark_op(bad, OpStatus::Conflict, Some("remote edited"))
            .unwrap();
        s.refresh();
        assert_eq!(s.counts, [1, 1, 0]);
        assert_eq!(s.queued_ids, HashSet::from(["a".to_string()]));
        assert_eq!(s.problem_ids, HashSet::from(["b".to_string()]));
        let p = &s.problems[0];
        assert_eq!(
            (
                p.kind,
                p.label.as_str(),
                p.page.as_deref(),
                p.error.as_str()
            ),
            ("edit", "y", Some("pg"), "remote edited")
        );
        let _ = crate::view::view(&s);

        up(&mut s, Message::ToggleProblems);
        assert!(s.problems_open);
        let _ = crate::view::view(&s);
        up(&mut s, Message::SearchEscape);
        assert!(!s.problems_open, "Esc closes it");
        up(&mut s, Message::ToggleProblems);
        up(&mut s, Message::ToggleProblems);
        assert!(!s.problems_open, "a second click closes it");

        // a click opens the page and focuses the block
        up(&mut s, Message::Open("a".into()));
        up(&mut s, Message::ToggleProblems);
        up(&mut s, Message::OpenProblem(bad));
        assert!(!s.problems_open);
        assert_eq!(s.current.as_deref(), Some("pg"));
        assert_eq!(editing(&s), Some("b"));

        up(&mut s, Message::Discard(bad));
        assert!(s
            .store
            .ops_by_status(OpStatus::Conflict)
            .unwrap()
            .is_empty());
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "pg".into(),
                force: true
            }
        );
        assert!(s.problems.is_empty() && s.problem_ids.is_empty());
        assert_eq!(s.counts, [1, 0, 0]);
        up(&mut s, Message::ToggleProblems);
        assert!(!s.problems_open, "nothing to list");

        // a failed create: listed, focused and dotted by its own block;
        // Discard drops the local block and the ops on it too
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: vec![RichText::plain("new")],
            color: "default".into(),
        });
        let local = sync::local_create_block(&s.store, "a", None, para).unwrap();
        s.refresh();
        assert!(
            s.queued_ids.contains(&local),
            "its own row, not the parent's"
        );
        let op = s.store.queued_ops().unwrap().last().unwrap().id;
        s.store.mark_op(op, OpStatus::Failed, Some("400")).unwrap();
        s.refresh();
        let p = &s.problems[0];
        assert_eq!(
            (p.kind, p.label.as_str(), p.block.as_str()),
            ("create", "new", local.as_str())
        );
        assert_eq!(s.problem_ids, HashSet::from([local.clone()]));
        // a later delete of it, queued: goes with the discard
        let del = s
            .store
            .enqueue(
                OpKind::DeleteBlock {
                    block_id: local.clone(),
                },
                None,
                None,
            )
            .unwrap();
        up(&mut s, Message::Discard(op));
        assert!(s.store.get_block(&local).unwrap().is_none());
        assert!(s.store.queued_ops().unwrap().iter().all(|o| o.id != del));
    }

    // ------------------------------------------------------------ Q1b

    fn row_text(s: &State, id: &str) -> String {
        let r = s.rows.iter().find(|r| r.block_id == id).unwrap();
        plain_text(&r.rich_text)
    }

    fn queued(s: &State) -> usize {
        s.store.queued_ops().unwrap().len()
    }

    /// Focus `id`, type `t` at the end, Esc (commit).
    fn edit_row(s: &mut State, id: &str, t: &str) {
        up(s, Message::Focus(id.into()));
        type_text(s, t);
        up(s, Message::EditKey(EditKey::Escape));
    }

    #[test]
    fn ctrl_z_and_ctrl_shift_z_walk_the_edits() {
        let (mut s, _rx) = editing_state();
        edit_row(&mut s, "b", "1");
        edit_row(&mut s, "a", "2");
        assert_eq!((s.undo.len(), queued(&s)), (2, 2));
        // the editor leaves Ctrl+Z to the window listener (no double undo)
        up(&mut s, Message::Focus("b".into()));
        assert!(press(&mut s, "ctrl+z", Some("z")));
        assert_eq!(s.undo.len(), 2);

        chord(&mut s, "z", true, false);
        assert_eq!(
            (row_text(&s, "a"), row_text(&s, "b")),
            ("a".into(), "b1".into())
        );
        assert_eq!(queued(&s), 1);
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "b"), "b");
        assert_eq!(queued(&s), 0);
        chord(&mut s, "z", true, false);
        assert_eq!((s.undo.len(), s.redo.len()), (0, 2), "nothing left");

        chord(&mut s, "z", true, true);
        assert_eq!(row_text(&s, "b"), "b1");
        chord(&mut s, "z", true, true);
        assert_eq!(
            (row_text(&s, "a"), row_text(&s, "b")),
            ("a2".into(), "b1".into())
        );
        assert_eq!((queued(&s), s.undo.len(), s.redo.len()), (2, 2, 0));
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let (mut s, _rx) = editing_state();
        edit_row(&mut s, "b", "1");
        chord(&mut s, "z", true, false);
        assert_eq!(s.redo.len(), 1);
        edit_row(&mut s, "a", "2");
        assert!(s.redo.is_empty());
        chord(&mut s, "z", true, true);
        assert_eq!(row_text(&s, "b"), "b", "nothing redone");
    }

    #[test]
    fn undo_is_blocked_during_and_after_a_push() {
        let (mut s, _rx) = editing_state();
        edit_row(&mut s, "b", "1");
        up(&mut s, Message::Push);
        assert!(s.pushing());
        chord(&mut s, "z", true, false);
        assert_eq!(
            (row_text(&s, "b"), queued(&s)),
            ("b1".into(), 1),
            "mid-push"
        );
        // the worker pushed it
        let op = s.store.queued_ops().unwrap()[0].id;
        s.store.complete_op(op).unwrap();
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Pushed(Default::default()))),
        );
        assert!(s.undo.is_empty() && s.redo.is_empty());
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "b"), "b1");

        // the re-pull after the push answers (nothing new)
        up(
            &mut s,
            Message::Worker(Arc::new(Event::Pulled {
                id: "pg".into(),
                report: notion_core::sync::PullReport {
                    skipped: true,
                    ..Default::default()
                },
            })),
        );
        // refused once the op was completed or marked
        edit_row(&mut s, "a", "2");
        let op = s.store.queued_ops().unwrap()[0].id;
        s.store.complete_op(op).unwrap();
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "a"), "a2");
        assert!(s.last_error.as_deref().unwrap().contains("can't undo"));
        assert!(s.undo.is_empty());
    }

    #[test]
    fn ctrl_z_in_the_editor_undoes_the_typing_first() {
        let (mut s, _rx) = editing_state();
        edit_row(&mut s, "b", "1");
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "2");
        chord(&mut s, "z", true, false);
        assert_eq!(editing(&s), Some("b"));
        assert_eq!(buffer(&s), "b1", "typing undone");
        assert_eq!((s.undo.len(), s.redo.len()), (1, 1));
        chord(&mut s, "z", true, true);
        assert_eq!(buffer(&s), "b12", "and redone");
        assert_eq!(row_text(&s, "b"), "b12");
        chord(&mut s, "z", true, false);
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "b"), "b");
        assert_eq!(editing(&s), Some("b"), "still editing it");
        assert_eq!(buffer(&s), "b");
        assert_eq!(queued(&s), 0);

        // the title too
        up(&mut s, Message::EditTitle);
        up(&mut s, Message::TitleInput("New".into()));
        chord(&mut s, "z", true, false);
        assert_eq!((s.title.as_str(), s.title_edit.as_deref()), ("pg", None));
        chord(&mut s, "z", true, true);
        assert_eq!(s.title, "New");
    }

    #[test]
    fn pulls_pause_and_clear_undo() {
        let (mut s, _rx) = editing_state();
        edit_row(&mut s, "b", "1");
        up(&mut s, Message::Pull { force: false });
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "b"), "b1", "ignored while the pull runs");
        let pulled = |skipped| {
            Message::Worker(Arc::new(Event::Pulled {
                id: "pg".into(),
                report: notion_core::sync::PullReport {
                    skipped,
                    ..Default::default()
                },
            }))
        };
        up(&mut s, pulled(true));
        assert_eq!(s.undo.len(), 1, "a skipped pull wrote nothing");
        chord(&mut s, "z", true, false);
        assert_eq!(row_text(&s, "b"), "b");
        assert_eq!(s.redo.len(), 1);
        up(&mut s, Message::Pull { force: false });
        up(&mut s, pulled(false));
        assert!(s.undo.is_empty() && s.redo.is_empty());
    }

    #[test]
    fn enter_is_one_undo_step() {
        let (mut s, _rx) = editing_state();
        let before = ids(&s).iter().map(|x| x.to_string()).collect::<Vec<_>>();
        up(&mut s, Message::Focus("b".into()));
        type_text(&mut s, "x");
        up(&mut s, Message::EditKey(EditKey::Enter));
        assert_eq!(ids(&s).len(), before.len() + 1);
        assert_eq!(s.undo.len(), 1, "commit and create together");
        chord(&mut s, "z", true, false);
        assert_eq!(ids(&s), before);
        assert_eq!((row_text(&s, "b"), queued(&s)), ("b".into(), 0));
        // and the other edits: delete, indent, to-do
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Indent));
        assert_eq!(depth(&s, "b"), 1);
        chord(&mut s, "z", true, false);
        assert_eq!(depth(&s, "b"), 0);
        assert_eq!(queued(&s), 0);
    }

    #[test]
    fn undo_opens_the_edits_page() {
        let (mut s, _rx) = editing_state();
        s.store
            .upsert_page(&page(
                "p2",
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        edit_row(&mut s, "b", "1");
        up(&mut s, Message::Open("p2".into()));
        chord(&mut s, "z", true, false);
        assert_eq!(s.current.as_deref(), Some("pg"));
        assert_eq!(row_text(&s, "b"), "b");
    }

    #[test]
    fn recent_pages_in_the_overlay() {
        let (mut s, _rx) = tree_state();
        for p in ["par", "kid", "unc", "par"] {
            up(&mut s, Message::Open(p.into()));
        }
        assert_eq!(s.recent, ["par", "unc", "kid"]);
        let ids: Vec<String> = s.recent_pages().into_iter().map(|r| r.0).collect();
        assert_eq!(ids, ["par", "kid"], "only cached pages");

        chord(&mut s, "k", true, false);
        assert!(s.searching);
        let _ = crate::view::view(&s);
        chord(&mut s, "down", false, false);
        assert_eq!(s.recent_sel, 1);
        chord(&mut s, "down", false, false);
        assert_eq!(s.recent_sel, 0, "wraps");
        chord(&mut s, "up", false, false);
        up(&mut s, Message::SearchSubmit);
        assert!(!s.searching);
        assert_eq!(s.current.as_deref(), Some("kid"));
        assert_eq!(s.recent[0], "kid");
    }

    #[test]
    fn session_round_trips_through_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _rx) = tree_state();
        let _ = s.restore_session(dir.path().into(), Session::default(), None);
        up(&mut s, Message::Open("par".into()));
        up(&mut s, Message::NewTab);
        up(&mut s, Message::PickResult("kid".into()));
        up(&mut s, Message::Scrolled(30.0));
        up(&mut s, Message::ToggleSidebar);
        up(&mut s, Message::Resized(900.0, 700.0));
        let saved = s.session();
        drop(s);
        let (loaded, problem) = session::load(dir.path());
        assert_eq!(problem, None);
        assert_eq!(loaded, saved, "written on drop");
        assert_eq!(loaded.tabs[1].page.as_deref(), Some("kid"));
        assert_eq!(loaded.tabs[1].scroll, 30.0);

        let (mut t, mut rx) = tree_state();
        let _ = t.restore_session(dir.path().into(), loaded.clone(), Some("bad".into()));
        assert_eq!(t.session(), loaded);
        assert_eq!(t.current.as_deref(), Some("kid"));
        assert!(t.last_error.as_deref().unwrap().contains("bad"));
        assert!(rx.try_recv().is_err(), "cached: no pull");

        // an unknown page is pulled when its tab is activated
        let mut unknown = loaded;
        unknown.tabs[0].page = Some("gone".into());
        unknown.active = 0;
        let _ = t.restore_session(dir.path().into(), unknown, None);
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "gone".into(),
                force: false
            }
        );
    }

    #[test]
    fn title_delete_convert_and_check_undo() {
        let (mut s, _rx) = editing_state();
        let rows = ids(&s).iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // title
        up(&mut s, Message::EditTitle);
        up(&mut s, Message::TitleInput("New".into()));
        up(&mut s, Message::TitleCommit);
        assert_eq!((s.title.as_str(), queued(&s)), ("New", 1));
        chord(&mut s, "z", true, false);
        assert_eq!((s.title.as_str(), queued(&s)), ("pg", 0));

        // Backspace on the emptied row deletes it
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::EditKey(EditKey::Backspace));
        up(&mut s, Message::EditKey(EditKey::Backspace));
        assert!(!ids(&s).contains(&"b"));
        chord(&mut s, "z", true, false);
        assert_eq!(ids(&s), rows);
        assert_eq!((row_text(&s, "b"), queued(&s)), ("b".into(), 0));

        // convert a real block (create + delete), then check it
        up(&mut s, Message::Focus("b".into()));
        up(&mut s, Message::Command(Command::Block(TargetKind::ToDo)));
        let todo = editing(&s).unwrap().to_string();
        assert_ne!(todo, "b");
        up(&mut s, Message::ToggleCheck(todo.clone()));
        assert_eq!(marker(&s), Marker::Todo(true));
        chord(&mut s, "z", true, false);
        assert_eq!(marker(&s), Marker::Todo(false));
        chord(&mut s, "z", true, false);
        assert_eq!(ids(&s), rows, "the original block is back");
        assert_eq!(queued(&s), 0, "create and delete gone");
        assert_eq!(s.undo.len(), 0);
        assert_eq!(s.redo.len(), 2, "the convert cleared the older redos");
    }
}
