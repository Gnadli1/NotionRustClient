//! App state, the page body and the focused-row editor. The shell around it
//! (sync, tabs, switcher, session, frame) is in `shell.rs`, drawing of the
//! chrome in `view/`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc::Receiver, Arc};
use std::time::{Duration, Instant};

use eframe::egui::{
    self,
    text::{CCursor, CCursorRange, LayoutJob},
    text_edit::TextEditState,
    Color32, Frame, Galley, Id, Key, Label, Margin, Modifiers, RichText, Sense, TextBuffer, Ui,
};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, BlockKind, TargetKind, TextBlock, User};
use notion_core::ops::CommentTarget;
use notion_core::store::{PageEntry, Store};
use notion_core::sync::{self, ChangeOutcome};
use tokio::sync::mpsc::UnboundedSender;

use crate::edit::EditKey;
use crate::keys::{Command, Keymap};
use crate::links::{self, Link};
use crate::menu::{self, menu_step, Kind, Menu, Outcome, Pick, Sources};
use crate::nav::{Find, History};
use crate::outline::{self, Marker, Row};
use crate::pages::{Dlg, Meta};
use crate::problems::Problem;
use crate::profiles::Profiles;
use crate::session;
use crate::slash;
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
    /// The cursor (char index) as the keys that need it last saw it.
    pub(crate) cursor: usize,
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
            cursor: 0,
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
    Link {
        link: Link,
        new_tab: bool,
    },
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
    /// Another workspace profile.
    SwitchProfile(String),
    /// The workspace switcher's "Add workspace…" / "Edit current…".
    NewProfile,
    EditProfile,
    ProfileSave,
    ProfileCancel,
    /// The private browser-session login.
    BrowserLoginStart,
    BrowserLoginConnect,
    BrowserLoginCancel,
    /// An outline tick: scroll to this row.
    Goto(usize),
    NewTab,
    /// Opens or closes the Updates inbox.
    Updates,
    /// Opens the Ctrl+K switcher.
    Search,
    /// The sidebar's "Sync everything".
    SyncAll,
    ToggleProblems,
    OpenProblem(i64),
    /// Keep theirs: the old Discard.
    Discard(i64),
    /// Opens the conflict panel for an op.
    Resolve(i64),
    KeepMine(i64),
    /// Use merged: the panel's text.
    Merge(i64),
    /// A row's 💬 count: the comments panel on that row.
    OpenComments(String),
    CloseComments,
    /// A comment box's Ctrl+Enter: (where it goes, Markdown text).
    CommentSend(CommentTarget, String),
    /// (comment id, Markdown text).
    CommentSave(String, String),
    CommentDelete(String),
    /// Esc in a comment box.
    CommentsBack,
    /// Esc in the editor: commit and select the block.
    Select,
    /// A block-editing command on the edited row (Ctrl+D, Ctrl+Shift+Up/Down).
    Run(Command),
    /// A `/` colour pick: the editor's text without the trigger, the colour.
    Color {
        text: String,
        color: String,
    },
    /// A multi-line paste into an empty row.
    Paste(String),
    /// Ctrl+N, or `/page` with the editor's text without the trigger.
    NewPage(Option<String>),
    /// Opens a page dialog (icon, cover, trash view).
    Dlg(Dlg),
    /// A click on the open database table.
    Db(crate::grid::Act),
    /// A slash pick or Markdown shortcut on the edited row: `text` is the
    /// editor's text without the trigger, `typed` as it was before, `at` the
    /// cursor (a char index into `text`).
    Convert {
        kind: TargetKind,
        text: String,
        typed: String,
        at: usize,
    },
    /// The sidebar header's cluster: collapse/show the tree, and the page
    /// history arrows in the open tab.
    ToggleSidebar,
    Back,
    Forward,
    /// A row's hover "+": a new paragraph after it, edited.
    InsertBelow(usize),
    /// A click below the last block: edit the trailing empty line, else add one.
    AppendEnd,
}

/// Opens a web link outside the app (injectable for tests).
pub(crate) type Opener = Box<dyn Fn(&str) -> Result<(), String>>;

/// ((editor id, name), "Edited by X · 5 min ago") of the page header.
pub(crate) type PageHead = (Option<(String, String)>, String);

/// (page, block, table cell) of an edit.
pub(crate) type PollKey = (String, String, Option<(String, usize)>);

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
    /// The open data source as a table (then `rows` is empty).
    pub(crate) db: Option<crate::grid::Db>,
    /// The sort of each data source this session: (column name, descending).
    pub(crate) db_sorts: HashMap<String, (String, bool)>,
    /// F5: the chosen view (label) of each data source, read from the store
    /// once; the saved views Notion sent; the data sources already asked.
    pub(crate) db_views: HashMap<String, String>,
    pub(crate) saved_views: HashMap<String, Vec<notion_core::views::SavedView>>,
    pub(crate) views_asked: HashSet<String>,
    /// The selected blocks: first and last of a run of siblings (ids).
    pub(crate) sel: Option<(String, String)>,
    pub(crate) keys: Keymap,
    pub(crate) title_edit: Option<TitleEdit>,
    /// The title has no mentions or equations (plain text round-trips).
    pub(crate) title_editable: bool,
    /// Local edits to undo / redo, newest last.
    pub(crate) undo: Vec<Step>,
    pub(crate) redo: Vec<Step>,
    /// Inside [`App::step`]: nested steps join the outer one.
    pub(crate) stepping: bool,
    /// Opens a web link (the system browser; tests swap it out).
    pub(crate) opener: Opener,
    /// The page's scroll offset, read back every frame.
    pub(crate) scroll: f32,
    /// An offset to apply on the next frame (tab switch, new page), once.
    pub(crate) pending_scroll: Option<f32>,
    /// Open tabs; `tabs[active]` is stale, `current`/`open`/`scroll` are its
    /// live state.
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// One page history per tab (grown on demand; see [`App::hist`]).
    pub(crate) hists: Vec<History>,
    /// Ctrl+F, while the bar is open.
    pub(crate) find: Option<Find>,
    /// A row to scroll into view on the next frame, once.
    pub(crate) goto: Option<usize>,
    /// The open page's headings (row, level, text), for the outline.
    pub(crate) heads: Vec<(usize, u8, String)>,
    /// Recently opened pages, most recent first.
    pub(crate) recent: Vec<String>,
    /// When each page was last opened this session (unix seconds).
    pub(crate) visited: HashMap<String, i64>,
    pub(crate) window: (f32, f32),
    /// Where the session file is written on exit; None (tests) writes nothing.
    pub(crate) session_dir: Option<PathBuf>,
    /// The session file of the active workspace (in `session_dir`).
    pub(crate) session_file: String,
    /// One token per workspace, and the active one.
    pub(crate) profiles: Profiles,
    pub(crate) profiles_dir: Option<PathBuf>,
    /// settings.json, applied at start and on change.
    pub(crate) settings: crate::settings::Settings,
    /// The Settings window, while open.
    pub(crate) settings_ui: Option<crate::view::settings::SettingsUi>,
    /// The AI panel, while open.
    pub(crate) ai: Option<crate::view::ai::AiPanel>,
    /// API keys for the AI agent (ai-keys.json).
    pub(crate) ai_secrets: crate::ai::Secrets,
    pub(crate) data_dir: Option<PathBuf>,
    /// The add/edit workspace dialog, while open.
    pub(crate) ws_form: Option<crate::profiles::Form>,
    /// A workspace switch asked for this frame: (name, reload even if active).
    pub(crate) pending_switch: Option<(String, bool)>,
    /// The profiles the pending switch adopts (and saves) once it succeeds.
    pub(crate) pending_profiles: Option<Profiles>,
    /// The browser-session (`token_v2`) login dialog.
    pub(crate) browser_login: Option<crate::browser_login::Flow>,
    /// A quiet startup refresh that discovers the account's shared workspaces.
    pub(crate) space_refresh: Option<crate::browser_login::Flow>,
    /// One entry per command sent and not yet answered (the worker answers
    /// each with one event, in order).
    pub(crate) sent: VecDeque<Cmd>,
    /// F7: pictures (textures, downloads asked for).
    pub(crate) media: crate::media::Media,
    /// (done, total) of the sync-all in flight; total 0 until the first report.
    pub(crate) progress: Option<(usize, usize)>,
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
    /// The conflict list's selected entry (Up/Down, Enter opens it).
    pub(crate) problem_sel: usize,
    /// The conflict panel, while open.
    pub(crate) conflict: Option<crate::resolve::Panel>,
    /// The comments panel, while open.
    pub(crate) comments: Option<crate::comments::Panel>,
    /// Cached comments per page or block of the open page (the 💬 counts).
    pub(crate) comment_counts: HashMap<String, usize>,
    /// The integration's bot user id, once known: its comments are the
    /// editable ones.
    pub(crate) me: Option<String>,
    pub(crate) me_asked: bool,
    /// The cached workspace users (by name), reloaded by `refresh`.
    pub(crate) users: Vec<User>,
    /// When the users were last asked for; None until the first pull is sent
    /// (by [`App::users_tick`] once nothing else is in flight).
    pub(crate) users_at: Option<Instant>,
    /// No more user pulls this session: a 403 (no "read user information"
    /// capability) or no token.
    pub(crate) users_stop: bool,
    /// The open page's last editor (user id) and edit time (unix seconds).
    pub(crate) page_edit: Option<(Option<String>, i64)>,
    /// The page header's editor line, built at most once a minute:
    /// (now / 60, ((editor id, name), text)).
    pub(crate) page_head: Option<(i64, Option<PageHead>)>,
    /// The open page's blocks: (last editor id, edit time) by block id,
    /// loaded with the page so a hover costs no query.
    pub(crate) block_edits: std::collections::HashMap<String, (Option<String>, i64)>,
    /// A standing note from the users pull (the 403); outlives other pulls.
    pub(crate) users_note: Option<String>,
    pub(crate) switcher: Option<Switcher>,
    /// The page dialog (icon, cover, trash confirm, trash view), while open.
    pub(crate) dlg: Option<Dlg>,
    /// The Updates inbox view (C5), and the unread count the sidebar shows.
    pub(crate) inbox: Option<crate::inbox::Inbox>,
    pub(crate) unread: usize,
    /// No person identity (NOTION_USER_ID or a person-owned integration): nothing is recorded.
    pub(crate) no_person: bool,
    /// The open page's icon, cover and lock.
    pub(crate) meta: Meta,
    /// When each open page was last polled (see [`App::poll_at`]).
    pub(crate) polled: HashMap<String, Instant>,
    /// When each page was last pulled in full, by anyone.
    pub(crate) full_at: HashMap<String, Instant>,
    /// How long each page's last full pull by a poll took, and when the
    /// poll in flight was sent: live polling spaces full pulls by their cost.
    pub(crate) pull_took: HashMap<String, Duration>,
    pub(crate) poll_sent: Option<Instant>,
    /// The (page, block, cell) of the last poll flush, the checkpoint under the
    /// undo step it made, and that step: the next poll flush extends it (see `poll_flush`).
    pub(crate) poll_cp: Option<(PollKey, sync::Checkpoint, crate::undo::Step)>,
    /// Blocks a poll changed, with the end of their highlight.
    pub(crate) flash: HashMap<String, Instant>,
    pub(crate) was_focused: bool,
    pub(crate) worker_gone: bool,
    /// The window title last sent.
    pub(crate) shown_title: String,
    pub(crate) keylog: bool,
    /// Today's date, asked when an `@` menu opens (tests swap it out).
    pub(crate) clock: fn(i64) -> String,
}

/// The text edit's id for a block; it must not change while the row moves.
pub(crate) fn edit_id(block_id: &str) -> Id {
    Id::unique(("edit", block_id))
}

/// (font size, bold) for a row's text.
fn look(m: &Marker) -> (f32, bool) {
    match m {
        Marker::Heading(l) => (theme::H_SIZE[usize::from(*l).clamp(1, 3) - 1], true),
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
    let moving = |a: &Action| {
        matches!(
            a,
            Action::Key(_)
                | Action::Run(_)
                | Action::Paste(_)
                | Action::Color { .. }
                | Action::NewPage(_)
        )
    };
    if let Some(k) = acts.iter().position(moving) {
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
            db: None,
            db_sorts: HashMap::new(),
            db_views: HashMap::new(),
            saved_views: HashMap::new(),
            views_asked: HashSet::new(),
            keys: Keymap::default(),
            title_edit: None,
            title_editable: false,
            undo: Vec::new(),
            redo: Vec::new(),
            stepping: false,
            opener: Box::new(links::open_url),
            scroll: 0.0,
            pending_scroll: None,
            tabs: vec![Tab::default()],
            active: 0,
            hists: Vec::new(),
            find: None,
            goto: None,
            heads: Vec::new(),
            recent: Vec::new(),
            visited: HashMap::new(),
            window: session::Session::default().window,
            session_dir: None,
            session_file: crate::shell::SESSION_FILE.to_string(),
            profiles: Profiles::default(),
            profiles_dir: None,
            settings: Default::default(),
            settings_ui: None,
            ai: None,
            ai_secrets: Default::default(),
            data_dir: None,
            ws_form: None,
            pending_switch: None,
            pending_profiles: None,
            browser_login: None,
            space_refresh: None,
            sent: VecDeque::new(),
            media: Default::default(),
            progress: None,
            pending_open: None,
            last_error: None,
            info: None,
            counts: [0; 3],
            queued_ids: HashSet::new(),
            problem_ids: HashSet::new(),
            problems: Vec::new(),
            problems_open: false,
            problem_sel: 0,
            conflict: None,
            comments: None,
            comment_counts: HashMap::new(),
            me: None,
            me_asked: false,
            users: Vec::new(),
            users_at: None,
            users_stop: false,
            page_edit: None,
            page_head: None,
            block_edits: Default::default(),
            users_note: None,
            switcher: None,
            dlg: None,
            inbox: None,
            unread: 0,
            no_person: true,
            meta: Meta::default(),
            polled: HashMap::new(),
            full_at: HashMap::new(),
            pull_took: HashMap::new(),
            poll_sent: None,
            poll_cp: None,
            flash: HashMap::new(),
            was_focused: false,
            worker_gone: false,
            shown_title: String::new(),
            keylog,
            sel: None,
            clock: notion_core::markdown::local_date_iso,
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
        self.sel = None;
        let r = &self.rows[i];
        let code = r.marker == Marker::Equation || matches!(r.marker, Marker::Code(_));
        let original = if code {
            plain_text(&r.rich_text)
        } else {
            to_markdown(&r.rich_text)
        };
        let text = typed.unwrap_or_else(|| original.clone());
        self.focus = Some(Focus::new(
            i,
            r.block_id.clone(),
            original,
            text,
            code,
            grab,
        ));
    }

    /// A to-do's box: queues the change. Locked rows and pushes refuse.
    pub(crate) fn check(&mut self, i: usize) {
        let r = &self.rows[i];
        let Marker::Todo(checked) = r.marker else {
            return;
        };
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
    pub(crate) fn convert(&mut self, kind: TargetKind, text: String, typed: String, at: usize) {
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
            Action::Select => {
                let id = self.focus.as_ref().map(|f| f.block_id.clone());
                self.apply(Action::Blur);
                if let Some(id) = id.filter(|_| self.focus.is_none()) {
                    self.sel = Some((id.clone(), id));
                }
            }
            Action::Run(c) => self.step(|s| s.block_cmd(c)),
            Action::Color { text, color } => self.step(|s| s.set_color(text, color)),
            Action::Paste(text) => self.step(|s| s.paste_lines(&text)),
            Action::NewPage(text) => self.new_page(text),
            Action::Updates
                if self.switcher.is_some() || self.conflict.is_some() || self.dlg.is_some() => {}
            Action::Updates => self.toggle_inbox(),
            Action::Dlg(d) => {
                if self.commit() {
                    self.dlg = Some(d);
                }
            }
            Action::Db(a) => self.db_act(a),
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
            Action::SwitchProfile(name) => self.pending_switch = Some((name, false)),
            Action::NewProfile => self.ws_form = Some(crate::profiles::Form::add()),
            Action::EditProfile => {
                if let Some(p) = self.profiles.active_profile() {
                    self.ws_form = Some(crate::profiles::Form::edit(p));
                }
            }
            Action::ProfileSave => self.save_workspace_form(),
            Action::ProfileCancel => self.ws_form = None,
            Action::BrowserLoginStart => self.start_browser_login(),
            Action::BrowserLoginConnect => {
                if let Some(flow) = &mut self.browser_login {
                    flow.connect();
                }
            }
            Action::BrowserLoginCancel => self.browser_login = None,
            Action::Goto(i) if i < self.rows.len() => self.show_row(i),
            Action::Goto(_) => {}
            Action::NewTab => self.new_tab_with_switcher(),
            Action::Search => self.open_switcher(),
            Action::SyncAll => self.sync_all(),
            Action::ToggleProblems => {
                self.problems_open = !self.problems_open && !self.problems.is_empty();
                self.problem_sel = 0;
            }
            Action::OpenProblem(op) => self.open_problem(op),
            Action::Discard(op) => self.discard(op),
            Action::Resolve(op) => self.open_conflict(op),
            Action::KeepMine(op) => self.resolve(op, None),
            Action::Merge(op) => self.merge_conflict(op),
            Action::OpenComments(block) => self.open_comments(Some(block)),
            Action::CloseComments => self.close_comments(false),
            Action::CommentSend(target, text) => self.send_comment(target, text),
            Action::CommentSave(id, text) => self.save_comment(id, text),
            Action::CommentDelete(id) => self.delete_comment(id),
            Action::CommentsBack => self.comments_back(),
            Action::ToggleSidebar => self.sidebar = !self.sidebar,
            Action::Back => self.go_history(true),
            Action::Forward => self.go_history(false),
            Action::InsertBelow(i) if i < self.rows.len() => self.insert_block_after(i),
            Action::InsertBelow(_) => {}
            Action::AppendEnd => self.append_block_at_end(),
        }
    }

    pub(crate) fn page_ui(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut acts = Vec::new();
        let c = theme::colors(ui);
        let title = crate::view::untitled(&self.title).to_string();
        let edited = self.page_edited(crate::comments::now_secs());
        let mut area = egui::ScrollArea::vertical()
            .id_salt("page")
            .auto_shrink(false);
        if let Some(y) = self.pending_scroll.take() {
            area = area.vertical_scroll_offset(y);
        }
        let out = area.show(ui, |ui| {
            let w = ui.available_width();
            let col = (w - 2.0 * theme::PAGE_PAD)
                .clamp(200.0, if self.settings.full_width { f32::INFINITY } else { theme::PAGE_MAX_WIDTH })
                .min(w);
            if std::env::var_os("NOTION_EGUI_DEBUG").is_some() {
                eprintln!(
                    "page w={w} col={col} min={:?} max={:?} ppp={}",
                    ui.min_rect().min,
                    ui.max_rect().max,
                    ui.ctx().pixels_per_point()
                );
            }
            ui.horizontal_top(|ui| {
                ui.add_space((w - col) / 2.0);
                ui.vertical(|ui| {
                    ui.set_max_width(col);
                    ui.spacing_mut().item_spacing.y = theme::BLOCK_GAP;
                    ui.add_space(theme::TITLE_TOP);
                    acts.extend(crate::pages::header(
                        ui,
                        c,
                        &self.meta,
                        self.current
                            .as_deref()
                            .and_then(|id| self.cover_shown(ui, id)),
                    ));
                    // breadcrumb, title and edited line share the text rows' left edge
                    Frame::new()
                        .inner_margin(Margin {
                            left: theme::TEXT_INSET as i8,
                            ..Margin::ZERO
                        })
                        .show(ui, |ui| {
                            acts.extend(self.title_ui(ui, &title));
                            if let Some((who, text)) = &edited {
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    if let Some((id, name)) = who {
                                        crate::people::avatar(ui, id, name, 16.0);
                                    }
                                    ui.label(RichText::new(text).size(12.0).color(c.secondary));
                                });
                            }
                        });
                    ui.add_space(12.0);
                    let selected = self.sel_rows();
                    let mut idx = 0;
                    while idx < self.rows.len() {
                        if !self.shown[idx] {
                            idx += 1;
                            continue;
                        }
                        // a column list draws its whole subtree side by side
                        if matches!(self.rows[idx].marker, Marker::ColumnList) {
                            let end = idx + 1 + outline::subtree(&self.rows, idx).len();
                            acts.extend(self.columns_ui(ui, idx));
                            idx = end;
                            continue;
                        }
                        // painted behind the row once its height is known: the
                        // block colour, then the flash / find / selection tint
                        let tint = if self.flash.contains_key(&self.rows[idx].block_id) {
                            Some(0.18)
                        } else if selected.contains(&idx) {
                            Some(0.25)
                        } else {
                            self.find.as_ref().and_then(|f| f.tint(idx))
                        };
                        let block =
                            theme::block_color(&self.rows[idx].color, ui.visuals().dark_mode).1;
                        let fills = block
                            .into_iter()
                            .chain(tint.map(|t| c.accent.gamma_multiply(t)));
                        let bgs: Vec<_> = fills
                            .map(|f| (ui.painter().add(egui::Shape::Noop), f))
                            .collect();
                        if let Marker::Heading(l) = self.rows[idx].marker {
                            ui.add_space(theme::H_ABOVE[usize::from(l).clamp(1, 3) - 1]);
                        }
                        let top = ui.cursor().top();
                        acts.extend(self.row_ui(ui, idx));
                        if matches!(self.rows[idx].marker, Marker::Heading(_)) {
                            ui.add_space(theme::H_BELOW);
                        }
                        let r = egui::Rect::from_x_y_ranges(
                            ui.max_rect().x_range(),
                            top..=ui.cursor().top(),
                        );
                        for (bg, f) in bgs {
                            ui.painter().set(bg, egui::Shape::rect_filled(r, 3.0, f));
                        }
                        // Notion's floating block handles in the left
                        // margin: add-below and the drag grip.
                        let hover = r.expand2(egui::vec2(42.0, 0.0));
                        if ui.rect_contains_pointer(hover)
                            && self.focus.as_ref().is_none_or(|f| f.idx != idx)
                        {
                            let y = r.top() + 11.0;
                            let cx = (r.left() - 32.0).max(ui.clip_rect().left() + 12.0);
                            let plus = egui::Rect::from_center_size(
                                egui::pos2(cx, y),
                                egui::vec2(20.0, 20.0),
                            );
                            let dots = egui::Rect::from_center_size(
                                egui::pos2(cx + 18.0, y),
                                egui::vec2(20.0, 20.0),
                            );
                            let p = ui.interact(
                                plus,
                                Id::unique(("row-plus", self.rows[idx].block_id.as_str())),
                                Sense::click(),
                            );
                            if p.hovered() {
                                ui.painter().rect_filled(plus, 4.0, c.hover);
                            }
                            crate::icons::paint(
                                ui,
                                plus.shrink(5.0),
                                crate::icons::Icon::Plus,
                                c.secondary,
                            );
                            if p.on_hover_text("Add a block below").clicked() {
                                acts.push(Action::InsertBelow(idx));
                            }
                            let d = ui.interact(
                                dots,
                                Id::unique(("row-grip", self.rows[idx].block_id.as_str())),
                                Sense::click_and_drag(),
                            );
                            if d.hovered() {
                                ui.painter().rect_filled(dots, 4.0, c.hover);
                            }
                            d.on_hover_cursor(egui::CursorIcon::Grab);
                            crate::icons::paint(
                                ui,
                                dots.shrink(5.0),
                                crate::icons::Icon::Dots,
                                c.secondary,
                            );
                        }
                        if self.goto == Some(idx) {
                            self.goto = None;
                            ui.scroll_to_rect_animation(
                                r,
                                Some(egui::Align::Center),
                                egui::style::ScrollAnimation::none(),
                            );
                        }
                        idx += 1;
                    }
                    self.goto = None;
                    let (_, end) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 200.0),
                        Sense::click(),
                    );
                    if end.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                        acts.push(Action::AppendEnd);
                    }
                });
            });
        });
        self.scroll = out.state.clamped_offset().y;
        acts
    }

    /// A `column_list`: its columns side by side, each with its subtree.
    fn columns_ui(&mut self, ui: &mut Ui, idx: usize) -> Vec<Action> {
        let ranges = outline::columns(&self.rows, idx);
        if ranges.is_empty() {
            return Vec::new();
        }
        let n = ranges.len();
        let base = self.rows[idx].depth + 2;
        let inset = (theme::TEXT_INSET - ui.spacing().item_spacing.x) as i8;
        let mut acts = Vec::new();
        ui.add_space(2.0);
        Frame::new()
            .inner_margin(Margin {
                left: inset,
                right: theme::TEXT_INSET as i8,
                ..Margin::ZERO
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = theme::COLUMN_GAP;
                ui.columns(n, |cols| {
                    for (col_ui, range) in cols.iter_mut().zip(ranges) {
                        col_ui.spacing_mut().item_spacing = egui::vec2(8.0, theme::BLOCK_GAP);
                        for i in range {
                            if !self.shown[i] {
                                continue;
                            }
                            let depth = self.rows[i].depth.saturating_sub(base);
                            if let Marker::Heading(l) = self.rows[i].marker {
                                col_ui.add_space(theme::H_ABOVE[usize::from(l).clamp(1, 3) - 1]);
                            }
                            acts.extend(self.row_ui_at(col_ui, i, depth, 0.0));
                            if matches!(self.rows[i].marker, Marker::Heading(_)) {
                                col_ui.add_space(theme::H_BELOW);
                            }
                        }
                    }
                });
            });
        ui.add_space(2.0);
        acts
    }

    fn row_ui(&mut self, ui: &mut Ui, idx: usize) -> Vec<Action> {
        let gutter = theme::TEXT_INSET - ui.spacing().item_spacing.x;
        let depth = self.rows[idx].depth;
        self.row_ui_at(ui, idx, depth, gutter)
    }

    /// A row with an explicit nesting depth and left gutter: page rows use the
    /// page's text inset, a column's rows start at the column's own edge.
    fn row_ui_at(&mut self, ui: &mut Ui, idx: usize, depth: usize, gutter: f32) -> Vec<Action> {
        if matches!(self.rows[idx].marker, Marker::Table(_)) {
            return self.table_ui(ui, idx);
        }
        let (editable, toggle, level) = {
            let r = &self.rows[idx];
            (r.editable, r.toggle, r.level)
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
        let comments = self
            .comment_counts
            .get(&self.rows[idx].block_id)
            .copied()
            .unwrap_or(0);
        // room for the 💬 count at the right
        let full = ui.available_width();
        let row = ui.horizontal_top(|ui| {
            if comments > 0 {
                ui.set_max_width(full - 52.0);
            }
            let (gutter, _) = ui.allocate_exact_size(egui::vec2(gutter, 20.0), Sense::hover());
            if let Some(col) = dot {
                ui.painter().circle_filled(gutter.center(), 3.0, col);
            }
            ui.add_space(26.0 * depth as f32);
            // the left-hand glyph: bullet, number, checkbox or toggle arrow
            if toggle {
                let open = self.open.contains(&self.rows[idx].block_id);
                let (rect, r) = ui.allocate_exact_size(egui::vec2(22.0, 20.0), Sense::click());
                let chev = if open {
                    crate::icons::Icon::ChevronDown
                } else {
                    crate::icons::Icon::ChevronRight
                };
                crate::icons::paint(ui, rect.shrink(5.0), chev, grey);
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    act = Some(Action::Toggle(idx));
                }
            } else {
                match &marker {
                    Marker::Bullet | Marker::Number(_) => {
                        let g = match &marker {
                            Marker::Bullet => outline::bullet(level).to_string(),
                            Marker::Number(n) => outline::number(*n, level),
                            _ => unreachable!(),
                        };
                        ui.add_sized(
                            [22.0, 20.0],
                            Label::new(RichText::new(g).size(size).color(grey)),
                        );
                    }
                    Marker::Todo(checked) => {
                        let (rect, r) =
                            ui.allocate_exact_size(egui::vec2(22.0, 20.0), Sense::click());
                        let box_r =
                            egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0));
                        if *checked {
                            ui.painter().rect_filled(box_r, 3.0, c.accent);
                            crate::icons::paint(
                                ui,
                                box_r.shrink(3.0),
                                crate::icons::Icon::Check,
                                Color32::WHITE,
                            );
                        } else {
                            ui.painter().rect_stroke(
                                box_r,
                                3.0,
                                egui::Stroke::new(1.5, c.secondary),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            act = Some(Action::Check(idx));
                        }
                    }
                    _ => {}
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
                    ui.painter()
                        .rect_filled(bar, 1.0, c.text.gamma_multiply(0.45));
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
                Marker::ChildDatabase(t) => {
                    let block_id = self.rows[idx].block_id.clone();
                    if let Some(a) = self.embedded_db_ui(ui, &block_id, t) {
                        act = Some(a);
                    }
                }
                Marker::ChildPage(t) => {
                    let (rect, r) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 24.0),
                        Sense::click(),
                    );
                    crate::icons::paint(
                        ui,
                        egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::vec2(16.0, 16.0),
                        ),
                        crate::icons::Icon::Page,
                        grey,
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + 20.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        if t.is_empty() { "Untitled" } else { t.as_str() },
                        egui::FontId::proportional(16.0),
                        c.text,
                    );
                    let tw = ui
                        .painter()
                        .layout_no_wrap(t.clone(), egui::FontId::proportional(16.0), c.text)
                        .size()
                        .x;
                    ui.painter().line_segment(
                        [
                            egui::pos2(rect.left() + 20.0, rect.center().y + 9.0),
                            egui::pos2(rect.left() + 20.0 + tw, rect.center().y + 9.0),
                        ],
                        egui::Stroke::new(1.0, c.text.gamma_multiply(0.35)),
                    );
                    if let Some(new_tab) = clicked_new_tab(ui, &r) {
                        let link = Link::Page(self.rows[idx].block_id.clone());
                        act = Some(Action::Link { link, new_tab });
                    }
                }
                Marker::LinkToPage { id, title, .. } => {
                    let text = if title.is_empty() {
                        "Untitled".to_string()
                    } else {
                        title.clone()
                    };
                    let (rect, r) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 24.0),
                        Sense::click(),
                    );
                    crate::icons::paint(
                        ui,
                        egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::vec2(16.0, 16.0),
                        ),
                        crate::icons::Icon::Page,
                        c.secondary,
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + 20.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        &text,
                        egui::FontId::proportional(16.0),
                        c.text,
                    );
                    let tw = ui
                        .painter()
                        .layout_no_wrap(text.clone(), egui::FontId::proportional(16.0), c.text)
                        .size()
                        .x;
                    ui.painter().line_segment(
                        [
                            egui::pos2(rect.left() + 20.0, rect.center().y + 9.0),
                            egui::pos2(rect.left() + 20.0 + tw, rect.center().y + 9.0),
                        ],
                        egui::Stroke::new(1.0, c.text.gamma_multiply(0.35)),
                    );
                    if let Some(new_tab) = clicked_new_tab(ui, &r) {
                        act = Some(Action::Link {
                            link: Link::Page(id.clone()),
                            new_tab,
                        });
                    }
                }
                Marker::LinkCard { kind, url, label } if kind != "image" => {
                    let text =
                        format!("🔗 [{kind}] {}", if label.is_empty() { url } else { label });
                    Frame::new()
                        .fill(ui.visuals().faint_bg_color)
                        .corner_radius(6.0)
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            let r = ui.add(Label::new(text).sense(Sense::click()));
                            if clicked_new_tab(ui, &r).is_some() {
                                act = Some(Action::Link {
                                    link: crate::links::route(url),
                                    new_tab: false,
                                });
                            }
                        });
                }
                // columns are laid out by `columns_ui`; the markers themselves
                // never reach here (and must not print `[column]`)
                Marker::ColumnList | Marker::Column => {}
                Marker::Synced | Marker::Breadcrumb => {
                    ui.label(RichText::new(marker.label()).color(grey));
                }
                // the outline turns an image with a URL into a link card: draw the picture
                Marker::Unsupported(t) | Marker::LinkCard { kind: t, .. } if t == "image" => {
                    let id = &self.rows[idx].block_id;
                    let (url, label) = match &marker {
                        Marker::LinkCard { url, label, .. } => (url.as_str(), label.as_str()),
                        _ => ("", ""),
                    };
                    let shown = self
                        .media
                        .src(&self.store, id, false)
                        .map(|src| self.media.show(ui.ctx(), id, false, &src));
                    if !url.is_empty()
                        && !matches!(
                            shown,
                            Some(crate::media::Shown::Tex(_) | crate::media::Shown::Loading)
                        )
                    {
                        if url.starts_with("attachment:") {
                            Frame::new()
                                .fill(ui.visuals().faint_bg_color)
                                .corner_radius(6.0)
                                .inner_margin(Margin::symmetric(10, 8))
                                .show(ui, |ui| {
                                    ui.label(RichText::new("Image could not be loaded").strong());
                                    ui.label(
                                        RichText::new(
                                            "Notion's file session may have expired. Retry, or \
                                             reopen the page.",
                                        )
                                        .small()
                                        .color(grey),
                                    );
                                    if ui.button("Retry").clicked() {
                                        self.media.retry_failed();
                                        ui.ctx().request_repaint();
                                    }
                                });
                        } else {
                            let r = ui.add(
                                Label::new(format!(
                                    "🔗 [image] {}",
                                    if label.is_empty() { url } else { label }
                                ))
                                .sense(Sense::click()),
                            );
                            if clicked_new_tab(ui, &r).is_some() {
                                act = Some(Action::Link {
                                    link: crate::links::route(url),
                                    new_tab: false,
                                });
                            }
                        }
                    } else if let Some(shown) = shown {
                        ui.vertical(|ui| {
                            crate::media::block_image(ui, &shown, grey);
                            if !label.is_empty() && label != url {
                                ui.add_space(2.0);
                                ui.label(RichText::new(label).small().italics().color(grey));
                            }
                        });
                    } else {
                        ui.label(RichText::new("[image]").color(grey));
                    }
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
        self.edited_hover(ui, idx, row.response.rect);
        if comments > 0 {
            let mut r = row.response.rect;
            r.set_right(ui.max_rect().right());
            let id = Id::unique(("comments-badge", self.rows[idx].block_id.as_str()));
            if crate::comments::badge(ui, c, r, id, comments) {
                act = Some(Action::OpenComments(self.rows[idx].block_id.clone()));
            }
        }
        act.into_iter().collect()
    }

    /// The page header's editor line, owned: ((editor id, name), "Edited by X · 5 min ago").
    /// Built once a minute (and again when the page or the users change).
    pub(crate) fn page_edited(&mut self, now: i64) -> Option<PageHead> {
        if self.page_head.as_ref().is_none_or(|(m, _)| *m != now / 60) {
            let head = self.page_edit.as_ref().map(|(id, then)| {
                let who = id
                    .as_ref()
                    .map(|i| (i.clone(), crate::people::name_of(&self.users, i)));
                let text =
                    crate::people::edited_header(who.as_ref().map(|w| w.1.as_str()), *then, now);
                (who, text)
            });
            self.page_head = Some((now / 60, head));
        }
        self.page_head.as_ref().and_then(|(_, h)| h.clone())
    }

    /// "Edited by X, 5 min ago" for a cached block (None for an unpushed one).
    pub(crate) fn block_edited_tip(&self, block_id: &str, now: i64) -> Option<String> {
        if block_id.starts_with("local-") {
            return None;
        }
        let (by, then) = self.block_edits.get(block_id)?;
        let who = by
            .as_deref()
            .map(|u| crate::people::name_of(&self.users, u));
        Some(crate::people::edited_tip(who.as_deref(), *then, now))
    }

    /// A tooltip over the row's full width saying who edited the block last.
    /// Not on the row being edited.
    fn edited_hover(&self, ui: &Ui, idx: usize, mut rect: egui::Rect) {
        let id = &self.rows[idx].block_id;
        rect.set_right(ui.max_rect().right());
        if !ui.rect_contains_pointer(rect) {
            return;
        }
        let hover = ui.interact(
            rect,
            Id::unique(("row-edited", id.as_str())),
            Sense::hover(),
        );
        if hover.hovered() && self.focus.as_ref().is_none_or(|f| f.idx != idx) {
            if let Some(t) = self.block_edited_tip(id, crate::comments::now_secs()) {
                hover.on_hover_text(t);
            }
        }
    }

    /// A table: a cell is edited in place on a click, unless the table is
    /// locked (synced) or the cell is read-only (a mention).
    fn table_ui(&mut self, ui: &mut Ui, idx: usize) -> Vec<Action> {
        let Marker::Table(t) = self.rows[idx].marker.clone() else {
            return Vec::new();
        };
        let (indent, locked) = (26.0 * self.rows[idx].depth as f32, self.rows[idx].locked);
        let mut acts = Vec::new();
        let c = theme::colors(ui);
        let mut cells = Vec::new();
        let grid = ui.horizontal_top(|ui| {
            // the same gutter text rows reserve, so tables share their left edge
            ui.allocate_exact_size(
                egui::vec2(theme::TEXT_INSET - ui.spacing().item_spacing.x, 20.0),
                Sense::hover(),
            );
            ui.add_space(indent);
            egui::ScrollArea::horizontal()
                .id_salt(("table-scroll", self.rows[idx].block_id.clone()))
                .show(ui, |ui| {
                    let header_bg = ui.painter().add(egui::Shape::Noop);
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                        egui::Grid::new(("table", idx))
                            .min_col_width(0.0)
                            .min_row_height(0.0)
                            .show(ui, |ui| {
                                for (ri, r) in t.rows.iter().enumerate() {
                                    for (ci, cell) in r.cells.iter().enumerate() {
                                        let pad = Margin::symmetric(
                                            theme::TABLE_PAD_X,
                                            theme::TABLE_PAD_Y,
                                        );
                                        let rect = Frame::new().inner_margin(pad).show(ui, |ui| {
                                            let editing = self.focus.as_ref().is_some_and(|f| {
                                                f.idx == idx && f.cell == Some((r.id.clone(), ci))
                                            });
                                            if editing {
                                                acts.extend(self.cell_edit(ui));
                                                return;
                                            }
                                            let users = &self.users;
                                            let job = self
                                                .table_jobs
                                                .entry((idx, ri, ci))
                                                .or_insert_with(|| {
                                                    md::rich_job(
                                                        &cell.rich_text,
                                                        15.0,
                                                        false,
                                                        users,
                                                    )
                                                });
                                            let mut l = Label::new(job.clone());
                                            let click = !locked && cell.editable;
                                            if click {
                                                l = l.sense(Sense::click()).selectable(false);
                                            }
                                            let resp = ui.add(l);
                                            if click
                                                && resp
                                                    .on_hover_cursor(egui::CursorIcon::Text)
                                                    .clicked()
                                            {
                                                acts.push(Action::EditCell(r.id.clone(), ci));
                                            }
                                        });
                                        cells.push((ri, ci, rect.response.rect));
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                    // cell borders: every cell padded to its column's width and row's height
                    let (mut cw, mut rh) = (Vec::<f32>::new(), Vec::<f32>::new());
                    for &(ri, ci, r) in &cells {
                        if cw.len() <= ci {
                            cw.resize(ci + 1, 0.0);
                        }
                        if rh.len() <= ri {
                            rh.resize(ri + 1, 0.0);
                        }
                        cw[ci] = cw[ci].max(r.width());
                        rh[ri] = rh[ri].max(r.height());
                    }
                    let Some(&(_, _, first)) = cells.first() else {
                        return;
                    };
                    let stroke = egui::Stroke::new(1.0, c.border);
                    // grid lines drawn once; a ragged row is padded to the widest
                    let (w, h) = (cw.iter().sum::<f32>(), rh.iter().sum::<f32>());
                    let whole = egui::Rect::from_min_size(first.min, egui::vec2(w, h));
                    ui.painter()
                        .rect_stroke(whole, 0.0, stroke, egui::StrokeKind::Inside);
                    let mut x = first.left();
                    for cwi in cw.iter().take(cw.len().saturating_sub(1)) {
                        x += cwi;
                        ui.painter().line_segment(
                            [egui::pos2(x, whole.top()), egui::pos2(x, whole.bottom())],
                            stroke,
                        );
                    }
                    let mut y = first.top();
                    for rhi in rh.iter().take(rh.len().saturating_sub(1)) {
                        y += rhi;
                        ui.painter().line_segment(
                            [egui::pos2(whole.left(), y), egui::pos2(whole.right(), y)],
                            stroke,
                        );
                    }
                    let header = (t.column_header && !rh.is_empty())
                        .then(|| egui::Rect::from_min_size(first.min, egui::vec2(w, rh[0])));
                    if let Some(r) = header {
                        ui.painter().set(
                            header_bg,
                            egui::Shape::rect_filled(
                                r,
                                0.0,
                                c.text.gamma_multiply(theme::TABLE_HEADER_TINT),
                            ),
                        );
                    }
                });
        });
        self.edited_hover(ui, idx, grid.response.rect);
        acts
    }

    /// The row's text: a cached label, or the editor if it has focus. A click
    /// on a link follows it, any other click edits the row.
    fn body(&mut self, ui: &mut Ui, idx: usize, editable: bool) -> Option<Action> {
        if self.focus.as_ref().is_some_and(|f| f.idx == idx) {
            return self.edit_body(ui, idx);
        }
        let (rows, users) = (&self.rows, &self.users);
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
                Marker::TableOfContents => {
                    let mut job = md::rich_job(&r.rich_text, size, bold, users);
                    for s in &mut job.sections {
                        s.format.line_height = Some(size * theme::TOC_LINE);
                    }
                    job
                }
                _ => md::rich_job(&r.rich_text, size, bold, users),
            }
        });
        let linked = rows[idx]
            .rich_text
            .iter()
            .any(|r| links::run_link(r).is_some());
        let mut tinted = job.clone();
        if matches!(rows[idx].marker, Marker::Todo(true)) {
            // a checked item reads as done: muted and struck through
            let weak = ui.visuals().weak_text_color();
            tinted.sections.iter_mut().for_each(|s| {
                s.format.strikethrough = egui::Stroke::new(1.0, weak);
                if s.format.color == Color32::PLACEHOLDER {
                    s.format.color = weak;
                }
            });
        }
        if matches!(rows[idx].marker, Marker::TableOfContents) {
            // at draw time, so a theme change recolours it
            let border = egui::Stroke::new(1.0, theme::colors(ui).border);
            tinted
                .sections
                .iter_mut()
                .for_each(|s| s.format.underline = border);
        }
        if let Some(t) = theme::block_color(&rows[idx].color, ui.visuals().dark_mode).0 {
            md::tint(&mut tinted, t);
        }
        let mut l = Label::new(tinted).wrap();
        if editable || linked {
            l = l.sense(Sense::click()).selectable(false);
        }
        if !linked {
            // display equations sit in the middle of the text column
            let r = if matches!(rows[idx].marker, Marker::Equation) {
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| ui.add(l))
                    .inner
            } else {
                ui.add(l)
            };
            return if editable {
                // the whole line is clickable, not just its text: an empty
                // line has no text to hit
                let (size, _) = look(&rows[idx].marker);
                let line = egui::Rect::from_min_max(
                    r.rect.min,
                    egui::pos2(
                        ui.max_rect().right(),
                        r.rect.bottom().max(r.rect.top() + size * 1.4),
                    ),
                );
                let hit = ui.interact(
                    line,
                    Id::unique(("row-hit", rows[idx].block_id.as_str())),
                    Sense::click(),
                );
                (r.on_hover_cursor(egui::CursorIcon::Text).clicked()
                    || hit.on_hover_cursor(egui::CursorIcon::Text).clicked())
                .then_some(Action::Edit(idx, Grab::End))
            } else {
                None
            };
        }
        // the link under the pointer, found in the label's own layout
        let (pos, galley, r) = l.layout_in_ui(ui);
        if ui.is_rect_visible(r.rect) {
            let color = ui.visuals().text_color();
            ui.painter()
                .add(egui::epaint::TextShape::new(pos, galley.clone(), color));
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
        let text_color = theme::block_color(&self.rows[idx].color, ui.visuals().dark_mode).0;
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
        let mut filtered = false;
        // not focused (e.g. a click elsewhere this frame): leave every key alone
        let focused = ctx.memory(|m| m.has_focus(id));
        if !focused {
        } else if let Some(menu) = f.menu.as_mut() {
            if menu.kind == Kind::TurnInto {
                // Ctrl+/ has no trigger text: what is typed filters the menu
                // instead of going into the row
                let (typed, backspaces) = ctx.input_mut(|i| {
                    let mut typed = String::new();
                    i.events.retain(|e| match e {
                        egui::Event::Text(t) => {
                            typed.push_str(t);
                            false
                        }
                        _ => true,
                    });
                    (
                        typed,
                        i.count_and_consume_key(Modifiers::NONE, Key::Backspace),
                    )
                });
                menu.filter.push_str(&typed);
                for _ in 0..backspaces {
                    close |= menu.filter.pop().is_none();
                }
                filtered = !typed.is_empty() || backspaces > 0;
            }
            let q = menu.query(&f.text, line, col).unwrap_or_default();
            let src = Sources {
                tree: &self.tree,
                recent: &self.recent,
                users: &self.users,
            };
            for (key, ek) in [
                (Key::ArrowUp, EditKey::Up),
                (Key::ArrowDown, EditKey::Down),
                (Key::Escape, EditKey::Escape),
                (Key::Tab, EditKey::Indent),
            ] {
                for _ in 0..take(key) {
                    match menu_step(menu, &menu::entries(menu, &q, src), ek) {
                        Outcome::Pick(p) => picked = Some(p),
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
        let src = Sources {
            tree: &self.tree,
            recent: &self.recent,
            users: &self.users,
        };
        let mut layouter = |ui: &Ui, text: &dyn TextBuffer, wrap: f32| {
            let mut job = md::md_job(text.as_str(), size, bold, dim);
            if let Some(t) = text_color {
                md::tint(&mut job, t);
            }
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
        f.cursor = cur;
        let (line, col) = line_col(&f.text, cur);
        let typed = resp.changed() && f.text.len() > f.prev_len;
        let mut new_cursor = None;

        if let Some(m) = f.menu.as_mut() {
            let n = m
                .query(&f.text, line, col)
                .map(|q| menu::entries(m, &q, src).len());
            match n {
                Some(n) if n > 0 && !close => {
                    m.selected = if resp.changed() || filtered {
                        0
                    } else {
                        m.selected.min(n - 1)
                    };
                }
                _ => f.menu = None, // Esc, no match, or the cursor left the query
            }
        }
        if enter {
            // the query as it stands after this frame's typing
            if let Some(m) = f.menu.as_mut() {
                let q = m.query(&f.text, line, col).unwrap_or_default();
                if let Outcome::Pick(p) = menu_step(m, &menu::entries(m, &q, src), EditKey::Enter) {
                    picked = Some(p);
                }
            }
        }
        let mut convert = None;
        let mut color = None;
        let mut new_page = false;
        let mut upload = false;
        let mut want_users = false;
        let was_picked = picked.is_some();
        let pick_in = picked.and_then(|p| f.menu.take().map(|m| (p, m)));
        if let Some((pick, m)) = pick_in {
            let before = f.text.clone();
            let trigger_chars =
                |text: &str| text[..byte_at(text, m.line, m.trigger)].chars().count();
            match pick {
                Pick::Block(kind) => {
                    if m.kind == Kind::TurnInto {
                        new_cursor = Some(cur);
                    } else {
                        f.text = m.replace(&f.text, col, "");
                        new_cursor = Some(trigger_chars(&f.text));
                    }
                    convert = Some((kind, before));
                }
                Pick::Color(c) => {
                    if m.kind == Kind::TurnInto {
                        new_cursor = Some(cur);
                    } else {
                        f.text = m.replace(&f.text, col, "");
                        new_cursor = Some(trigger_chars(&f.text));
                    }
                    color = Some(c);
                }
                Pick::NewPage => {
                    f.text = m.replace(&f.text, col, "");
                    new_cursor = Some(trigger_chars(&f.text));
                    new_page = true;
                }
                Pick::Upload => {
                    f.text = m.replace(&f.text, col, "");
                    new_cursor = Some(trigger_chars(&f.text));
                    upload = true;
                }
                // a mention goes in as its Markdown token and is committed
                // with the row
                Pick::Insert(md) => {
                    let at = trigger_chars(&f.text) + md.chars().count();
                    f.text = m.replace(&f.text, col, &md);
                    new_cursor = Some(at);
                }
            }
        } else if f.menu.is_none() && typed && !f.code {
            let head = &f.text[..byte_at(&f.text, line, col)];
            // `/` and `@` open after whitespace (not in `a@b.c`), `[[` anywhere
            let after_space = head.chars().rev().nth(1).is_none_or(char::is_whitespace);
            // not inside `inline code`: an odd number of unescaped backticks so far
            let in_code = head
                .char_indices()
                .filter(|&(i, c)| c == '`' && !head[..i].ends_with('\\'))
                .count()
                % 2
                == 1;
            let open = if head.ends_with("[[") && !in_code {
                Some((Kind::PageLink, 2))
            } else if head.ends_with('@') && after_space && !in_code {
                Some((Kind::Mention, 1))
            } else if head.ends_with('/') && after_space {
                Some((Kind::Slash, 1))
            } else {
                None
            };
            if let Some((kind, len)) = open {
                let days = if kind == Kind::Mention {
                    menu::days(self.clock)
                } else {
                    Default::default()
                };
                want_users = kind == Kind::Mention;
                f.menu = Some(Menu::new(kind, line, col - len, days));
            }
        }
        // Markdown shortcut: Space right after `# `, `- `, `1. ` at the row start
        if f.menu.is_none()
            && !was_picked
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
        if let Some(color) = color {
            act = Some(Action::Color {
                text: f.text.clone(),
                color,
            });
        }
        if upload {
            act = Some(Action::Dlg(crate::pages::Dlg::Upload {
                text: String::new(),
            }));
        }
        if new_page {
            act = Some(Action::NewPage(Some(f.text.clone())));
        }
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
            let q = m.query(&f.text, line, col).unwrap_or_default();
            // under the trigger, or the cursor for Ctrl+/
            let at = if m.kind == Kind::TurnInto {
                cur
            } else {
                f.text[..byte_at(&f.text, m.line, m.trigger)]
                    .chars()
                    .count()
            };
            let pos = out.galley_pos
                + out
                    .galley
                    .pos_from_cursor(CCursor::new(at))
                    .left_bottom()
                    .to_vec2();
            menu::show(&ctx, pos, m, &menu::entries(m, &q, src));
        }
        if want_users {
            self.ensure_users();
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
            assert_eq!(w.menu().map(|m| (m.line, m.trigger)), Some((1, 0)));
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
            w.ctx.memory_mut(|m| m.surrender_focus(edit_id("p-0")));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::None, "no pick without focus");
        }

        // ----------------------------------------------- @, [[ and Ctrl+/

        use crate::testkit::add_page;
        use notion_core::ops::OpKind;
        use notion_core::store::Store;
        use serde_json::json;

        /// Page "p" (rows "" and "x"), pages "Alpha" and "Beta"; today is 2026-11-20.
        fn win_with_pages() -> Win {
            let store = Store::open_in_memory().unwrap();
            add_page(&store, "p", None, &["", "x"]);
            add_page(&store, "Alpha", None, &["a"]);
            add_page(&store, "Beta", None, &[]);
            let mut w = Win::new(store);
            w.app.clock = |n| {
                ["2026-11-20", "2026-11-21", "2026-11-19"][if n < 0 { 2 } else { n as usize }]
                    .into()
            };
            w.app.apply(Action::Open("p".into()));
            w.frames(vec![]);
            w
        }

        /// The body of the one queued block update.
        fn queued_body(w: &Win) -> serde_json::Value {
            let ops = w.app.store.queued_ops().unwrap();
            assert_eq!(ops.len(), 1);
            let OpKind::UpdateBlock { body, .. } = &ops[0].kind else {
                panic!()
            };
            body.clone()
        }

        #[test]
        fn slash_menu_takes_german_and_umlaut_free_queries() {
            for (q, want) in [
                ("überschrift 2", Marker::Heading(2)),
                ("ueberschrift 3", Marker::Heading(3)),
                ("liste", Marker::Bullet),
                ("Zitat", Marker::Quote),
            ] {
                let mut w = win(&[""]);
                w.edit(0);
                w.send(Event::Text("/".into()));
                w.send(Event::Text(q.into()));
                w.send(key(Key::Enter));
                assert_eq!(w.app.rows[0].marker, want, "/{q}");
            }
            let mut w = win(&[""]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            w.send(Event::Text("Glei".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::Equation);
        }

        #[test]
        fn at_menu_picks_a_page_mention_with_the_keyboard_alone() {
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("@".into()));
            assert_eq!(w.menu().map(|m| m.kind), Some(Kind::Mention));
            w.send(Event::Text("alp".into()));
            assert_eq!(w.text(), "@alp", "typing filters, the text stays");
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "@[Alpha](page:Alpha)");
            assert!(w.menu().is_none());
            assert_eq!(w.app.rows.len(), 2, "Enter picked, it added no block");
            w.send(key(Key::Escape)); // commits the row
            assert!(w.app.focus.is_none());
            let r = &w.app.rows[0].rich_text;
            assert_eq!(r.len(), 1);
            assert!(matches!(
                &r[0].kind,
                notion_core::model::RichTextKind::Mention(_)
            ));
            let body = queued_body(&w);
            let m = &body["paragraph"]["rich_text"][0];
            assert_eq!(m["type"], "mention");
            assert_eq!(
                m["mention"],
                json!({ "type": "page", "page": { "id": "Alpha" } })
            );
            assert_eq!(m["plain_text"], "Alpha");
        }

        fn cache_users(w: &mut Win) {
            let u = |id: &str, ty: &str, name: &str| {
                notion_core::model::User::from_value(
                    &json!({ "object": "user", "id": id, "type": ty, "name": name }),
                )
                .unwrap()
            };
            w.app
                .store
                .put_users(&[
                    u("b1", "bot", "Zapper"),
                    u("u1", "person", "Zelda Fitz"),
                    u("u2", "person", "Jürgen"),
                ])
                .unwrap();
            w.app.load_users();
        }

        #[test]
        fn at_menu_picks_a_user_mention_with_the_keyboard_alone() {
            let mut w = win_with_pages();
            cache_users(&mut w);
            w.edit(0);
            w.send(Event::Text("@".into()));
            w.send(Event::Text("za".into()));
            assert_eq!(w.text(), "@za", "typing filters, the text stays");
            w.send(key(Key::Enter));
            assert_eq!(
                w.text(),
                "@[Zelda Fitz](user:u1)",
                "persons come before bots"
            );
            assert!(w.menu().is_none());
            w.send(key(Key::Escape)); // commits the row
            let body = queued_body(&w);
            let m = &body["paragraph"]["rich_text"][0];
            assert_eq!(m["mention"]["type"], "user");
            assert_eq!(m["mention"]["user"]["id"], "u1");
            assert_eq!(m["plain_text"], "Zelda Fitz");
            assert!(w.cmds().is_empty(), "users are cached: nothing asked for");
            // the row shows it as @Name
            assert!(w.shown.contains("@Zelda Fitz"), "{}", w.shown);

            // ↓ reaches the bot; umlauts fold
            let mut w = win_with_pages();
            cache_users(&mut w);
            w.edit(0);
            w.send(Event::Text("@".into()));
            w.send(Event::Text("za".into()));
            w.send(key(Key::ArrowDown));
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "@[Zapper](user:b1)");
            w.edit(1);
            w.send(Event::Text(" @".into()));
            w.send(Event::Text("juerg".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "x @[Jürgen](user:u2)");
        }

        #[test]
        fn at_menu_asks_for_users_once_when_none_are_cached() {
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("@".into()));
            assert_eq!(w.cmds(), [Cmd::PullUsers]);
            w.send(key(Key::Escape));
            w.edit(0);
            w.send(Event::Text("@".into()));
            assert!(w.cmds().is_empty(), "one is already out");
            // answered with nothing found: asked again next time, unless users are off
            w.app.sent.clear();
            w.app.users_stop = true;
            w.send(key(Key::Escape));
            w.edit(0);
            w.send(Event::Text("@".into()));
            assert!(w.cmds().is_empty(), "off after a 403");
        }

        #[test]
        fn on_demand_user_pulls_wait_a_minute_and_need_a_named_user() {
            use std::time::{Duration, Instant};
            let stub =
                notion_core::model::User::from_value(&json!({ "object": "user", "id": "u9" }))
                    .unwrap();
            let mut w = win_with_pages();
            // an inline id-only user doesn't make the cache "non-empty"
            w.app.store.put_users(&[stub]).unwrap();
            w.app.load_users();
            assert_eq!(w.app.users.len(), 1);
            w.app.users_at = Some(Instant::now());
            w.app.ensure_users();
            assert!(w.cmds().is_empty(), "pulled a moment ago");
            w.app.users_at = Some(Instant::now() - Duration::from_secs(61));
            w.app.ensure_users();
            assert_eq!(w.cmds(), [Cmd::PullUsers]);
            assert!(w.app.users_at.unwrap().elapsed() < Duration::from_secs(5));
            w.app.sent.clear();
            w.app.ensure_users();
            assert!(w.cmds().is_empty(), "and not again at once");
            // a named user: nothing to ask for, however old the last pull is
            cache_users(&mut w);
            w.app.users_at = Some(Instant::now() - Duration::from_secs(600));
            w.app.ensure_users();
            assert!(w.cmds().is_empty());
        }

        #[test]
        fn at_menu_arrows_pick_a_date_mention() {
            let mut w = win_with_pages();
            w.edit(0);
            w.type_text("see ");
            w.send(Event::Text("@".into()));
            assert!(w.menu().is_some());
            // pages first (p, Alpha, Beta), then Heute, Morgen, Gestern: ↑ wraps to the last
            w.send(key(Key::ArrowUp));
            assert_eq!(w.menu().unwrap().selected, 5);
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "see @[2026-11-19](date:2026-11-19)");
            w.send(key(Key::Escape));
            let body = queued_body(&w);
            assert_eq!(body["paragraph"]["rich_text"][0]["plain_text"], "see ");
            assert_eq!(
                body["paragraph"]["rich_text"][1]["mention"],
                json!({ "type": "date", "date": { "start": "2026-11-19", "end": null } })
            );
            // by name, in either language
            w.edit(1);
            w.send(Event::Text(" @".into()));
            w.send(Event::Text("tomo".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "x @[2026-11-21](date:2026-11-21)");
        }

        #[test]
        fn at_needs_a_space_before_and_closes_like_the_slash_menu() {
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("a@".into()));
            assert!(w.menu().is_none(), "an e-mail address is no mention");
            w.send(Event::Text(" @".into()));
            assert!(w.menu().is_some());
            w.send(key(Key::Backspace));
            assert!(w.menu().is_none(), "backspace past the @ closes it");
            assert_eq!(w.text(), "a@ ");
            w.send(Event::Text("@".into()));
            w.send(key(Key::Escape));
            assert!(w.menu().is_none());
            assert!(w.app.focus.is_some(), "the first Esc only closes the menu");
            w.send(Event::Text("@".into()));
            w.send(Event::Text("zzz".into()));
            assert!(w.menu().is_none(), "no match closes it");
        }

        #[test]
        fn at_and_double_bracket_stay_closed_in_inline_code_and_after_a_space() {
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("`x @".into()));
            assert!(w.menu().is_none(), "inside `code`");
            w.send(Event::Text(" a[[".into()));
            assert!(w.menu().is_none(), "inside `code`");
            w.send(Event::Text("` and @".into()));
            assert!(w.menu().is_some(), "the span is closed again");
            w.send(Event::Text(" ".into()));
            assert!(w.menu().is_none(), "a space right after @ closes it");
            assert_eq!(w.text(), "`x @ a[[` and @ ");
            // an escaped backtick opens no span; `/` is not affected
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("\\` @".into()));
            assert!(w.menu().is_some());
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("`a /".into()));
            assert!(w.menu().is_some(), "the slash menu is unchanged");
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("[[".into()));
            w.send(Event::Text(" ".into()));
            assert!(w.menu().is_none(), "[[ plus a space closes too");
        }

        #[test]
        fn ctrl_period_opens_turn_into_where_ctrl_slash_is_ctrl_shift_7() {
            let mut w = win_with_pages();
            w.edit(1);
            w.press(Key::Period, Modifiers::CTRL);
            assert_eq!(w.menu().map(|m| m.kind), Some(Kind::TurnInto));
            w.send(Event::Text("zitat".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[1].marker, Marker::Quote);
        }

        #[test]
        fn double_bracket_picks_a_page_link_and_lists_pages_only() {
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("[[".into()));
            assert_eq!(w.menu().map(|m| m.kind), Some(Kind::PageLink));
            w.send(Event::Text("heu".into()));
            assert!(w.menu().is_none(), "no dates in the page picker");
            let mut w = win_with_pages();
            w.edit(0);
            w.send(Event::Text("[[".into()));
            w.send(Event::Text("bet".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.text(), "@[Beta](page:Beta)");
            w.send(key(Key::Escape));
            let body = queued_body(&w);
            assert_eq!(
                body["paragraph"]["rich_text"][0]["mention"],
                json!({ "type": "page", "page": { "id": "Beta" } })
            );
        }

        #[test]
        fn a_row_with_a_page_mention_is_editable_and_commits_valid_json() {
            let store = Store::open_in_memory().unwrap();
            add_page(&store, "p", None, &[]);
            let rich = json!([
                { "type": "text", "text": { "content": "go " }, "plain_text": "go ", "annotations": {} },
                { "type": "mention", "mention": { "type": "page", "page": { "id": "Alpha" } },
                  "plain_text": "Alpha", "annotations": { "bold": true }, "href": "https://notion.so/Alpha" },
            ]);
            store
                .replace_children(
                    "p",
                    "p",
                    &[crate::outline::tests::blk(
                        "m",
                        "p",
                        "paragraph",
                        json!({ "rich_text": rich }),
                    )],
                )
                .unwrap();
            let mut w = Win::new(store);
            w.app.apply(Action::Open("p".into()));
            w.frames(vec![]);
            assert!(
                w.app.rows[0].editable,
                "a mention no longer makes the row read-only"
            );
            w.edit(0);
            assert_eq!(w.text(), "go **@[Alpha](page:Alpha)**");
            w.send(key(Key::Escape));
            assert_eq!(w.queued(), 0, "unchanged: nothing written");
            w.edit(0);
            w.type_text("!");
            w.send(key(Key::Escape));
            let body = queued_body(&w);
            let rt = &body["paragraph"]["rich_text"];
            assert_eq!(rt[0]["text"]["content"], "go ");
            assert_eq!(rt[1]["type"], "mention");
            assert_eq!(
                rt[1]["mention"],
                json!({ "type": "page", "page": { "id": "Alpha" } })
            );
            assert_eq!(rt[1]["annotations"]["bold"], true);
            assert_eq!(rt[2]["text"]["content"], "!");
        }

        #[test]
        fn ctrl_slash_turns_the_row_into_a_block_by_keyboard() {
            let mut w = win_with_pages();
            w.edit(1);
            w.press(Key::Slash, Modifiers::CTRL);
            assert_eq!(w.menu().map(|m| m.kind), Some(Kind::TurnInto));
            assert_eq!(w.text(), "x", "the menu opens without touching the text");
            // typing filters the menu, not the row; Backspace edits the filter
            w.send(Event::Text("ueberschrift 2".into()));
            assert_eq!(w.text(), "x");
            assert_eq!(w.menu().unwrap().filter, "ueberschrift 2");
            w.send(key(Key::Backspace));
            w.send(Event::Text("2".into()));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[1].marker, Marker::Heading(2));
            assert_eq!(w.text(), "x");
            assert!(w.menu().is_none());
        }

        #[test]
        fn ctrl_slash_arrows_esc_and_empty_backspace() {
            let mut w = win_with_pages();
            w.edit(0); // an empty row: Backspace must not delete it
            w.press(Key::Slash, Modifiers::CTRL);
            w.send(key(Key::ArrowDown));
            w.send(key(Key::ArrowDown));
            assert_eq!(w.menu().unwrap().selected, 2);
            w.send(key(Key::Backspace));
            assert!(w.menu().is_none(), "Backspace on an empty filter closes it");
            assert_eq!(w.app.rows.len(), 2, "and deleted nothing");
            w.press(Key::Slash, Modifiers::CTRL);
            w.send(Event::Text("code".into()));
            w.send(key(Key::Escape));
            assert!(
                w.menu().is_none() && w.app.focus.is_some(),
                "Esc only closes the menu"
            );
            w.press(Key::Slash, Modifiers::CTRL);
            w.press(Key::Slash, Modifiers::CTRL);
            assert!(w.menu().is_none(), "Ctrl+/ again closes it");
            w.press(Key::Slash, Modifiers::CTRL);
            w.send(Event::Text("liste".into()));
            w.send(key(Key::ArrowDown));
            w.send(key(Key::Enter));
            assert_eq!(
                w.app.rows[0].marker,
                Marker::Number(1),
                "second of Bulleted, Numbered"
            );
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
