//! App state, the page body and the focused-row editor. The shell around it
//! (sync, tabs, switcher, session, frame) is in `shell.rs`, drawing of the
//! chrome in `view/`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc::Receiver, Arc};
use std::time::Instant;

use eframe::egui::{
    self,
    text::{CCursor, CCursorRange, LayoutJob},
    text_edit::TextEditState,
    Color32, Frame, Galley, Id, Key, Label, Margin, Modifiers, RichText, Sense, TextBuffer, Ui,
};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, BlockKind, TargetKind, TextBlock};
use notion_core::sync::{self, ChangeOutcome};
use notion_core::store::{PageEntry, Store};
use tokio::sync::mpsc::UnboundedSender;

use crate::edit::EditKey;
use crate::keys::Keymap;
use crate::links::{self, Link};
use crate::menu::{menu_step, Outcome};
use crate::outline::{self, Marker, Row};
use crate::problems::Problem;
use crate::session;
use crate::slash::{self, Menu};
use crate::switcher::Switcher;
use crate::tabs::Tab;
use crate::undo::Step;
use crate::worker::{Cmd, Event};
use crate::{md, theme};

/// Where the cursor goes when a row's editor opens.
#[derive(Clone, Copy)]
pub(crate) enum Grab {
    Start,
    End,
    /// A char index.
    At(usize),
}

impl Grab {
    /// The char index in `text`.
    pub(crate) fn index(self, text: &str) -> usize {
        let end = text.chars().count();
        match self {
            Grab::Start => 0,
            Grab::End => end,
            Grab::At(n) => n.min(end),
        }
    }
}

/// The row being edited: its text lives here until Esc, a move or a sync
/// command commits it to the store (`App::flush`).
pub(crate) struct Focus {
    pub(crate) idx: usize,
    /// Survives a rebuild of `rows`, which can move `idx`.
    pub(crate) block_id: String,
    /// The text as the editor started (or was last written): an unchanged
    /// commit writes nothing.
    pub(crate) original: String,
    pub(crate) text: String,
    /// Code and equation rows are edited as raw text, not Markdown.
    pub(crate) code: bool,
    pub(crate) menu: Option<Menu>,
    /// A table cell (table row id, column): `block_id` is then the table's
    /// and `idx` its row.
    pub(crate) cell: Option<(String, usize)>,
    galley: Option<Arc<Galley>>,
    /// Set until the editor has taken keyboard focus and its cursor.
    grab: Option<Grab>,
    pub(crate) prev_len: usize,
    scroll: bool,
    /// The last write of this text failed: a second Esc leaves without it.
    pub(crate) failed: bool,
}

impl Focus {
    /// An editor on row `idx` (a table for a cell); the cursor goes to `grab`.
    pub(crate) fn new(
        idx: usize,
        block_id: String,
        original: String,
        text: String,
        code: bool,
        grab: Grab,
    ) -> Focus {
        Focus {
            idx,
            block_id,
            original,
            prev_len: text.len(),
            text,
            code,
            menu: None,
            cell: None,
            galley: None,
            grab: Some(grab),
            scroll: true,
            failed: false,
        }
    }

    pub(crate) fn take_grab(&mut self) -> Option<Grab> {
        self.grab.take()
    }
}

/// The text edit's title, while it is being edited in place.
pub(crate) struct TitleEdit {
    pub(crate) text: String,
    /// The text as the edit started (or was last written).
    pub(crate) original: String,
    /// Set until the edit has taken keyboard focus and its cursor.
    pub(crate) grab: bool,
}

pub(crate) enum Action {
    Edit(usize, Grab),
    Blur,
    /// Enter, Backspace, Tab ... on the edited row (see `App::on_edit_key`).
    Key(EditKey),
    /// A click on a link; `new_tab` for Ctrl+click and middle-click.
    Link { link: Link, new_tab: bool },
    EditTitle,
    TitleCommit,
    /// A table cell: (table row id, column).
    EditCell(String, usize),
    /// A page link, sidebar row or breadcrumb: shown, or pulled if uncached.
    Open(String),
    Toggle(usize),
    Check(usize),
    /// Expand or collapse a sidebar page.
    ToggleTree(String),
    Switch(usize),
    CloseTab(usize),
    NewTab,
    /// Opens the Ctrl+K switcher.
    Search,
    ToggleProblems,
    OpenProblem(i64),
    Discard(i64),
    /// A slash pick or Markdown shortcut on the edited row: `text` is the
    /// editor's text without the trigger, `typed` as it was before, `at` the
    /// cursor (a char index into `text`).
    Convert {
        kind: TargetKind,
        text: String,
        typed: String,
        at: usize,
    },
}

/// Opens a web link outside the app (injectable for tests).
pub(crate) type Opener = Box<dyn Fn(&str) -> Result<(), String>>;

pub struct App {
    pub(crate) store: Store,
    /// None once the worker is shut down.
    pub(crate) cmds: Option<UnboundedSender<Cmd>>,
    pub(crate) events: Receiver<Event>,
    pub(crate) worker: Option<std::thread::JoinHandle<()>>,
    /// Sidebar pages; [`crate::sidebar`] nests them.
    pub(crate) tree: Vec<PageEntry>,
    /// Expanded sidebar pages.
    pub(crate) tree_open: HashSet<String>,
    pub(crate) sidebar: bool,
    pub(crate) current: Option<String>,
    pub(crate) title: String,
    pub(crate) rows: Vec<Row>,
    pub(crate) shown: Vec<bool>,
    /// Expanded toggles; every other toggle is collapsed (Notion's default).
    pub(crate) open: HashSet<String>,
    /// Layout of each read-only row, built once (not per frame).
    pub(crate) jobs: HashMap<usize, LayoutJob>,
    /// Table cells by (row, table row, column).
    pub(crate) table_jobs: HashMap<(usize, usize, usize), LayoutJob>,
    pub(crate) focus: Option<Focus>,
    pub(crate) keys: Keymap,
    pub(crate) title_edit: Option<TitleEdit>,
    /// The title has no mentions or equations (plain text round-trips).
    pub(crate) title_editable: bool,
    /// Local edits to undo / redo, newest last.
    pub(crate) undo: Vec<Step>,
    pub(crate) redo: Vec<Step>,
    /// Inside [`App::step`]: nested steps join the outer one.
    pub(crate) stepping: bool,
    /// Opens a web link (`xdg-open`; tests swap it out).
    pub(crate) opener: Opener,
    /// The page's scroll offset, read back every frame.
    pub(crate) scroll: f32,
    /// An offset to apply on the next frame (tab switch, new page), once.
    pub(crate) pending_scroll: Option<f32>,
    /// Open tabs; `tabs[active]` is stale, `current`/`open`/`scroll` are its
    /// live state.
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// Recently opened pages, most recent first.
    pub(crate) recent: Vec<String>,
    pub(crate) window: (f32, f32),
    /// Where session-egui.json is written on exit; None (tests) writes nothing.
    pub(crate) session_dir: Option<PathBuf>,
    /// One entry per command sent and not yet answered (the worker answers
    /// each with one event, in order); true = Push.
    pub(crate) sent: VecDeque<bool>,
    /// Page waiting for its pull, to be opened when it arrives.
    pub(crate) pending_open: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) info: Option<String>,
    /// queued, conflict, failed
    pub(crate) counts: [usize; 3],
    /// Resolved targets of queued ops, and of conflict/failed ones.
    pub(crate) queued_ids: HashSet<String>,
    pub(crate) problem_ids: HashSet<String>,
    pub(crate) problems: Vec<Problem>,
    pub(crate) problems_open: bool,
    pub(crate) switcher: Option<Switcher>,
    pub(crate) last_auto: Option<Instant>,
    /// The command in flight is an auto-pull.
    pub(crate) auto_pulling: bool,
    pub(crate) was_focused: bool,
    pub(crate) next_tick: Instant,
    pub(crate) worker_gone: bool,
    /// The window title last sent.
    pub(crate) shown_title: String,
    pub(crate) keylog: bool,
}

/// The text edit's id for a block; it must not change while the row moves.
pub(crate) fn edit_id(block_id: &str) -> Id {
    Id::unique(("edit", block_id))
}

/// (font size, bold) for a row's text.
fn look(m: &Marker) -> (f32, bool) {
    match m {
        Marker::Heading(1) => (30.0, true),
        Marker::Heading(2) => (24.0, true),
        Marker::Heading(_) => (19.0, true),
        Marker::Code(_) => (14.0, false),
        _ => (16.0, false),
    }
}

/// Blur first, then the rest in order: a Blur from the edited row must not be
/// lost to (or undo) another row's action in the same frame.
/// A row index taken from the old outline is stale once a key action
/// (create, delete, move) ran, so those after it are dropped.
pub(crate) fn blur_first(acts: &mut Vec<Action>) {
    acts.sort_by_key(|a| !matches!(a, Action::Blur | Action::TitleCommit));
    if let Some(k) = acts.iter().position(|a| matches!(a, Action::Key(_))) {
        let mut i = 0;
        acts.retain(|a| {
            i += 1;
            i <= k + 1 || !matches!(a, Action::Edit(..) | Action::Check(_) | Action::Toggle(_))
        });
    }
}

/// Some(open in a new tab?) when `r` was clicked: Ctrl+click and a middle
/// click open a new tab.
fn clicked_new_tab(ui: &Ui, r: &egui::Response) -> Option<bool> {
    let mid = r.middle_clicked();
    (r.clicked() || mid).then(|| mid || ui.input(|i| i.modifiers.command))
}

/// (line, byte column in that line) of the cursor at char index `at`.
fn line_col(text: &str, at: usize) -> (usize, usize) {
    let b = text.char_indices().nth(at).map_or(text.len(), |(b, _)| b);
    let before = &text[..b];
    (
        before.matches('\n').count(),
        b - before.rfind('\n').map_or(0, |n| n + 1),
    )
}

/// Byte offset of (line, col).
fn byte_at(text: &str, line: usize, col: usize) -> usize {
    text.split('\n')
        .take(line)
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + col
}

pub(crate) fn set_cursor(ctx: &egui::Context, id: Id, mut st: TextEditState, at: usize) {
    st.cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
    st.store(ctx, id);
}

/// The nearest shown, editable row above (-1) or below (+1) `idx`.
pub(crate) fn neighbour(rows: &[Row], shown: &[bool], idx: usize, dir: isize) -> Option<usize> {
    let mut i = idx as isize + dir;
    while (0..rows.len() as isize).contains(&i) {
        if shown[i as usize] && rows[i as usize].editable {
            return Some(i as usize);
        }
        i += dir;
    }
    None
}


impl App {
    pub fn new(
        store: Store,
        cmds: UnboundedSender<Cmd>,
        events: Receiver<Event>,
        keylog: bool,
    ) -> Self {
        let mut app = App {
            store,
            cmds: Some(cmds),
            events,
            worker: None,
            tree: Vec::new(),
            tree_open: HashSet::new(),
            sidebar: true,
            current: None,
            title: String::new(),
            rows: Vec::new(),
            shown: Vec::new(),
            open: HashSet::new(),
            jobs: HashMap::new(),
            table_jobs: HashMap::new(),
            focus: None,
            keys: Keymap::default(),
            title_edit: None,
            title_editable: false,
            undo: Vec::new(),
            redo: Vec::new(),
            stepping: false,
            opener: Box::new(links::xdg_open),
            scroll: 0.0,
            pending_scroll: None,
            tabs: vec![Tab::default()],
            active: 0,
            recent: Vec::new(),
            window: session::Session::default().window,
            session_dir: None,
            sent: VecDeque::new(),
            pending_open: None,
            last_error: None,
            info: None,
            counts: [0; 3],
            queued_ids: HashSet::new(),
            problem_ids: HashSet::new(),
            problems: Vec::new(),
            problems_open: false,
            switcher: None,
            last_auto: None,
            auto_pulling: false,
            was_focused: false,
            next_tick: Instant::now() + crate::shell::AUTO_PULL_EVERY,
            worker_gone: false,
            shown_title: String::new(),
            keylog,
        };
        app.refresh();
        app
    }

    /// Starts editing row `i`. Not while a push runs: the worker may already
    /// have sent the queued ops an edit would rewrite.
    pub(crate) fn start_edit(&mut self, i: usize, grab: Grab) {
        if i >= self.rows.len() || self.pushing() || !self.commit() {
            return;
        }
        self.open_editor(i, grab, None);
    }

    /// The editor on row `i`, with `typed` in it if the text differs from the
    /// row's (a conversion keeps what was typed).
    pub(crate) fn open_editor(&mut self, i: usize, grab: Grab, typed: Option<String>) {
        let r = &self.rows[i];
        let code = r.marker == Marker::Equation || matches!(r.marker, Marker::Code(_));
        let original = if code {
            plain_text(&r.rich_text)
        } else {
            to_markdown(&r.rich_text)
        };
        let text = typed.unwrap_or_else(|| original.clone());
        self.focus = Some(Focus::new(i, r.block_id.clone(), original, text, code, grab));
    }

    /// A to-do's box: queues the change. Locked rows and pushes refuse.
    pub(crate) fn check(&mut self, i: usize) {
        let r = &self.rows[i];
        let Marker::Todo(checked) = r.marker else { return };
        if r.locked || self.pushing() || !self.flush() {
            return;
        }
        let res = self
            .store
            .resolve_id(&self.rows[i].block_id)
            .and_then(|id| sync::local_set_checked(&self.store, &id, !checked));
        if let Err(e) = res {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
    }

    /// The edit is kept as typed, the trigger and all.
    fn refuse(&mut self, typed: String, err: String) {
        self.last_error = Some(err);
        if let Some(f) = &mut self.focus {
            f.prev_len = typed.len();
            f.text = typed;
            f.menu = None;
            f.grab = Some(Grab::End);
        }
    }

    /// Converts the edited block through core (a new id for a synced block)
    /// and goes on editing it with `text`. Mirrors notion-gui's `convert`.
    fn convert(&mut self, kind: TargetKind, text: String, typed: String, at: usize) {
        if self.pushing() {
            return self.refuse(typed, "editing is paused while a push runs".into());
        }
        let Some(f) = &mut self.focus else { return };
        f.text = text.clone();
        let (idx, old) = (f.idx, f.block_id.clone());
        let row = self.rows[idx].clone();
        // a divider after the text, which stays
        if kind == TargetKind::Divider && !text.trim().is_empty() {
            if !self.flush() {
                return;
            }
            match sync::local_create_block(&self.store, &row.parent, Some(&old), BlockKind::Divider)
            {
                Ok(_) => self.refresh(),
                Err(e) => self.last_error = Some(e.to_string()),
            }
            return;
        }
        // code is raw text: no Markdown marks
        // ponytail: the cursor goes to the start, not mapped through the marks
        let (text, at) = if kind == TargetKind::Code {
            (plain_text(&from_markdown(&text)), 0)
        } else {
            (text, at)
        };
        let new = match self
            .store
            .resolve_id(&old)
            .and_then(|id| sync::local_change_type(&self.store, &id, kind).map(|o| (id, o)))
        {
            Ok((_, ChangeOutcome::Replaced(new))) => new,
            Ok((id, ChangeOutcome::Kept)) => id,
            Err(e) => return self.refuse(typed, e.to_string()),
        };
        // the old id may be gone: refresh must not discard the editor
        self.focus = None;
        self.refresh();
        let Some(mut i) = self.rows.iter().position(|r| r.block_id == new) else {
            return;
        };
        if !self.rows[i].editable {
            // a divider has no text: go on in a new paragraph below it
            let para = BlockKind::Paragraph(TextBlock {
                rich_text: Vec::new(),
                color: "default".into(),
            });
            match sync::local_create_block(&self.store, &row.parent, Some(&new), para) {
                Ok(p) => {
                    self.refresh();
                    match self.rows.iter().position(|r| r.block_id == p) {
                        Some(j) => i = j,
                        None => return,
                    }
                    self.open_editor(i, Grab::End, None);
                }
                Err(e) => self.last_error = Some(e.to_string()),
            }
            return;
        }
        self.open_editor(i, Grab::At(at), Some(text));
        self.flush();
    }

    pub(crate) fn apply(&mut self, act: Action) {
        match act {
            Action::Edit(i, _) | Action::Toggle(i) | Action::Check(i) if i >= self.rows.len() => {}
            Action::Key(k) => self.on_edit_key(k),
            Action::Link { link, new_tab } => self.open_link(link, new_tab),
            Action::EditTitle => self.start_title(),
            Action::TitleCommit => {
                self.commit();
            }
            Action::EditCell(row, col) => self.start_cell(&row, col),
            Action::Edit(i, grab) => self.start_edit(i, grab),
            Action::Blur => {
                // a write that keeps failing must not trap the editor: the
                // second try leaves without the text
                if !self.commit() {
                    if let Some(f) = &mut self.focus {
                        if f.failed {
                            self.focus = None;
                            self.last_error = Some("edit discarded: it could not be saved".into());
                        } else {
                            f.failed = true;
                        }
                    }
                }
            }
            Action::Open(id) => self.open_or_pull(&id),
            Action::Toggle(i) => {
                let id = self.rows[i].block_id.clone();
                if !self.open.remove(&id) {
                    self.open.insert(id);
                }
                self.shown = outline::shown(&self.rows, &self.open);
            }
            Action::Check(i) => self.step(|s| s.check(i)),
            Action::Convert {
                kind,
                text,
                typed,
                at,
            } => self.step(|s| s.convert(kind, text, typed, at)),
            Action::ToggleTree(id) => {
                if !self.tree_open.remove(&id) {
                    self.tree_open.insert(id);
                }
            }
            Action::Switch(i) => self.switch_tab(i),
            Action::CloseTab(i) => self.close_tab(i),
            Action::NewTab => self.new_tab_with_switcher(),
            Action::Search => self.open_switcher(),
            Action::ToggleProblems => {
                self.problems_open = !self.problems_open && !self.problems.is_empty();
            }
            Action::OpenProblem(op) => self.open_problem(op),
            Action::Discard(op) => self.discard(op),
        }
    }

    pub(crate) fn page_ui(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut acts = Vec::new();
        let c = theme::colors(ui);
        // owned: the rows below borrow `self` mutably
        let crumbs = self.breadcrumb();
        let title = crate::view::untitled(&self.title).to_string();
        let mut area = egui::ScrollArea::vertical()
            .id_salt("page")
            .auto_shrink(false);
        if let Some(y) = self.pending_scroll.take() {
            area = area.vertical_scroll_offset(y);
        }
        let out = area.show(ui, |ui| {
            let w = ui.available_width();
            let col = w.min(900.0);
            ui.horizontal_top(|ui| {
                ui.add_space((w - col) / 2.0);
                ui.vertical(|ui| {
                    ui.set_max_width(col);
                    ui.add_space(16.0);
                    acts.extend(crate::view::breadcrumb(ui, c, &crumbs, &title));
                    ui.add_space(8.0);
                    acts.extend(self.title_ui(ui, &title));
                    ui.add_space(12.0);
                    for idx in 0..self.rows.len() {
                        if self.shown[idx] {
                            acts.extend(self.row_ui(ui, idx));
                        }
                    }
                    ui.add_space(200.0);
                });
            });
        });
        self.scroll = out.state.clamped_offset().y;
        acts
    }

    fn row_ui(&mut self, ui: &mut Ui, idx: usize) -> Vec<Action> {
        if matches!(self.rows[idx].marker, Marker::Table(_)) {
            return self.table_ui(ui, idx);
        }
        let (depth, editable, toggle, level) = {
            let r = &self.rows[idx];
            (r.depth, r.editable, r.toggle, r.level)
        };
        let marker = self.rows[idx].marker.clone();
        let (size, _) = look(&marker);
        let grey = ui.visuals().weak_text_color();
        // the sync gutter: red for a conflict/failed op, accent for a queued one
        let c = theme::colors(ui);
        let block = &self.rows[idx].block_id;
        let dot = if self.problem_ids.contains(block) {
            Some(c.error)
        } else if self.queued_ids.contains(block) {
            Some(c.accent)
        } else {
            None
        };
        let mut act = None;
        ui.horizontal_top(|ui| {
            let (gutter, _) = ui.allocate_exact_size(egui::vec2(12.0, 20.0), Sense::hover());
            if let Some(col) = dot {
                ui.painter().circle_filled(gutter.center(), 3.0, col);
            }
            ui.add_space(26.0 * depth as f32);
            // the left-hand glyph: bullet, number, checkbox or toggle arrow
            let glyph = if toggle {
                let open = self.open.contains(&self.rows[idx].block_id);
                Some((
                    if open { "▾" } else { "▸" }.to_string(),
                    Action::Toggle(idx),
                ))
            } else {
                match &marker {
                    Marker::Bullet => Some((outline::bullet(level).to_string(), Action::Blur)),
                    Marker::Number(n) => Some((outline::number(*n, level), Action::Blur)),
                    Marker::Todo(c) => {
                        Some((if *c { "☑" } else { "☐" }.to_string(), Action::Check(idx)))
                    }
                    _ => None,
                }
            };
            if let Some((g, on_click)) = glyph {
                let l = Label::new(RichText::new(g).size(size)).sense(Sense::click());
                if ui.add_sized([22.0, 20.0], l).clicked() && !matches!(on_click, Action::Blur) {
                    act = Some(on_click);
                }
            }
            let fill = Color32::from_rgba_unmultiplied(128, 128, 128, 36);
            let mut body = |this: &mut Self, ui: &mut Ui| {
                if let Some(a) = this.body(ui, idx, editable) {
                    act = Some(a);
                }
            };
            match &marker {
                Marker::Quote => {
                    let r = Frame::new()
                        .inner_margin(Margin {
                            left: 12,
                            right: 0,
                            top: 2,
                            bottom: 2,
                        })
                        .show(ui, |ui| body(self, ui))
                        .response
                        .rect;
                    let bar = egui::Rect::from_min_size(r.left_top(), egui::vec2(3.0, r.height()));
                    ui.painter().rect_filled(bar, 1.0, Color32::GRAY);
                }
                Marker::Callout(icon) => {
                    Frame::new()
                        .fill(fill)
                        .corner_radius(4.0)
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new(icon).size(20.0));
                                body(self, ui);
                            });
                        });
                }
                Marker::Code(lang) => {
                    Frame::new()
                        .fill(fill)
                        .corner_radius(4.0)
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if !lang.is_empty() {
                                ui.label(RichText::new(lang).small().color(grey));
                            }
                            body(self, ui);
                        });
                }
                Marker::Divider => {
                    ui.separator();
                }
                Marker::ChildPage(t) | Marker::ChildDatabase(t) => {
                    let page = matches!(marker, Marker::ChildPage(_));
                    let text = format!("{} {t}", if page { "📄" } else { "🗄" });
                    let r = ui.add(Label::new(text).sense(Sense::click()));
                    if let Some(new_tab) = page.then(|| clicked_new_tab(ui, &r)).flatten() {
                        let link = Link::Page(self.rows[idx].block_id.clone());
                        act = Some(Action::Link { link, new_tab });
                    }
                }
                Marker::LinkToPage {
                    id,
                    title,
                    database,
                } => {
                    let text = if title.is_empty() {
                        "→ (page)".to_string()
                    } else {
                        format!("→ {title}")
                    };
                    let r = ui.add(Label::new(text).sense(Sense::click()));
                    if let Some(new_tab) = (!database).then(|| clicked_new_tab(ui, &r)).flatten() {
                        act = Some(Action::Link {
                            link: Link::Page(id.clone()),
                            new_tab,
                        });
                    }
                }
                Marker::LinkCard { kind, url, label } => {
                    let text = format!("🔗 [{kind}] {}", if label.is_empty() { url } else { label });
                    let r = ui.add(Label::new(text).sense(Sense::click()));
                    if clicked_new_tab(ui, &r).is_some() {
                        act = Some(Action::Link { link: crate::links::route(url), new_tab: false });
                    }
                }
                Marker::Synced | Marker::ColumnList | Marker::Column | Marker::Breadcrumb => {
                    ui.label(RichText::new(marker.label()).color(grey));
                }
                Marker::Unsupported(t) => {
                    ui.label(RichText::new(format!("[{t}]")).color(grey));
                }
                Marker::TableOfContents => {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Table of contents").color(grey));
                        body(self, ui);
                    });
                }
                Marker::Equation => {
                    body(self, ui);
                }
                _ => body(self, ui),
            }
        });
        act.into_iter().collect()
    }

    /// A table: a cell is edited in place on a click, unless the table is
    /// locked (synced) or the cell is read-only (a mention).
    fn table_ui(&mut self, ui: &mut Ui, idx: usize) -> Vec<Action> {
        let Marker::Table(t) = self.rows[idx].marker.clone() else {
            return Vec::new();
        };
        let (indent, locked) = (26.0 * self.rows[idx].depth as f32, self.rows[idx].locked);
        let mut acts = Vec::new();
        ui.horizontal_top(|ui| {
            ui.add_space(indent);
            egui::Grid::new(("table", idx)).striped(true).show(ui, |ui| {
                for (ri, r) in t.rows.iter().enumerate() {
                    for (ci, c) in r.cells.iter().enumerate() {
                        let editing = self
                            .focus
                            .as_ref()
                            .is_some_and(|f| f.idx == idx && f.cell == Some((r.id.clone(), ci)));
                        if editing {
                            acts.extend(self.cell_edit(ui));
                            continue;
                        }
                        let job = self
                            .table_jobs
                            .entry((idx, ri, ci))
                            .or_insert_with(|| md::rich_job(&c.rich_text, 15.0, false));
                        let mut l = Label::new(job.clone());
                        let click = !locked && c.editable;
                        if click {
                            l = l.sense(Sense::click()).selectable(false);
                        }
                        let resp = ui.add(l);
                        if click && resp.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                            acts.push(Action::EditCell(r.id.clone(), ci));
                        }
                    }
                    ui.end_row();
                }
            });
        });
        acts
    }

    /// The row's text: a cached label, or the editor if it has focus. A click
    /// on a link follows it, any other click edits the row.
    fn body(&mut self, ui: &mut Ui, idx: usize, editable: bool) -> Option<Action> {
        if self.focus.as_ref().is_some_and(|f| f.idx == idx) {
            return self.edit_body(ui, idx);
        }
        let rows = &self.rows;
        let job = self.jobs.entry(idx).or_insert_with(|| {
            let r = &rows[idx];
            let (size, bold) = look(&r.marker);
            match r.marker {
                Marker::Code(_) => LayoutJob::simple(
                    plain_text(&r.rich_text),
                    egui::FontId::monospace(size),
                    Color32::PLACEHOLDER,
                    f32::INFINITY,
                ),
                _ => md::rich_job(&r.rich_text, size, bold),
            }
        });
        let linked = rows[idx].rich_text.iter().any(|r| links::run_link(r).is_some());
        let mut l = Label::new(job.clone()).wrap();
        if editable || linked {
            l = l.sense(Sense::click()).selectable(false);
        }
        if !linked {
            let r = ui.add(l);
            return if editable {
                r.on_hover_cursor(egui::CursorIcon::Text)
                    .clicked()
                    .then_some(Action::Edit(idx, Grab::End))
            } else {
                None
            };
        }
        // the link under the pointer, found in the label's own layout
        let (pos, galley, r) = l.layout_in_ui(ui);
        if ui.is_rect_visible(r.rect) {
            let color = ui.visuals().text_color();
            ui.painter().add(egui::epaint::TextShape::new(pos, galley.clone(), color));
        }
        let rich = &rows[idx].rich_text;
        let at = |p: egui::Pos2| {
            let rel = p - pos;
            let c = galley.cursor_from_pos(rel).index.0;
            let x = galley.pos_from_cursor(CCursor::new(c)).min.x;
            links::link_at(rich, job, if rel.x >= x { c } else { c.saturating_sub(1) })
        };
        let hover = r.hover_pos().and_then(at);
        if hover.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        } else if editable {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }
        let new_tab = clicked_new_tab(ui, &r)?;
        match r.interact_pointer_pos().and_then(at) {
            Some(link) => Some(Action::Link { link, new_tab }),
            None => editable.then_some(Action::Edit(idx, Grab::End)),
        }
    }

    fn edit_body(&mut self, ui: &mut Ui, idx: usize) -> Option<Action> {
        let ctx = ui.ctx().clone();
        let id = edit_id(&self.focus.as_ref()?.block_id);
        let dim = ui.visuals().weak_text_color();
        let (size, bold) = look(&self.rows[idx].marker);
        let f = self.focus.as_mut()?;
        let mut act = None;

        if let Some(g) = f.grab.take() {
            ctx.memory_mut(|m| m.request_focus(id));
            set_cursor(&ctx, id, TextEditState::default(), g.index(&f.text));
        }
        let cur = TextEditState::load(&ctx, id)
            .and_then(|s| s.cursor.char_range())
            .map_or(0, |r| r.primary.index.0);
        let (line, col) = line_col(&f.text, cur);

        // Keys the text edit must not see: take them before it runs.
        // egui's EventFilter only decides who gets focus-navigation keys; the
        // TextEdit still acts on whatever stays in `input.events`.
        let take = |k: Key| ctx.input_mut(|i| i.count_and_consume_key(Modifiers::NONE, k));
        let mut picked = None;
        let mut close = false;
        let mut enter = false;
        // not focused (e.g. a click elsewhere this frame): leave every key alone
        let focused = ctx.memory(|m| m.has_focus(id));
        if !focused {
        } else if let Some(menu) = f.menu.as_mut() {
            let q = menu.query(&f.text, line, col).unwrap_or("").to_string();
            for (key, ek) in [
                (Key::ArrowUp, EditKey::Up),
                (Key::ArrowDown, EditKey::Down),
                (Key::Escape, EditKey::Escape),
                (Key::Tab, EditKey::Indent),
            ] {
                for _ in 0..take(key) {
                    match menu_step(menu, &q, ek) {
                        Outcome::Pick(k) => picked = Some(k),
                        Outcome::Close => close = true,
                        Outcome::Handled | Outcome::Pass => {}
                    }
                }
            }
            // Enter is only noted here: text typed in the same frame changes
            // the query, so the pick is resolved after the edit has run
            enter = take(Key::Enter) > 0;
        } else {
            // ↑ on the first visual line / ↓ on the last leaves the row; ↑ from
            // the first row goes to the title
            if let (Some(g), true) = (&f.galley, ctx.input(|i| i.modifiers.is_none())) {
                let row = g.layout_from_cursor(CCursor::new(cur)).row;
                if row == 0 && ctx.input(|i| i.key_pressed(Key::ArrowUp)) {
                    if let Some(j) = neighbour(&self.rows, &self.shown, idx, -1) {
                        take(Key::ArrowUp);
                        act = Some(Action::Edit(j, Grab::End));
                    } else if self.title_editable {
                        take(Key::ArrowUp);
                        act = Some(Action::EditTitle);
                    }
                } else if row + 1 >= g.rows.len() && ctx.input(|i| i.key_pressed(Key::ArrowDown)) {
                    if let Some(j) = neighbour(&self.rows, &self.shown, idx, 1) {
                        take(Key::ArrowDown);
                        act = Some(Action::Edit(j, Grab::Start));
                    }
                }
            }
        }

        // Enter, Tab, Esc, Backspace, keymap commands; text edited by a mark
        // is read back below
        if focused {
            act = self.editor_keys(&ctx, id).or(act);
        }
        let f = self.focus.as_mut()?;
        let mut layouter = |ui: &Ui, text: &dyn TextBuffer, wrap: f32| {
            let mut job = md::md_job(text.as_str(), size, bold, dim);
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let mut out = egui::TextEdit::multiline(&mut f.text)
            .id(id)
            .desired_width(f32::INFINITY)
            .desired_rows(1)
            .event_filter(egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            })
            .layouter(&mut layouter)
            .show(ui);
        f.galley = Some(out.galley.clone());
        let resp = out.response.response.clone();
        if f.scroll {
            resp.scroll_to_me(None);
            f.scroll = false;
        }
        if resp.lost_focus() && act.is_none() {
            act = Some(Action::Blur);
        }
        let cur = out.cursor_range.map_or(cur, |r| r.primary.index.0);
        let (line, col) = line_col(&f.text, cur);
        let typed = resp.changed() && f.text.len() > f.prev_len;
        let mut new_cursor = None;

        if let Some(m) = f.menu.as_mut() {
            match m.query(&f.text, line, col).map(slash::filter) {
                Some(items) if !items.is_empty() && !close => {
                    m.selected = if resp.changed() {
                        0
                    } else {
                        m.selected.min(items.len() - 1)
                    };
                }
                _ => f.menu = None, // Esc, no match, or the cursor left the query
            }
        }
        if enter {
            // the query as it stands after this frame's typing
            if let Some(m) = f.menu.as_mut() {
                let q = m.query(&f.text, line, col).unwrap_or("").to_string();
                if let Outcome::Pick(k) = menu_step(m, &q, EditKey::Enter) {
                    picked = Some(k);
                }
            }
        }
        let mut convert = None;
        if let Some(kind) = picked {
            let before = f.text.clone();
            if let Some(m) = &f.menu {
                f.text = m.strip(&f.text, col);
                new_cursor = Some(f.text[..byte_at(&f.text, m.line, m.slash)].chars().count());
            }
            f.menu = None;
            convert = Some((kind, before));
        } else if f.menu.is_none()
            && typed
            && !f.code
            && f.text[..byte_at(&f.text, line, col)].ends_with('/')
        {
            let slash = col - 1;
            let before = f.text[..byte_at(&f.text, line, col)].chars().rev().nth(1);
            if before.is_none_or(char::is_whitespace) {
                f.menu = Some(Menu {
                    line,
                    slash,
                    selected: 0,
                });
            }
        }
        // Markdown shortcut: Space right after `# `, `- `, `1. ` at the row start
        if f.menu.is_none()
            && picked.is_none()
            && typed
            && !f.code
            && f.text[..byte_at(&f.text, line, col)].ends_with(' ')
        {
            if let Some((kind, rest)) = slash::shortcut(&f.text, line, col) {
                let before = std::mem::replace(&mut f.text, rest);
                new_cursor = Some(0);
                convert = Some((kind, before));
            }
        }
        if let Some(at) = new_cursor {
            out.state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
            out.state.store(&ctx, id);
        }
        f.prev_len = f.text.len();
        if let Some((kind, typed)) = convert {
            let at = new_cursor.unwrap_or_else(|| f.text.chars().count());
            act = Some(Action::Convert {
                kind,
                text: f.text.clone(),
                typed,
                at,
            });
        }

        if let Some(m) = &f.menu {
            let items = slash::filter(m.query(&f.text, line, col).unwrap_or(""));
            let slash_char = f.text[..byte_at(&f.text, m.line, m.slash)].chars().count();
            let pos = out.galley_pos
                + out
                    .galley
                    .pos_from_cursor(CCursor::new(slash_char))
                    .left_bottom()
                    .to_vec2();
            let hl = ui.visuals().selection.bg_fill;
            egui::Area::new(Id::unique("slash-menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(240.0);
                        for (n, it) in items.iter().enumerate() {
                            let mut t = RichText::new(format!("{}   {}", it.name, it.hint));
                            if n == m.selected {
                                t = t.strong().background_color(hl);
                            }
                            ui.add(Label::new(t).selectable(false));
                        }
                    });
                });
        }
        act
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn cursor_math_handles_lines_and_multibyte() {
        assert_eq!(line_col("ab\nßc", 4), (1, 2)); // the column is in bytes: ß is 2
        assert_eq!(byte_at("ab\nßc", 1, 2), 5);
        assert_eq!(line_col("", 0), (0, 0));
    }

    /// Drives the real widgets through egui with synthetic input, to check the
    /// menu really takes ↑/↓/Enter/Esc away from the TextEdit.
    mod headless {
        use super::*;
        use egui::Event;

        use crate::testkit::{key, Win};

        fn win(texts: &[&str]) -> Win {
            Win::with_texts(texts)
        }

        #[test]
        fn slash_menu_takes_the_keys_and_picks() {
            let mut w = win(&[""]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            assert!(w.menu().is_some(), "typing / opens the menu");
            w.send(Event::Text("head".into()));
            assert_eq!(w.text(), "/head");
            w.send(key(Key::ArrowDown));
            assert_eq!(w.menu().unwrap().selected, 1);
            w.send(key(Key::Tab));
            assert_eq!(w.text(), "/head", "Tab is ignored");
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::Heading(2));
            assert_eq!(w.text(), "", "/query removed, no newline typed by Enter");
            assert!(w.menu().is_none());
        }

        #[test]
        fn esc_closes_menu_then_row_and_backspace_past_slash_closes() {
            let mut w = win(&["abc"]);
            w.edit(0);
            w.send(Event::Text(" /".into()));
            assert!(w.menu().is_some());
            w.send(key(Key::Escape));
            assert!(w.menu().is_none());
            assert!(w.app.focus.is_some(), "first Esc only closes the menu");
            w.send(Event::Text(" /".into()));
            assert!(w.menu().is_some());
            w.send(key(Key::Backspace));
            assert!(w.menu().is_none(), "backspace past the slash closes it");
            assert_eq!(w.text(), "abc / ");
            w.send(key(Key::Escape));
            assert!(w.app.focus.is_none(), "Esc commits the row");
            assert_eq!(plain_text(&w.app.rows[0].rich_text), "abc / ");
        }

        #[test]
        fn pick_on_a_later_line_keeps_the_text_after_the_cursor() {
            let mut w = win(&["x\n äö"]);
            w.edit(0);
            let st = TextEditState::default();
            set_cursor(&w.ctx, edit_id("p-0"), st, 2); // start of line 1
            w.frames(vec![]);
            w.send(Event::Text("/".into()));
            assert_eq!(w.menu().map(|m| (m.line, m.slash)), Some((1, 0)));
            w.send(Event::Text("h1".into()));
            assert_eq!(w.text(), "x\n/h1 äö");
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::Heading(1));
            assert_eq!(w.text(), "x\n äö", "only /h1 is removed");
        }

        #[test]
        fn enter_in_the_same_frame_as_typing_uses_the_new_query() {
            let mut w = win(&[""]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            w.frames(vec![Event::Text("h1".into()), key(Key::Enter)]);
            assert_eq!(w.app.rows[0].marker, Marker::Heading(1));
            assert_eq!(w.text(), "");
        }

        #[test]
        fn unfocused_editor_leaves_menu_keys_alone() {
            let mut w = win(&["", "b"]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            assert!(w.menu().is_some());
            w.ctx
                .memory_mut(|m| m.surrender_focus(edit_id("p-0")));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::None, "no pick without focus");
        }

        #[test]
        fn index_actions_after_a_key_are_dropped() {
            let mut acts = vec![
                Action::Edit(1, Grab::End),
                Action::Key(EditKey::Enter),
                Action::Check(2),
                Action::Edit(3, Grab::End),
                Action::Toggle(1),
                Action::Blur,
            ];
            blur_first(&mut acts);
            assert!(matches!(acts[0], Action::Blur));
            assert_eq!(acts.len(), 3, "Edit(1) stays before the key, the rest go");
            assert!(matches!(acts[2], Action::Key(_)));
        }

        #[test]
        fn blur_is_applied_before_other_actions() {
            let mut acts = vec![Action::Edit(3, Grab::End), Action::Blur, Action::Toggle(1)];
            blur_first(&mut acts);
            assert!(matches!(acts[0], Action::Blur));
            assert!(
                matches!(acts[1], Action::Edit(3, _)),
                "the rest keep their order"
            );
        }

        #[test]
        fn shortcut_and_row_hopping() {
            let mut w = win(&["", "second"]);
            w.edit(0);
            w.send(Event::Text("#".into()));
            w.send(Event::Text(" ".into()));
            assert_eq!(w.app.rows[0].marker, Marker::Heading(1));
            assert_eq!(w.text(), "");
            w.send(key(Key::ArrowDown));
            assert_eq!(
                w.app.focus.as_ref().unwrap().idx,
                1,
                "↓ on the last line moves to the next row"
            );
            w.send(key(Key::ArrowUp));
            assert_eq!(w.app.focus.as_ref().unwrap().idx, 0);
        }
    }
}
