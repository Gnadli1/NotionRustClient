//! The shell around the page: store refresh, committing edits, sync commands
//! and worker events, tabs, the switcher, the problem list, session and the
//! frame. It mirrors notion-gui's `update()`; no editor features (E2).

use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use eframe::egui::{self, Frame, Key, Margin, Modifiers, Ui, ViewportCommand};
use notion_core::markdown::from_markdown;
use notion_core::model::{BlockKind, PropertyValue, RichText, RichTextKind};
use notion_core::ops::OpStatus;
use notion_core::sync;

use crate::app::{blur_first, Action, App, Grab};
use crate::outline::{self, Marker};
use crate::problems::{self, problem};
use crate::session::{self, Session};
use crate::switcher::{Out, Switcher};
use crate::tabs::{self, Tab};
use crate::view::{self, untitled};
use crate::worker::{Cmd, Event};
use crate::keys::{self, Command};
use crate::{sidebar, theme};

/// Auto-pulls at most this often, however the focus flaps.
const AUTO_PULL_GAP: Duration = Duration::from_secs(30);
/// The auto-pull timer.
pub const AUTO_PULL_EVERY: Duration = Duration::from_secs(300);
/// Next to notion-gui's `session.json`, which it must not clobber.
/// How long closing waits for the worker (a stalled request has no timeout).
const WORKER_JOIN: Duration = Duration::from_secs(10);
pub const SESSION_FILE: &str = "session-egui.json";

/// Whether an auto-pull may run: idle (see [`App::idle`]) and none in the
/// last [`AUTO_PULL_GAP`] (`since_last`: time since the last one, if any).
pub fn auto_pull_due(since_last: Option<Duration>, idle: bool) -> bool {
    idle && since_last.is_none_or(|d| d >= AUTO_PULL_GAP)
}

impl App {
    // ------------------------------------------------------------ store

    /// Reloads sidebar, outline and op counts from the store.
    pub(crate) fn refresh(&mut self) {
        if let Some(id) = &self.current {
            session::touch(&mut self.recent, id);
        }
        if let Err(e) = self.load_page().and_then(|()| self.load_ops()) {
            self.last_error = Some(e.to_string());
        }
        self.load_comments();
        // a push may have renamed the edited local-* block
        if let Some(f) = &mut self.focus {
            match self.store.resolve_id(&f.block_id) {
                Ok(id) => f.block_id = id,
                Err(e) => self.last_error = Some(e.to_string()),
            }
        }
        // the rows moved: find the edited one again, or drop the edit
        if let Some(f) = &mut self.focus {
            let found = self.rows.iter().position(|r| r.block_id == f.block_id);
            // a cell needs its row and column too
            let found = found.filter(|&i| match (&f.cell, &self.rows[i].marker) {
                (Some((row, col)), Marker::Table(t)) => {
                    t.rows.iter().any(|tr| tr.id == *row && tr.cells.len() > *col)
                }
                (Some(_), _) => false,
                (None, _) => true,
            });
            match found {
                Some(i) => f.idx = i,
                None => {
                    self.focus = None;
                    self.last_error = Some("the edited block is gone; edit discarded".into());
                }
            }
        }
        // a push renames local-* toggles: keep them expanded
        let open = std::mem::take(&mut self.open);
        self.open = open
            .into_iter()
            .map(|id| self.store.resolve_id(&id).unwrap_or(id))
            .collect();
        // a pull may have hidden the edited row inside a collapsed toggle
        if let Some(f) = &self.focus {
            for id in outline::ancestors(&self.rows, f.idx) {
                if self.rows.iter().any(|r| r.block_id == id && r.toggle) {
                    self.open.insert(id.to_string());
                }
            }
        }
        self.shown = outline::shown(&self.rows, &self.open);
    }

    fn load_page(&mut self) -> notion_core::Result<()> {
        self.tree = self.store.page_tree()?;
        self.jobs.clear();
        self.table_jobs.clear();
        let Some(id) = &self.current else {
            // an empty tab
            self.title.clear();
            self.title_editable = false;
            self.rows.clear();
            return Ok(());
        };
        let page = self.store.get_page(id)?;
        self.title = page
            .as_ref()
            .map_or_else(|| "(not cached)".into(), |p| p.title());
        // mentions, equations, links and formatting don't survive a plain-text edit
        self.title_editable = page.is_some_and(|p| {
            p.properties.values().any(|v| match v {
                PropertyValue::Title(rt) => rt.iter().all(|r| {
                    matches!(r.kind, RichTextKind::Text { .. })
                        && r.href.is_none()
                        && r.annotations == Default::default()
                }),
                _ => false,
            })
        });
        match outline::flatten(&self.store, id) {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                self.rows.clear();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Op counts, the rows' sync dots and the problem list.
    pub(crate) fn load_ops(&mut self) -> notion_core::Result<()> {
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

    pub(crate) fn cached(&self, id: &str) -> bool {
        matches!(self.store.get_page(id), Ok(Some(_)))
    }

    /// Expands the sidebar pages above the current one.
    pub(crate) fn reveal_current(&mut self) {
        let Some(id) = &self.current else { return };
        let up: Vec<String> = sidebar::ancestors(&self.tree, id)
            .into_iter()
            .map(String::from)
            .collect();
        self.tree_open.extend(up);
    }

    // ------------------------------------------------------------ edits

    /// Writes the edited text (row, cell or title) to the store if it changed
    /// (one op, through core, one undo step), keeping the editor open. False
    /// if the write failed. Locked and non-editable rows are never written.
    pub(crate) fn flush(&mut self) -> bool {
        let dirty = self.title_edit.as_ref().is_some_and(|t| t.text != t.original)
            || self.focus.as_ref().is_some_and(|f| f.text != f.original);
        if !dirty {
            return true;
        }
        self.step(Self::flush_now)
    }

    fn flush_now(&mut self) -> bool {
        let titled = self.write_title();
        let ok = self.write_row();
        if titled {
            self.refresh();
        }
        ok
    }

    fn write_row(&mut self) -> bool {
        let Some(f) = &self.focus else { return true };
        let Some(row) = self.rows.get(f.idx) else {
            return true;
        };
        // a table row is no text block: its cells are edited, not it
        if f.text == f.original || row.locked || (f.cell.is_none() && !row.editable) {
            return true;
        }
        let (idx, code, text) = (f.idx, f.code, f.text.clone());
        if let Some((row, col)) = f.cell.clone() {
            if !self.write_cell(&row, col, &text) {
                return false;
            }
            if let Some(f) = &mut self.focus {
                f.original = text;
                f.failed = false;
            }
            self.refresh(); // the table's cells
            return true;
        }
        let rich = if !code {
            from_markdown(&text)
        } else if text.is_empty() {
            Vec::new()
        } else {
            vec![RichText::plain(&text)]
        };
        // the block's kind in the store decides, not the row's marker
        let written = self.store.resolve_id(&f.block_id).and_then(|id| {
            match self.store.get_block(&id)?.map(|b| b.kind) {
                Some(BlockKind::Equation { .. }) => sync::local_edit_equation(&self.store, &id, &text),
                _ => sync::local_edit_rich(&self.store, &id, rich.clone()),
            }
        });
        if let Err(e) = written {
            self.last_error = Some(e.to_string());
            return false;
        }
        if let Some(f) = &mut self.focus {
            f.original = text;
            f.failed = false;
        }
        self.rows[idx].rich_text = rich;
        self.jobs.remove(&idx);
        // the queued count and the row's dot
        if let Err(e) = self.load_ops() {
            self.last_error = Some(e.to_string());
        }
        true
    }

    /// [`flush`](Self::flush), then stop editing; the editor stays if the
    /// write failed.
    pub(crate) fn commit(&mut self) -> bool {
        let ok = self.flush();
        if ok {
            self.focus = None;
            self.title_edit = None;
        }
        ok
    }

    // ------------------------------------------------------------- sync

    pub(crate) fn send(&mut self, cmd: Cmd) {
        match self.cmds.as_ref().map(|c| c.send(cmd.clone())) {
            Some(Ok(())) => self.sent.push_back(cmd),
            _ => self.last_error = Some("sync worker stopped".into()),
        }
    }

    /// A push is queued or running: editing is paused.
    pub(crate) fn pushing(&self) -> bool {
        self.sent.contains(&Cmd::Push)
    }

    /// Nothing is being edited, searched or sent, and a page is open.
    pub(crate) fn idle(&self) -> bool {
        self.current.is_some()
            && self.sent.is_empty()
            && self.focus.is_none()
            && self.title_edit.is_none()
            && self.switcher.is_none()
    }

    /// Ctrl+S. A failed commit keeps the editor; pushing now would lose the
    /// text. The editor closes: edits are paused until the push is done.
    pub(crate) fn push(&mut self) {
        if self.commit() {
            self.send(Cmd::Push);
        }
    }

    /// Ctrl+R / Ctrl+Shift+R: pulls the open page. The edit is written first
    /// (a pull keeps blocks with queued edits) and the editor stays.
    pub(crate) fn pull(&mut self, force: bool) {
        self.flush();
        if let Some(id) = self.current.clone() {
            self.send(Cmd::Pull { id, force });
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(ev) => self.on_event(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.worker_gone {
                        self.worker_gone = true;
                        self.sent.clear();
                        self.auto_pulling = false;
                        self.last_error.get_or_insert("sync worker stopped".into());
                    }
                    break;
                }
            }
        }
    }

    /// The worker answers each command with one event, in order.
    pub(crate) fn on_event(&mut self, ev: Event) {
        // pushed ops can't be undone, nor edits a pull rewrote under them
        let pulled = matches!(&ev, Event::Pulled { report, .. } if !report.skipped);
        let answered = self.sent.pop_front();
        if answered == Some(Cmd::Push) || pulled {
            self.undo.clear();
            self.redo.clear();
        }
        // auto-pulls only start with nothing in flight: this answers it
        if std::mem::take(&mut self.auto_pulling) {
            // quiet: no error shown or cleared, pending opens kept
            self.refresh();
            return;
        }
        match ev {
            Event::Pulled { id, report } => {
                let opened = self.pending_open.as_ref() == Some(&id) && self.commit();
                if opened {
                    self.pending_open = None;
                    self.set_current(&id);
                }
                self.last_error = None;
                self.info = Some(if report.skipped {
                    "up to date".into()
                } else {
                    format!("pulled: {} updated", report.updated)
                });
                // the editor keeps its own buffer; only the other rows change
                self.refresh();
                if opened {
                    self.reveal_current();
                }
            }
            Event::Pushed(r) => {
                // ids of pushed local blocks are gone now: drop the focus
                self.commit();
                self.last_error = None;
                self.info = Some(format!(
                    "pushed: {} applied, {} conflicts, {} failed",
                    r.applied, r.conflicts, r.failed
                ));
                self.refresh();
                // force: the page's edit time may match the cache, and the
                // re-pull is what turns local ids into real ones
                if let Some(id) = self.current.clone() {
                    self.send(Cmd::Pull { id, force: true });
                }
            }
            Event::CommentsPulled => self.refresh(),
            Event::Me(id) => self.me = Some(id),
            Event::SearchResults(r) => {
                if let Some(sw) = &mut self.switcher {
                    sw.results = r;
                }
            }
            Event::Error(e) => {
                // a failed comment fetch says nothing about the page being opened
                if !matches!(answered, Some(Cmd::PullComments { .. } | Cmd::Me)) {
                    self.pending_open = None;
                }
                // a push can fail after applying some ops
                self.refresh();
                self.info = None;
                self.last_error = Some(e);
            }
        }
    }

    // ------------------------------------------------------- navigation

    /// Makes `id` the page of the active tab; a different page starts at the top.
    pub(crate) fn set_current(&mut self, id: &str) {
        if self.current.as_deref() != Some(id) {
            self.scroll = 0.0;
            self.pending_scroll = Some(0.0);
        }
        self.current = Some(id.to_string());
    }

    /// Shows a cached page, or pulls an uncached one and opens it on arrival.
    pub(crate) fn open_or_pull(&mut self, id: &str) {
        if !self.commit() {
            return;
        }
        if self.cached(id) {
            self.pending_open = None;
            self.set_current(id);
            self.refresh();
            self.reveal_current();
        } else {
            self.pending_open = Some(id.to_string());
            self.send(Cmd::Pull {
                id: id.to_string(),
                force: false,
            });
        }
    }

    /// The switcher picked `id`; `new_tab` opens it in a tab of its own.
    pub(crate) fn pick(&mut self, id: &str, new_tab: bool) {
        self.switcher = None;
        if new_tab && !self.add_tab() {
            return;
        }
        self.open_or_pull(id);
    }

    pub(crate) fn open_switcher(&mut self) {
        if self.commit() {
            self.switcher = Some(Switcher::default());
        }
    }

    /// The active tab's live state.
    fn live_tab(&mut self) -> Tab {
        Tab {
            page: self.current.clone(),
            scroll: self.scroll,
            open: std::mem::take(&mut self.open),
            title: self.title.clone(),
        }
    }

    /// Commits the edit, parks the active tab and loads tab `to`. A failed
    /// commit keeps the editor and the tab.
    pub(crate) fn switch_tab(&mut self, to: usize) {
        if to == self.active || to >= self.tabs.len() || !self.commit() {
            return;
        }
        let live = self.live_tab();
        let next = tabs::switch(&mut self.tabs, self.active, to, live);
        self.active = to;
        self.load_tab(next);
    }

    /// Makes `t` the live state (editing has ended) and scrolls back to it.
    fn load_tab(&mut self, t: Tab) {
        // a late pull must not open its page in this tab
        self.pending_open = None;
        // Ctrl+W / Ctrl+Tab with the switcher open: it goes with the tab
        self.switcher = None;
        self.current = t.page;
        self.open = t.open;
        self.scroll = t.scroll;
        self.pending_scroll = Some(t.scroll);
        self.refresh();
        self.reveal_current();
        // e.g. a restored tab: pulled like any uncached page
        if let Some(id) = self.current.clone().filter(|id| !self.cached(id)) {
            self.pending_open = Some(id.clone());
            self.send(Cmd::Pull { id, force: false });
        }
    }

    /// A new empty tab, now active. False if the edit could not be committed.
    fn add_tab(&mut self) -> bool {
        if !self.commit() {
            return false;
        }
        self.tabs.push(Tab::default());
        let to = self.tabs.len() - 1;
        self.switch_tab(to);
        self.active == to
    }

    /// Ctrl+T and the `+` button: a new tab and the switcher over it.
    pub(crate) fn new_tab_with_switcher(&mut self) {
        if self.add_tab() {
            self.open_switcher();
        }
    }

    pub(crate) fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if i != self.active {
            self.active = tabs::close(&mut self.tabs, self.active, i);
            return;
        }
        if !self.commit() {
            return;
        }
        self.active = tabs::close(&mut self.tabs, self.active, i);
        let t = self.tabs[self.active].clone();
        self.load_tab(t);
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab.
    fn cycle_tab(&mut self, step: isize) {
        self.switch_tab(tabs::cycle(self.tabs.len(), self.active, step));
    }

    // --------------------------------------------------------- problems

    /// A problem was clicked: go to its page, and to the block if shown.
    pub(crate) fn open_problem(&mut self, op: i64) {
        let Some(p) = self.problems.iter().find(|p| p.op == op).cloned() else {
            return;
        };
        self.problems_open = false;
        let Some(page) = p.page else { return };
        self.open_or_pull(&page);
        if self.current.as_deref() == Some(page.as_str()) {
            if let Some(i) = self
                .rows
                .iter()
                .position(|r| r.block_id == p.block && r.editable)
            {
                self.start_edit(i, Grab::End);
            }
        }
    }

    /// Drops a conflict/failed op and force-pulls its page: the remote
    /// version wins. Not while a push runs: it is working on the ops.
    pub(crate) fn discard(&mut self, op: i64) {
        let Some(p) = self.problems.iter().find(|p| p.op == op).cloned() else {
            return;
        };
        if self.pushing() || !self.commit() {
            return;
        }
        // undo steps may hold the discarded ops
        self.undo.clear();
        self.redo.clear();
        if let Err(e) = problems::discard(&self.store, &p) {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
        if let Some(id) = p.page {
            self.send(Cmd::Pull { id, force: true });
        }
    }

    // ---------------------------------------------------------- session

    /// The state to write to session-egui.json.
    pub(crate) fn session(&self) -> Session {
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

    /// Restores a saved session (from [`session::load_from`]) and saves it to
    /// `dir` on exit; `problem` goes to the status bar.
    pub fn restore(&mut self, dir: std::path::PathBuf, s: Session, problem: Option<String>) {
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
        self.load_tab(t);
    }

    pub fn set_worker(&mut self, handle: std::thread::JoinHandle<()>) {
        self.worker = Some(handle);
    }

    /// Exit: the edit is written (unless a push owns the ops), the session
    /// saved, and the worker stopped, waiting for the command in flight: a
    /// push killed mid-way would re-send applied ops.
    pub(crate) fn shutdown(&mut self) {
        if !self.pushing() {
            self.commit();
        }
        if let Some(dir) = &self.session_dir {
            if let Err(e) = session::save_to(dir, SESSION_FILE, &self.session()) {
                eprintln!("cannot write the session: {e}");
            }
        }
        // the worker ends when its command sender is gone
        self.cmds = None;
        if let Some(h) = self.worker.take() {
            join_within(h, WORKER_JOIN);
        }
    }

    // ------------------------------------------------------------ frame

    /// "Page title — notion-egui", or just the crate name with no page open.
    pub(crate) fn window_title(&self) -> String {
        let app = env!("CARGO_PKG_NAME");
        match self.current {
            Some(_) => format!("{} — {app}", untitled(&self.title)),
            None => app.to_owned(),
        }
    }

    /// (page icon, title) of the pages above the open one, root first.
    pub(crate) fn breadcrumb(&self) -> Vec<(String, String, String)> {
        let Some(id) = &self.current else {
            return Vec::new();
        };
        sidebar::trail(&self.tree, id)
            .into_iter()
            .map(|e| (e.id.clone(), icon_of(e), untitled(&e.title).to_string()))
            .collect()
    }

    /// (icon, title) per tab.
    fn tab_labels(&self) -> Vec<(String, String)> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (page, parked) = if i == self.active {
                    (self.current.as_ref(), self.title.as_str())
                } else {
                    (t.page.as_ref(), t.title.as_str())
                };
                let Some(id) = page else {
                    return (String::new(), "New tab".to_string());
                };
                match self.tree.iter().find(|e| e.id == *id) {
                    Some(e) => (icon_of(e), untitled(&e.title).to_string()),
                    None => ("📄".to_string(), untitled(parked).to_string()),
                }
            })
            .collect()
    }

    /// Takes the global keymap commands from this frame's input, before any
    /// widget runs. The sidebar toggle is done at once; the ones that commit
    /// the edit are only noted: the editor has not taken this frame's typing
    /// yet, [`run_shortcut`](Self::run_shortcut) runs them after the page.
    fn shortcuts(&mut self, ctx: &egui::Context) -> Vec<Command> {
        let ctrl = ctx.input(|i| i.modifiers.ctrl);
        let map = &self.keys;
        let mut out =
            ctx.input_mut(|i| keys::take_commands(&mut i.events, ctrl, map, Command::is_global));
        if out.contains(&Command::ToggleSidebar) {
            self.sidebar = !self.sidebar;
            out.retain(|c| *c != Command::ToggleSidebar);
        }
        if self.problems_open
            && self.switcher.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.problems_open = false;
        }
        out
    }

    fn run_shortcut(&mut self, c: Command) {
        match c {
            Command::NextTab => self.cycle_tab(1),
            Command::PrevTab => self.cycle_tab(-1),
            Command::ForcePull => self.pull(true),
            Command::Pull => self.pull(false),
            Command::Push => self.push(),
            Command::Search if self.switcher.is_some() => self.switcher = None,
            Command::Search => self.open_switcher(),
            Command::NewTab => self.new_tab_with_switcher(),
            Command::CloseTab => self.close_tab(self.active),
            Command::Undo => self.undo_redo(true),
            Command::Redo => self.undo_redo(false),
            Command::Comments => self.toggle_comments(),
            _ => {}
        }
    }

    /// The switcher's keys, taken before any widget sees them.
    fn switcher_keys(&mut self, ctx: &egui::Context) {
        let Some(mut sw) = self.switcher.take() else {
            return;
        };
        let items = sw.items(&self.tree, &self.recent);
        match sw.keys(ctx, &items) {
            Out::None => self.switcher = Some(sw),
            Out::Close => {}
            Out::Pick { id, new_tab } => self.pick(&id, new_tab),
        }
    }

    /// Auto-pull: on the timer and when the window gains focus; never while
    /// editing, searching or with a command in flight, nor while offline.
    pub(crate) fn auto_pull_at(&mut self, now: Instant, gained_focus: bool) {
        let tick = now >= self.next_tick;
        if tick {
            self.next_tick = now + AUTO_PULL_EVERY;
        }
        if !(tick || gained_focus) {
            return;
        }
        let offline = self
            .last_error
            .as_deref()
            .is_some_and(|e| e.starts_with("offline"));
        let due = !offline && auto_pull_due(self.last_auto.map(|t| now - t), self.idle());
        if let Some(id) = self.current.clone().filter(|_| due) {
            self.last_auto = Some(now);
            self.send(Cmd::Pull { id, force: false });
            self.auto_pulling = !self.sent.is_empty();
        }
    }

    /// One frame (split from `App::ui` so tests can drive it without a window).
    pub(crate) fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.keylog {
            ctx.input(|i| keys::log(&i.raw.events));
        }
        self.drain_events();
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            self.window = (r.width(), r.height());
        }
        let shortcuts = self.shortcuts(&ctx);
        self.switcher_keys(&ctx);
        let focused = ctx.input(|i| i.focused);
        let gained = focused && !self.was_focused;
        self.was_focused = focused;
        let now = Instant::now();
        self.auto_pull_at(now, gained);
        ctx.request_repaint_after(self.next_tick.saturating_duration_since(now));
        let title = self.window_title();
        if title != self.shown_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }

        let c = theme::colors(ui);
        let mut acts: Vec<Action> = Vec::new();
        let labels = self.tab_labels();
        let bar = Frame::new()
            .fill(c.sidebar)
            .inner_margin(Margin::symmetric(6, 4));
        acts.extend(
            egui::Panel::top("tabs")
                .frame(bar)
                .show(ui, |ui| view::tabbar::show(ui, c, &labels, self.active))
                .inner,
        );
        let status = view::status::Status {
            counts: self.counts,
            busy: match (self.pushing(), self.sent.is_empty()) {
                (true, _) => Some("pushing…"),
                (false, false) => Some("syncing…"),
                (false, true) => None,
            },
            error: self.last_error.as_deref(),
            info: self.info.as_deref(),
            problems_open: self.problems_open,
            has_problems: !self.problems.is_empty(),
        };
        let bar = Frame::new()
            .fill(c.bg)
            .inner_margin(Margin::symmetric(12, 4));
        acts.extend(
            egui::Panel::bottom("status")
                .frame(bar)
                .show(ui, |ui| view::status::show(ui, c, &status))
                .inner,
        );
        if self.sidebar {
            let side = Frame::new().fill(c.sidebar).inner_margin(Margin::same(6));
            acts.extend(
                egui::Panel::left("tree")
                    .frame(side)
                    .size_range(180.0..=420.0)
                    .default_size(260.0)
                    .show(ui, |ui| {
                        view::sidebar::show(ui, c, &self.tree, &self.tree_open, self.current.as_deref())
                    })
                    .inner,
            );
        }
        self.comments_follow_focus();
        if self.comments.is_some() {
            let side = Frame::new().fill(c.sidebar).inner_margin(Margin::same(8));
            acts.extend(
                egui::Panel::right("comments")
                    .frame(side)
                    .size_range(260.0..=520.0)
                    .default_size(340.0)
                    .show(ui, |ui| self.comments_ui(ui, c))
                    .inner,
            );
        }
        let page = egui::CentralPanel::default()
            .show(ui, |ui| {
                if self.current.is_some() {
                    self.page_ui(ui)
                } else {
                    view::empty_tab(ui, c);
                    Vec::new()
                }
            })
            .inner;
        // a Blur from the edited row must not be lost to another row's action
        // (click on row B while A loses focus): Blur first, then the rest
        let mut page = page;
        page.extend(acts);
        let mut acts = page;
        if self.problems_open {
            acts.extend(view::status::problems(&ctx, c, &self.problems));
        }
        if let Some(mut sw) = self.switcher.take() {
            let items = sw.items(&self.tree, &self.recent);
            let out = view::switcher::show(&ctx, c, &mut sw, &items);
            if let Some(q) = sw.due_search(now) {
                self.send(Cmd::Search(q));
            }
            if let Some(wait) = sw.pending(now) {
                ctx.request_repaint_after(wait);
            }
            match out {
                Out::None => self.switcher = Some(sw),
                Out::Close => {}
                Out::Pick { id, new_tab } => self.pick(&id, new_tab),
            }
        }
        blur_first(&mut acts);
        for a in acts {
            self.apply(a);
        }
        if !shortcuts.is_empty() {
            for s in shortcuts {
                self.run_shortcut(s);
            }
            ctx.request_repaint(); // the panels above were drawn before it
        }
    }
}

/// Joins `h` if it ends within `wait`, else leaves it running: the process
/// exit takes it down.
fn join_within(h: std::thread::JoinHandle<()>, wait: Duration) {
    let end = Instant::now() + wait;
    while !h.is_finished() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    if h.is_finished() {
        let _ = h.join();
    }
}

fn icon_of(e: &notion_core::store::PageEntry) -> String {
    e.icon.clone().unwrap_or_else(|| "📄".into())
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn on_exit(&mut self) {
        self.shutdown();
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{add_page, key, page_in, Win};
    use eframe::egui::Event as Ev;
    use notion_core::model::{plain_text, Block, BlockKind, TargetKind};
    use notion_core::store::Store;
    use notion_core::sync::PullReport;

    fn secs(n: u64) -> Option<Duration> {
        Some(Duration::from_secs(n))
    }

    /// The iced rule, verbatim: idle, and not twice within the gap.
    #[test]
    fn auto_pull_rule() {
        assert!(auto_pull_due(None, true));
        assert!(auto_pull_due(secs(30), true));
        assert!(!auto_pull_due(secs(29), true), "focus flapping");
        assert!(!auto_pull_due(None, false));
    }

    #[test]
    fn auto_pull_on_focus_and_timer_only_when_idle() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = Instant::now();
        // nothing: no focus gain, no tick
        w.app.auto_pull_at(t0, false);
        assert!(w.cmds().is_empty());
        // focus gained: pulls the open page, quietly
        w.app.auto_pull_at(t0, true);
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: "p".into(),
                force: false
            }]
        );
        assert!(w.app.auto_pulling);
        // its answer is quiet and clears the flag
        w.answer(Event::Error("boom".into()));
        assert!(!w.app.auto_pulling);
        assert_eq!(w.app.last_error, None, "auto-pull errors are not shown");
        // flapping focus inside the gap: nothing
        w.app.auto_pull_at(t0 + Duration::from_secs(10), true);
        assert!(w.cmds().is_empty());
        // the 5 min timer
        w.app.auto_pull_at(t0 + AUTO_PULL_EVERY + AUTO_PULL_EVERY, false);
        assert_eq!(w.cmds().len(), 1);
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        // never while editing, or with a command in flight
        let later = t0 + Duration::from_secs(10_000);
        w.edit(0);
        w.app.auto_pull_at(later, true);
        assert!(w.cmds().is_empty(), "editing");
        w.app.commit();
        w.app.sent.push_back(Cmd::Search(String::new()));
        w.app.auto_pull_at(later, true);
        assert!(w.cmds().is_empty(), "in flight");
        w.app.sent.clear();
        // offline: not again until something else clears the message
        w.app.last_error = Some("offline: NOTION_TOKEN not set".into());
        w.app.auto_pull_at(later, true);
        assert!(w.cmds().is_empty(), "offline");
    }

    #[test]
    fn window_gaining_focus_triggers_the_auto_pull_in_a_frame() {
        let mut w = Win::with_texts(&["a"]);
        w.app.was_focused = false; // the window was in the background
        w.frames(vec![]);
        assert_eq!(w.cmds().len(), 1, "focused again: one pull");
        assert!(w.app.was_focused);
    }

    #[test]
    fn sync_shortcuts_send_the_right_commands() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::S, Modifiers::CTRL);
        assert_eq!(w.cmds(), [Cmd::Push]);
        assert!(w.app.pushing());
        w.app.sent.clear();
        let pull = |force| Cmd::Pull {
            id: "p".into(),
            force,
        };
        w.press(Key::R, Modifiers::CTRL);
        assert_eq!(w.cmds(), [pull(false)]);
        w.press(Key::R, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(w.cmds(), [pull(true)], "Shift+R is the force pull alone");
    }

    #[test]
    fn push_answers_with_a_forced_repull_and_pull_rebuilds_rows() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.press(Key::S, Modifiers::CTRL);
        w.cmds();
        w.answer(Event::Pushed(Default::default()));
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: "p".into(),
                force: true
            }]
        );
        assert_eq!(w.app.sent.len(), 1, "the re-pull is in flight");
        // a pull changed the cache: the rows follow
        let kid = crate::outline::tests::blk(
            "p-0",
            "p",
            "paragraph",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("remote") }),
        );
        w.app.store.put_block(&kid).unwrap();
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        assert!(w.app.sent.is_empty());
        assert_eq!(plain_text(&w.app.rows[0].rich_text), "remote");
    }

    #[test]
    fn a_pull_keeps_the_text_in_the_focused_editor() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(1);
        w.type_text("!");
        assert_eq!(w.text(), "b!");
        // the remote added a block above: the edited row moves to index 2
        let store = &w.app.store;
        let new = crate::outline::tests::blk(
            "new",
            "p",
            "paragraph",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("new") }),
        );
        let old: Vec<Block> = store.children("p").unwrap();
        let mut all = vec![old[0].clone(), new];
        all.push(old[1].clone());
        store.replace_children("p", "p", &all).unwrap();
        w.app.sent.push_back(Cmd::Search(String::new()));
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        let f = w.app.focus.as_ref().expect("still editing");
        assert_eq!((f.idx, f.text.as_str()), (2, "b!"));
        assert_eq!(f.block_id, "p-1");
        // and it is still the row that takes the typing
        w.type_text("?");
        assert_eq!(w.text(), "b!?");
        // a pull that drops the block ends the edit with a message
        store_drop(&mut w, "p-1");
        w.app.sent.push_back(Cmd::Search(String::new()));
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        assert!(w.app.focus.is_none());
        assert!(w.app.last_error.as_deref().unwrap().contains("gone"));
    }

    fn store_drop(w: &mut Win, id: &str) {
        w.app.store.delete_block(id).unwrap();
    }

    // ------------------------------------------------------- committing

    #[test]
    fn committing_a_changed_row_queues_one_op_and_an_unchanged_one_none() {
        let mut w = Win::with_texts(&["alpha", "beta"]);
        w.edit(0);
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.focus.is_none());
        assert_eq!(w.queued(), 0, "unchanged: nothing queued");

        w.edit(1);
        w.type_text(" x");
        assert_eq!(w.queued(), 0, "typing alone writes nothing");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!(w.queued(), 1, "exactly one op");
        assert_eq!(w.app.counts, [1, 0, 0]);
        assert!(w.app.queued_ids.contains("p-1"));
        assert_eq!(plain_text(&w.app.rows[1].rich_text), "beta x");
        // committing it again (re-focus, no change) adds none
        w.edit(1);
        w.app.commit();
        assert_eq!(w.queued(), 1);
        // the cached block holds the new text
        let b = w.app.store.get_block("p-1").unwrap().unwrap();
        assert_eq!(plain_text(&b.kind.text().unwrap().rich_text), "beta x");
    }

    #[test]
    fn locked_rows_are_never_written() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("z");
        w.app.rows[0].locked = true;
        assert!(w.app.commit());
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn a_failed_write_keeps_the_editor() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("z");
        w.app.store.delete_block("p-0").unwrap();
        assert!(!w.app.commit());
        assert!(w.app.focus.is_some());
        assert!(w.app.last_error.is_some());
    }

    #[test]
    fn editing_is_paused_while_a_push_runs() {
        let mut w = Win::with_texts(&["a"]);
        w.app.sent.push_back(Cmd::Push);
        w.app.apply(Action::Edit(0, Grab::End));
        assert!(w.app.focus.is_none());
    }

    // --------------------------------------------------------- problems

    #[test]
    fn dots_problem_list_and_discard() {
        let mut w = Win::with_texts(&["a", "b"]);
        sync::local_edit_text(&w.app.store, "p-0", "x").unwrap();
        let bad = sync::local_edit_text(&w.app.store, "p-1", "y").unwrap();
        w.app
            .store
            .mark_op(bad, OpStatus::Conflict, Some("remote edited"))
            .unwrap();
        w.app.refresh();
        w.frames(vec![]);
        assert_eq!(w.app.counts, [1, 1, 0]);
        assert_eq!(w.app.queued_ids.len(), 1);
        assert!(w.app.queued_ids.contains("p-0"));
        assert!(w.app.problem_ids.contains("p-1"));
        let p = &w.app.problems[0];
        assert_eq!(
            (p.kind, p.label.as_str(), p.page.as_deref(), p.error.as_str()),
            ("edit", "y", Some("p"), "remote edited")
        );

        w.app.apply(Action::ToggleProblems);
        assert!(w.app.problems_open);
        w.frames(vec![]); // draws the list
        w.press(Key::Escape, Modifiers::NONE);
        assert!(!w.app.problems_open, "Esc closes it");
        w.app.apply(Action::ToggleProblems);

        // a click goes to the block
        w.app.apply(Action::OpenProblem(bad));
        assert!(!w.app.problems_open);
        assert_eq!(w.app.focus.as_ref().map(|f| f.block_id.as_str()), Some("p-1"));
        w.app.commit();

        w.app.apply(Action::Discard(bad));
        assert!(w
            .app
            .store
            .ops_by_status(OpStatus::Conflict)
            .unwrap()
            .is_empty());
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: "p".into(),
                force: true
            }],
            "the remote version wins"
        );
        assert!(w.app.problems.is_empty() && w.app.problem_ids.is_empty());
        assert_eq!(w.app.counts, [1, 0, 0]);
        w.app.apply(Action::ToggleProblems);
        assert!(!w.app.problems_open, "nothing to list");

        // not while a push runs
        let bad = sync::local_edit_text(&w.app.store, "p-1", "q").unwrap();
        w.app.store.mark_op(bad, OpStatus::Failed, Some("400")).unwrap();
        w.app.refresh();
        w.app.sent.push_back(Cmd::Push);
        w.app.apply(Action::Discard(bad));
        assert_eq!(w.app.problems.len(), 1);
    }

    // -------------------------------------------------------------- tabs

    fn two_pages() -> Win {
        let store = Store::open_in_memory().unwrap();
        let tall: Vec<String> = (0..80).map(|i| format!("line {i}")).collect();
        let tall: Vec<&str> = tall.iter().map(String::as_str).collect();
        add_page(&store, "p1", None, &tall);
        add_page(&store, "p2", None, &tall);
        Win::new(store)
    }

    #[test]
    fn tabs_open_close_cycle_and_restore_scroll_and_toggles() {
        let mut w = two_pages();
        w.app.apply(Action::Open("p1".into()));
        w.frames(vec![]);
        // scrolled down on p1, one toggle expanded
        w.app.pending_scroll = Some(120.0);
        w.app.open.insert("tog".into());
        w.frames(vec![]);
        assert!((w.app.scroll - 120.0).abs() < 1.0, "{}", w.app.scroll);

        // Ctrl+T: a new empty tab with the switcher over it
        w.press(Key::T, Modifiers::CTRL);
        assert_eq!((w.app.tabs.len(), w.app.active), (2, 1));
        assert!(w.app.switcher.is_some());
        assert_eq!(w.app.current, None);
        assert!(w.app.open.is_empty(), "toggles are per tab");
        assert_eq!(w.app.tabs[0].page.as_deref(), Some("p1"));
        assert_eq!(w.app.tabs[0].scroll, 120.0, "parked");
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.switcher.is_none());
        w.app.apply(Action::Open("p2".into()));
        w.frames(vec![]);
        assert_eq!(w.app.scroll, 0.0);

        // Ctrl+Shift+Tab / Ctrl+Tab cycle (wrapping) and restore each tab
        w.press(Key::Tab, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(w.app.active, 0);
        assert_eq!(w.app.current.as_deref(), Some("p1"));
        w.frames(vec![]);
        assert!((w.app.scroll - 120.0).abs() < 1.0, "scroll restored: {}", w.app.scroll);
        assert!(w.app.open.contains("tog"));
        w.press(Key::Tab, Modifiers::CTRL);
        assert_eq!(w.app.active, 1);
        assert_eq!(w.app.current.as_deref(), Some("p2"));
        w.press(Key::Tab, Modifiers::CTRL);
        assert_eq!(w.app.active, 0, "wraps");

        // Ctrl+W closes the active tab: its right neighbour takes over
        w.press(Key::W, Modifiers::CTRL);
        assert_eq!((w.app.tabs.len(), w.app.active), (1, 0));
        assert_eq!(w.app.current.as_deref(), Some("p2"));
        // the last one: a single empty tab
        w.press(Key::W, Modifiers::CTRL);
        assert_eq!((w.app.tabs.len(), w.app.current.clone()), (1, None));
        assert_eq!(w.app.window_title(), "notion-egui");
    }

    #[test]
    fn closing_a_background_tab_keeps_the_open_page() {
        let mut w = two_pages();
        w.app.apply(Action::Open("p1".into()));
        w.app.apply(Action::NewTab);
        w.app.apply(Action::Open("p2".into()));
        w.app.apply(Action::CloseTab(0));
        assert_eq!((w.app.tabs.len(), w.app.active), (1, 0));
        assert_eq!(w.app.current.as_deref(), Some("p2"));
    }

    #[test]
    fn middle_click_closes_a_tab_in_the_bar() {
        let mut w = two_pages();
        w.app.apply(Action::Open("p1".into()));
        w.app.apply(Action::NewTab);
        w.press(Key::Escape, Modifiers::NONE);
        w.frames(vec![]);
        // the first tab sits at the top-left of the window
        let pos = egui::pos2(40.0, 18.0);
        let click = |b| {
            vec![
                Ev::PointerMoved(pos),
                Ev::PointerButton {
                    pos,
                    button: b,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Ev::PointerButton {
                    pos,
                    button: b,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        w.frames(vec![Ev::PointerMoved(pos)]);
        w.frames(click(egui::PointerButton::Middle));
        w.frames(vec![]);
        assert_eq!(w.app.tabs.len(), 1, "the first tab is gone");
        assert_eq!(w.app.current, None, "the empty tab remains");
    }

    // ----------------------------------------------------------- sidebar

    #[test]
    fn opening_a_page_expands_its_ancestors_in_the_tree() {
        let store = Store::open_in_memory().unwrap();
        for (id, parent) in [("top", None), ("mid", Some("top")), ("leaf", Some("mid"))] {
            store.upsert_page(&page_in(id, parent)).unwrap();
        }
        store.upsert_page(&page_in("other", None)).unwrap();
        let mut w = Win::new(store);
        let shown = |w: &Win| -> Vec<String> {
            sidebar::rows(&w.app.tree, &w.app.tree_open)
                .iter()
                .map(|r| r.entry.id.clone())
                .collect()
        };
        assert_eq!(shown(&w), ["other", "top"], "collapsed at first");
        w.app.apply(Action::Open("leaf".into()));
        assert!(w.app.tree_open.contains("top") && w.app.tree_open.contains("mid"));
        assert_eq!(shown(&w), ["other", "top", "mid", "leaf"]);
        // ▸/▾ toggles one level
        w.app.apply(Action::ToggleTree("mid".into()));
        assert_eq!(shown(&w), ["other", "top", "mid"]);
        // the breadcrumb: ancestors root first
        w.app.apply(Action::Open("leaf".into()));
        let crumbs = w.app.breadcrumb();
        let ids: Vec<&str> = crumbs.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(ids, ["top", "mid"]);
        w.frames(vec![]); // draws sidebar, breadcrumb, tabs, status
    }

    #[test]
    fn sidebar_toggle_keys_and_window_title_sent_only_on_change() {
        let mut w = Win::with_texts(&["a"]);
        assert!(w.app.sidebar);
        w.press(Key::Backslash, Modifiers::CTRL);
        assert!(!w.app.sidebar);
        w.press(Key::Minus, Modifiers::CTRL); // Ctrl+ß
        assert!(w.app.sidebar);
        assert_eq!(w.app.shown_title, "p — notion-egui");
        let cmds = w.frames(vec![]);
        assert!(
            !cmds.iter().any(|c| matches!(c, ViewportCommand::Title(_))),
            "unchanged title: nothing sent"
        );
        // a new page: one Title command
        add_page(&w.app.store, "q", None, &["x"]);
        w.app.apply(Action::Open("q".into()));
        let cmds = w.frames(vec![]);
        let titles: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                ViewportCommand::Title(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["q — notion-egui"]);
    }

    // ---------------------------------------------------------- switcher

    fn switcher_win() -> Win {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "a", None, &["x"]);
        add_page(&store, "b", None, &["x"]);
        add_page(&store, "c", None, &["x"]);
        // titles are the ids; make them searchable words
        for (id, t) in [("a", "Alpha"), ("b", "Beta kit"), ("c", "Kitchen")] {
            let mut p = store.get_page(id).unwrap().unwrap();
            p.properties.clear();
            let mut v = serde_json::to_value(&p).unwrap();
            v["properties"] = serde_json::json!({ "Name": { "id": "title", "type": "title", "title": [
                { "type": "text", "text": { "content": t }, "plain_text": t, "annotations": {} }] } });
            store.upsert_page(&serde_json::from_value(v).unwrap()).unwrap();
        }
        Win::new(store)
    }

    #[test]
    fn switcher_with_the_keyboard_only() {
        let mut w = switcher_win();
        w.app.recent = vec!["a".into()];
        w.press(Key::K, Modifiers::CTRL);
        assert!(w.app.switcher.is_some());
        // empty query: the recent list
        let sw = w.app.switcher.as_ref().unwrap();
        let ids: Vec<_> = sw.items(&w.app.tree, &w.app.recent).iter().map(|i| i.id.clone()).collect();
        assert_eq!(ids, ["a"]);
        // type: the field has the focus without a click
        w.type_text("ki");
        assert_eq!(w.app.switcher.as_ref().unwrap().query, "ki");
        // "Kitchen" (prefix) first, then "Beta kit"; ↓ moves to the second
        w.send(key(Key::ArrowDown));
        assert_eq!(w.app.switcher.as_ref().unwrap().sel, 1);
        w.send(key(Key::Enter));
        assert!(w.app.switcher.is_none(), "closed");
        assert_eq!(w.app.current.as_deref(), Some("b"));
        assert_eq!(w.app.tabs.len(), 1);
        assert_eq!(w.app.recent[0], "b");
        assert!(w.cmds().is_empty(), "cached: no pull");
        // the search went out after the debounce
        // (a fresh switcher: type, wait, see the command)
        w.press(Key::K, Modifiers::CTRL);
        w.type_text("zz");
        std::thread::sleep(Duration::from_millis(350));
        w.frames(vec![]);
        assert_eq!(w.cmds(), [Cmd::Search("zz".into())]);
        w.frames(vec![]);
        assert!(w.cmds().is_empty(), "sent once");
        // the answer is merged in; Enter on an uncached hit pulls it
        w.app.sent.clear();
        w.answer(Event::SearchResults(vec![("remote".into(), "Zzz".into())]));
        w.send(key(Key::Enter));
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: "remote".into(),
                force: false
            }]
        );
        assert_eq!(w.app.pending_open.as_deref(), Some("remote"));
        assert_eq!(w.app.current.as_deref(), Some("b"), "opens when it arrives");
        add_page(&w.app.store, "remote", None, &["r"]);
        w.answer(Event::Pulled {
            id: "remote".into(),
            report: PullReport::default(),
        });
        assert_eq!(w.app.current.as_deref(), Some("remote"));
    }

    #[test]
    fn switcher_arrows_wrap_and_esc_closes() {
        let mut w = switcher_win();
        w.app.recent = vec!["a".into(), "b".into(), "c".into()];
        w.press(Key::K, Modifiers::CTRL);
        w.send(key(Key::ArrowUp));
        assert_eq!(w.app.switcher.as_ref().unwrap().sel, 2, "wraps to the last");
        w.send(key(Key::ArrowDown));
        assert_eq!(w.app.switcher.as_ref().unwrap().sel, 0);
        w.send(key(Key::Escape));
        assert!(w.app.switcher.is_none());
        assert_eq!(w.app.current, None);
    }

    #[test]
    fn ctrl_enter_opens_in_a_new_tab() {
        let mut w = switcher_win();
        w.app.apply(Action::Open("a".into()));
        w.app.recent = vec!["c".into(), "a".into()];
        w.press(Key::K, Modifiers::CTRL);
        w.frames(vec![key_ctrl_enter()]);
        w.frames(vec![]);
        assert_eq!((w.app.tabs.len(), w.app.active), (2, 1));
        assert_eq!(w.app.current.as_deref(), Some("c"));
        assert_eq!(w.app.tabs[0].page.as_deref(), Some("a"), "the old tab keeps its page");
        assert!(w.app.switcher.is_none());
    }

    fn key_ctrl_enter() -> Ev {
        crate::testkit::key_mod(Key::Enter, Modifiers::CTRL)
    }

    // ----------------------------------------------------------- session

    #[test]
    fn session_round_trip_through_a_tempdir() {
        let dir = tempfile::tempdir().unwrap().keep();
        let conf = dir.join("notion-client");
        let mut w = two_pages();
        w.app.apply(Action::Open("p1".into()));
        w.app.apply(Action::ToggleTree("p1".into()));
        w.app.apply(Action::NewTab);
        w.press(Key::Escape, Modifiers::NONE);
        w.app.apply(Action::Open("p2".into()));
        w.app.window = (900.0, 700.0);
        w.app.session_dir = Some(conf.clone());
        w.app.shutdown();
        assert!(conf.join(SESSION_FILE).exists());
        assert!(!conf.join("session.json").exists(), "the iced app's file is left alone");

        let (s, problem) = session::load_from(&conf, SESSION_FILE);
        assert_eq!(problem, None);
        assert_eq!((s.tabs.len(), s.active), (2, 1));
        assert_eq!(s.tabs[0].page.as_deref(), Some("p1"));
        assert_eq!(s.tabs[1].page.as_deref(), Some("p2"));
        assert!(s.expanded.contains("p1"));
        assert_eq!(s.recent[0], "p2");
        assert_eq!(s.window, (900.0, 700.0));

        // a fresh app picks it up
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p1", None, &["a"]);
        add_page(&store, "p2", None, &["b"]);
        let mut w2 = Win::new(store);
        w2.app.restore(conf.clone(), s, None);
        assert_eq!((w2.app.tabs.len(), w2.app.active), (2, 1));
        assert_eq!(w2.app.current.as_deref(), Some("p2"));
        assert!(w2.app.tree_open.contains("p1"));
        assert_eq!(w2.app.window, (900.0, 700.0));
        assert!(w2.cmds().is_empty(), "cached pages are not pulled");
        // an uncached restored page is pulled
        let mut w3 = Win::new(Store::open_in_memory().unwrap());
        let (s, _) = session::load_from(&conf, SESSION_FILE);
        w3.app.restore(conf, s, None);
        assert_eq!(w3.cmds().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn shutdown_commits_the_edit_saves_and_joins_the_worker() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("z");
        // a worker that exits once its command sender is dropped
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        w.app.cmds = Some(tx);
        w.app.set_worker(std::thread::spawn(move || {
            while rx.blocking_recv().is_some() {}
        }));
        w.app.shutdown();
        assert!(w.app.worker.is_none() && w.app.cmds.is_none());
        assert_eq!(w.queued(), 1, "the typed text was committed");
    }

    // ------------------------------------- conversions are written, not shown

    fn kind_of(w: &Win, id: &str) -> String {
        w.app.store.get_block(id).unwrap().unwrap().kind.type_name().to_string()
    }

    fn focused_id(w: &Win) -> String {
        w.app.focus.as_ref().expect("editing").block_id.clone()
    }

    #[test]
    fn slash_equation_then_typing_writes_an_equation_op() {
        let mut w = Win::with_texts(&[""]);
        w.edit(0);
        w.type_text("/");
        w.type_text("equation");
        assert!(w.menu().is_some());
        w.send(key(Key::Enter));
        // a synced block is replaced: the editor follows the new id
        let id = focused_id(&w);
        assert!(id.starts_with("local-"), "{id}");
        assert_eq!(kind_of(&w, &id), "equation");
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Equation);
        assert!(w.app.focus.as_ref().unwrap().code, "raw TeX from now on");
        assert_eq!(w.app.last_error, None);
        w.type_text("x^2");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!(w.app.last_error, None);
        assert!(w.app.focus.is_none(), "the editor can be left");
        let b = w.app.store.get_block(&id).unwrap().unwrap();
        assert!(matches!(b.kind, BlockKind::Equation { ref expression } if expression == "x^2"));
        // the create and the delete of the old block; the edit rewrote the create
        assert_eq!(w.queued(), 2);
    }

    #[test]
    fn converting_an_equation_back_writes_rich_text() {
        let mut w = Win::with_texts(&["a"]);
        w.app.apply(Action::Edit(0, Grab::End));
        w.app.apply(Action::Convert {
            kind: TargetKind::Equation,
            text: "a".into(),
            typed: "a".into(),
            at: 1,
        });
        w.frames(vec![]);
        w.app.commit();
        let eq = w.app.rows[0].block_id.clone();
        assert_eq!(kind_of(&w, &eq), "equation");
        // /text on it
        w.edit(0);
        w.type_text("b");
        w.app.apply(Action::Convert {
            kind: TargetKind::Paragraph,
            text: "ab".into(),
            typed: "ab".into(),
            at: 2,
        });
        w.frames(vec![]);
        let id = focused_id(&w);
        assert_eq!(kind_of(&w, &id), "paragraph");
        assert!(!w.app.focus.as_ref().unwrap().code);
        w.type_text("c");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!((w.app.last_error.clone(), w.app.focus.is_none()), (None, true));
        let b = w.app.store.get_block(&id).unwrap().unwrap();
        assert_eq!(plain_text(&b.kind.text().unwrap().rich_text), "abc");
    }

    #[test]
    fn a_write_that_keeps_failing_does_not_trap_the_editor() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("z");
        w.app.store.delete_block("p-0").unwrap();
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.focus.is_some(), "first Esc: the text stays");
        assert!(w.app.last_error.is_some());
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.focus.is_none(), "second Esc leaves");
    }

    #[test]
    fn slash_h2_on_a_cached_block_queues_the_conversion() {
        let mut w = Win::with_texts(&["", "keep"]);
        w.edit(0);
        w.type_text("/");
        w.type_text("h2");
        w.send(key(Key::Enter));
        let id = focused_id(&w);
        assert!(id.starts_with("local-"));
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Heading(2));
        assert_eq!(w.queued(), 2, "create + delete");
        assert_eq!(w.app.last_error, None);
        assert_eq!(w.app.rows[1].block_id, "p-1", "the neighbour is untouched");
        // the Markdown shortcut is the same path
        w.press(Key::Escape, Modifiers::NONE);
        w.edit(1);
        w.type_text("");
        w.app.apply(Action::Edit(1, Grab::Start));
        w.frames(vec![]);
        w.type_text("# ");
        assert_eq!(w.app.rows[1].marker, crate::outline::Marker::Heading(1));
        assert!(focused_id(&w).starts_with("local-"));
    }

    #[test]
    fn converting_a_block_with_children_is_refused_with_a_message() {
        let mut w = Win::with_texts(&["a"]);
        let kid = crate::outline::tests::blk(
            "c",
            "p-0",
            "paragraph",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("kid") }),
        );
        w.app.store.replace_children("p", "p-0", &[kid]).unwrap();
        w.app.refresh();
        w.edit(0);
        w.type_text(" /");
        w.type_text("h1");
        w.send(key(Key::Enter));
        assert!(w.app.last_error.as_deref().unwrap().contains("children"));
        assert_eq!(focused_id(&w), "p-0");
        assert_eq!(w.text(), "a /h1", "left as typed");
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::None);
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn checking_a_todo_queues_an_op_and_is_refused_while_pushing() {
        let mut w = Win::with_texts(&["x"]);
        let todo = crate::outline::tests::blk(
            "p-0",
            "p",
            "to_do",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("t"), "checked": false }),
        );
        w.app.store.replace_children("p", "p", &[todo]).unwrap();
        w.app.refresh();
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(false));
        w.app.sent.push_back(Cmd::Push);
        w.app.apply(Action::Check(0));
        assert_eq!(w.queued(), 0, "not while a push runs");
        w.app.sent.clear();
        w.app.apply(Action::Check(0));
        assert_eq!(w.queued(), 1);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(true));
        w.app.apply(Action::Check(0));
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(false));
        assert_eq!(w.queued(), 2, "each change is its own op");
    }

    // ------------------------------------------------------- same-frame keys

    #[test]
    fn typing_and_ctrl_s_in_one_frame_commit_the_typing() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.frame_with(
            vec![Ev::Text("x".into()), crate::testkit::key_mod(Key::S, Modifiers::CTRL)],
            Modifiers::CTRL,
        );
        w.frames(vec![]);
        assert_eq!(w.queued(), 1);
        let b = w.app.store.get_block("p-0").unwrap().unwrap();
        assert_eq!(plain_text(&b.kind.text().unwrap().rich_text), "ax");
        assert_eq!(w.cmds(), [Cmd::Push]);
    }

    #[test]
    fn joining_a_stuck_worker_gives_up() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        let t = Instant::now();
        join_within(h, Duration::from_millis(100));
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(tx);
    }

    #[test]
    fn failed_comment_fetch_keeps_the_pending_page_open() {
        let mut w = Win::with_texts(&["a"]);
        w.app.pending_open = Some("remote".into());
        for cmd in [Cmd::PullComments { id: "p".into() }, Cmd::Me] {
            w.app.sent.push_back(cmd);
            w.answer(Event::Error("403".into()));
            assert_eq!(w.app.pending_open.as_deref(), Some("remote"));
        }
        assert_eq!(w.app.last_error.as_deref(), Some("403"), "still shown");
        // the page's own pull failing cancels the open
        w.app.sent.push_back(Cmd::Pull { id: "remote".into(), force: false });
        w.answer(Event::Error("404".into()));
        assert_eq!(w.app.pending_open, None);
    }
}
