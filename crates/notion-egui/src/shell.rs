//! The shell around the page: store refresh, committing edits, sync commands
//! and worker events, tabs, the switcher, the problem list, session and the
//! frame. It mirrors notion-gui's `update()`; no editor features (E2).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use eframe::egui::{self, Frame, Key, Margin, Modifiers, Ui, ViewportCommand};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, BlockKind, PropertyValue, RichText, RichTextKind};
use notion_core::ops::OpStatus;
use notion_core::sync::{self, Skip};

use crate::app::{blur_first, Action, App, Grab};
use crate::keys::{self, Command};
use crate::nav::{self, Find, History};
use crate::outline::{self, Marker, Row};
use crate::problems::{self, problem};
use crate::session::{self, Session};
use crate::switcher::{Out, Switcher};
use crate::tabs::{self, Tab};
use crate::view::{self, untitled, FindOut};
use crate::worker::{Cmd, Event};
use crate::{sidebar, theme};

/// The open page is polled this often while the window has focus (near
/// live), this often without it, and every other tab's page this often.
pub const POLL_LIVE: Duration = Duration::from_secs(3);
pub const POLL_OPEN: Duration = Duration::from_secs(15);
pub const POLL_BG: Duration = Duration::from_secs(60);
/// The comment panel's threads are polled this often while the window has focus.
pub const POLL_COMMENTS: Duration = Duration::from_secs(10);
/// A window that gains focus polls its page unless it was polled this lately.
const FOCUS_GAP: Duration = Duration::from_secs(5);
/// Notion's edit time has minute precision, so a poll of a recently edited
/// page pulls in full even if the time is unchanged: at most this often per page.
pub(crate) const FULL_MIN: Duration = Duration::from_secs(60);
/// Live polling pulls the open page in full every [`FULL_COST`] times as long
/// as its last full pull took (that keeps a big page at about a third of the
/// worker's time), but not more often than this nor less than [`FULL_MIN`].
const FULL_LIVE_MIN: Duration = Duration::from_secs(5);
const FULL_COST: u32 = 3;
/// How long a block a poll changed stays tinted.
pub const FLASH: Duration = Duration::from_secs(2);
/// The workspace's users are pulled again this often (they rarely change).
pub const USERS_EVERY: Duration = Duration::from_secs(3600);
/// The least time between on-demand user pulls (`@` menu, switcher).
const USERS_MIN: Duration = Duration::from_secs(60);
/// Next to notion-gui's `session.json`, which it must not clobber.
/// How long closing waits for the worker (a stalled request has no timeout).
const WORKER_JOIN: Duration = Duration::from_secs(10);
pub const SESSION_FILE: &str = "session-egui.json";

impl App {
    // ------------------------------------------------------------ store

    /// Reloads sidebar, outline and op counts from the store.
    pub(crate) fn refresh(&mut self) {
        self.media.forget_srcs();
        if let Some(id) = &self.current {
            session::touch(&mut self.recent, id);
            self.visited.insert(id.clone(), crate::comments::now_secs());
        }
        // the table's cells show user names
        self.load_users();
        if let Err(e) = self.load_page().and_then(|()| self.load_ops()) {
            self.last_error = Some(e.to_string());
        }
        self.load_comments();
        self.refresh_unread();
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
                (Some((row, col)), Marker::Table(t)) => t
                    .rows
                    .iter()
                    .any(|tr| tr.id == *row && tr.cells.len() > *col),
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
        if let Some((a, b)) = self.sel.take() {
            let r = |id: String| self.store.resolve_id(&id).unwrap_or(id);
            self.sel = Some((r(a), r(b)));
            if self.sel_idx().is_empty() {
                self.sel = None;
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
        self.heads = nav::headings(&self.rows);
        if let Some(f) = &mut self.find {
            f.search(&self.rows);
        }
    }

    fn load_page(&mut self) -> notion_core::Result<()> {
        self.tree = self.store.page_tree()?;
        self.jobs.clear();
        self.table_jobs.clear();
        let Some(id) = &self.current else {
            // an empty tab
            self.title.clear();
            self.title_editable = false;
            self.page_edit = None;
            self.meta = Default::default();
            self.page_head = None;
            self.block_edits.clear();
            self.rows.clear();
            self.db = None;
            return Ok(());
        };
        if self.store.data_source(id)?.is_some() {
            return self.load_db(&id.clone());
        }
        self.db = None;
        let page = self.store.get_page(id)?;
        self.page_edit = page.as_ref().map(|p| {
            (
                p.last_edited_by().map(str::to_string),
                p.last_edited_time.timestamp(),
            )
        });
        self.page_head = None;
        self.meta = page
            .as_ref()
            .map(crate::pages::Meta::of)
            .unwrap_or_default();
        self.block_edits = self.store.block_edits(id)?;
        // viewing the page reads its Updates entries (C5), once the window is
        // focused (on focus gain the frame loop marks it)
        if let Some(e) = self
            .was_focused
            .then(|| self.store.mark_page_read(id))
            .and_then(Result::err)
        {
            self.last_error = Some(e.to_string());
        }
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

    /// A cached page or data source.
    pub(crate) fn cached(&self, id: &str) -> bool {
        matches!(self.store.get_page(id), Ok(Some(_)))
            || matches!(self.store.data_source(id), Ok(Some(_)))
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
        // a table has no block or title editor, only a cell
        if self.db.is_some() {
            return self.db_flush();
        }
        let dirty = self
            .title_edit
            .as_ref()
            .is_some_and(|t| t.text != t.original)
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
                Some(BlockKind::Equation { .. }) => {
                    sync::local_edit_equation(&self.store, &id, &text)
                }
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
        let ok = if self.db.is_some() {
            self.db_commit(true)
        } else {
            self.flush() && self.db_commit(true)
        };
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

    /// Asks the worker for the pictures the last frame wanted, a few at a time.
    fn media_pump(&mut self) {
        for id in self.media.take_bad() {
            let _ = notion_core::media::forget(&self.store, &id); // a retry downloads again
        }
        if self.worker_gone || self.cmds.is_none() {
            return;
        }
        let out = self
            .sent
            .iter()
            .filter(|c| matches!(c, Cmd::Media { .. }))
            .count();
        for (id, page) in self
            .media
            .take_wants(crate::media::IN_FLIGHT.saturating_sub(out))
        {
            self.send(Cmd::Media { id, page });
        }
    }

    /// A command other than a picture download is in flight (those never block).
    fn busy(&self) -> bool {
        self.sent.iter().any(|c| !matches!(c, Cmd::Media { .. }))
    }

    /// A push is queued or running: editing is paused.
    pub(crate) fn pushing(&self) -> bool {
        self.sent.contains(&Cmd::Push)
    }

    /// Reloads the cached workspace users (names, mentions, avatars).
    pub(crate) fn load_users(&mut self) {
        match self.store.users() {
            Ok(u) => {
                self.users = u;
                self.page_head = None;
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Asks the worker for the workspace's users and starts the hour.
    pub(crate) fn request_users(&mut self) {
        self.users_at = Some(Instant::now());
        self.send(Cmd::PullUsers);
    }

    /// The `@` menu and the switcher need names: pulls them when no user with
    /// a name is cached (inline id-only ones don't count) and the last pull
    /// is a minute old, unless one is already out or users are off (403, no
    /// token).
    pub(crate) fn ensure_users(&mut self) {
        let settled = self.users_at.is_none_or(|t| t.elapsed() >= USERS_MIN);
        if !self.users.iter().any(|u| !u.name.is_empty())
            && settled
            && !self.users_stop
            && !self.worker_gone
            && !self.sent.contains(&Cmd::PullUsers)
        {
            self.request_users();
        }
    }

    /// The startup pull (`users_at` is None) and the hourly refresh, only
    /// when nothing else is in flight: never ahead of the restore page pull.
    pub(crate) fn users_tick(&mut self, now: Instant) {
        let due = self
            .users_at
            .is_none_or(|t| now.saturating_duration_since(t) >= USERS_EVERY);
        if due && !self.users_stop && !self.worker_gone && !self.busy() {
            self.users_at = Some(now);
            self.send(Cmd::PullUsers);
        }
    }

    /// A poll must wait: a command is in flight, the switcher, title editor or
    /// a menu is open, we are offline, or the worker is gone. Editing a block
    /// does not block it.
    fn poll_blocked(&self) -> bool {
        self.worker_gone
            || self.busy()
            || self.title_edit.is_some()
            || self.switcher.is_some()
            || self.focus.as_ref().is_some_and(|f| f.menu.is_some())
            || self.offline()
    }

    /// The pages to poll with their period: the open one, then the other
    /// tabs' (once each).
    pub(crate) fn poll_pages(&self) -> Vec<(String, Duration)> {
        let open = if self.was_focused {
            POLL_LIVE
        } else {
            POLL_OPEN
        };
        let mut v: Vec<(String, Duration)> =
            self.current.iter().map(|p| (p.clone(), open)).collect();
        for (i, t) in self.tabs.iter().enumerate() {
            if let Some(p) = t
                .page
                .as_ref()
                .filter(|p| i != self.active && v.iter().all(|(q, _)| q != *p))
            {
                v.push((p.clone(), POLL_BG));
            }
        }
        // a page that isn't pushed yet doesn't exist for Notion
        v.retain(|(p, _)| !p.starts_with("local-"));
        v
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
        if let Some(id) = self.current.clone().filter(|id| !id.starts_with("local-")) {
            self.send(Cmd::Pull { id, force });
        }
    }

    /// Pulls every shared page and data source. The editor's text is written
    /// first (a pull keeps blocks with queued edits); you can read meanwhile.
    pub(crate) fn sync_all(&mut self) {
        if self.sent.contains(&Cmd::SyncAll) {
            return;
        }
        self.flush();
        self.progress = Some((0, 0));
        self.send(Cmd::SyncAll);
    }

    /// The status bar's "pushing…" / "syncing 12/164…" while a command is in flight.
    pub(crate) fn busy_label(&self) -> Option<String> {
        match (self.pushing(), !self.busy(), self.progress) {
            (true, ..) => Some("pushing…".into()),
            (false, false, Some((done, total @ 1..))) => Some(format!("syncing {done}/{total}…")),
            (false, false, _) => Some("syncing…".into()),
            (false, true, _) => None,
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
                        self.media.worker_stopped();
                        self.sent.clear();
                        self.progress = None;
                        self.last_error.get_or_insert("sync worker stopped".into());
                    }
                    break;
                }
            }
        }
    }

    /// The worker answers each command with one event, in order.
    pub(crate) fn on_event(&mut self, ev: Event) {
        // not an answer: `sent` keeps its command
        if let Event::Progress { done, total } = ev {
            self.progress = Some((done, total));
            return;
        }
        if let Event::Media { id, path } = ev {
            self.sent.pop_front();
            self.media.downloaded(id, path);
            return;
        }
        if matches!(
            ev,
            Event::Pulled { .. } | Event::Pushed(_) | Event::SyncedAll(_)
        ) {
            self.media.retry_failed();
        }
        // pushed ops can't be undone, nor edits a pull rewrote under them; a
        // poll clears only what it changed (see `on_poll`)
        let pulled = matches!(&ev, Event::Pulled { report, .. } if !report.skipped);
        let answered = self.sent.pop_front();
        let poll = matches!(answered, Some(Cmd::Poll { .. }));
        if answered == Some(Cmd::Push) || (pulled && !poll) {
            self.undo.clear();
            self.redo.clear();
        }
        if let Event::Pulled { id, report } = &ev {
            if !report.skipped {
                self.full_at.insert(id.clone(), Instant::now());
                if let Some(t) = self.poll_sent.filter(|_| poll) {
                    self.pull_took.insert(id.clone(), t.elapsed());
                }
            }
        }
        if poll {
            self.poll_sent = None;
        }
        if poll {
            self.on_poll(ev);
            return;
        }
        match ev {
            Event::Pulled { id, report } => {
                let opened = self.pending_open.as_ref() == Some(&id) && self.commit();
                if opened {
                    self.pending_open = None;
                    // a database opens as its first data source
                    let to = self.db_target(&id).unwrap_or_else(|| id.clone());
                    self.full_at.insert(to.clone(), Instant::now());
                    self.set_current(&to);
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
                // a new page's title edit must write to its real id
                self.remap_ids();
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
            Event::CommentsPulled | Event::UsersPulled => self.refresh(),
            Event::SyncedAll(r) => {
                self.progress = None;
                // pages were rewritten under the undo steps
                if r.pages + r.data_sources > 0 {
                    self.undo.clear();
                    self.redo.clear();
                }
                self.last_error = r
                    .errors
                    .first()
                    .map(|e| format!("{} failed, first: {e}", r.errors.len()));
                self.info = Some(format!(
                    "synced: {} pages, {} databases, {} up to date",
                    r.pages, r.data_sources, r.skipped
                ));
                self.refresh();
            }
            Event::Progress { .. } => {}
            Event::Views { ds, views } => {
                self.saved_views.insert(ds, views);
                self.refresh();
            }
            // saved views are optional: any error leaves the local views
            Event::Error(_) if matches!(answered, Some(Cmd::Views { .. })) => {}
            Event::Me(id) => self.me = Some(id),
            Event::UsersDenied => {
                // said once: users_stop keeps every later trigger quiet
                self.users_stop = true;
                self.users_note = Some(
                    "Can't read workspace users (403): turn on \"Read user information\" for the integration. Names show as ids."
                        .into(),
                );
            }
            // no token: nothing to say (the first real command will); else retry in an hour
            Event::Error(e) if answered == Some(Cmd::PullUsers) => {
                if e.starts_with("offline") {
                    self.users_stop = true;
                } else {
                    self.last_error = Some(format!("users not loaded: {e}"));
                }
            }
            Event::Fetched { op, what } => {
                if let Some(p) = self.conflict.as_mut().filter(|p| p.op == op) {
                    p.got(what);
                }
            }
            Event::Resolved(_) => {
                self.conflict = None;
                // the op and the cache changed under the undo steps
                self.undo.clear();
                self.redo.clear();
                self.last_error = None;
                self.info = Some("re-queued: Ctrl+S pushes it".into());
                self.refresh();
            }
            Event::SearchResults(r) => {
                if let Some(sw) = &mut self.switcher {
                    sw.results = r;
                }
            }
            Event::Media { .. } => {} // taken above
            Event::Error(e) => {
                if answered == Some(Cmd::SyncAll) {
                    self.progress = None;
                }
                // a failed comment fetch says nothing about the page being opened
                if !matches!(
                    answered,
                    Some(
                        Cmd::PullComments { .. }
                            | Cmd::Me
                            | Cmd::Fetch { .. }
                            | Cmd::Resolve { .. }
                            | Cmd::SyncAll
                    )
                ) {
                    self.pending_open = None;
                }
                match (&answered, &mut self.conflict) {
                    // only the open panel's own fetch: a late answer for a closed one is not its
                    (Some(Cmd::Fetch { op }), Some(p)) if p.op == *op => {
                        p.error_fetching = Some(e.clone())
                    }
                    // Notion's text moved on: show the new one
                    (Some(Cmd::Resolve { op, .. }), Some(p))
                        if p.op == *op && e.contains("changed again") =>
                    {
                        p.fetched = None;
                        p.error_fetching = None;
                        self.send(Cmd::Fetch { op: *op });
                    }
                    _ => {}
                }
                // a push can fail after applying some ops
                self.refresh();
                self.info = None;
                self.last_error = Some(e);
            }
        }
    }

    /// A poll's answer, quietly: no message, pending open or `current` touched.
    /// A skipped poll changed nothing. A pulled one drops the undo steps of
    /// its page only if the page really changed, tints the changed blocks and
    /// re-seeds a focused block the user has not typed in. A dirty buffer is
    /// never touched (its edit was written before the poll, see `poll_flush`).
    fn on_poll(&mut self, ev: Event) {
        let (id, report) = match ev {
            Event::Pulled { id, report } if !report.skipped => (id, report),
            // back off as for any command
            Event::Error(e) if e.starts_with("offline") => {
                self.last_error = Some(e);
                return;
            }
            _ => return,
        };
        let old = self.rows.clone();
        self.refresh();
        let here = self.current.as_ref() == Some(&id);
        let changed = if here {
            changed_ids(&old, &self.rows)
        } else {
            Vec::new()
        };
        let moved = here
            && old
                .iter()
                .map(|r| &r.block_id)
                .ne(self.rows.iter().map(|r| &r.block_id));
        if report.updated > 0 || !changed.is_empty() || moved {
            self.undo.retain(|(p, _)| *p != id);
            self.redo.retain(|(p, _)| *p != id);
        }
        let until = Instant::now() + FLASH;
        self.flash
            .extend(changed.iter().map(|b| (b.clone(), until)));
        // typed in since the poll was sent: the buffer stays, but the push
        // would write it over the remote edit
        if self
            .focus
            .as_ref()
            .is_some_and(|f| f.text != f.original && changed.contains(&f.block_id))
        {
            self.info = Some(
                "this block was edited in Notion meanwhile — your push may overwrite it".into(),
            );
        }
        // a clean editor on a changed block would write its stale text over
        // the remote edit with the new base: show the new text (egui clamps
        // the cursor); a table cell is simply closed
        if self
            .focus
            .as_ref()
            .is_some_and(|f| f.text == f.original && changed.contains(&f.block_id))
        {
            let f = self.focus.as_mut().expect("checked");
            match self.rows.get(f.idx).filter(|_| f.cell.is_none()) {
                Some(r) => {
                    f.code = r.marker == Marker::Equation || matches!(r.marker, Marker::Code(_));
                    let t = if f.code {
                        plain_text(&r.rich_text)
                    } else {
                        to_markdown(&r.rich_text)
                    };
                    (f.prev_len, f.original, f.text) = (t.len(), t.clone(), t);
                }
                None => self.focus = None,
            }
        }
    }

    // ------------------------------------------------------- navigation

    /// Makes `id` the page of the active tab; a different page starts at the top.
    pub(crate) fn set_current(&mut self, id: &str) {
        if self.current.as_deref() != Some(id) {
            self.scroll = 0.0;
            self.pending_scroll = Some(0.0);
            self.find = None;
            self.sel = None;
        }
        let from = self.current.take();
        self.hist().go(from.as_deref(), id);
        self.current = Some(id.to_string());
    }

    /// The active tab's page history (the list grows with the tabs).
    fn hist(&mut self) -> &mut History {
        let n = self.tabs.len().max(self.active + 1);
        self.hists.resize_with(n, Default::default);
        &mut self.hists[self.active]
    }

    /// Back / forward in the tab's history; it is not a new step.
    pub(crate) fn go_history(&mut self, back: bool) {
        if !self.commit() {
            return;
        }
        let h = self.hist();
        if let Some(id) = if back { h.back() } else { h.forward() } {
            self.open_or_pull(&id);
        }
    }

    /// Ctrl+Shift+U: the page above the open one, if it is cached.
    fn open_parent(&mut self) {
        let parent = self
            .current
            .as_ref()
            .and_then(|id| self.tree.iter().find(|e| e.id == *id));
        match parent
            .and_then(|e| e.parent.clone())
            .filter(|p| self.cached(p))
        {
            Some(p) => self.open_or_pull(&p),
            None => self.info = Some("no cached parent page".into()),
        }
    }

    /// Scrolls row `i` into view on the next frame, opening the toggles above it.
    pub(crate) fn show_row(&mut self, i: usize) {
        let up = outline::ancestors(&self.rows, i);
        let toggles: Vec<String> = (self.rows.iter())
            .filter(|r| r.toggle && up.contains(&r.block_id.as_str()))
            .map(|r| r.block_id.clone())
            .collect();
        self.open.extend(toggles);
        self.shown = outline::shown(&self.rows, &self.open);
        self.goto = Some(i);
    }

    fn find_out(&mut self, out: FindOut) {
        let Some(f) = &mut self.find else { return };
        let row = match out {
            FindOut::Typed => {
                f.restart(&self.rows);
                f.hits.first().copied()
            }
            FindOut::Step(d) => f.step(d, &self.rows),
            FindOut::Close => {
                self.find = None;
                return;
            }
        };
        if let Some(i) = row {
            self.show_row(i);
        }
    }

    /// The first rows of a page's text, for the switcher's preview.
    fn preview_lines(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.preview_under(id, &mut out);
        out
    }

    /// Depth-first, stopping at 10 text rows. Child pages are leaves.
    fn preview_under(&self, parent: &str, out: &mut Vec<String>) {
        for b in self.store.children(parent).unwrap_or_default() {
            if out.len() >= 10 {
                return;
            }
            let text = b.kind.text().map(|t| plain_text(&t.rich_text));
            out.extend(text.filter(|t| !t.is_empty()));
            if !matches!(
                b.kind,
                BlockKind::ChildPage { .. } | BlockKind::ChildDatabase { .. }
            ) {
                self.preview_under(&b.id, out);
            }
        }
    }

    /// Shows a cached page, or pulls an uncached one and opens it on arrival.
    pub(crate) fn open_or_pull(&mut self, id: &str) {
        if !self.commit() {
            return;
        }
        // a database (a child_database block, a link) is its first data source
        let id = self.db_target(id).unwrap_or_else(|| id.to_string());
        let id = id.as_str();
        if self.cached(id) {
            self.pending_open = None;
            self.set_current(id);
            self.refresh();
            self.reveal_current();
            self.db_pull_if_stale();
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
        let moving = self.switcher.take().and_then(|s| s.moving);
        if let Some(page) = moving {
            return self.move_page(&page, id);
        }
        if new_tab && !self.add_tab() {
            return;
        }
        self.open_or_pull(id);
    }

    pub(crate) fn open_switcher(&mut self) {
        if self.commit() {
            let today = notion_core::markdown::local_day(crate::comments::now_secs());
            let day = |id: &String| {
                let t = self.visited.get(id).copied().or_else(|| {
                    let p = self.store.get_page(id).ok().flatten();
                    p.map(|p| p.last_edited_time.timestamp())
                });
                t.map(|t| (id.clone(), notion_core::markdown::local_day(t)))
            };
            let days = self.recent.iter().filter_map(day).collect();
            let mut sw = Switcher::default();
            (sw.today, sw.days, sw.paths) = (today, days, crate::switcher::paths(&self.tree));
            self.switcher = Some(sw);
            self.ensure_users();
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
        self.find = None;
        self.sel = None;
        self.current = t.page;
        self.open = t.open;
        self.scroll = t.scroll;
        self.pending_scroll = Some(t.scroll);
        self.refresh();
        self.reveal_current();
        // e.g. a restored tab: pulled like any uncached page
        if let Some(id) = self
            .current
            .clone()
            .filter(|id| !self.cached(id) && !id.starts_with("local-"))
        {
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
            self.drop_tab(i);
            return;
        }
        if !self.commit() {
            return;
        }
        self.drop_tab(i);
        let t = self.tabs[self.active].clone();
        self.load_tab(t);
    }

    /// Removes tab `i` and its history.
    fn drop_tab(&mut self, i: usize) {
        self.active = tabs::close(&mut self.tabs, self.active, i);
        if i < self.hists.len() {
            self.hists.remove(i);
        }
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
        self.conflict = None;
        // undo steps may hold the discarded ops
        self.undo.clear();
        self.redo.clear();
        // a never-created page can't be pulled; if it is gone, its tabs leave it
        let gone = p.page.clone().filter(|id| id.starts_with("local-"));
        let up = gone
            .as_ref()
            .and_then(|id| self.store.get_page(id).ok().flatten())
            .and_then(|pg| match pg.parent {
                notion_core::model::Parent::Page(x) => Some(x),
                _ => None,
            });
        if let Err(e) = problems::discard(&self.store, &p) {
            self.last_error = Some(e.to_string());
        }
        if let Some(id) = gone.filter(|id| !self.cached(id)) {
            self.leave_gone(&id, up);
        }
        self.refresh();
        if let Some(id) = p.page.filter(|id| !id.starts_with("local-")) {
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

    /// The workspace profiles, and where their files live.
    pub fn set_profiles(
        &mut self,
        profiles: crate::profiles::Profiles,
        profiles_dir: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        session_file: String,
    ) {
        self.profiles = profiles;
        self.profiles_dir = profiles_dir;
        self.data_dir = data_dir;
        self.session_file = session_file;
    }

    pub(crate) fn workspace_name(&self) -> &str {
        self.profiles
            .active_profile()
            .map_or("No workspace", |p| p.name.as_str())
    }

    /// Writes the active workspace's session, if there is a config dir.
    pub(crate) fn save_session(&self) {
        let Some(dir) = &self.session_dir else { return };
        if let Err(e) = session::save_to(dir, &self.session_file, &self.session()) {
            eprintln!("cannot write the session: {e}");
        }
    }

    /// Exit: the edit is written (unless a push owns the ops), the session
    /// saved, and the worker stopped, waiting for the command in flight: a
    /// push killed mid-way would re-send applied ops.
    pub(crate) fn shutdown(&mut self) {
        if !self.pushing() {
            self.commit();
        }
        self.save_session();
        // the worker ends when its command sender is gone
        self.cmds = None;
        if let Some(h) = self.worker.take() {
            join_within(h, WORKER_JOIN);
        }
    }

    // --------------------------------------------------------- workspaces

    /// Saves the add/edit workspace form. Adding a workspace, or editing the
    /// active one, switches to it (a token change restarts the worker).
    pub(crate) fn save_workspace_form(&mut self) {
        let Some(form) = self.ws_form.clone() else {
            return;
        };
        let before = form
            .original
            .as_deref()
            .and_then(|n| self.profiles.get(n))
            .cloned();
        let profile = match form.to_profile(before.as_ref()) {
            Ok(p) => p,
            Err(e) => {
                if let Some(f) = &mut self.ws_form {
                    f.error = Some(e);
                }
                return;
            }
        };
        if let Some(existing) = self.profiles.get(&profile.name) {
            if before.as_ref().map(|p| p.name.as_str()) != Some(existing.name.as_str()) {
                if let Some(f) = &mut self.ws_form {
                    f.error = Some(format!(
                        "a workspace named {:?} already exists",
                        profile.name
                    ));
                }
                return;
            }
        }
        let editing_active = before
            .as_ref()
            .is_some_and(|p| self.profiles.active.as_deref() == Some(p.name.as_str()));
        let adding = before.is_none();
        let mut next = self.profiles.clone();
        if let Some(old) = before.as_ref() {
            if old.name != profile.name {
                next.remove(&old.name);
            }
        }
        next.upsert(profile.clone());
        if adding || editing_active {
            next.set_active(&profile.name);
        }
        if let Some(dir) = self.profiles_dir.as_deref() {
            if let Err(e) = next.save(dir) {
                if let Some(f) = &mut self.ws_form {
                    f.error = Some(format!("cannot save profiles.json: {e}"));
                }
                return;
            }
        }
        self.profiles = next;
        self.ws_form = None;
        if adding || editing_active {
            self.pending_switch = Some((profile.name, true));
        }
    }

    /// Replaces the store, worker and session with another workspace. The
    /// current edit must be committed; a running push is waited for.
    pub(crate) fn switch_workspace(&mut self, name: &str, force: bool, ctx: &egui::Context) {
        if !force && self.workspace_name() == name {
            return;
        }
        let Some(profile) = self.profiles.get(name).cloned() else {
            self.last_error = Some(format!("unknown workspace {name:?}"));
            return;
        };
        if self.pushing() {
            self.last_error =
                Some("wait for the push to finish before switching workspaces".into());
            return;
        }
        if !self.commit() {
            return;
        }
        let Some(db) = profile.cache_path(self.data_dir.as_deref()) else {
            self.last_error = Some("no data folder for the workspace cache".into());
            return;
        };
        if let Some(dir) = db.parent().filter(|d| !d.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(dir) {
                self.last_error = Some(format!("cannot create {}: {e}", dir.display()));
                return;
            }
        }
        let store = match notion_core::store::Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                self.last_error = Some(format!("cannot open {}: {e}", db.display()));
                return;
            }
        };
        let session_file = profile.session_file();
        let (sess, problem) = self
            .session_dir
            .as_deref()
            .map(|d| session::load_from(d, &session_file))
            .unwrap_or_default();
        let token = profile.has_token().then(|| profile.token.clone());
        let browser = (profile.kind == crate::profiles::AuthKind::Browser)
            .then(|| {
                self.profiles
                    .browser
                    .clone()
                    .and_then(|b| profile.space_id.clone().map(|space| (b, space)))
            })
            .flatten();
        let user_id = profile
            .user_id
            .clone()
            .or_else(|| browser.as_ref().and_then(|(b, _)| b.user_id.clone()))
            .or_else(|| {
                std::env::var("NOTION_USER_ID")
                    .ok()
                    .filter(|u| !u.trim().is_empty())
            });
        self.save_session();
        // the old worker ends when its command sender is gone
        self.cmds = None;
        if let Some(h) = self.worker.take() {
            join_within(h, WORKER_JOIN);
        }
        let (cmds, events, worker) = match browser {
            Some((browser, space)) if !browser.token_v2.trim().is_empty() => {
                let cookies = browser.cookies.clone();
                crate::worker::spawn_browser(
                    db,
                    ctx.clone(),
                    browser.token_v2,
                    space,
                    user_id,
                    cookies,
                )
            }
            _ => crate::worker::spawn_profile(db, ctx.clone(), token, user_id),
        };
        let mut fresh = App::new(store, cmds, events, self.keylog);
        fresh.keys = self.keys.clone();
        fresh.opener = std::mem::replace(&mut self.opener, Box::new(|_| Ok(())));
        fresh.window = self.window;
        fresh.session_dir = self.session_dir.clone();
        fresh.session_file = session_file;
        fresh.profiles_dir = self.profiles_dir.clone();
        fresh.data_dir = self.data_dir.clone();
        fresh.clock = self.clock;
        let mut profiles = std::mem::take(&mut self.profiles);
        profiles.set_active(&profile.name);
        fresh.profiles = profiles;
        fresh.set_worker(worker);
        match fresh.session_dir.clone() {
            Some(dir) => fresh.restore(dir, sess, problem),
            None => fresh.last_error = problem,
        }
        *self = fresh;
        if let Some(dir) = self.profiles_dir.as_deref() {
            if let Err(e) = self.profiles.save(dir) {
                self.last_error = Some(format!("cannot save profiles.json: {e}"));
            }
        }
        // A browser workspace that has never been opened has an empty cache.
        // Discover its pages on first entry instead of showing a blank sidebar.
        if self.tree.is_empty()
            && self
                .profiles
                .active_profile()
                .is_some_and(|p| p.kind == crate::profiles::AuthKind::Browser)
        {
            self.sync_all();
        }
        ctx.request_repaint();
    }

    /// Runs a workspace switch asked for by an action this frame.
    pub(crate) fn apply_pending_switch(&mut self, ctx: &egui::Context) {
        if let Some((name, force)) = self.pending_switch.take() {
            self.switch_workspace(&name, force, ctx);
        }
    }

    fn save_profiles(&mut self) {
        if let Some(dir) = self.profiles_dir.as_deref() {
            if let Err(e) = self.profiles.save(dir) {
                self.last_error = Some(format!("cannot save profiles.json: {e}"));
            }
        }
    }

    // ------------------------------------------------------------ sign in

    /// "Sign in with Notion…": starts the stored OAuth app, or asks for its
    /// client id/secret the first time.
    pub(crate) fn start_oauth(&mut self) {
        self.ws_form = None;
        let app = self.profiles.oauth.clone().unwrap_or_default();
        if !app.is_configured() {
            self.oauth_setup = Some(app);
            self.oauth_error = None;
            return;
        }
        self.begin_oauth(app);
    }

    fn begin_oauth(&mut self, app: crate::profiles::OAuthApp) {
        match crate::oauth::Flow::start(app.clone()) {
            Ok(mut flow) => {
                if let Err(e) = flow.open_again() {
                    flow.error = Some(e);
                }
                self.profiles.oauth = Some(app);
                self.save_profiles();
                self.oauth = Some(flow);
                self.oauth_setup = None;
                self.oauth_error = None;
            }
            Err(e) => {
                self.oauth_setup = Some(app);
                self.oauth_error = Some(e);
            }
        }
    }

    pub(crate) fn save_oauth_setup(&mut self) {
        let Some(mut app) = self.oauth_setup.clone() else {
            return;
        };
        app.client_id = app.client_id.trim().to_string();
        app.client_secret = app.client_secret.trim().to_string();
        app.redirect_uri = app.redirect_uri.trim().to_string();
        let problem = if !app.is_configured() {
            Some("Both the OAuth client id and client secret are required.".to_string())
        } else if !(app.redirect_uri.starts_with("http://")
            || app.redirect_uri.starts_with("https://"))
        {
            Some("The redirect URI must be an http:// or https:// URL.".to_string())
        } else {
            None
        };
        if let Some(p) = problem {
            self.oauth_setup = Some(app);
            self.oauth_error = Some(p);
            return;
        }
        self.oauth_setup = None;
        self.oauth_error = None;
        self.begin_oauth(app);
    }

    pub(crate) fn start_browser_login(&mut self) {
        self.ws_form = None;
        self.oauth_setup = None;
        let (prefill, cookies) = self
            .profiles
            .browser
            .as_ref()
            .map(|b| (b.token_v2.as_str(), b.cookies.as_deref().unwrap_or("")))
            .unwrap_or(("", ""));
        self.browser_login = Some(crate::browser_login::Flow::new(prefill, cookies));
    }

    /// Discovers the account's shared workspaces in the background, so they
    /// appear in the picker even when the cookie was saved on an earlier run.
    pub(crate) fn start_space_refresh(&mut self) {
        if self.space_refresh.is_some() {
            return;
        }
        let Some(browser) = self.profiles.browser.as_ref() else {
            return;
        };
        let token = browser.token_v2.trim();
        if token.is_empty()
            || !self
                .profiles
                .profiles
                .iter()
                .any(|p| p.kind == crate::profiles::AuthKind::Browser)
        {
            return;
        }
        let cookies = browser.cookies.as_deref().unwrap_or("").to_string();
        let mut flow = crate::browser_login::Flow::new(token, &cookies);
        flow.connect();
        self.space_refresh = Some(flow);
    }

    pub(crate) fn poll_space_refresh(&mut self, ctx: &egui::Context) {
        let Some(flow) = self.space_refresh.as_mut() else {
            return;
        };
        let Some(result) = flow.poll() else {
            flow.tick(ctx);
            return;
        };
        let token = flow.token.trim().to_string();
        let cookies = crate::browser_login::normalize_header(&flow.cookies);
        self.space_refresh = None;
        let Ok((user_id, spaces)) = result else {
            return;
        };
        if spaces.is_empty() {
            return;
        }
        let before = self.profiles.profiles.len();
        let cookies = (!cookies.is_empty()).then_some(cookies);
        self.profiles
            .sync_browser_spaces(&token, user_id, cookies, &spaces);
        let added = self.profiles.profiles.len().saturating_sub(before);
        self.save_profiles();
        if added > 0 {
            self.info = Some(format!(
                "browser session: {added} more workspace{}",
                if added == 1 { "" } else { "s" }
            ));
        }
    }

    /// Polls the browser login; on success creates one profile per workspace.
    pub(crate) fn poll_browser_login(&mut self, ctx: &egui::Context) {
        let result = self
            .browser_login
            .as_mut()
            .and_then(crate::browser_login::Flow::poll);
        let Some(result) = result else {
            if let Some(flow) = &self.browser_login {
                flow.tick(ctx);
            }
            return;
        };
        match result {
            Ok((user_id, spaces)) => {
                if spaces.is_empty() {
                    if let Some(flow) = &mut self.browser_login {
                        flow.error = Some(
                            "No workspaces found. The cookie may be expired, or the account has \
                             no workspace access."
                                .into(),
                        );
                    }
                    return;
                }
                let (token, cookies) = self
                    .browser_login
                    .as_ref()
                    .map(|f| {
                        (
                            f.token.clone(),
                            crate::browser_login::normalize_header(&f.cookies),
                        )
                    })
                    .unwrap_or_default();
                let cookies = (!cookies.is_empty()).then_some(cookies);
                let names = self
                    .profiles
                    .sync_browser_spaces(&token, user_id, cookies, &spaces);
                self.save_profiles();
                self.browser_login = None;
                // Restart the current browser workspace so it gets the new
                // cookies; a first browser login falls back to the first one.
                let target = self
                    .profiles
                    .active
                    .clone()
                    .filter(|name| {
                        self.profiles.profiles.iter().any(|p| {
                            &p.name == name && p.kind == crate::profiles::AuthKind::Browser
                        })
                    })
                    .or_else(|| names.first().cloned());
                if let Some(target) = target {
                    self.pending_switch = Some((target, true));
                }
                self.info = Some(format!(
                    "browser session: {} workspace{}",
                    names.len(),
                    if names.len() == 1 { "" } else { "s" }
                ));
            }
            Err(e) => {
                if let Some(flow) = &mut self.browser_login {
                    flow.error = Some(e);
                }
            }
        }
    }

    fn finish_oauth(&mut self, token: notion_core::oauth::TokenResponse) {
        let profile = self.profiles.upsert_oauth(&token, "Notion workspace");
        self.save_profiles();
        self.info = Some(format!("signed in to {}", profile.name));
        self.pending_switch = Some((profile.name, true));
    }

    /// Polls the callback/listener once a frame while a sign-in is open.
    pub(crate) fn poll_oauth(&mut self, ctx: &egui::Context) {
        let Some(result) = self.oauth.as_mut().and_then(crate::oauth::Flow::poll) else {
            if self.oauth.is_some() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            return;
        };
        match result {
            Ok(token) => {
                self.oauth = None;
                self.finish_oauth(token);
            }
            Err(e) => {
                if let Some(f) = &mut self.oauth {
                    f.error = Some(e);
                    f.busy = false;
                }
            }
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
                    None => (String::new(), untitled(parked).to_string()),
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
        out.extend(ctx.input(|i| keys::mouse_nav(&i.events)));
        if self.ws_form.is_some()
            || self.oauth_setup.is_some()
            || self.oauth.is_some()
            || self.browser_login.is_some()
        {
            // a workspace/OAuth dialog has the keys
            out.clear();
        } else if out.contains(&Command::ToggleSidebar) {
            self.sidebar = !self.sidebar;
            out.retain(|c| *c != Command::ToggleSidebar);
        }
        if self.ws_form.is_some()
            || self.oauth_setup.is_some()
            || self.oauth.is_some()
            || self.browser_login.is_some()
        {
            // modal: nothing else runs
        } else if self.inbox.is_some() {
            // the inbox is modal: it has the keys, bar its own two
            out.retain(|c| matches!(c, Command::Updates | Command::MarkAllRead));
        } else if self.dlg.is_some() {
            // a page dialog is modal: it has the keys
            out.clear();
        } else if self.switcher.is_some() {
            // the switcher has Esc
        } else if let Some(p) = &self.conflict {
            // Esc closes it and changes nothing; Ctrl+Enter is Use merged
            let op = p.op;
            // not while another text box (a comment draft, the editor) has the keys
            let mine = !ctx.egui_wants_keyboard_input() || ctx.memory(|m| m.has_focus(p.merged_id));
            let (esc, go) = if mine {
                ctx.input_mut(|i| {
                    (
                        i.consume_key(Modifiers::NONE, Key::Escape),
                        keys::take_exact(&mut i.events, Modifiers::CTRL, Key::Enter).0 > 0,
                    )
                })
            } else {
                (false, false)
            };
            if esc {
                self.conflict = None;
            } else if go {
                self.merge_conflict(op);
            }
        } else if self.problems_open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.problems_open = false;
            } else if ctx.memory(|m| m.focused().is_none()) {
                // the selected entry: Up/Down move it, Enter opens it
                let (up, down, enter) = ctx.input_mut(|i| {
                    (
                        i.consume_key(Modifiers::NONE, Key::ArrowUp),
                        i.consume_key(Modifiers::NONE, Key::ArrowDown),
                        i.consume_key(Modifiers::NONE, Key::Enter),
                    )
                });
                let last = self.problems.len().saturating_sub(1);
                let sel = self.problem_sel.min(last);
                self.problem_sel = if up {
                    sel.saturating_sub(1)
                } else if down {
                    (sel + 1).min(last)
                } else {
                    sel
                };
                if enter {
                    if let Some(p) = self.problems.get(self.problem_sel) {
                        self.open_conflict(p.op);
                    }
                }
            }
        }
        // Esc closes the find bar when nothing else wants it
        let free = self.switcher.is_none()
            && self.dlg.is_none()
            && self.inbox.is_none()
            && self.conflict.is_none()
            && self.comments.is_none()
            && !self.problems_open
            && self.focus.as_ref().is_none_or(|f| f.menu.is_none())
            && !ctx.egui_wants_keyboard_input();
        if self.find.is_some()
            && free
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.find = None;
        }
        out
    }

    fn run_shortcut(&mut self, c: Command, ctx: &egui::Context) {
        match c {
            Command::Back
            | Command::Forward
            | Command::Parent
            | Command::Find
            | Command::CopyLink
                if self.switcher.is_some() || self.conflict.is_some() => {}
            Command::NewPage
            | Command::TrashPage
            | Command::PageIcon
            | Command::PageCover
            | Command::UploadFile
            | Command::MovePage
            | Command::ToggleLock
            | Command::TrashView
                if self.switcher.is_some() || self.conflict.is_some() => {}
            Command::NewPage
            | Command::TrashPage
            | Command::PageIcon
            | Command::PageCover
            | Command::UploadFile
            | Command::MovePage
            | Command::ToggleLock
            | Command::TrashView => self.page_command(c),
            Command::Updates | Command::MarkAllRead
                if self.switcher.is_some() || self.conflict.is_some() || self.dlg.is_some() => {}
            Command::Updates => self.toggle_inbox(),
            Command::MarkAllRead => self.mark_all_read(),
            Command::ToggleAll
                if self.switcher.is_some()
                    || self.conflict.is_some()
                    || self.focus.as_ref().is_some_and(|f| f.menu.is_some()) => {}
            Command::ToggleAll => self.toggle_all(),
            Command::Back => self.go_history(true),
            Command::Forward => self.go_history(false),
            Command::Parent => self.open_parent(),
            Command::Find if self.db.is_some() => {
                if let Some(d) = &mut self.db {
                    d.focus_filter = true;
                }
            }
            Command::Find if self.current.is_some() => {
                self.find.get_or_insert_with(Find::default).focus = true;
            }
            Command::CopyLink => {
                let page = self
                    .current
                    .as_deref()
                    .and_then(|id| self.store.get_page(id).ok().flatten());
                if let Some(p) = page.filter(|p| !p.url.is_empty()) {
                    ctx.copy_text(p.url);
                    self.info = Some("link copied".into());
                }
            }
            Command::NextTab => self.cycle_tab(1),
            Command::PrevTab => self.cycle_tab(-1),
            Command::ForcePull => self.pull(true),
            Command::Pull => self.pull(false),
            Command::Push => self.push(),
            Command::SyncAll => self.sync_all(),
            Command::Search if self.switcher.is_some() => self.switcher = None,
            Command::Search => self.open_switcher(),
            Command::NewTab => self.new_tab_with_switcher(),
            Command::CloseTab => self.close_tab(self.active),
            Command::Undo => self.undo_redo(true),
            Command::Redo => self.undo_redo(false),
            Command::Comments if self.db.is_some() => {
                self.info = Some("comments: open a row as a page (Ctrl+Shift+O)".into());
            }
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

    /// Polls one due page, the open one first: every [`POLL_OPEN`], the other
    /// tabs' every [`POLL_BG`], and the open one when the window gains focus.
    /// A poll is one request unless the page changed (see [`Skip`]). It runs
    /// while a block is edited: the edit is written first, so the pull keeps
    /// it (queued edits survive a pull) and a remote change to that block is
    /// a conflict at push; the editor's buffer and cursor are never touched.
    /// Pages seen for the first time wait a period (startup pulls go first).
    pub(crate) fn poll_at(&mut self, now: Instant, gained_focus: bool) {
        let open = self.current.clone();
        let due = self.poll_pages().into_iter().find(|(id, every)| {
            let focus = gained_focus && open.as_ref() == Some(id);
            let first = if focus {
                now.checked_sub(FOCUS_GAP).unwrap_or(now)
            } else {
                now
            };
            let last = *self.polled.entry(id.clone()).or_insert(first);
            now.saturating_duration_since(last) >= if focus { FOCUS_GAP } else { *every }
        });
        let Some((id, _)) = due.filter(|_| !self.poll_blocked()) else {
            return;
        };
        self.polled.insert(id.clone(), now);
        if open.as_ref() == Some(&id) && !self.poll_flush() {
            return;
        }
        let gap = match self.pull_took.get(&id) {
            Some(t) if self.was_focused && open.as_ref() == Some(&id) => {
                (*t * FULL_COST).clamp(FULL_LIVE_MIN, FULL_MIN)
            }
            _ => FULL_MIN,
        };
        let full_due = self
            .full_at
            .get(&id)
            .is_none_or(|t| now.saturating_duration_since(*t) >= gap);
        let skip = if full_due {
            sync::poll_settle()
        } else {
            Skip::IfUnchanged
        };
        // the real clock: the answer's `elapsed` measures the pull
        self.poll_sent = Some(Instant::now());
        self.send(Cmd::Poll { id, skip });
    }

    /// The threads in the comment panel (the page's, and the shown row's)
    /// every [`POLL_COMMENTS`] while the window has focus; the first a period
    /// after the panel opened (opening fetched them). One request each.
    pub(crate) fn poll_comments_at(&mut self, now: Instant) {
        if !self.was_focused || self.poll_blocked() {
            return;
        }
        let ids: Vec<String> = match &self.comments {
            Some(p) => self
                .current
                .iter()
                .chain(&p.block)
                .filter(|i| !i.starts_with("local-"))
                .cloned()
                .collect(),
            None => return,
        };
        let p = self.comments.as_mut().expect("checked");
        let last = *p.polled.get_or_insert(now);
        if ids.is_empty() || now.saturating_duration_since(last) < POLL_COMMENTS {
            return;
        }
        p.polled = Some(now);
        for id in ids {
            self.send(Cmd::PullComments { id });
        }
    }

    /// [`flush`](Self::flush) for a poll. Typing in one block for a minute
    /// must stay one undo step, so a flush that follows the previous poll
    /// flush with no other step in between replaces that step: it is redone
    /// from the older checkpoint.
    fn poll_flush(&mut self) -> bool {
        // a cell's text box is left alone: half-typed values would be refused
        if self.db.is_some() {
            return true;
        }
        let focus = self.focus.as_ref().filter(|f| f.text != f.original);
        let key = self
            .current
            .clone()
            .zip(focus)
            .map(|(p, f)| (p, f.block_id.clone(), f.cell.clone()));
        let Some(key) = key.filter(|_| !self.stepping) else {
            return self.flush();
        };
        let page = key.0.clone();
        // the same page, block and cell, and nothing else since
        let kept = self
            .poll_cp
            .take()
            .filter(|(k, _, step)| *k == key && self.undo.last() == Some(step));
        let old = kept.as_ref().and_then(|_| self.undo.pop());
        let cp = match kept {
            Some((_, cp, _)) => cp,
            None => match sync::checkpoint(&self.store, &[&page]) {
                Ok(cp) => cp,
                Err(e) => {
                    self.last_error = Some(format!("undo: {e}"));
                    return self.flush();
                }
            },
        };
        let before = self.undo.len();
        let ok = self.flush();
        let pushed = self.undo.len() == before + 1;
        match (cp.changes(&self.store), pushed) {
            (Ok(Some(c)), true) => {
                let step = (page, c);
                *self.undo.last_mut().expect("just pushed") = step.clone();
                self.poll_cp = Some((key, cp, step));
            }
            (_, pushed) => {
                if let Some(old) = old {
                    // the old step stands, below the new one if there is one
                    let new = pushed.then(|| self.undo.pop()).flatten();
                    self.poll_cp = (new.is_none()).then(|| (key, cp, old.clone()));
                    self.undo.push(old);
                    self.undo.extend(new);
                }
            }
        }
        ok
    }

    /// Drops ended highlights.
    pub(crate) fn expire_flash(&mut self, now: Instant) {
        self.flash.retain(|_, until| *until > now);
    }

    /// How long until the frame loop must run again: the next poll or the end
    /// of a highlight. A poll that is due but blocked is retried every second.
    fn next_wake(&self, now: Instant) -> Duration {
        let polls = if self.worker_gone || self.offline() {
            POLL_OPEN
        } else {
            let due = |(id, every): (String, Duration)| {
                (self.polled.get(&id).map_or(now, |t| *t + every)).saturating_duration_since(now)
            };
            let comments = self
                .comments
                .as_ref()
                .filter(|_| self.was_focused)
                .map(|p| {
                    p.polled.map_or(POLL_COMMENTS, |t| {
                        (t + POLL_COMMENTS).saturating_duration_since(now)
                    })
                });
            self.poll_pages()
                .into_iter()
                .map(due)
                .chain(comments)
                .min()
                .unwrap_or(POLL_BG)
                .max(Duration::from_secs(1))
        };
        let ends = self
            .flash
            .values()
            .map(|t| t.saturating_duration_since(now));
        ends.chain([polls]).min().unwrap_or(polls)
    }

    /// One frame (split from `App::ui` so tests can drive it without a window).
    pub(crate) fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.keylog {
            ctx.input(|i| keys::log(&i.raw.events));
        }
        self.drain_events();
        self.poll_oauth(&ctx);
        self.poll_browser_login(&ctx);
        self.poll_space_refresh(&ctx);
        self.media_pump();
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            self.window = (r.width(), r.height());
        }
        let shortcuts = self.shortcuts(&ctx);
        self.selection_keys(&ctx);
        self.db_keys(&ctx);
        self.switcher_keys(&ctx);
        if let Some(d) = self.dlg_keys(&ctx) {
            self.dlg_done(d);
        }
        self.inbox_keys(&ctx);
        let focused = ctx.input(|i| i.focused);
        let gained = focused && !self.was_focused;
        self.was_focused = focused;
        if gained {
            self.mark_open_page_read();
        }
        let now = Instant::now();
        self.poll_at(now, gained);
        self.poll_comments_at(now);
        self.users_tick(now);
        self.expire_flash(now);
        ctx.request_repaint_after(self.next_wake(now));
        let title = self.window_title();
        if title != self.shown_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }

        let c = theme::colors(ui);
        let mut acts: Vec<Action> = Vec::new();
        let labels = self.tab_labels();
        let workspace = self.workspace_name().to_string();
        let browser_ok = self
            .profiles
            .browser
            .as_ref()
            .is_some_and(|b| !b.token_v2.trim().is_empty());
        let names: Vec<String> = self
            .profiles
            .profiles
            .iter()
            .filter(|p| match p.kind {
                crate::profiles::AuthKind::Browser => browser_ok,
                crate::profiles::AuthKind::Official => p.has_token(),
            })
            .map(|p| p.name.clone())
            .collect();
        let no_token = !self.profiles.active_has_token();
        let (can_back, can_forward) = self
            .hists
            .get(self.active)
            .map_or((false, false), |h| (h.can_back(), h.can_forward()));
        let header = crate::view::sidebar::Header {
            workspace: &workspace,
            names: &names,
            no_token,
            can_back,
            can_forward,
        };
        // The sidebar runs the full height of the window, like Notion's, and
        // the tab strip only covers the content area next to it.
        if self.sidebar {
            let side = Frame::new().fill(c.sidebar);
            acts.extend(
                egui::Panel::left("tree")
                    .frame(side)
                    .size_range(200.0..=420.0)
                    .default_size(260.0)
                    .show(ui, |ui| {
                        let head = Frame::new()
                            .inner_margin(Margin::symmetric(6, 6))
                            .show(ui, |ui| crate::view::sidebar::header(ui, c, &header))
                            .inner;
                        let body = Frame::new()
                            .inner_margin(Margin::symmetric(6, 0))
                            .show(ui, |ui| {
                                let row = crate::view::sidebar::workspace_row(ui, c, &header);
                                row.or_else(|| {
                                    crate::view::sidebar::show(
                                        ui,
                                        c,
                                        &self.tree,
                                        &self.tree_open,
                                        self.current.as_deref(),
                                        self.unread,
                                        self.no_person,
                                    )
                                })
                            })
                            .inner;
                        head.or(body)
                    })
                    .inner,
            );
        }
        let bar = Frame::new()
            .fill(c.sidebar)
            .inner_margin(Margin::symmetric(6, 6));
        acts.extend(
            egui::Panel::top("tabs")
                .frame(bar)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut act = None;
                        if !self.sidebar {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if crate::icons::button(
                                ui,
                                28.0,
                                crate::icons::Icon::Sidebar,
                                c.secondary,
                                c.hover,
                                Some("Show sidebar"),
                            )
                            .clicked()
                            {
                                act = Some(Action::ToggleSidebar);
                            }
                            let back_c = if can_back {
                                c.secondary
                            } else {
                                c.secondary.gamma_multiply(0.35)
                            };
                            if crate::icons::button(
                                ui,
                                24.0,
                                crate::icons::Icon::Back,
                                back_c,
                                c.hover,
                                Some("Back"),
                            )
                            .clicked()
                                && can_back
                            {
                                act = Some(Action::Back);
                            }
                            let fwd_c = if can_forward {
                                c.secondary
                            } else {
                                c.secondary.gamma_multiply(0.35)
                            };
                            if crate::icons::button(
                                ui,
                                24.0,
                                crate::icons::Icon::Forward,
                                fwd_c,
                                c.hover,
                                Some("Forward"),
                            )
                            .clicked()
                                && can_forward
                            {
                                act = Some(Action::Forward);
                            }
                            ui.add_space(6.0);
                        }
                        if act.is_none() {
                            act = view::tabbar::show(ui, c, &labels, self.active);
                        }
                        act
                    })
                    .inner
                })
                .inner,
        );
        let status = view::status::Status {
            counts: self.counts,
            busy: self.busy_label(),
            error: self.last_error.as_deref().or(self.users_note.as_deref()),
            info: self.info.as_deref(),
            problems_open: self.problems_open,
            has_problems: !self.problems.is_empty(),
        };
        // Notion has no permanent status strip: only show one when there is
        // something to say (queued work, a sync, an error, a hint).
        let notable = status.counts != [0, 0, 0]
            || status.busy.is_some()
            || status.error.is_some()
            || status.info.is_some()
            || status.has_problems;
        if notable {
            let bar = Frame::new()
                .fill(c.bg)
                .inner_margin(Margin::symmetric(12, 4));
            acts.extend(
                egui::Panel::bottom("status")
                    .frame(bar)
                    .show(ui, |ui| view::status::show(ui, c, &status))
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
        if self.current.is_some() && self.find.is_some() {
            let bar = Frame::new()
                .fill(c.bg)
                .inner_margin(Margin::symmetric(12, 4));
            let out = egui::Panel::top("find")
                .frame(bar)
                .show(ui, |ui| {
                    self.find.as_mut().and_then(|f| view::find_bar(ui, c, f))
                })
                .inner;
            if let Some(o) = out {
                self.find_out(o);
            }
        }
        if self.current.is_some() && self.heads.len() >= 2 && ctx.content_rect().width() >= 1000.0 {
            acts.extend(
                egui::Panel::right("outline")
                    .frame(Frame::new().fill(c.bg))
                    .exact_size(28.0)
                    .resizable(false)
                    .show_separator_line(false)
                    .show(ui, |ui| view::outline(ui, c, &self.heads))
                    .inner,
            );
        }
        let page = egui::CentralPanel::default()
            .show(ui, |ui| {
                if self.db.is_some() {
                    self.db_ui(ui)
                } else if self.current.is_some() {
                    self.page_ui(ui)
                } else {
                    view::empty_tab(ui, c).into_iter().collect()
                }
            })
            .inner;
        // a Blur from the edited row must not be lost to another row's action
        // (click on row B while A loses focus): Blur first, then the rest
        let mut page = page;
        page.extend(acts);
        let mut acts = page;
        if self.problems_open {
            let sel = self.problem_sel.min(self.problems.len().saturating_sub(1));
            acts.extend(view::status::problems(&ctx, c, &self.problems, sel));
        }
        acts.extend(self.conflict_ui(&ctx, c));
        if let Some(mut sw) = self.switcher.take() {
            let items = sw.items(&self.tree, &self.recent);
            // read when the selection changes, never per frame
            let want = items
                .get(sw.sel.min(items.len().saturating_sub(1)))
                .map(|i| i.id.as_str());
            if want != sw.preview.as_ref().map(|p| p.0.as_str()) {
                sw.preview = want.map(|id| (id.to_string(), self.preview_lines(id)));
            }
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
        if let Some(d) = self.dlg_ui(&ctx, c) {
            self.dlg_done(d);
        }
        self.inbox_ui(&ctx, c);
        if let Some(mut form) = self.ws_form.take() {
            if let Some(a) = view::workspace::form(&ctx, c, &mut form) {
                acts.push(a);
            }
            if self.ws_form.is_none() {
                self.ws_form = Some(form);
            }
        }
        if let Some(mut app) = self.oauth_setup.take() {
            let mut err = self.oauth_error.take();
            if let Some(a) = view::oauth::setup(&ctx, c, &mut app, &mut err) {
                acts.push(a);
            }
            self.oauth_setup = Some(app);
            self.oauth_error = err;
        }
        if let Some(mut flow) = self.oauth.take() {
            if let Some(a) = view::oauth::waiting(&ctx, c, &mut flow) {
                acts.push(a);
            }
            self.oauth = Some(flow);
        }
        if let Some(mut flow) = self.browser_login.take() {
            if let Some(a) = view::browser_login::dialog(&ctx, c, &mut flow) {
                acts.push(a);
            }
            self.browser_login = Some(flow);
        }
        blur_first(&mut acts);
        for a in acts {
            self.apply(a);
        }
        if !shortcuts.is_empty() {
            for s in shortcuts {
                self.run_shortcut(s, &ctx);
            }
            ctx.request_repaint(); // the panels above were drawn before it
        }
        self.apply_pending_switch(&ctx);
    }
}

/// Rows that are new or whose text or type changed (a list number shifting
/// is not a change).
fn changed_ids(old: &[Row], new: &[Row]) -> Vec<String> {
    let old: HashMap<&str, &Row> = old.iter().map(|r| (r.block_id.as_str(), r)).collect();
    let same =
        |a: &Marker, b: &Marker| a == b || matches!((a, b), (Marker::Number(_), Marker::Number(_)));
    new.iter()
        .filter(|n| {
            old.get(n.block_id.as_str())
                .is_none_or(|o| o.rich_text != n.rich_text || !same(&o.marker, &n.marker))
        })
        .map(|r| r.block_id.clone())
        .collect()
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
    use crate::switcher::Switcher;
    use crate::tabs::Tab;
    use crate::testkit::{add_page, key, page_in, Win};
    use eframe::egui::Event as Ev;
    use notion_core::model::{plain_text, Block, BlockKind, TargetKind};
    use notion_core::store::Store;
    use notion_core::sync::PullReport;

    fn skipped(id: &str) -> Event {
        Event::Pulled {
            id: id.into(),
            report: PullReport {
                skipped: true,
                ..Default::default()
            },
        }
    }

    fn changed(id: &str) -> Event {
        Event::Pulled {
            id: id.into(),
            report: PullReport {
                fetched: 1,
                updated: 1,
                skipped: false,
            },
        }
    }

    fn poll(id: &str, skip: Skip) -> Cmd {
        Cmd::Poll {
            id: id.into(),
            skip,
        }
    }

    /// A window with page "p" open and a background tab on each of `bg`.
    fn with_tabs(bg: &[&str]) -> Win {
        let mut w = Win::with_texts(&["a", "b"]);
        for id in bg {
            w.app.tabs.push(Tab {
                page: Some(id.to_string()),
                ..Default::default()
            });
        }
        w
    }

    /// Polls once a second for 5 minutes, answering every poll "unchanged":
    /// the open page every 15 s, each other tab every 60 s. That is
    /// 4 + 4 requests a minute for five tabs, nowhere near Notion's 180
    /// (the client's limiter caps the bursts, see core's `limiter_caps_a_minute_for_any_number_of_callers`).
    #[test]
    fn polls_the_open_page_every_15s_and_the_other_tabs_every_60s() {
        let mut w = with_tabs(&["q1", "q2", "q3", "q4"]);
        w.app.was_focused = false;
        let t0 = w.app.polled["p"];
        let mut polls: Vec<(u64, String)> = Vec::new();
        for t in 0..=300 {
            // five times a second: every page can be due at once
            for _ in 0..5 {
                w.app.poll_at(t0 + Duration::from_secs(t), false);
                for c in w.cmds() {
                    let Cmd::Poll { id, .. } = c else {
                        panic!("{c:?}")
                    };
                    polls.push((t, id.clone()));
                    w.app.on_event(skipped(&id));
                }
            }
        }
        let of = |id: &str| {
            polls
                .iter()
                .filter(|p| p.1 == id)
                .map(|p| p.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(of("p"), (1..=20).map(|n| n * 15).collect::<Vec<_>>());
        for q in ["q1", "q2", "q3", "q4"] {
            assert_eq!(of(q), [60, 120, 180, 240, 300], "{q}");
        }
        assert_eq!(polls.len(), 40, "8 a minute");
    }

    /// A page edited in the last two minutes is pulled in full by a poll, but
    /// not twice within a minute: the second poll may skip on an unchanged time.
    #[test]
    fn a_page_is_pulled_in_full_at_most_once_a_minute_by_polls() {
        let mut w = Win::with_texts(&["a"]);
        w.app.was_focused = false;
        let t0 = w.app.polled["p"];
        let at = |w: &mut Win, s: u64| {
            w.app.poll_at(t0 + Duration::from_secs(s), false);
            w.cmds()
        };
        assert_eq!(at(&mut w, 15), [poll("p", sync::poll_settle())]);
        w.app.on_event(changed("p"));
        for s in [30, 45] {
            assert_eq!(at(&mut w, s), [poll("p", Skip::IfUnchanged)], "{s}");
            w.app.on_event(skipped("p"));
        }
        assert_eq!(
            at(&mut w, 75),
            [poll("p", sync::poll_settle())],
            "a minute on"
        );
    }

    /// With focus the open page is polled every 3 s, without it every 15 s.
    #[test]
    fn a_focused_window_polls_its_page_every_3s() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = w.app.polled["p"];
        let mut at = Vec::new();
        for t in 0..=30 {
            w.app.was_focused = t < 15;
            w.app.poll_at(t0 + Duration::from_secs(t), false);
            if !w.cmds().is_empty() {
                at.push(t);
                w.app.on_event(skipped("p"));
            }
        }
        assert_eq!(at, [3, 6, 9, 12, 27], "live, then 15 s after the last poll");
    }

    /// A live page edited lately is pulled in full every 3× as long as its
    /// last full pull took, between 5 s and a minute; unchanged polls between.
    #[test]
    fn live_full_pulls_are_spaced_by_their_cost() {
        for (took, gap) in [(0u64, 5u64), (4, 12), (30, 60)] {
            let mut w = Win::with_texts(&["a"]);
            let t0 = w.app.polled["p"];
            w.app.poll_at(t0 + POLL_LIVE, false);
            assert_eq!(w.cmds(), [poll("p", sync::poll_settle())]);
            w.app.on_event(changed("p"));
            assert!(
                w.app.pull_took.contains_key("p"),
                "a poll's full pull is timed"
            );
            w.app
                .pull_took
                .insert("p".into(), Duration::from_secs(took));
            let full = w.app.full_at["p"];
            w.app.polled.insert("p".into(), full);
            let mut fulls = Vec::new();
            for s in 1..=60 {
                w.app.poll_at(full + Duration::from_secs(s), false);
                for c in w.cmds() {
                    if c == poll("p", sync::poll_settle()) {
                        fulls.push(s);
                    }
                    w.app.on_event(skipped("p"));
                }
            }
            // the first poll (every 3 s) at or after the gap
            assert_eq!(fulls.first(), Some(&(gap.div_ceil(3) * 3)), "took {took}s");
        }
    }

    /// The comment panel's threads (page and row) every 10 s while focused.
    #[test]
    fn the_comment_panel_is_polled_while_focused() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = Instant::now();
        w.app.poll_comments_at(t0 + POLL_COMMENTS);
        assert!(w.cmds().is_empty(), "no panel");
        w.app.open_comments(Some("p-0".into()));
        w.cmds();
        w.app.sent.clear();
        // pages aren't due: only comments
        w.app.polled.insert("p".into(), t0 + POLL_COMMENTS * 10);
        w.app.poll_comments_at(t0);
        assert!(w.cmds().is_empty(), "opening fetched them");
        w.app.poll_comments_at(t0 + POLL_COMMENTS);
        let c = w.cmds();
        assert_eq!(
            c,
            [
                Cmd::PullComments { id: "p".into() },
                Cmd::PullComments { id: "p-0".into() }
            ]
        );
        w.app.poll_comments_at(t0 + POLL_COMMENTS * 2);
        assert!(w.cmds().is_empty(), "waits for the answers");
        w.app.on_event(Event::CommentsPulled);
        w.app.on_event(Event::CommentsPulled);
        w.app.was_focused = false;
        w.app.poll_comments_at(t0 + POLL_COMMENTS * 3);
        assert!(w.cmds().is_empty(), "not without focus");
        w.app.was_focused = true;
        w.app.poll_comments_at(t0 + POLL_COMMENTS * 3);
        assert_eq!(w.cmds().len(), 2);
    }

    #[test]
    fn polls_wait_for_everything_else_and_never_double_up() {
        let mut w = with_tabs(&["q"]);
        let t0 = w.app.polled["p"];
        let due = t0 + POLL_BG * 10;
        // a command in flight, the switcher, the title editor, offline, no worker
        w.app.sent.push_back(Cmd::Search(String::new()));
        w.app.poll_at(due, false);
        w.app.sent.clear();
        w.app.switcher = Some(Switcher::default());
        w.app.poll_at(due, false);
        w.app.switcher = None;
        w.app.last_error = Some("offline: NOTION_TOKEN not set".into());
        w.app.poll_at(due, false);
        w.app.last_error = None;
        w.app.worker_gone = true;
        w.app.poll_at(due, false);
        w.app.worker_gone = false;
        assert!(w.cmds().is_empty());
        // free again: one poll, and while it is out no second one
        w.app.poll_at(due, false);
        w.app.poll_at(due + POLL_BG, false);
        w.app.poll_at(due + POLL_BG * 2, true);
        assert_eq!(w.cmds(), [poll("p", sync::poll_settle())]);
        assert_eq!(w.app.sent.len(), 1);
    }

    #[test]
    fn a_poll_runs_while_editing_and_leaves_buffer_and_cursor_alone() {
        let mut w = Win::with_texts(&["a", "b"]);
        let t0 = w.app.polled["p"];
        w.edit(1);
        w.type_text("!");
        w.select(1, 1);
        let id = crate::app::edit_id("p-1");
        let cursor = |w: &Win| {
            egui::text_edit::TextEditState::load(&w.ctx, id)
                .unwrap()
                .cursor
                .char_range()
        };
        let before = cursor(&w);
        w.app.poll_at(t0 + POLL_OPEN, false);
        assert_eq!(w.cmds(), [poll("p", sync::poll_settle())]);
        // the edit was written first, so the pull keeps it (queued edits survive)
        assert_eq!(w.queued(), 1);
        // the remote changed the other block and the edited one
        let kid = |id: &str, t: &str| {
            crate::outline::tests::blk(
                id,
                "p",
                "paragraph",
                serde_json::json!({ "rich_text": crate::outline::tests::rt(t) }),
            )
        };
        w.app.store.put_block(&kid("p-0", "remote")).unwrap();
        w.answer(changed("p"));
        assert_eq!(w.row_text("p-0"), "remote");
        assert_eq!(w.text(), "b!");
        assert_eq!(w.app.focus.as_ref().unwrap().idx, 1);
        assert_eq!(cursor(&w), before);
        // still typing into the same buffer, at the same cursor
        w.type_text("?");
        assert_eq!(w.text(), "b?!");
        // a menu open: the poll waits
        w.app.polled.insert("p".into(), t0);
        w.type_text(" /");
        w.app.poll_at(t0 + POLL_OPEN * 4, false);
        assert!(w.menu().is_some() && w.cmds().is_empty());
    }

    #[test]
    fn a_poll_highlights_only_changed_blocks_and_only_for_a_while() {
        let mut w = Win::with_texts(&["a", "b", "c"]);
        let t0 = w.app.polled["p"];
        let put = |w: &Win, id: &str, t: &str| {
            let b = crate::outline::tests::blk(
                id,
                "p",
                "paragraph",
                serde_json::json!({ "rich_text": crate::outline::tests::rt(t) }),
            );
            w.app.store.put_block(&b).unwrap();
        };
        // a pull the user started highlights nothing
        w.app.pull(false);
        put(&w, "p-0", "pulled");
        w.answer(changed("p"));
        assert!(w.app.flash.is_empty());
        // a poll that skipped changes nothing
        w.app.poll_at(t0 + POLL_OPEN, false);
        w.answer(skipped("p"));
        assert!(w.app.flash.is_empty());
        // a poll that pulled: p-1 changed, p-2 did not
        w.app.poll_at(t0 + POLL_OPEN * 2, false);
        put(&w, "p-1", "B2");
        w.answer(changed("p"));
        assert_eq!(w.app.flash.keys().collect::<Vec<_>>(), ["p-1"]);
        assert!(
            w.app.next_wake(Instant::now()) <= FLASH,
            "wakes for the end"
        );
        let end = Instant::now() + FLASH;
        w.app.expire_flash(end - Duration::from_millis(100));
        assert_eq!(w.app.flash.len(), 1);
        w.app.expire_flash(end + Duration::from_millis(100));
        assert!(w.app.flash.is_empty());
        w.frames(vec![]); // drawn with a tint without trouble
    }

    /// Puts block `id` of page "p" in the store with text `t`.
    fn put(w: &Win, id: &str, t: &str) {
        let b = crate::outline::tests::blk(
            id,
            "p",
            "paragraph",
            serde_json::json!({ "rich_text": crate::outline::tests::rt(t) }),
        );
        w.app.store.put_block(&b).unwrap();
    }

    /// Polls "p" at `t0 + s` and answers with `ev`.
    fn poll_answered(w: &mut Win, t0: Instant, s: u64, ev: Event) {
        w.app.poll_at(t0 + Duration::from_secs(s), false);
        assert_eq!(w.cmds().len(), 1);
        w.answer(ev);
    }

    #[test]
    fn polls_clear_undo_only_for_a_page_that_really_changed() {
        let mut w = with_tabs(&["q"]);
        let t0 = w.app.polled["p"];
        w.edit(0);
        w.type_text("x");
        w.app.commit();
        assert_eq!(w.app.undo.len(), 1);
        // a full poll that finds nothing new keeps the steps
        let same = PullReport {
            fetched: 2,
            updated: 0,
            skipped: false,
        };
        poll_answered(
            &mut w,
            t0,
            15,
            Event::Pulled {
                id: "p".into(),
                report: same,
            },
        );
        assert_eq!(w.app.undo.len(), 1);
        // another tab changed: not this page's steps
        w.app.sent.push_back(poll("q", Skip::Never));
        w.app.on_event(changed("q"));
        assert_eq!(w.app.undo.len(), 1);
        // this page changed: its steps go
        put(&w, "p-1", "B2");
        poll_answered(&mut w, t0, 30, changed("p"));
        assert!(w.app.undo.is_empty() && w.app.redo.is_empty());
    }

    #[test]
    fn polls_while_typing_make_one_undo_step_until_a_commit_ends_it() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = w.app.polled["p"];
        w.edit(0);
        w.type_text("1");
        poll_answered(&mut w, t0, 15, skipped("p"));
        assert_eq!(w.app.undo.len(), 1);
        w.type_text("2");
        poll_answered(&mut w, t0, 30, skipped("p"));
        assert_eq!(
            w.app.undo.len(),
            1,
            "same step (each flush still queues its own op)"
        );
        // nothing typed: the step stands
        poll_answered(&mut w, t0, 45, skipped("p"));
        assert_eq!(w.app.undo.len(), 1);
        // typing after the last poll, then an explicit commit: a step of its own
        w.type_text("3");
        w.app.commit();
        assert_eq!(w.app.undo.len(), 2);
        // and a later poll flush does not join that one
        w.edit(0);
        w.type_text("4");
        poll_answered(&mut w, t0, 60, skipped("p"));
        assert_eq!(w.app.undo.len(), 3);
        w.app.undo_redo(true);
        w.app.undo_redo(true);
        w.app.undo_redo(true);
        assert_eq!(
            w.row_text("p-0"),
            "a",
            "the typing session of 1 and 2 is one step"
        );
    }

    #[test]
    fn a_clean_editor_on_a_remotely_changed_block_shows_the_new_text() {
        let mut w = Win::with_texts(&["a", "b"]);
        let t0 = w.app.polled["p"];
        w.edit(1);
        put(&w, "p-1", "remote");
        poll_answered(&mut w, t0, 15, changed("p"));
        assert_eq!(
            (w.text(), w.app.focus.as_ref().unwrap().original.as_str()),
            ("remote", "remote")
        );
        assert_eq!(w.queued(), 0, "nothing written");
        // typing continues from the new text, and its base is the new version
        w.type_text("!");
        assert!(w.text().contains('!') && w.text().len() == 7);
        // a block typed in is not re-seeded: its edit was written, the cache kept it
        let mut w = Win::with_texts(&["a", "b"]);
        let t0 = w.app.polled["p"];
        w.edit(1);
        w.type_text("!");
        put(&w, "p-1", "remote");
        poll_answered(&mut w, t0, 15, changed("p"));
        assert_eq!(w.text(), "b!");
    }

    #[test]
    fn a_poll_flush_never_merges_into_another_page_or_block() {
        let mut w = with_tabs(&["q"]);
        add_page(&w.app.store, "q", None, &["x"]);
        let t0 = w.app.polled["p"];
        w.edit(0);
        w.type_text("1");
        poll_answered(&mut w, t0, 15, skipped("p"));
        assert_eq!(w.app.undo.len(), 1);
        // another block of the same page: a step of its own
        w.edit(1);
        w.type_text("2");
        poll_answered(&mut w, t0, 30, skipped("p"));
        assert_eq!(w.app.undo.len(), 2);
        // another page: A's steps stay, B's is filed under B
        w.app.commit();
        w.app.switch_tab(1);
        assert_eq!(w.app.current.as_deref(), Some("q"));
        w.app.polled.insert("q".into(), t0);
        w.edit(0);
        w.type_text("3");
        w.app.poll_at(t0 + Duration::from_secs(100), false);
        assert_eq!(w.cmds(), [poll("q", sync::poll_settle())]);
        w.answer(skipped("q"));
        let pages: Vec<&str> = w.app.undo.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(pages, ["p", "p", "q"]);
    }

    #[test]
    fn typing_across_live_polls_then_ctrl_s_is_one_op_and_one_undo_step() {
        let mut w = Win::with_texts(&["a"]);
        w.app.was_focused = true;
        let t0 = w.app.polled["p"];
        w.edit(0);
        for (i, c) in ["1", "2", "3"].into_iter().enumerate() {
            w.type_text(c);
            poll_answered(&mut w, t0, 3 * (i as u64 + 1), skipped("p"));
        }
        w.frame_with(
            vec![crate::testkit::key_mod(Key::S, Modifiers::CTRL)],
            Modifiers::CTRL,
        );
        w.frames(vec![]);
        assert_eq!(w.queued(), 1);
        assert_eq!(w.app.undo.len(), 1);
        let b = w.app.store.get_block("p-0").unwrap().unwrap();
        assert_eq!(plain_text(&b.kind.text().unwrap().rich_text), "a123");
        assert_eq!(w.cmds(), [Cmd::Push]);
        w.app.sent.clear(); // the push is not in flight
        w.app.undo_redo(true);
        assert_eq!(w.queued(), 0, "one undo reverts everything typed");
        assert!(w.app.undo.is_empty());
    }

    #[test]
    fn a_focused_window_without_a_comment_panel_pulls_no_comments() {
        let mut w = Win::with_texts(&["a"]);
        w.app.was_focused = true;
        assert!(w.app.comments.is_none());
        let t0 = Instant::now();
        for s in [0, 10, 20, 100] {
            w.app.poll_comments_at(t0 + Duration::from_secs(s));
        }
        assert!(w
            .cmds()
            .iter()
            .all(|c| !matches!(c, Cmd::PullComments { .. })));
    }

    #[test]
    fn a_typed_in_block_that_changed_remotely_warns_and_a_reseed_follows_the_kind() {
        let mut w = Win::with_texts(&["a", "b"]);
        let t0 = w.app.polled["p"];
        w.edit(1);
        w.app.poll_at(t0 + POLL_OPEN, false);
        w.cmds();
        w.type_text("!"); // after the poll was sent
        put(&w, "p-1", "remote");
        w.answer(changed("p"));
        assert_eq!(w.text(), "b!", "the buffer stays");
        assert!(w.app.info.as_deref().unwrap().contains("edited in Notion"));
        // a clean editor on a block that became code is a code editor
        let mut w = Win::with_texts(&["a"]);
        let t0 = w.app.polled["p"];
        w.edit(0);
        let code = crate::outline::tests::blk(
            "p-0",
            "p",
            "code",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("fn"), "language": "rust" }),
        );
        w.app.store.put_block(&code).unwrap();
        poll_answered(&mut w, t0, 15, changed("p"));
        let f = w.app.focus.as_ref().unwrap();
        assert_eq!((f.code, f.text.as_str()), (true, "fn"));
    }

    #[test]
    fn an_offline_poll_error_sets_the_message_other_errors_stay_quiet() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = w.app.polled["p"];
        poll_answered(&mut w, t0, 15, Event::Error("502 bad gateway".into()));
        assert_eq!(w.app.last_error, None);
        poll_answered(
            &mut w,
            t0,
            30,
            Event::Error("offline: NOTION_TOKEN not set".into()),
        );
        assert!(w.app.offline());
        w.app.poll_at(t0 + POLL_OPEN * 10, false);
        assert!(w.cmds().is_empty(), "backed off");
    }

    #[test]
    fn a_background_poll_changes_neither_the_page_nor_a_pending_open() {
        let mut w = with_tabs(&["q"]);
        let t0 = w.app.polled["p"];
        w.app.poll_at(t0, false); // stamps q
        w.app.poll_at(t0 + POLL_BG, false); // the open page first
        w.app.on_event(skipped("p"));
        w.app.pending_open = Some("remote".into());
        w.app.poll_at(t0 + POLL_BG, false);
        assert_eq!(
            w.cmds(),
            [
                poll("p", sync::poll_settle()),
                poll("q", sync::poll_settle())
            ]
        );
        w.app.on_event(changed("q"));
        assert_eq!(w.app.current.as_deref(), Some("p"));
        assert_eq!(w.app.pending_open.as_deref(), Some("remote"));
        assert!(w.app.flash.is_empty() && w.app.last_error.is_none());
    }

    #[test]
    fn focus_gain_polls_the_open_page_unless_it_was_just_polled() {
        let mut w = Win::with_texts(&["a"]);
        let t0 = w.app.polled["p"];
        w.app.poll_at(t0 + Duration::from_secs(3), true);
        assert!(w.cmds().is_empty(), "focus flapping");
        w.app.poll_at(t0 + Duration::from_secs(6), true);
        assert_eq!(w.cmds().len(), 1);
        // in a frame
        w.app.sent.clear();
        w.app
            .polled
            .insert("p".into(), Instant::now() - Duration::from_secs(6));
        w.app.was_focused = false; // the window was in the background
        w.frames(vec![]);
        assert_eq!(w.cmds().len(), 1, "focused again: one poll");
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
            (
                p.kind,
                p.label.as_str(),
                p.page.as_deref(),
                p.error.as_str()
            ),
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
        assert_eq!(
            w.app.focus.as_ref().map(|f| f.block_id.as_str()),
            Some("p-1")
        );
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
        w.app
            .store
            .mark_op(bad, OpStatus::Failed, Some("400"))
            .unwrap();
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
        assert!(
            (w.app.scroll - 120.0).abs() < 1.0,
            "scroll restored: {}",
            w.app.scroll
        );
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
        // the first tab sits right after the full-height sidebar
        let pos = egui::pos2(350.0, 18.0);
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
            store
                .upsert_page(&serde_json::from_value(v).unwrap())
                .unwrap();
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
        let ids: Vec<_> = sw
            .items(&w.app.tree, &w.app.recent)
            .iter()
            .map(|i| i.id.clone())
            .collect();
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
        assert_eq!(
            w.cmds(),
            [Cmd::PullUsers],
            "cached: no page pull; the empty user cache is asked once"
        );
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
        assert_eq!(
            w.app.tabs[0].page.as_deref(),
            Some("a"),
            "the old tab keeps its page"
        );
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
        assert!(
            !conf.join("session.json").exists(),
            "the iced app's file is left alone"
        );

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
        w.app.set_worker(std::thread::spawn(
            move || {
                while rx.blocking_recv().is_some() {}
            },
        ));
        w.app.shutdown();
        assert!(w.app.worker.is_none() && w.app.cmds.is_none());
        assert_eq!(w.queued(), 1, "the typed text was committed");
    }

    // ------------------------------------- conversions are written, not shown

    fn kind_of(w: &Win, id: &str) -> String {
        w.app
            .store
            .get_block(id)
            .unwrap()
            .unwrap()
            .kind
            .type_name()
            .to_string()
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
        assert_eq!(
            (w.app.last_error.clone(), w.app.focus.is_none()),
            (None, true)
        );
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
        assert_eq!(
            w.queued(),
            1,
            "the same block again: one op, the last state"
        );
    }

    // ------------------------------------------------------- same-frame keys

    #[test]
    fn typing_and_ctrl_s_in_one_frame_commit_the_typing() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.frame_with(
            vec![
                Ev::Text("x".into()),
                crate::testkit::key_mod(Key::S, Modifiers::CTRL),
            ],
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

    // ------------------------------------------------------------ users

    #[test]
    fn users_are_pulled_once_at_startup_then_at_most_hourly() {
        let mut w = Win::with_texts(&["a"]);
        w.app.users_at = None;
        w.app.users_tick(Instant::now());
        assert_eq!(
            w.cmds(),
            [Cmd::PullUsers],
            "the startup pull comes from the tick"
        );
        let asked = w.app.users_at.unwrap();
        w.answer(Event::UsersPulled);
        w.app
            .users_tick(asked + USERS_EVERY - Duration::from_secs(1));
        assert!(w.cmds().is_empty(), "not within the hour");
        w.app.sent.push_back(Cmd::Push);
        w.app.users_tick(asked + USERS_EVERY);
        assert!(w.cmds().is_empty(), "not while something else is in flight");
        w.app.sent.clear();
        w.app.users_tick(asked + USERS_EVERY);
        assert_eq!(w.cmds(), [Cmd::PullUsers]);
        w.app
            .users_tick(asked + USERS_EVERY + Duration::from_secs(5));
        assert!(w.cmds().is_empty(), "the hour restarts at each pull");
        // frames never ask on their own
        for _ in 0..5 {
            w.frames(vec![]);
        }
        assert!(w.cmds().is_empty());
    }

    #[test]
    fn the_startup_users_pull_never_queues_ahead_of_other_work() {
        let mut w = Win::with_texts(&["a"]);
        w.app.users_at = None;
        // the restore page pull is out: the users wait for it
        w.app.send(Cmd::Pull {
            id: "p".into(),
            force: false,
        });
        for _ in 0..3 {
            w.frames(vec![]);
        }
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: "p".into(),
                force: false
            }]
        );
        assert_eq!(w.app.users_at, None);
        // answered: the next frame asks, once
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        w.frames(vec![]);
        assert_eq!(w.cmds(), [Cmd::PullUsers]);
        assert!(w.app.users_at.is_some());
        w.frames(vec![]);
        assert!(w.cmds().is_empty());
    }

    #[test]
    fn sync_all_is_one_command_with_progress_and_refreshes_the_sidebar_when_done() {
        let mut w = Win::with_texts(&["a"]);
        assert_eq!(w.app.busy_label(), None);
        w.press(Key::Y, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(w.cmds(), [Cmd::SyncAll]);
        assert_eq!(w.app.busy_label().as_deref(), Some("syncing…"));
        w.app.apply(Action::SyncAll);
        assert!(w.cmds().is_empty(), "one at a time");
        // progress is not an answer
        w.answer(Event::Progress {
            done: 12,
            total: 164,
        });
        assert_eq!(w.app.sent.len(), 1);
        assert_eq!(w.app.busy_label().as_deref(), Some("syncing 12/164…"));
        // polls wait behind it, but pages stay readable
        w.app.apply(Action::Open("p".into()));
        assert_eq!(w.app.current.as_deref(), Some("p"));
        w.frames(vec![]);
        assert!(w.cmds().is_empty());
        // the worker cached a page and a database with a row
        crate::testkit::add_page(&w.app.store, "q", None, &[]);
        let ds: notion_core::model::DataSource = serde_json::from_value(serde_json::json!({
            "object": "data_source", "id": "ds", "title": [], "parent": { "type": "database_id", "database_id": "db" },
            "last_edited_time": "2025-01-01T00:00:00.000Z", "properties": {},
        }))
        .unwrap();
        w.app.store.upsert_data_source(&ds).unwrap();
        let row = crate::testkit::page(
            "r",
            serde_json::json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" }),
        );
        w.app.store.upsert_page(&row).unwrap();
        assert!(w.app.tree.iter().all(|e| e.id != "q"));
        w.answer(Event::SyncedAll(Default::default()));
        assert!(w.app.sent.is_empty() && w.app.progress.is_none());
        assert_eq!(w.app.busy_label(), None);
        assert!(w.app.info.as_deref().unwrap().starts_with("synced"));
        let at = |id: &str| w.app.tree.iter().find(|e| e.id == id).cloned();
        assert!(at("q").is_some());
        assert!(at("ds").unwrap().database);
        assert_eq!(at("r").unwrap().parent.as_deref(), Some("ds"));
        // a failed run clears the progress too, and says why
        w.press(Key::Y, Modifiers::CTRL | Modifiers::SHIFT);
        w.answer(Event::Progress { done: 1, total: 5 });
        w.answer(Event::Error("notion 401 unauthorized: bad".into()));
        assert!(w.app.sent.is_empty() && w.app.progress.is_none());
        assert!(w.app.last_error.as_deref().unwrap().contains("401"));
        // a database opens as a table, and is pulled
        w.app.apply(Action::Open("ds".into()));
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert!(w.app.db.is_some());
        assert!(w.cmds().contains(&Cmd::Pull {
            id: "ds".into(),
            force: false
        }));
    }

    #[test]
    fn a_403_on_users_is_said_once_and_never_repeated() {
        let mut w = Win::with_texts(&["a"]);
        w.app.pending_open = Some("remote".into());
        w.app.request_users();
        w.cmds();
        w.answer(Event::UsersDenied);
        let msg = w.app.users_note.clone().expect("a message");
        assert!(
            msg.contains("403") && msg.contains("Read user information"),
            "{msg}"
        );
        assert_eq!(
            w.app.pending_open.as_deref(),
            Some("remote"),
            "unrelated to the page being opened"
        );
        // neither the hourly tick, nor the @ menu, nor the switcher, nor a frame asks again
        w.app.users_tick(Instant::now() + USERS_EVERY * 5);
        w.app.ensure_users();
        w.press(Key::K, Modifiers::CTRL);
        for _ in 0..5 {
            w.frames(vec![]);
        }
        assert!(w.cmds().is_empty(), "no repeat");
        assert_eq!(w.app.last_error, None, "and no second message");
        // a later pull's event doesn't wipe it, and the status bar still shows it
        w.app.sent.push_back(Cmd::Pull {
            id: "p".into(),
            force: false,
        });
        w.answer(Event::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        assert!(w.app.users_note.is_some());
        w.frames(vec![]);
        assert!(w.shown.contains("Read user information"), "{}", w.shown);
    }

    #[test]
    fn other_user_pull_errors_are_shown_and_retried_hourly_but_no_token_is_silent() {
        let mut w = Win::with_texts(&["a"]);
        w.app.pending_open = Some("remote".into());
        w.app.request_users();
        w.answer(Event::Error("http: timeout".into()));
        assert_eq!(
            w.app.last_error.as_deref(),
            Some("users not loaded: http: timeout")
        );
        assert!(!w.app.users_stop);
        assert_eq!(w.app.pending_open.as_deref(), Some("remote"));
        w.cmds();
        w.app.users_tick(w.app.users_at.unwrap() + USERS_EVERY);
        assert_eq!(w.cmds(), [Cmd::PullUsers]);

        let mut w = Win::with_texts(&["a"]);
        w.app.request_users();
        w.answer(Event::Error("offline: NOTION_TOKEN not set".into()));
        assert_eq!(w.app.last_error, None, "no token: no banner at startup");
        assert!(w.app.users_stop);
    }

    #[test]
    fn pulled_users_and_inline_ones_reach_the_app() {
        let mut w = Win::with_texts(&["a"]);
        assert!(w.app.users.is_empty());
        let u = |id: &str, name: &str| {
            notion_core::model::User::from_value(
                &serde_json::json!({ "object": "user", "id": id, "type": "person", "name": name }),
            )
            .unwrap()
        };
        w.app.store.put_users(&[u("u1", "Ada")]).unwrap();
        w.app.sent.push_back(Cmd::PullUsers);
        w.answer(Event::UsersPulled);
        assert_eq!(w.app.users.len(), 1);
        // a page pull brings inline users along
        w.app.store.put_users(&[u("u2", "Bo")]).unwrap();
        w.app.sent.push_back(Cmd::PullComments { id: "p".into() });
        w.answer(Event::CommentsPulled);
        assert_eq!(w.app.users.len(), 2);
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
        w.app.sent.push_back(Cmd::Pull {
            id: "remote".into(),
            force: false,
        });
        w.answer(Event::Error("404".into()));
        assert_eq!(w.app.pending_open, None);
    }

    // -------------------------------------------------------- navigation

    fn nav_win() -> Win {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "a", None, &["x"]);
        add_page(&store, "b", Some("a"), &["x"]);
        add_page(&store, "c", None, &["x"]);
        add_page(&store, "d", None, &["x"]);
        add_page(&store, "e", Some("ghost"), &["x"]);
        Win::new(store)
    }

    fn open(w: &mut Win, id: &str) {
        w.app.apply(Action::Open(id.into()));
        w.frames(vec![]);
    }

    fn cur(w: &Win) -> &str {
        w.app.current.as_deref().unwrap_or("-")
    }

    #[test]
    fn back_and_forward_by_every_key_and_the_mouse() {
        let mut w = nav_win();
        for id in ["a", "b", "c"] {
            open(&mut w, id);
        }
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "b");
        w.press(Key::ArrowLeft, Modifiers::ALT);
        assert_eq!(cur(&w), "a");
        w.press(Key::ArrowLeft, Modifiers::ALT);
        assert_eq!(cur(&w), "a", "first entry: nothing");
        w.press(Key::ArrowRight, Modifiers::ALT);
        assert_eq!(cur(&w), "b");
        w.press(Key::CloseBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "c");
        w.press(Key::CloseBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "c", "last entry: nothing");
        let click = |button| Ev::PointerButton {
            pos: egui::Pos2::ZERO,
            button,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        w.send(click(egui::PointerButton::Extra1));
        assert_eq!(cur(&w), "b");
        w.send(click(egui::PointerButton::Extra2));
        assert_eq!(cur(&w), "c");
        // going back is no step: a new page drops the forward branch
        w.press(Key::OpenBracket, Modifiers::CTRL);
        open(&mut w, "d");
        w.press(Key::CloseBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "d");
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "b");
    }

    #[test]
    fn the_switcher_and_sidebar_push_history_and_each_tab_has_its_own() {
        let mut w = nav_win();
        open(&mut w, "a");
        w.app.pick("b", false);
        w.frames(vec![]);
        // a new tab: its history starts empty
        w.press(Key::T, Modifiers::CTRL);
        w.press(Key::Escape, Modifiers::NONE);
        open(&mut w, "c");
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "c", "tab 2 has nothing behind c");
        w.press(Key::Tab, Modifiers::CTRL);
        assert_eq!(cur(&w), "b");
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "a", "tab 1 went back");
        // closing tab 1 takes its history along; tab 2 is clean
        w.press(Key::W, Modifiers::CTRL);
        assert_eq!(cur(&w), "c");
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "c");
    }

    #[test]
    fn a_back_that_never_arrives_leaves_the_history_alone() {
        let mut w = nav_win();
        w.app.hist().go(None, "remote");
        open(&mut w, "c");
        assert_eq!(w.app.hist().pages(), ["remote", "c"]);
        w.cmds();
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(w.app.pending_open.as_deref(), Some("remote"));
        assert!(w.cmds().contains(&Cmd::Pull {
            id: "remote".into(),
            force: false
        }));
        w.answer(Event::Error("404".into()));
        assert_eq!((w.app.pending_open.clone(), cur(&w)), (None, "c"));
        open(&mut w, "d");
        assert_eq!(w.app.hist().pages(), ["remote", "c", "d"]);
    }

    #[test]
    fn ctrl_alt_bracket_is_not_back() {
        let mut w = nav_win();
        open(&mut w, "a");
        open(&mut w, "b");
        w.press(Key::OpenBracket, Modifiers::CTRL | Modifiers::ALT);
        assert_eq!(cur(&w), "b");
    }

    #[test]
    fn ctrl_shift_u_opens_the_cached_parent() {
        let mut w = nav_win();
        open(&mut w, "b");
        w.press(Key::U, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(cur(&w), "a");
        // the parent is in history, too
        w.press(Key::OpenBracket, Modifiers::CTRL);
        assert_eq!(cur(&w), "b");
        // top level: nothing; an uncached parent: nothing, and no pull
        w.press(Key::U, Modifiers::CTRL | Modifiers::SHIFT);
        w.press(Key::U, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(cur(&w), "a");
        open(&mut w, "e");
        w.cmds();
        w.press(Key::U, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(cur(&w), "e");
        assert_eq!(w.app.pending_open, None);
        assert!(w.cmds().iter().all(|c| !matches!(c, Cmd::Pull { .. })));
    }

    #[test]
    fn ctrl_l_copies_the_page_url() {
        let mut w = nav_win();
        w.press(Key::L, Modifiers::CTRL);
        assert!(w.copied.is_empty(), "no page, no link");
        open(&mut w, "a");
        w.press(Key::L, Modifiers::CTRL);
        assert_eq!(w.copied, ["https://notion.so/x"]);
        assert_eq!(w.app.info.as_deref(), Some("link copied"));
    }

    fn find_win() -> Win {
        let mut texts: Vec<String> = (0..80).map(|i| format!("line {i}")).collect();
        texts[2] = "Grüße".into();
        texts[40] = "gruesse hier".into();
        texts[75] = "GRÜSSE".into();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut w = Win::with_texts(&texts);
        w.press(Key::F, Modifiers::CTRL);
        w.frames(vec![]);
        w
    }

    #[test]
    fn ctrl_f_counts_steps_wraps_scrolls_and_esc_closes() {
        let mut w = find_win();
        assert!(w.app.find.is_some());
        let cur = |w: &Win| w.app.find.as_ref().unwrap().cur;
        w.type_text("gruesse");
        assert_eq!(
            w.app.find.as_ref().unwrap().hits,
            [2, 40, 75],
            "case and umlauts folded"
        );
        assert!(w.shown.contains("1/3"), "{}", w.shown);
        w.send(key(Key::Enter));
        assert_eq!(cur(&w), 1);
        assert!(w.shown.contains("2/3"));
        assert!(w.app.scroll > 100.0, "scrolled to row 40: {}", w.app.scroll);
        w.send(key(Key::Enter));
        w.send(key(Key::Enter));
        assert_eq!(cur(&w), 0, "wraps");
        w.press(Key::Enter, Modifiers::SHIFT);
        assert_eq!(cur(&w), 2, "wraps back");
        // the typed text did not go anywhere else
        assert_eq!(w.queued(), 0);
        w.send(key(Key::Escape));
        assert!(w.app.find.is_none());
    }

    #[test]
    fn esc_in_the_find_bar_leaves_the_editor_alone() {
        let mut w = Win::with_texts(&["alpha", "beta"]);
        w.edit(1);
        w.press(Key::F, Modifiers::CTRL);
        w.frames(vec![]);
        w.type_text("alp");
        assert_eq!(w.app.find.as_ref().unwrap().hits, [0]);
        w.send(key(Key::Escape));
        assert!(w.app.find.is_none());
        assert_eq!((w.queued(), w.row_text("p-1").as_str()), (0, "beta"));
    }

    #[test]
    fn esc_closes_the_find_bar_without_the_box_having_focus() {
        let mut w = find_win();
        w.type_text("line");
        w.ctx
            .memory_mut(|m| m.surrender_focus(egui::Id::unique("find-box")));
        w.frames(vec![]);
        w.send(key(Key::Escape));
        assert!(w.app.find.is_none());
    }

    #[test]
    fn ctrl_f_while_editing_keeps_the_edit_and_esc_closes_the_bar() {
        let mut w = Win::with_texts(&["alpha", "beta"]);
        w.edit(1);
        w.type_text("X");
        w.press(Key::F, Modifiers::CTRL);
        w.frames(vec![]);
        w.type_text("alp");
        w.send(key(Key::Escape));
        assert!(w.app.find.is_none());
        let kept = match &w.app.focus {
            Some(f) => f.text.clone(),
            None => w.row_text("p-1"),
        };
        assert_eq!(kept, "betaX", "kept in the editor or committed");
        assert_eq!(w.row_text("p-0"), "alpha");
    }

    #[test]
    fn a_match_in_a_collapsed_toggle_opens_it() {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["x"]);
        let t = crate::outline::tests::blk(
            "tg",
            "p",
            "toggle",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("T") }),
        );
        let k = crate::outline::tests::blk(
            "kid",
            "tg",
            "paragraph",
            serde_json::json!({ "rich_text": crate::outline::tests::rt("needle") }),
        );
        store.replace_children("p", "p", &[t]).unwrap();
        store.replace_children("tg", "tg", &[k]).unwrap();
        let mut w = Win::new(store);
        open(&mut w, "p");
        assert!(!w.app.open.contains("tg"));
        w.press(Key::F, Modifiers::CTRL);
        w.frames(vec![]);
        w.type_text("needle");
        assert!(w.app.open.contains("tg"));
        assert!(w.shown.contains("needle"));
    }

    fn headed_win() -> Win {
        use crate::outline::tests::{blk, rt};
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_page(&crate::testkit::page_in("p", None))
            .unwrap();
        let mut kids = vec![blk(
            "h1",
            "p",
            "heading_1",
            serde_json::json!({ "rich_text": rt("One") }),
        )];
        kids.extend((0..60).map(|i| {
            blk(
                &format!("t{i}"),
                "p",
                "paragraph",
                serde_json::json!({ "rich_text": rt("t") }),
            )
        }));
        kids.push(blk(
            "h2",
            "p",
            "heading_2",
            serde_json::json!({ "rich_text": rt("Two") }),
        ));
        store.replace_children("p", "p", &kids).unwrap();
        let mut w = Win::new(store);
        open(&mut w, "p");
        w
    }

    #[test]
    fn the_outline_lists_headings_once_and_a_click_scrolls() {
        let mut w = headed_win();
        assert_eq!(
            w.app.heads,
            [(0, 1, "One".to_string()), (61, 2, "Two".to_string())]
        );
        w.app.apply(Action::Goto(61));
        w.frames(vec![]);
        w.frames(vec![]);
        assert!(w.app.scroll > 100.0, "{}", w.app.scroll);
        assert_eq!(w.app.goto, None);
        // fewer than two headings: no outline data worth drawing
        let mut w = Win::with_texts(&["x"]);
        assert!(w.app.heads.is_empty());
        w.frames(vec![]);
    }

    #[test]
    fn the_switcher_groups_recent_pages_and_previews_the_selection_once() {
        let mut w = switcher_win();
        for (id, t) in [("a", ["one", "two"]), ("b", ["three", "four"])] {
            let kids: Vec<_> = t
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    crate::outline::tests::blk(
                        &format!("{id}-{i}"),
                        id,
                        "paragraph",
                        serde_json::json!({ "rich_text": crate::outline::tests::rt(s) }),
                    )
                })
                .collect();
            w.app.store.replace_children(id, id, &kids).unwrap();
        }
        w.app.recent = vec!["a".into(), "b".into()];
        w.app
            .visited
            .insert("a".into(), crate::comments::now_secs());
        w.press(Key::K, Modifiers::CTRL);
        w.frames(vec![]);
        w.frames(vec![]);
        for header in ["Heute", "Älter"] {
            assert!(w.shown.contains(header), "{header}: {}", w.shown);
        }
        assert!(!w.shown.contains("Gestern"));
        let pv = |w: &Win| w.app.switcher.as_ref().unwrap().preview.clone().unwrap();
        assert_eq!(
            pv(&w),
            ("a".to_string(), vec!["one".to_string(), "two".to_string()])
        );
        // frames do not re-read the page
        w.app.store.replace_children("a", "a", &[]).unwrap();
        w.frames(vec![]);
        assert_eq!(pv(&w).1, ["one", "two"]);
        // ↓ lands on the next page, not on a header
        w.send(key(Key::ArrowDown));
        assert_eq!(
            pv(&w),
            (
                "b".to_string(),
                vec!["three".to_string(), "four".to_string()]
            )
        );
        w.send(key(Key::Enter));
        assert_eq!(cur(&w), "b");
        // a search is a flat list
        w.press(Key::K, Modifiers::CTRL);
        w.type_text("al");
        assert!(!w.shown.contains("Heute"));
    }

    #[test]
    fn switching_workspaces_reopens_the_store_and_uses_its_session() {
        use crate::profiles::{Profile, Profiles};
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("config");
        let data = dir.path().join("data");
        let mut w = Win::with_texts(&["a"]);
        w.app.session_dir = Some(conf.clone());
        w.app.session_file = "session-egui-one.json".into();
        let mut profiles = Profiles::default();
        profiles.upsert(Profile::new("One", ""));
        profiles.upsert(Profile::new("Two", ""));
        profiles.set_active("One");
        w.app.profiles = profiles;
        w.app.profiles_dir = Some(conf.clone());
        w.app.data_dir = Some(data.clone());
        w.app.pending_switch = Some(("Two".into(), true));
        w.app.apply_pending_switch(&w.ctx);
        assert_eq!(w.app.workspace_name(), "Two");
        assert_eq!(w.app.session_file, "session-egui-two.json");
        assert!(
            data.join("cache-two.db").exists(),
            "the profile's own cache"
        );
        assert!(w.app.tree.is_empty(), "the new workspace starts empty");
        assert!(
            conf.join("session-egui-one.json").exists(),
            "the old session was saved"
        );
        let saved = std::fs::read_to_string(conf.join(crate::profiles::FILE)).unwrap();
        assert!(saved.contains("\"active\": \"Two\""), "{saved}");
        w.app.shutdown();
    }

    #[test]
    fn sign_in_without_an_oauth_app_opens_the_setup_dialog() {
        let mut w = Win::with_texts(&["a"]);
        w.app.profiles.oauth = None;
        w.app.apply(Action::OAuthStart);
        assert!(w.app.oauth.is_none(), "no listener without credentials");
        assert!(w.app.oauth_setup.is_some());

        // saving an incomplete app keeps the dialog open with a message
        w.app.apply(Action::OAuthSetupSave);
        assert!(w.app.oauth_setup.is_some());
        assert!(w.app.oauth_error.as_deref().unwrap().contains("client id"));
        w.app.apply(Action::OAuthSetupCancel);
        assert!(w.app.oauth_setup.is_none() && w.app.oauth_error.is_none());
    }

    #[test]
    fn the_workspace_form_adds_a_profile_and_makes_it_active() {
        use crate::profiles::{Form, Profiles};
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("config");
        let data = dir.path().join("data");
        let mut w = Win::with_texts(&["a"]);
        w.app.session_dir = Some(conf.clone());
        w.app.profiles_dir = Some(conf.clone());
        w.app.data_dir = Some(data);
        w.app.profiles = Profiles::default();
        w.app.ws_form = Some(Form {
            original: None,
            name: "Acme".into(),
            token: "ntn_x".into(),
            user_id: String::new(),
            error: None,
        });
        w.app.save_workspace_form();
        assert_eq!(w.app.workspace_name(), "Acme");
        assert_eq!(
            w.app.pending_switch.as_ref().map(|p| p.0.as_str()),
            Some("Acme")
        );
        let saved = std::fs::read_to_string(conf.join(crate::profiles::FILE)).unwrap();
        assert!(saved.contains("Acme") && saved.contains("ntn_x"), "{saved}");
        // never switch in the test: the fake token would start a live worker
        w.app.pending_switch = None;
        w.app.shutdown();
    }
}
