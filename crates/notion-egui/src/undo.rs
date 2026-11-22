//! Undo / redo of local edits (ported from notion-gui): every command that
//! may change the page runs inside [`App::step`], whose changes (a core
//! checkpoint diff) become one step on the undo stack.

use notion_core::sync;

use crate::app::{App, Grab};

const UNDO_MAX: usize = 100;

/// (page, what one step changed).
pub(crate) type Step = (String, sync::Change);

impl App {
    /// Runs `f`, which may edit the current page locally, as one undo step.
    /// Nested steps join the outer one.
    // ponytail: snapshots the whole page + op table per step and keeps only the
    // changed rows; diff per block if huge pages make edits lag
    pub(crate) fn step<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.step_page(self.current.iter().cloned().collect(), f)
    }

    /// [`step`](Self::step) for edits of `pages`, which needn't be open: the
    /// first is the step's page (what undo shows), the rest are also changed.
    pub(crate) fn step_page<R>(&mut self, pages: Vec<String>, f: impl FnOnce(&mut Self) -> R) -> R {
        let Some(page) = pages.first().cloned().filter(|_| !self.stepping) else {
            return f(self);
        };
        let cp = sync::checkpoint(&self.store, &pages.iter().map(String::as_str).collect::<Vec<_>>());
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

    /// Undo (`back`) or redo of the last local edit; uncommitted typing is
    /// committed first, so this undoes exactly it. Does nothing while a
    /// command is in flight (a pull writes rows it read before the undo) or
    /// the switcher is open; refused once the edit's op was completed.
    pub(crate) fn undo_redo(&mut self, back: bool) {
        if !self.sent.is_empty() || self.switcher.is_some() || self.dlg.is_some() || !self.flush() {
            return;
        }
        let Some((page, c)) = (if back { &mut self.undo } else { &mut self.redo }).pop() else {
            return;
        };
        // just written: the buffer is clean
        let edited = self
            .focus
            .take()
            .filter(|f| f.cell.is_none())
            .map(|f| f.block_id);
        self.title_edit = None;
        if let Some(d) = &mut self.db {
            d.edit = None;
        }
        match sync::restore(&self.store, &c) {
            Ok(true) => {
                let inv = (page.clone(), c.inverse());
                if back { &mut self.redo } else { &mut self.undo }.push(inv);
                // not into a trashed page (redoing a trash, undoing a restore)
                let is_table = self.store.data_source(&page).ok().flatten().is_some();
                let trashed = !is_table && self.store.get_page(&page).ok().flatten().is_none_or(|p| p.in_trash);
                if self.current.as_deref() != Some(page.as_str()) && !trashed {
                    self.pending_open = None;
                    self.set_current(&page);
                }
            }
            res => {
                self.undo.clear();
                self.redo.clear();
                self.last_error = Some(match res {
                    Err(e) => format!("undo: {e}"),
                    _ => "can't undo: pushed or changed since".into(),
                });
            }
        }
        // redoing a trash of the open page, or undoing its creation
        if let Some(id) = self.current.clone().filter(|id| !self.cached(id) || self.store.get_page(id).ok().flatten().is_some_and(|p| p.in_trash)) {
            self.current = None;
            self.info = Some(format!("page {id} is gone"));
        }
        self.refresh();
        self.reveal_current();
        if let Some(i) = edited.and_then(|id| {
            self.rows
                .iter()
                .position(|r| r.block_id == id && r.editable)
        }) {
            self.start_edit(i, Grab::End);
        }
    }
}
