//! F4 databases, part 1: a data source as a table. Columns are the title, then
//! the schema's properties; rows are the cached pages of the data source. Sort
//! and filter are local (nothing is asked of Notion); every edit is a local
//! edit through core (`sync::local_set_property`, queued, one undo step,
//! pushed with Ctrl+S). The table's keys are taken before any widget.

use std::cmp::Ordering;

use eframe::egui::{
    self, Align2, Context, FontId, Frame, Id, Key, Margin, Modifiers, Rect, RichText, Sense, Stroke, TextEdit, Ui,
    Vec2,
};
use eframe::egui::{pos2, vec2};
use notion_core::model::{DataSource, Page, User};
use notion_core::{props, sync};
use serde_json::Value;

use crate::app::{Action, App};
use crate::keys::{take_exact, Command};
use crate::pages::Dlg;
use crate::slash::fold;
use crate::theme::{self, Colors};
use crate::view::untitled;
use crate::worker::Cmd;

const COL_W: f32 = 170.0;
const ROW_H: f32 = 26.0;
/// The open-as-page and trash buttons in front of each row.
const LEAD_W: f32 = 56.0;

pub(crate) struct Col {
    pub name: String,
    pub kind: String,
    /// The select, multi-select or status options.
    pub opts: Vec<String>,
}

pub(crate) struct DbRow {
    pub id: String,
    /// The page's raw properties.
    pub props: Value,
    /// What each column shows, folded for sorting, and all of it folded for
    /// the filter.
    pub cells: Vec<String>,
    keys: Vec<String>,
    nums: Vec<Option<f64>>,
    fold: String,
}

/// The cell being edited: a text box, or a popup of choices (select, status,
/// multi-select, people). `text` is in the format `props::encode` reads.
pub(crate) struct CellEdit {
    pub row: String,
    pub col: usize,
    pub text: String,
    /// The text as the edit started: an unchanged commit writes nothing.
    pub original: String,
    /// The cached cell's text as the edit last saw it: another one at a write
    /// means a pull changed the cell meanwhile.
    seen: String,
    /// The popup's highlighted choice.
    pub cur: usize,
    /// Set until the text box has taken the keyboard focus.
    grab: bool,
    /// The last commit failed (a second one while leaving discards the edit).
    pub failed: bool,
}

pub(crate) struct Db {
    pub ds: String,
    /// The database's data sources (id, title), when there are several.
    pub tabs: Vec<(String, String)>,
    pub cols: Vec<Col>,
    pub rows: Vec<DbRow>,
    /// Indexes into `rows`: what the filter lets through, in sort order.
    pub view: Vec<usize>,
    pub filter: String,
    /// (column, descending).
    pub sort: Option<(usize, bool)>,
    /// The selected cell: (row id, column).
    pub sel: Option<(String, usize)>,
    pub edit: Option<CellEdit>,
    pub focus_filter: bool,
    /// Scroll the selected cell into view.
    reveal: bool,
    offset: f32,
    height: f32,
}

pub(crate) enum Act {
    Cell(String, usize),
    Sort(usize),
    /// A popup choice (index).
    Pick(usize),
    /// Closes the edit of this cell (a stale one, of another cell, is ignored).
    Commit(String, usize),
    OpenRow(String),
    Trash(String),
    NewRow,
    Tab(String),
}

pub(crate) fn is_choice(kind: &str) -> bool {
    matches!(kind, "select" | "status" | "multi_select" | "people")
}

fn items(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// `text` with `v` added, or removed if it is in it.
fn toggled(text: &str, v: &str) -> String {
    let mut l = items(text);
    match l.iter().position(|x| x == v) {
        Some(i) => {
            l.remove(i);
        }
        None => l.push(v.to_string()),
    }
    l.join(",")
}

fn options(p: &Value) -> Vec<String> {
    let kind = p["type"].as_str().unwrap_or_default();
    let o = p[kind]["options"].as_array().map(Vec::as_slice).unwrap_or_default();
    o.iter().filter_map(|o| o["name"].as_str().map(String::from)).collect()
}

fn filter_id() -> Id {
    Id::unique("db-filter")
}

impl Db {
    /// The table of `ds` with its (live) `pages`, in the order given. `sort`
    /// is the remembered (column name, descending).
    pub fn new(ds: &DataSource, pages: &[Page], users: &[User], sort: Option<&(String, bool)>) -> Db {
        let schema = ds.properties.as_object();
        let col = |(name, p): (&String, &Value)| Col {
            name: name.clone(),
            kind: p["type"].as_str().unwrap_or_default().to_string(),
            opts: options(p),
        };
        let mut cols: Vec<Col> = Vec::new();
        for want_title in [true, false] {
            let all = schema.into_iter().flatten();
            cols.extend(all.filter(|(_, p)| (p["type"] == "title") == want_title).map(col));
        }
        let who = |id: &str| crate::people::name_of(users, id);
        let rows = pages
            .iter()
            .map(|p| {
                let props = p.raw["properties"].clone();
                let cells: Vec<String> = cols.iter().map(|c| props::cell_text(&props[&c.name], &who)).collect();
                DbRow {
                    id: p.id.clone(),
                    keys: cells.iter().map(|c| fold(c)).collect(),
                    nums: cols.iter().map(|c| props::sort_num(&props[&c.name])).collect(),
                    fold: fold(&cells.join("\n")),
                    cells,
                    props,
                }
            })
            .collect();
        let sort = sort.and_then(|(n, d)| Some((cols.iter().position(|c| c.name == *n)?, *d)));
        let mut db = Db {
            ds: ds.id.clone(),
            tabs: Vec::new(),
            cols,
            rows,
            view: Vec::new(),
            filter: String::new(),
            sort,
            sel: None,
            edit: None,
            focus_filter: false,
            reveal: false,
            offset: 0.0,
            height: 0.0,
        };
        db.arrange();
        db
    }

    /// Rebuilds `view`: the filter (case-insensitive, umlauts folded, over
    /// all cell text), then the sort (empty cells last, either way). Drops a
    /// selection the filter hides.
    pub fn arrange(&mut self) {
        let q = fold(self.filter.trim());
        let mut v: Vec<usize> = (0..self.rows.len()).filter(|&i| q.is_empty() || self.rows[i].fold.contains(&q)).collect();
        if let Some((c, desc)) = self.sort {
            v.sort_by(|&a, &b| {
                let (a, b) = (&self.rows[a], &self.rows[b]);
                match (a.cells[c].is_empty(), b.cells[c].is_empty()) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Greater,
                    (false, true) => return Ordering::Less,
                    _ => {}
                }
                let o = match (a.nums[c], b.nums[c]) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    _ => a.keys[c].cmp(&b.keys[c]),
                };
                if desc { o.reverse() } else { o }
            });
        }
        self.view = v;
        if self.sel.as_ref().is_some_and(|(id, _)| self.pos(id).is_none()) {
            self.sel = None;
        }
    }

    /// Where row `id` is in the view.
    pub fn pos(&self, id: &str) -> Option<usize> {
        self.view.iter().position(|&i| self.rows[i].id == id)
    }

    pub fn row(&self, id: &str) -> Option<&DbRow> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// Header click: ascending, descending, none.
    pub fn cycle_sort(&mut self, col: usize) {
        self.sort = match self.sort {
            Some((c, false)) if c == col => Some((col, true)),
            Some((c, true)) if c == col => None,
            _ => Some((col, false)),
        };
        self.arrange();
    }

    /// Moves the selection (the first cell if there is none yet).
    pub fn step(&mut self, dr: isize, dc: isize) {
        let Some((id, c)) = self.sel.clone() else {
            if let Some(&i) = self.view.first() {
                self.sel = Some((self.rows[i].id.clone(), 0));
            }
            return;
        };
        let Some(p) = self.pos(&id) else { return };
        let p = (p as isize + dr).clamp(0, self.view.len() as isize - 1) as usize;
        let c = (c as isize + dc).clamp(0, self.cols.len() as isize - 1) as usize;
        self.sel = Some((self.rows[self.view[p]].id.clone(), c));
        self.reveal = true;
    }

    /// (value, label) of the popup's choices for column `col`.
    pub fn choices(&self, col: usize, users: &[User]) -> Vec<(String, String)> {
        let c = &self.cols[col];
        let opts = c.opts.iter().map(|o| (o.clone(), o.clone()));
        match c.kind.as_str() {
            "select" => std::iter::once((String::new(), "— none —".to_string())).chain(opts).collect(),
            "people" => users
                .iter()
                .map(|u| (u.id.clone(), if u.name.is_empty() { u.id.chars().take(8).collect() } else { u.name.clone() }))
                .collect(),
            _ => opts.collect(),
        }
    }

    /// After a push: the local ids of the selected and edited row are real now.
    pub fn remap(&mut self, r: impl Fn(&str) -> String) {
        self.sel = self.sel.take().map(|(id, c)| (r(&id), c));
        if let Some(e) = &mut self.edit {
            e.row = r(&e.row);
        }
    }

    /// The cached text of a cell, as `begin` reads it.
    fn cell_now(&self, row: &str, col: usize) -> Option<String> {
        Some(props::edit_text(&self.row(row)?.props[&self.cols.get(col)?.name]))
    }

    /// Starts editing a cell; the error says why it can't be.
    pub fn begin(&mut self, row: &str, col: usize, users: &[User]) -> Result<(), String> {
        let c = self.cols.get(col).ok_or("no such column")?;
        if !props::editable(&c.kind) {
            return Err(format!("read-only: a {} property can't be edited", c.kind));
        }
        let r = self.row(row).ok_or("no such row")?;
        let text = props::edit_text(&r.props[&c.name]);
        let cur = if is_choice(&c.kind) {
            let first = items(&text).into_iter().next().unwrap_or_default();
            self.choices(col, users).iter().position(|(v, _)| *v == first).unwrap_or(0)
        } else {
            0
        };
        self.edit = Some(CellEdit { row: row.into(), col, original: text.clone(), seen: text.clone(), text, cur, grab: true, failed: false });
        Ok(())
    }
}

impl App {
    /// The data source `id` stands for: itself, or the first of a database's.
    pub(crate) fn db_target(&self, id: &str) -> Option<String> {
        if matches!(self.store.data_source(id), Ok(Some(_))) {
            return Some(id.to_string());
        }
        self.store.data_sources_of(id).ok()?.into_iter().next().map(|d| d.id)
    }

    /// Builds the table if the open page is a cached data source (else `db`
    /// is None); filter, selection and edit survive a reload.
    pub(crate) fn load_db(&mut self, id: &str) -> notion_core::Result<()> {
        let Some(ds) = self.store.data_source(id)? else {
            self.db = None;
            return Ok(());
        };
        self.title = ds.title.clone();
        self.title_editable = false;
        self.page_edit = None;
        self.meta = Default::default();
        self.page_head = None;
        self.block_edits.clear();
        self.rows.clear();
        let mut pages: Vec<Page> = self.store.rows(id)?.into_iter().filter(|p| !p.in_trash).collect();
        pages.sort_by(|a, b| (a.created_time, &a.id).cmp(&(b.created_time, &b.id)));
        let mut db = Db::new(&ds, &pages, &self.users, self.db_sorts.get(id));
        let all = self.store.data_sources_of(&ds.database_id)?;
        if all.len() > 1 {
            db.tabs = all.into_iter().map(|d| (d.id, d.title)).collect();
        }
        if let Some(old) = self.db.take().filter(|d| d.ds == id) {
            let r = |s: String| self.store.resolve_id(&s).unwrap_or(s);
            db.filter = old.filter;
            db.sel = old.sel.map(|(row, c)| (r(row), c));
            db.edit = old.edit.map(|mut e| {
                e.row = r(e.row);
                e
            });
            (db.focus_filter, db.reveal, db.offset, db.height) = (old.focus_filter, old.reveal, old.offset, old.height);
            if db.edit.as_ref().is_some_and(|e| db.row(&e.row).is_none() || e.col >= db.cols.len()) {
                db.edit = None;
            }
            // a pull may have dropped columns
            if db.sel.as_ref().is_some_and(|(_, c)| *c >= db.cols.len()) {
                db.sel = None;
            }
            db.arrange();
        }
        self.db = Some(db);
        Ok(())
    }

    /// A row of the open table.
    pub(crate) fn db_has_row(&self, id: &str) -> bool {
        self.db.as_ref().is_some_and(|d| d.row(id).is_some())
    }

    /// Pulls the open table unless that happened lately or is under way.
    pub(crate) fn db_pull_if_stale(&mut self) {
        let Some(ds) = self.db.as_ref().map(|d| d.ds.clone()) else { return };
        let fresh = self.full_at.get(&ds).is_some_and(|t| t.elapsed() < crate::shell::FULL_MIN);
        let out = self.sent.iter().any(|c| matches!(c, Cmd::Pull { id, .. } if *id == ds));
        if !(fresh || out || self.worker_gone || self.offline() || ds.starts_with("local-")) {
            self.send(Cmd::Pull { id: ds, force: false });
        }
    }

    /// One cell written to the store as one undo step. The caller refreshes.
    fn db_write(&mut self, row: &str, col: usize, text: &str) -> bool {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return false;
        }
        let Some((ds, name)) = self.db.as_ref().and_then(|d| Some((d.ds.clone(), d.cols.get(col)?.name.clone()))) else {
            return false;
        };
        match self.step_page(vec![ds, row.to_string()], |s| sync::local_set_property(&s.store, row, &name, text)) {
            Ok(_) => true,
            Err(e) => {
                self.last_error = Some(e.to_string());
                false
            }
        }
    }

    /// [`Self::db_write`] unless the cell changed in the cache since the edit began.
    fn db_write_edit(&mut self, row: &str, col: usize, text: &str) -> bool {
        let stale = self.db.as_ref().and_then(|d| Some(d.cell_now(row, col)? != d.edit.as_ref()?.seen));
        if stale == Some(true) {
            self.last_error = Some("changed in Notion meanwhile: Esc to discard your edit".into());
            return false;
        }
        self.db_write(row, col, text)
    }

    /// The text box's edit written to the store, the box staying open (a
    /// pull, undo or Ctrl+S needs it in the store). Choices wait for their
    /// commit.
    pub(crate) fn db_flush(&mut self) -> bool {
        let Some(d) = &self.db else { return true };
        let Some(e) = d.edit.as_ref().filter(|e| e.text != e.original && !is_choice(&d.cols[e.col].kind)) else {
            return true;
        };
        let (row, col, text) = (e.row.clone(), e.col, e.text.clone());
        if !self.db_write_edit(&row, col, &text) {
            return false;
        }
        self.refresh();
        if let Some(d) = &mut self.db {
            let now = d.cell_now(&row, col);
            if let Some(e) = d.edit.as_mut() {
                (e.original, e.failed) = (text, false);
                e.seen = now.unwrap_or_else(|| e.original.clone());
            }
        }
        true
    }

    /// Writes the edit (if it changed) and closes it. A refused value keeps
    /// the editor, but when `leaving` the second try discards it.
    pub(crate) fn db_commit(&mut self, leaving: bool) -> bool {
        let Some(e) = self.db.as_ref().and_then(|d| d.edit.as_ref()) else { return true };
        let wrote = e.text != e.original;
        if wrote {
            let (row, col, text) = (e.row.clone(), e.col, e.text.clone());
            if !self.db_write_edit(&row, col, &text) {
                let d = self.db.as_mut().expect("open");
                let e = d.edit.as_mut().expect("open");
                if leaving && e.failed {
                    d.edit = None;
                    self.last_error = Some("edit discarded: it could not be saved".into());
                    return true;
                }
                e.failed |= leaving;
                return false;
            }
        }
        if let Some(d) = &mut self.db {
            d.edit = None;
        }
        if wrote {
            self.refresh();
        }
        true
    }

    /// Enter or a click on the selected cell: edits it (a checkbox toggles).
    fn db_activate(&mut self) {
        let Some((row, col)) = self.db.as_ref().and_then(|d| d.sel.clone()) else { return };
        let kind = self.db.as_ref().expect("open").cols[col].kind.clone();
        if !props::editable(&kind) {
            self.info = Some(format!("read-only: a {kind} property can't be edited"));
            return;
        }
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        if kind == "checkbox" {
            return self.db_toggle(&row, col);
        }
        let (db, users) = (self.db.as_mut().expect("open"), &self.users);
        if let Err(e) = db.begin(&row, col, users) {
            self.info = Some(e);
        }
    }

    fn db_toggle(&mut self, row: &str, col: usize) {
        let Some(d) = &self.db else { return };
        let on = d.row(row).is_some_and(|r| r.props[&d.cols[col].name]["checkbox"] == true);
        if self.db_write(row, col, if on { "false" } else { "true" }) {
            self.refresh();
        }
    }

    /// Ctrl+Enter, Ctrl+N and "+ New": a row, its title in edit mode.
    pub(crate) fn db_new_row(&mut self) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        if !self.db_commit(true) {
            return;
        }
        let Some(ds) = self.db.as_ref().map(|d| d.ds.clone()) else { return };
        match self.step_page(vec![ds.clone()], |s| sync::local_create_row(&s.store, &ds)) {
            Ok(id) => {
                if let Some(d) = &mut self.db {
                    d.filter.clear();
                    d.sel = Some((id, 0));
                    d.reveal = true;
                }
                self.refresh();
                self.db_activate();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Asks before trashing `id` (the selected row if None).
    pub(crate) fn db_ask_trash(&mut self, id: Option<String>) {
        if !self.db_commit(true) {
            return;
        }
        let Some(d) = &self.db else { return };
        let Some(id) = id.or_else(|| d.sel.as_ref().map(|s| s.0.clone())) else {
            self.info = Some("select a row first".into());
            return;
        };
        if id.starts_with("local-") {
            self.last_error = Some("this row isn't pushed yet: push it (Ctrl+S) first".into());
            return;
        }
        let title = d.row(&id).and_then(|r| r.cells.first().cloned()).unwrap_or_default();
        self.dlg = Some(Dlg::Trash(id, title));
    }

    /// Queues the trashing of a row of the open table (one undo step that
    /// keeps the table open).
    pub(crate) fn trash_row(&mut self, id: String) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let Some(ds) = self.db.as_ref().map(|d| d.ds.clone()) else { return };
        match self.step_page(vec![ds, id.clone()], |s| sync::local_trash_page(&s.store, &id, true)) {
            Ok(_) => {
                self.info = Some("trashed: restore it from the Trash view".into());
                self.refresh();
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// The page commands (Ctrl+N, Ctrl+Shift+Backspace, ...) on a table.
    pub(crate) fn db_command(&mut self, c: Command) {
        match c {
            Command::NewPage => self.db_new_row(),
            Command::TrashPage => self.db_ask_trash(None),
            Command::TrashView if self.commit() => self.dlg = Some(Dlg::Bin { sel: 0 }),
            Command::TrashView => {}
            _ => self.info = Some("not available on a database: open a row as a page (Ctrl+Shift+O)".into()),
        }
    }

    /// What a click on the table asked for.
    pub(crate) fn db_act(&mut self, a: Act) {
        match a {
            Act::Cell(row, col) => {
                let same = self.db.as_ref().and_then(|d| d.edit.as_ref()).is_some_and(|e| e.row == row && e.col == col);
                if same || !self.db_commit(true) {
                    return;
                }
                if let Some(d) = &mut self.db {
                    d.sel = Some((row, col));
                }
                self.db_activate();
            }
            Act::Sort(col) => {
                let Some(d) = &mut self.db else { return };
                d.cycle_sort(col);
                match d.sort {
                    Some((c, desc)) => self.db_sorts.insert(d.ds.clone(), (d.cols[c].name.clone(), desc)),
                    None => self.db_sorts.remove(&d.ds),
                };
            }
            Act::Pick(i) => self.db_pick(i),
            Act::Commit(row, col) => {
                if self.db.as_ref().and_then(|d| d.edit.as_ref()).is_some_and(|e| e.row == row && e.col == col) {
                    self.db_commit(true);
                }
            }
            Act::OpenRow(id) if id.starts_with("local-") => {
                self.last_error = Some("this row isn't pushed yet: push it (Ctrl+S) first".into());
            }
            Act::OpenRow(id) => self.open_or_pull(&id),
            Act::Trash(id) => self.db_ask_trash(Some(id)),
            Act::NewRow => self.db_new_row(),
            Act::Tab(id) => self.open_or_pull(&id),
        }
    }

    /// A popup choice: a single choice is written, multiple ones toggle.
    fn db_pick(&mut self, i: usize) {
        let (db, users) = (self.db.as_mut(), &self.users);
        let Some(db) = db else { return };
        let Some((col, kind)) = db.edit.as_ref().map(|e| (e.col, db.cols[e.col].kind.clone())) else { return };
        let Some((v, _)) = db.choices(col, users).into_iter().nth(i) else { return };
        let e = db.edit.as_mut().expect("editing");
        e.cur = i;
        if kind == "multi_select" || kind == "people" {
            e.text = toggled(&e.text, &v);
        } else {
            e.text = v;
            self.db_commit(false);
        }
    }

    /// The table's keys, taken before any widget sees them.
    pub(crate) fn db_keys(&mut self, ctx: &Context) {
        let Some(db) = &self.db else { return };
        if self.switcher.is_some()
            || self.dlg.is_some()
            || self.conflict.is_some()
            || self.problems_open
            || self.comments.is_some()
            || self.title_edit.is_some()
        {
            return;
        }
        let kind = db.edit.as_ref().map(|e| db.cols[e.col].kind.clone());
        let take = |m: Modifiers, k: Key| {
            let (fresh, repeats) = ctx.input_mut(|i| take_exact(&mut i.events, m, k));
            (fresh, fresh + repeats)
        };
        let none = Modifiers::NONE;
        if let Some(kind) = kind {
            let esc = take(none, Key::Escape).0 > 0;
            let enter = take(none, Key::Enter).0 > 0;
            if esc {
                if let Some(d) = &mut self.db {
                    d.edit = None;
                }
            } else if enter {
                self.db_enter(&kind);
            } else if is_choice(&kind) {
                let (down, up) = (take(none, Key::ArrowDown).1, take(none, Key::ArrowUp).1);
                let space = take(none, Key::Space).0 > 0;
                let (db, users) = (self.db.as_mut().expect("open"), &self.users);
                let n = db.choices(db.edit.as_ref().expect("editing").col, users).len();
                let e = db.edit.as_mut().expect("editing");
                e.cur = (e.cur + down).min(n.saturating_sub(1)).saturating_sub(up);
                if space && kind != "select" && kind != "status" {
                    self.db_pick(self.db.as_ref().and_then(|d| d.edit.as_ref()).map_or(0, |e| e.cur));
                }
            }
            return;
        }
        if ctx.memory(|m| m.has_focus(filter_id())) {
            if take(none, Key::Escape).0 + take(none, Key::Enter).0 > 0 {
                ctx.memory_mut(|m| m.surrender_focus(filter_id()));
            }
            return;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (up, down) = (take(none, Key::ArrowUp).1, take(none, Key::ArrowDown).1);
        let (left, right) = (take(none, Key::ArrowLeft).1, take(none, Key::ArrowRight).1);
        let (enter, space) = (take(none, Key::Enter).0 > 0, take(none, Key::Space).0 > 0);
        let new = take(Modifiers::CTRL, Key::Enter).0 > 0;
        let open = take(Modifiers::CTRL | Modifiers::SHIFT, Key::O).0 > 0;
        let Some(db) = &mut self.db else { return };
        if up + down + left + right > 0 {
            db.step(down as isize - up as isize, right as isize - left as isize);
        }
        if enter {
            self.db_activate();
        }
        if space {
            let sel = self.db.as_ref().and_then(|d| d.sel.clone());
            if let Some((row, col)) = sel.filter(|(_, c)| self.db.as_ref().is_some_and(|d| d.cols[*c].kind == "checkbox")) {
                if self.pushing() {
                    self.last_error = Some("editing is paused while a push runs".into());
                } else {
                    self.db_toggle(&row, col);
                }
            }
        }
        if new {
            self.db_new_row();
        }
        if open {
            match self.db.as_ref().and_then(|d| d.sel.clone()) {
                Some((row, _)) if !row.starts_with("local-") => self.open_or_pull(&row),
                Some(_) => self.last_error = Some("this row isn't pushed yet: push it (Ctrl+S) first".into()),
                None => self.info = Some("select a row first".into()),
            }
        }
    }

    /// Enter in the editor: a single choice takes the highlighted one first.
    fn db_enter(&mut self, kind: &str) {
        if kind == "select" || kind == "status" {
            let cur = self.db.as_ref().and_then(|d| d.edit.as_ref()).map_or(0, |e| e.cur);
            return self.db_pick(cur);
        }
        self.db_commit(false);
    }

    /// The table, with its title, filter box and (several data sources) tabs.
    pub(crate) fn db_ui(&mut self, ui: &mut Ui) -> Vec<Action> {
        let c = theme::colors(ui);
        let crumbs = self.breadcrumb();
        let title = untitled(&self.title).to_string();
        let Some(mut db) = self.db.take() else { return Vec::new() };
        let choices = match &db.edit {
            Some(e) if is_choice(&db.cols[e.col].kind) => db.choices(e.col, &self.users),
            _ => Vec::new(),
        };
        let acts = grid_ui(ui, c, &mut db, &crumbs, &title, &choices);
        self.db = Some(db);
        acts
    }
}

fn grid_ui(ui: &mut Ui, c: &Colors, db: &mut Db, crumbs: &[(String, String, String)], title: &str, choices: &[(String, String)]) -> Vec<Action> {
    let mut acts = Vec::new();
    Frame::new().inner_margin(Margin::symmetric(16, 8)).show(ui, |ui| {
        acts.extend(crate::view::breadcrumb(ui, c, crumbs, title));
        ui.add_space(4.0);
        ui.label(RichText::new(title).size(26.0).strong());
        if db.tabs.len() > 1 {
            ui.horizontal(|ui| {
                for (id, t) in &db.tabs {
                    if ui.selectable_label(*id == db.ds, untitled(t)).clicked() && *id != db.ds {
                        acts.push(Action::Db(Act::Tab(id.clone())));
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            let f = ui.add(TextEdit::singleline(&mut db.filter).id(filter_id()).hint_text("Filter…  (Ctrl+F)").desired_width(220.0));
            if db.focus_filter {
                f.request_focus();
                db.focus_filter = false;
            }
            if f.changed() {
                db.arrange();
            }
            ui.label(RichText::new(format!("{} of {} rows", db.view.len(), db.rows.len())).small().color(c.secondary));
            if ui.button("+ New").on_hover_text("Ctrl+Enter").clicked() {
                acts.push(Action::Db(Act::NewRow));
            }
        });
        ui.add_space(6.0);
        table(ui, c, db, choices, &mut acts);
    });
    acts
}

/// Cell text, clipped to its cell.
fn cell_text(ui: &Ui, cell: Rect, text: &str, size: f32, color: egui::Color32) {
    ui.painter().with_clip_rect(cell.shrink2(vec2(2.0, 0.0))).text(
        cell.left_center() + vec2(6.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(size),
        color,
    );
}

fn table(ui: &mut Ui, c: &Colors, db: &mut Db, choices: &[(String, String)], acts: &mut Vec<Action>) {
    let width = LEAD_W + COL_W * db.cols.len() as f32;
    let avail = (ui.available_height() - 2.0 * ROW_H - 20.0).max(100.0);
    let mut popup: Option<Rect> = None;
    let mut revealed = false;
    egui::ScrollArea::horizontal().id_salt(("db-h", db.ds.clone())).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let (head, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
        ui.painter().rect_filled(head, 0.0, c.sidebar);
        for (ci, col) in db.cols.iter().enumerate() {
            let r = Rect::from_min_size(pos2(head.left() + LEAD_W + COL_W * ci as f32, head.top()), vec2(COL_W, ROW_H));
            let resp = ui.interact(r, Id::unique(("db-head", &db.ds, ci)), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, 0.0, c.hover);
            }
            let mark = match db.sort {
                Some((s, false)) if s == ci => " ▲",
                Some((s, true)) if s == ci => " ▼",
                _ => "",
            };
            cell_text(ui, r, &format!("{}{mark}", col.name), 13.0, c.secondary);
            if resp.on_hover_text(format!("{} ({}): click to sort", col.name, col.kind)).clicked() {
                acts.push(Action::Db(Act::Sort(ci)));
            }
        }
        let mut area = egui::ScrollArea::vertical().id_salt(("db-v", db.ds.clone())).auto_shrink([false, false]).max_height(avail);
        if db.reveal {
            if let Some(p) = db.sel.as_ref().and_then(|(id, _)| db.pos(id)) {
                let top = p as f32 * ROW_H;
                let h = db.height.max(ROW_H);
                let off = if top < db.offset { top } else if top + ROW_H > db.offset + h { top + ROW_H - h } else { db.offset };
                area = area.vertical_scroll_offset(off);
            }
        }
        let out = area.show_rows(ui, ROW_H, db.view.len(), |ui, range| {
            for pos in range {
                let row = &db.rows[db.view[pos]];
                let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
                if pos % 2 == 1 {
                    ui.painter().rect_filled(rect, 0.0, c.hover);
                }
                ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, c.border));
                let local = row.id.starts_with("local-");
                for (n, glyph, tip) in [(0.0, "↗", "Open as a page (Ctrl+Shift+O)"), (1.0, "🗑", "Trash (Ctrl+Shift+Backspace)")] {
                    let r = Rect::from_min_size(rect.min + vec2(n * 28.0, 0.0), vec2(28.0, ROW_H));
                    let resp = ui.interact(r, Id::unique(("db-lead", &row.id, n as u32)), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(r, 3.0, c.hover);
                    }
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(13.0), c.secondary);
                    if resp.on_hover_text(tip).clicked() {
                        let id = row.id.clone();
                        acts.push(Action::Db(if n == 0.0 { Act::OpenRow(id) } else { Act::Trash(id) }));
                    }
                }
                for (ci, col) in db.cols.iter().enumerate() {
                    let cell = Rect::from_min_size(pos2(rect.left() + LEAD_W + COL_W * ci as f32, rect.top()), vec2(COL_W, ROW_H));
                    let selected = db.sel.as_ref().is_some_and(|(r, c)| *r == row.id && *c == ci);
                    let editing = db.edit.as_ref().is_some_and(|e| e.row == row.id && e.col == ci);
                    if selected {
                        ui.painter().rect_stroke(cell.shrink(1.0), 2.0, Stroke::new(2.0, c.accent), egui::StrokeKind::Inside);
                        if db.reveal {
                            ui.scroll_to_rect(cell, None);
                            revealed = true;
                        }
                    }
                    if editing && !is_choice(&col.kind) {
                        let e = db.edit.as_mut().expect("editing");
                        let eid = Id::unique(("db-edit", &e.row, e.col));
                        if e.grab {
                            let st = egui::text_edit::TextEditState::load(ui.ctx(), eid).unwrap_or_default();
                            crate::app::set_cursor(ui.ctx(), eid, st, e.text.chars().count());
                        }
                        let r = ui.put(cell, TextEdit::singleline(&mut e.text).id(eid).frame(Frame::NONE).margin(Margin::symmetric(6, 4)));
                        if e.grab {
                            r.request_focus();
                            e.grab = !r.has_focus();
                        } else if r.lost_focus() {
                            // a click elsewhere: Enter and Esc were taken before the box saw them
                            acts.push(Action::Db(Act::Commit(e.row.clone(), e.col)));
                        }
                        continue;
                    }
                    if editing {
                        popup = Some(cell);
                    }
                    let resp = ui.interact(cell, Id::unique(("db-cell", &row.id, ci)), Sense::click());
                    let ro = !props::editable(&col.kind);
                    let color = if ro || local && ci > 0 { c.secondary } else { c.text };
                    cell_text(ui, cell, &row.cells[ci], 14.0, color);
                    if resp.clicked() {
                        acts.push(Action::Db(Act::Cell(row.id.clone(), ci)));
                    }
                }
            }
        });
        db.offset = out.state.clamped_offset().y;
        db.height = out.inner_rect.height();
        if revealed {
            db.reveal = false;
        }
        ui.add_space(4.0);
        let (r, resp) = ui.allocate_exact_size(vec2(width.min(LEAD_W + COL_W), ROW_H), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, 3.0, c.hover);
        }
        cell_text(ui, r, "+ New", 14.0, c.secondary);
        if resp.clicked() {
            acts.push(Action::Db(Act::NewRow));
        }
    });
    if let (Some(cell), Some(e)) = (popup, &db.edit) {
        let multi = matches!(db.cols[e.col].kind.as_str(), "multi_select" | "people");
        let chosen = items(&e.text);
        let ctx = ui.ctx().clone();
        let area = egui::Area::new(Id::unique("db-choices")).order(egui::Order::Foreground).fixed_pos(cell.left_bottom()).show(&ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(COL_W);
                if choices.is_empty() {
                    ui.label(RichText::new("No options").color(c.secondary));
                }
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    for (i, (v, label)) in choices.iter().enumerate() {
                        let on = chosen.contains(v) || (v.is_empty() && chosen.is_empty());
                        let mark = match (multi, on) {
                            (true, true) => "☑ ",
                            (true, false) => "☐ ",
                            (false, true) => "● ",
                            (false, false) => "   ",
                        };
                        if ui.selectable_label(i == e.cur, format!("{mark}{label}")).clicked() {
                            acts.push(Action::Db(Act::Pick(i)));
                        }
                    }
                });
            });
        });
        // a click outside closes it, keeping what was picked
        let click = ctx.input(|i| i.pointer.primary_clicked().then(|| i.pointer.interact_pos()).flatten());
        if click.is_some_and(|p| !area.response.rect.contains(p) && !cell.contains(p)) {
            acts.push(Action::Db(Act::Commit(e.row.clone(), e.col)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::testkit::{add_page, Win};
    use crate::worker::Event as WEvent;
    use notion_core::ops::OpKind;
    use notion_core::store::Store;
    use notion_core::sync::PullReport;
    use serde_json::json;

    const T: &str = "2025-01-01T00:00:00.000Z";
    const NONE: Modifiers = Modifiers::NONE;
    const CS: Modifiers = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };

    fn schema() -> Value {
        let o = |names: &[&str]| json!({ "options": names.iter().map(|n| json!({ "id": format!("o-{n}"), "name": n, "color": "gray" })).collect::<Vec<_>>() });
        let mut m = serde_json::Map::new();
        for (name, kind, cfg) in [
            ("Name", "title", json!({})),
            ("Notes", "rich_text", json!({})),
            ("Qty", "number", json!({ "format": "number" })),
            ("Kind", "select", o(&["A", "B"])),
            ("Tags", "multi_select", o(&["x", "y"])),
            ("State", "status", o(&["Todo", "Done"])),
            ("Due", "date", json!({})),
            ("Done", "checkbox", json!({})),
            ("Site", "url", json!({})),
            ("Mail", "email", json!({})),
            ("Tel", "phone_number", json!({})),
            ("Who", "people", json!({})),
            ("Calc", "formula", json!({ "expression": "1" })),
            ("Made", "created_time", json!({})),
        ] {
            let id = if kind == "title" { "title".to_string() } else { format!("id-{name}") };
            m.insert(name.into(), json!({ "id": id, "name": name, "type": kind, kind: cfg }));
        }
        Value::Object(m)
    }

    fn ds(id: &str, title: &str, db: &str) -> DataSource {
        serde_json::from_value(json!({ "object": "data_source", "id": id, "parent": { "type": "database_id", "database_id": db },
            "title": [{ "type": "text", "text": { "content": title }, "plain_text": title, "annotations": {} }],
            "last_edited_time": T, "properties": schema() }))
        .unwrap()
    }

    fn run(t: &str) -> Value {
        json!({ "type": "text", "text": { "content": t }, "plain_text": t, "annotations": {} })
    }

    fn row(id: &str, created: &str, name: &str, qty: Option<f64>) -> Page {
        let who = if id == "r1" { json!([{ "object": "user", "id": "u1" }]) } else { json!([]) };
        let prop = |n: &str, k: &str, v: Value| json!({ "id": format!("id-{n}"), "type": k, k: v });
        serde_json::from_value(json!({
            "object": "page", "id": id, "created_time": created, "last_edited_time": T, "in_trash": false, "url": "https://notion.so/x",
            "parent": { "type": "data_source_id", "data_source_id": "ds", "database_id": "db" },
            "properties": {
                "Name": { "id": "title", "type": "title", "title": [run(name)] },
                "Notes": prop("Notes", "rich_text", json!([])),
                "Qty": prop("Qty", "number", json!(qty)),
                "Kind": prop("Kind", "select", Value::Null),
                "Tags": prop("Tags", "multi_select", json!([])),
                "State": prop("State", "status", Value::Null),
                "Due": prop("Due", "date", Value::Null),
                "Done": prop("Done", "checkbox", json!(false)),
                "Site": prop("Site", "url", Value::Null),
                "Mail": prop("Mail", "email", Value::Null),
                "Tel": prop("Tel", "phone_number", Value::Null),
                "Who": prop("Who", "people", who),
                "Calc": prop("Calc", "formula", json!({ "type": "string", "string": "f" })),
                "Made": prop("Made", "created_time", json!(created)),
            },
        }))
        .unwrap()
    }

    fn user(id: &str, name: &str) -> User {
        User::from_value(&json!({ "object": "user", "id": id, "type": "person", "name": name })).unwrap()
    }

    fn skipped(id: &str) -> WEvent {
        WEvent::Pulled { id: id.into(), report: PullReport { skipped: true, ..Default::default() } }
    }

    /// A table of three rows, Beta (5), alpha (10), Gamma (no quantity),
    /// created in that order, open, its pull answered.
    fn win() -> Win {
        let store = Store::open_in_memory().unwrap();
        store.upsert_data_source(&ds("ds", "Tasks", "db")).unwrap();
        for p in [row("r1", T, "Beta", Some(5.0)), row("r2", "2025-01-02T00:00:00.000Z", "alpha", Some(10.0)), row("r3", "2025-01-03T00:00:00.000Z", "Gamma", None)] {
            store.upsert_page(&p).unwrap();
        }
        store.put_users(&[user("u1", "Ada Lovelace"), user("u2", "Bob")]).unwrap();
        add_page(&store, "p", None, &["a"]);
        let mut w = Win::new(store);
        w.app.apply(Action::Open("ds".into()));
        w.frames(vec![]);
        assert_eq!(w.cmds(), [Cmd::Pull { id: "ds".into(), force: false }]);
        w.answer(skipped("ds"));
        w
    }

    fn db(w: &Win) -> &Db {
        w.app.db.as_ref().expect("a table is open")
    }

    fn col(w: &Win, name: &str) -> usize {
        db(w).cols.iter().position(|c| c.name == name).unwrap()
    }

    /// The row ids in the order shown.
    fn order(w: &Win) -> Vec<String> {
        db(w).view.iter().map(|&i| db(w).rows[i].id.clone()).collect()
    }

    fn select(w: &mut Win, row: &str, name: &str) {
        let c = col(w, name);
        w.app.db.as_mut().unwrap().sel = Some((row.into(), c));
    }

    fn ops(w: &Win) -> Vec<OpKind> {
        w.app.store.queued_ops().unwrap().into_iter().map(|o| o.kind).collect()
    }

    /// The properties the one queued update of `row` writes.
    fn sent(w: &Win, row: &str) -> Value {
        let mine: Vec<Value> = ops(w)
            .into_iter()
            .filter_map(|k| match k {
                OpKind::UpdatePageProperties { page_id, properties } if page_id == row => Some(properties),
                _ => None,
            })
            .collect();
        assert_eq!(mine.len(), 1, "one merged op for {row}");
        mine[0].clone()
    }

    fn cell(w: &Win, row: &str, name: &str) -> String {
        db(w).row(row).unwrap().cells[col(w, name)].clone()
    }

    #[test]
    fn opening_a_data_source_shows_its_columns_and_rows() {
        let w = win();
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert_eq!(w.app.title, "Tasks");
        let names: Vec<_> = db(&w).cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[0], "Name", "the title comes first");
        assert_eq!(names.len(), 14);
        assert_eq!(order(&w), ["r1", "r2", "r3"], "created order");
        for t in ["Name", "Qty", "Beta", "alpha", "Gamma", "Ada Lovelace", "3 of 3 rows"] {
            assert!(w.shown.contains(t), "{t} in {}", w.shown);
        }
        assert_eq!(cell(&w, "r1", "Who"), "Ada Lovelace", "people by their names");
        assert_eq!(cell(&w, "r1", "Calc"), "f");
        assert_eq!(cell(&w, "r2", "Qty"), "10");
    }

    #[test]
    fn a_database_opens_as_its_first_data_source_and_the_others_are_tabs() {
        let mut w = win();
        w.app.store.upsert_data_source(&ds("ds2", "Archive", "db")).unwrap();
        w.app.apply(Action::Open("db".into()));
        w.frames(vec![]);
        // by title: Archive < Tasks
        assert_eq!(w.app.current.as_deref(), Some("ds2"));
        assert_eq!(db(&w).tabs.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["ds2", "ds"]);
        assert_eq!(w.cmds(), [Cmd::Pull { id: "ds2".into(), force: false }]);
        w.answer(skipped("ds2"));
        assert!(db(&w).rows.is_empty() && w.shown.contains("Archive"));
        // another tab shows its own rows
        w.app.apply(Action::Db(Act::Tab("ds".into())));
        w.frames(vec![]);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert_eq!(db(&w).rows.len(), 3);
    }

    #[test]
    fn an_uncached_database_is_pulled_and_opens_when_its_data_source_arrives() {
        let mut w = win();
        w.app.apply(Action::Open("dbx".into()));
        assert_eq!(w.cmds(), [Cmd::Pull { id: "dbx".into(), force: false }]);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        // the worker cached the database's data source
        w.app.store.upsert_data_source(&ds("dsx", "Other", "dbx")).unwrap();
        w.answer(WEvent::Pulled { id: "dbx".into(), report: PullReport { fetched: 0, updated: 0, skipped: false } });
        assert_eq!(w.app.current.as_deref(), Some("dsx"));
        assert!(w.app.db.is_some() && w.app.pending_open.is_none());
    }

    #[test]
    fn the_sidebar_and_child_database_links_open_the_table() {
        let mut w = win();
        w.app.apply(Action::Open("p".into()));
        assert!(w.app.db.is_none());
        w.app.apply(Action::Open("ds".into()));
        assert!(w.app.db.is_some());
        w.app.apply(Action::Open("p".into()));
        assert!(w.app.db.is_none() && !w.app.rows.is_empty(), "back on a page");
    }

    #[test]
    fn a_header_click_cycles_ascending_descending_none_and_is_remembered() {
        let mut w = win();
        let (name, qty) = (col(&w, "Name"), col(&w, "Qty"));
        let click = |w: &mut Win, c: usize| {
            w.app.apply(Action::Db(Act::Sort(c)));
            w.frames(vec![]);
            order(w)
        };
        // text, case-insensitive
        assert_eq!(click(&mut w, name), ["r2", "r1", "r3"]);
        assert!(w.shown.contains("Name ▲"));
        assert_eq!(click(&mut w, name), ["r3", "r1", "r2"]);
        assert!(w.shown.contains("Name ▼"));
        assert_eq!(click(&mut w, name), ["r1", "r2", "r3"]);
        // numbers by value, empty last either way
        assert_eq!(click(&mut w, qty), ["r1", "r2", "r3"]);
        assert_eq!(click(&mut w, qty), ["r2", "r1", "r3"]);
        // remembered for the session: away and back
        w.app.apply(Action::Open("p".into()));
        w.app.apply(Action::Open("ds".into()));
        assert_eq!(db(&w).sort, Some((qty, true)));
        assert_eq!(order(&w), ["r2", "r1", "r3"]);
        // a sort stays when a poll brings a new row
        w.app.store.upsert_page(&row("r4", "2025-01-04T00:00:00.000Z", "Delta", Some(7.0))).unwrap();
        w.app.refresh();
        assert_eq!(order(&w), ["r2", "r4", "r1", "r3"]);
        assert_eq!(click(&mut w, qty), ["r1", "r2", "r3", "r4"], "cleared: created order");
    }

    #[test]
    fn the_filter_matches_all_cell_text_ignoring_case_and_umlauts() {
        let mut w = win();
        w.app.store.upsert_page(&row("r4", "2025-01-04T00:00:00.000Z", "Müller", Some(1.0))).unwrap();
        w.app.refresh();
        let filter = |w: &mut Win, f: &str| {
            let d = w.app.db.as_mut().unwrap();
            d.filter = f.into();
            d.arrange();
            order(w)
        };
        assert_eq!(filter(&mut w, "ALPHA"), ["r2"]);
        assert_eq!(filter(&mut w, "mueller"), ["r4"]);
        assert_eq!(filter(&mut w, "MÜLL"), ["r4"], "folded both ways");
        assert_eq!(filter(&mut w, "lovelace"), ["r1"], "any column, people by name");
        assert_eq!(filter(&mut w, "zzz"), Vec::<String>::new());
        assert_eq!(filter(&mut w, ""), ["r1", "r2", "r3", "r4"]);
        // typed into the box: Ctrl+F focuses it, Esc leaves it
        w.press(Key::F, Modifiers::CTRL);
        w.type_text("gam");
        assert_eq!(order(&w), ["r3"]);
        assert!(w.shown.contains("1 of 4 rows"));
        w.press(Key::Escape, NONE);
        w.press(Key::ArrowDown, NONE);
        assert_eq!(db(&w).sel.as_ref().map(|s| s.0.as_str()), Some("r3"), "the keys are the table's again");
        // a selected row the filter hides is deselected
        let d = w.app.db.as_mut().unwrap();
        d.filter = "beta".into();
        d.arrange();
        assert!(d.sel.is_none());
    }

    #[test]
    fn arrow_keys_move_the_selection_and_enter_and_escape_edit() {
        let mut w = win();
        assert!(db(&w).sel.is_none());
        w.press(Key::ArrowDown, NONE);
        assert_eq!(db(&w).sel, Some(("r1".into(), 0)), "the first cell");
        w.press(Key::ArrowDown, NONE);
        w.press(Key::ArrowRight, NONE);
        assert_eq!(db(&w).sel, Some(("r2".into(), 1)));
        for _ in 0..30 {
            w.press(Key::ArrowRight, NONE);
        }
        assert_eq!(db(&w).sel.as_ref().unwrap().1, 13, "clamped at the last column");
        for _ in 0..5 {
            w.press(Key::ArrowDown, NONE);
        }
        assert_eq!(db(&w).sel.as_ref().unwrap().0, "r3");
        w.press(Key::ArrowUp, NONE);
        w.press(Key::ArrowLeft, NONE);
        assert_eq!(db(&w).sel, Some(("r2".into(), 12)));
        // Enter edits, Esc cancels and writes nothing
        select(&mut w, "r1", "Name");
        w.press(Key::Enter, NONE);
        assert_eq!(db(&w).edit.as_ref().map(|e| e.text.as_str()), Some("Beta"));
        w.frames(vec![]);
        w.type_text("X");
        assert_eq!(db(&w).edit.as_ref().unwrap().text, "BetaX", "typing reaches the box");
        w.press(Key::Escape, NONE);
        assert!(db(&w).edit.is_none());
        assert_eq!(cell(&w, "r1", "Name"), "Beta");
        assert_eq!(w.queued(), 0);
        // the arrows leave a text box's own cursor alone
        select(&mut w, "r1", "Name");
        w.press(Key::Enter, NONE);
        w.press(Key::ArrowDown, NONE);
        assert_eq!(db(&w).sel, Some(("r1".into(), 0)));
    }

    fn edit(w: &mut Win, row: &str, name: &str, text: &str) {
        let c = col(w, name);
        w.app.apply(Action::Db(Act::Cell(row.into(), c)));
        w.frames(vec![]);
        w.app.db.as_mut().unwrap().edit.as_mut().expect("editing").text = text.into();
        w.press(Key::Enter, NONE);
        assert!(db(w).edit.is_none(), "{name}: {:?}", w.app.last_error);
    }

    #[test]
    fn each_editor_commits_one_property_body_merged_into_one_op() {
        let mut w = win();
        for (name, text, body) in [
            ("Name", "Hello", json!({ "title": [{ "type": "text", "text": { "content": "Hello" } }] })),
            ("Notes", "n", json!({ "rich_text": [{ "type": "text", "text": { "content": "n" } }] })),
            ("Qty", "7,5", json!({ "number": 7.5 })),
            ("Due", "2025-02-03/2025-02-05", json!({ "date": { "start": "2025-02-03", "end": "2025-02-05", "time_zone": null } })),
            ("Site", "https://a.at", json!({ "url": "https://a.at" })),
            ("Mail", "a@b.at", json!({ "email": "a@b.at" })),
            ("Tel", "+43 1 234", json!({ "phone_number": "+43 1 234" })),
        ] {
            edit(&mut w, "r1", name, text);
            assert_eq!(sent(&w, "r1")[name], body, "{name}");
        }
        assert_eq!(cell(&w, "r1", "Qty"), "7.5");
        assert_eq!(cell(&w, "r1", "Due"), "2025-02-03 → 2025-02-05");
        // a checkbox: Space toggles, no editor
        select(&mut w, "r1", "Done");
        w.press(Key::Space, NONE);
        assert_eq!(sent(&w, "r1")["Done"], json!({ "checkbox": true }));
        assert_eq!(cell(&w, "r1", "Done"), "☑");
        assert!(db(&w).edit.is_none());
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["Done"], json!({ "checkbox": false }), "Enter toggles too");
        // select: the popup, Down, Enter
        select(&mut w, "r1", "Kind");
        w.press(Key::Enter, NONE);
        w.press(Key::ArrowDown, NONE);
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["Kind"], json!({ "select": { "name": "A" } }));
        // clearing it: the "none" choice
        select(&mut w, "r1", "Kind");
        w.press(Key::Enter, NONE);
        w.press(Key::ArrowUp, NONE);
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["Kind"], json!({ "select": null }));
        // status
        select(&mut w, "r1", "State");
        w.press(Key::Enter, NONE);
        w.press(Key::ArrowDown, NONE);
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["State"], json!({ "status": { "name": "Done" } }));
        // multi-select: Space toggles at the highlight
        select(&mut w, "r1", "Tags");
        w.press(Key::Enter, NONE);
        w.press(Key::Space, NONE);
        w.press(Key::ArrowDown, NONE);
        w.press(Key::Space, NONE);
        assert!(db(&w).edit.is_some(), "still open");
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["Tags"], json!({ "multi_select": [{ "name": "x" }, { "name": "y" }] }));
        // people: from the cached users; r1 has Ada, swap her for Bob
        select(&mut w, "r1", "Who");
        w.press(Key::Enter, NONE);
        w.press(Key::Space, NONE);
        w.press(Key::ArrowDown, NONE);
        w.press(Key::Space, NONE);
        w.press(Key::Enter, NONE);
        assert_eq!(sent(&w, "r1")["Who"], json!({ "people": [{ "object": "user", "id": "u2" }] }));
        assert_eq!(cell(&w, "r1", "Who"), "Bob");
        // every one of them: one op, nothing else
        assert_eq!(w.queued(), 1);
        // the popup is drawn with its choices
        select(&mut w, "r2", "Tags");
        w.press(Key::Enter, NONE);
        assert!(w.shown.contains("☐ x") && w.shown.contains("☐ y"), "{}", w.shown);
    }

    #[test]
    fn an_unchanged_edit_writes_nothing_and_a_clicked_choice_is_written() {
        let mut w = win();
        edit(&mut w, "r1", "Name", "Beta");
        assert_eq!(w.queued(), 0);
        // clicks: a cell opens its editor, a choice writes at once
        let kind = col(&w, "Kind");
        w.app.apply(Action::Db(Act::Cell("r2".into(), kind)));
        w.app.apply(Action::Db(Act::Pick(2)));
        assert!(db(&w).edit.is_none());
        assert_eq!(sent(&w, "r2")["Kind"], json!({ "select": { "name": "B" } }));
        // a checkbox click toggles
        let done = col(&w, "Done");
        w.app.apply(Action::Db(Act::Cell("r2".into(), done)));
        assert_eq!(sent(&w, "r2")["Done"], json!({ "checkbox": true }));
        // another cell's click commits the open edit first
        let q = col(&w, "Qty");
        w.app.apply(Action::Db(Act::Cell("r3".into(), q)));
        w.app.db.as_mut().unwrap().edit.as_mut().unwrap().text = "1".into();
        let n = col(&w, "Notes");
        w.app.apply(Action::Db(Act::Cell("r3".into(), n)));
        assert_eq!(sent(&w, "r3")["Qty"], json!({ "number": 1 }));
        assert_eq!(db(&w).edit.as_ref().map(|e| e.col), Some(n));
    }

    #[test]
    fn a_stale_commit_is_ignored_and_a_remote_change_is_not_overwritten() {
        let mut w = win();
        let (n, q) = (col(&w, "Notes"), col(&w, "Qty"));
        w.app.apply(Action::Db(Act::Cell("r1".into(), n)));
        w.app.apply(Action::Db(Act::Commit("r1".into(), q)));
        assert_eq!(db(&w).edit.as_ref().map(|e| e.col), Some(n), "another cell's commit is ignored");
        // a pull changes the cell while it is typed in
        w.app.db.as_mut().unwrap().edit.as_mut().unwrap().text = "mine".into();
        let mut p = w.app.store.get_page("r1").unwrap().unwrap();
        p.properties.insert("Notes".into(), serde_json::from_value(json!({ "id": "id-Notes", "type": "rich_text", "rich_text": [run("theirs")] })).unwrap());
        w.app.store.upsert_page(&p).unwrap();
        w.app.refresh();
        w.app.apply(Action::Db(Act::Commit("r1".into(), n)));
        assert!(db(&w).edit.is_some() && w.queued() == 0);
        assert!(w.app.last_error.as_deref().is_some_and(|e| e.contains("changed in Notion")));
        assert_eq!(cell(&w, "r1", "Notes"), "theirs");
    }

    #[test]
    fn undoing_a_cell_edit_navigates_back_to_the_table() {
        let mut w = win();
        edit(&mut w, "r1", "Name", "Zed");
        w.app.apply(Action::Open("r1".into()));
        w.frames(vec![]);
        assert!(w.app.db.is_none());
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert_eq!(cell(&w, "r1", "Name"), "Beta");
    }

    #[test]
    fn a_dropped_column_clears_the_selection() {
        let mut w = win();
        let last = db(&w).cols.len() - 1;
        w.app.db.as_mut().unwrap().sel = Some(("r1".into(), last));
        let mut d = ds("ds", "Tasks", "db");
        d.properties.as_object_mut().unwrap().remove("Made");
        d.raw["properties"] = d.properties.clone();
        w.app.store.upsert_data_source(&d).unwrap();
        w.app.load_db("ds").unwrap();
        assert!(db(&w).sel.is_none());
    }

    #[test]
    fn read_only_types_refuse_to_edit() {
        let mut w = win();
        for name in ["Calc", "Made"] {
            select(&mut w, "r1", name);
            w.app.info = None;
            w.press(Key::Enter, NONE);
            assert!(db(&w).edit.is_none(), "{name}");
            assert!(w.app.info.as_deref().unwrap().starts_with("read-only"), "{name}");
            let c = col(&w, name);
            w.app.apply(Action::Db(Act::Cell("r2".into(), c)));
            assert!(db(&w).edit.is_none());
        }
        w.press(Key::Space, NONE);
        assert_eq!(w.queued(), 0);
        assert!(!props::editable("rollup") && !props::editable("relation") && !props::editable("files"));
    }

    #[test]
    fn a_refused_value_keeps_the_editor_and_a_second_leave_discards_it() {
        let mut w = win();
        select(&mut w, "r1", "Qty");
        w.press(Key::Enter, NONE);
        w.app.db.as_mut().unwrap().edit.as_mut().unwrap().text = "abc".into();
        w.press(Key::Enter, NONE);
        assert!(db(&w).edit.is_some(), "kept");
        assert!(w.app.last_error.as_deref().unwrap().contains("number"), "{:?}", w.app.last_error);
        assert_eq!(w.queued(), 0);
        // leaving (a tab, a page) once is refused, twice discards
        w.app.apply(Action::Open("p".into()));
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        w.app.apply(Action::Open("p".into()));
        assert_eq!(w.app.current.as_deref(), Some("p"));
        assert_eq!(w.queued(), 0);
        // a date and a url are checked too
        w.app.apply(Action::Open("ds".into()));
        w.frames(vec![]);
        for (name, bad) in [("Due", "2025-13-01"), ("Site", "ftp://x"), ("Mail", "nope")] {
            select(&mut w, "r1", name);
            w.press(Key::Enter, NONE);
            w.app.db.as_mut().unwrap().edit.as_mut().unwrap().text = bad.into();
            w.press(Key::Enter, NONE);
            assert!(db(&w).edit.is_some(), "{name}");
            w.press(Key::Escape, NONE);
        }
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn a_pushing_table_is_not_edited() {
        let mut w = win();
        w.app.send(Cmd::Push);
        select(&mut w, "r1", "Qty");
        w.press(Key::Enter, NONE);
        assert!(db(&w).edit.is_none());
        assert!(w.app.last_error.as_deref().unwrap().contains("paused"));
        w.press(Key::Enter, Modifiers::CTRL);
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn a_new_row_is_created_in_the_data_source_and_undo_removes_it() {
        let mut w = win();
        w.press(Key::Enter, Modifiers::CTRL);
        assert_eq!(db(&w).rows.len(), 4);
        let id = db(&w).sel.as_ref().unwrap().0.clone();
        assert!(id.starts_with("local-"));
        assert_eq!(order(&w).last(), Some(&id));
        assert_eq!(db(&w).edit.as_ref().map(|e| (e.row.as_str(), e.col)), Some((id.as_str(), 0)), "its title is in edit mode");
        match &ops(&w)[..] {
            [OpKind::CreatePage { parent_id, local_id, body }] => {
                assert_eq!((parent_id.as_str(), local_id.as_str()), ("ds", id.as_str()));
                assert_eq!(body["parent"], json!({ "type": "data_source_id", "data_source_id": "ds" }));
            }
            o => panic!("{o:?}"),
        }
        w.frames(vec![]);
        w.type_text("Fresh");
        w.press(Key::Enter, NONE);
        assert_eq!(cell(&w, &id, "Name"), "Fresh");
        edit(&mut w, &id, "Qty", "3");
        match &ops(&w)[..] {
            [OpKind::CreatePage { body, .. }] => {
                assert_eq!(body["properties"]["Name"], json!({ "title": [{ "type": "text", "text": { "content": "Fresh" } }] }));
                assert_eq!(body["properties"]["Qty"], json!({ "number": 3 }), "the create carries every edit");
            }
            o => panic!("{o:?}"),
        }
        // undo: the quantity, the title, then the row itself
        for _ in 0..3 {
            w.press(Key::Z, Modifiers::CTRL);
        }
        assert_eq!(db(&w).rows.len(), 3);
        assert_eq!(w.queued(), 0);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        w.press(Key::Z, CS);
        assert_eq!(db(&w).rows.len(), 4, "redo brings it back");
        assert_eq!(w.queued(), 1);
        // "+ New" does the same
        w.app.apply(Action::Db(Act::NewRow));
        assert_eq!(db(&w).rows.len(), 5);
        // the filter is cleared so the new row shows
        let d = w.app.db.as_mut().unwrap();
        d.edit = None;
        d.filter = "zzz".into();
        d.arrange();
        w.press(Key::N, Modifiers::CTRL);
        assert_eq!(order(&w).len(), 6, "Ctrl+N too, and the filter is cleared");
    }

    #[test]
    fn a_pushed_row_keeps_its_selection_under_the_real_id() {
        let mut w = win();
        w.app.apply(Action::Db(Act::NewRow));
        w.press(Key::Escape, NONE);
        let local = db(&w).sel.as_ref().unwrap().0.clone();
        w.app.store.rename_page(&local, "real1").unwrap();
        w.app.store.map_id(&local, "real1").unwrap();
        w.answer(WEvent::Pushed(Default::default()));
        assert_eq!(db(&w).sel, Some(("real1".into(), 0)));
        assert_eq!(w.cmds().last(), Some(&Cmd::Pull { id: "ds".into(), force: true }), "re-pulled in full");
        assert!(db(&w).row("real1").is_some());
    }

    #[test]
    fn a_row_is_trashed_after_a_confirm_and_undo_brings_it_back_in_the_table() {
        let mut w = win();
        select(&mut w, "r2", "Name");
        w.press(Key::Backspace, CS);
        assert!(matches!(&w.app.dlg, Some(Dlg::Trash(id, t)) if id == "r2" && t == "alpha"));
        w.press(Key::Escape, NONE);
        assert!(w.app.dlg.is_none() && order(&w).len() == 3 && w.queued() == 0, "Esc says no");
        w.press(Key::Backspace, CS);
        w.press(Key::Enter, NONE);
        assert_eq!(order(&w), ["r1", "r3"]);
        assert!(matches!(&ops(&w)[..], [OpKind::UpdatePage { page_id, body }] if page_id == "r2" && body == &json!({ "in_trash": true })));
        assert_eq!(w.app.current.as_deref(), Some("ds"), "the table stays open");
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(order(&w), ["r1", "r2", "r3"]);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert_eq!(w.queued(), 0);
        // the trash button on a row asks too; a new row can't be trashed yet
        w.app.apply(Action::Db(Act::Trash("r3".into())));
        assert!(matches!(&w.app.dlg, Some(Dlg::Trash(id, _)) if id == "r3"));
        w.press(Key::Escape, NONE);
        w.app.apply(Action::Db(Act::NewRow));
        w.press(Key::Escape, NONE);
        w.press(Key::Backspace, CS);
        assert!(w.app.dlg.is_none());
        assert!(w.app.last_error.as_deref().unwrap().contains("isn't pushed"));
    }

    #[test]
    fn a_row_opens_as_a_page_and_its_parent_is_the_table() {
        let mut w = win();
        select(&mut w, "r2", "Name");
        w.press(Key::O, CS);
        assert_eq!(w.app.current.as_deref(), Some("r2"));
        assert!(w.app.db.is_none());
        assert_eq!(w.app.title, "alpha");
        w.press(Key::U, CS);
        assert_eq!(w.app.current.as_deref(), Some("ds"));
        assert!(w.app.db.is_some());
        // the ↗ button
        w.app.apply(Action::Db(Act::OpenRow("r3".into())));
        assert_eq!(w.app.current.as_deref(), Some("r3"));
        // a row's page edit and a cell edit share one op
        w.app.apply(Action::EditTitle);
        w.frames(vec![]);
        w.type_text("!");
        w.press(Key::Enter, NONE);
        w.app.apply(Action::Open("ds".into()));
        edit(&mut w, "r3", "Qty", "4");
        let s = sent(&w, "r3");
        assert!(s["Name"].is_object() && s["Qty"] == json!({ "number": 4 }), "{s}");
        // commands that make no sense on a table say so
        w.app.info = None;
        w.press(Key::I, CS);
        assert!(w.app.info.as_deref().unwrap().contains("not available"));
        assert!(w.app.dlg.is_none());
    }

    #[test]
    fn the_open_table_is_polled_and_a_poll_updates_cells_keeping_filter_and_selection() {
        let mut w = win();
        let now = Instant::now();
        w.app.poll_at(now, false);
        w.app.poll_at(now + Duration::from_secs(15), false);
        assert!(matches!(&w.cmds()[..], [Cmd::Poll { id, .. }] if id == "ds"));
        // a row changed in Notion
        let mut r = row("r2", T, "alpha", Some(99.0));
        r.raw["properties"]["Qty"]["number"] = json!(99);
        w.app.store.upsert_page(&serde_json::from_value(r.raw.clone()).unwrap()).unwrap();
        let d = w.app.db.as_mut().unwrap();
        d.filter = "alp".into();
        d.arrange();
        select(&mut w, "r2", "Qty");
        w.answer(WEvent::Pulled { id: "ds".into(), report: PullReport { fetched: 1, updated: 1, skipped: false } });
        assert_eq!(cell(&w, "r2", "Qty"), "99");
        assert_eq!(db(&w).filter, "alp");
        assert_eq!(db(&w).sel, Some(("r2".into(), col(&w, "Qty"))));
        assert!(w.app.info.is_none() || !w.app.info.as_deref().unwrap().contains("pulled"), "quiet");
        // a poll in the middle of typing leaves the box alone
        w.press(Key::Enter, NONE);
        w.frames(vec![]);
        w.type_text("1");
        w.app.poll_at(now + Duration::from_secs(60), false);
        assert_eq!(db(&w).edit.as_ref().unwrap().text, "991");
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn opening_pulls_only_when_the_table_was_not_pulled_lately() {
        let mut w = win();
        let pulls = |w: &mut Win| w.cmds().into_iter().filter(|c| matches!(c, Cmd::Pull { id, .. } if id == "ds")).count();
        w.app.full_at.insert("ds".into(), Instant::now());
        w.app.apply(Action::Open("p".into()));
        w.app.apply(Action::Open("ds".into()));
        assert_eq!(pulls(&mut w), 0, "pulled a moment ago");
        w.app.full_at.insert("ds".into(), Instant::now() - Duration::from_secs(120));
        w.app.apply(Action::Open("p".into()));
        w.app.apply(Action::Open("ds".into()));
        assert_eq!(pulls(&mut w), 1);
        // not twice while one is out
        w.app.apply(Action::Open("p".into()));
        w.app.apply(Action::Open("ds".into()));
        assert_eq!(pulls(&mut w), 0);
    }

    #[test]
    fn a_tall_and_wide_table_draws_and_scrolls_the_selection_into_view() {
        let mut w = win();
        for i in 0..80 {
            w.app.store.upsert_page(&row(&format!("x{i:02}"), "2025-02-01T00:00:00.000Z", &format!("Row {i}"), Some(i as f64))).unwrap();
        }
        w.app.refresh();
        w.frames(vec![]);
        assert!(w.shown.contains("Row 0") && !w.shown.contains("Row 70"), "only the visible rows are drawn");
        for _ in 0..60 {
            w.press(Key::ArrowDown, NONE);
        }
        w.frames(vec![]);
        w.frames(vec![]);
        let sel = db(&w).sel.as_ref().unwrap().0.clone();
        assert!(w.shown.contains(&db(&w).row(&sel).unwrap().cells[0]), "the selected row is drawn: {sel}");
    }
}
