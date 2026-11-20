use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{
    self,
    text::{CCursor, CCursorRange, LayoutJob},
    text_edit::TextEditState,
    Color32, Frame, Galley, Id, Key, Label, Margin, Modifiers, RichText, Sense, TextBuffer, Ui,
};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{plain_text, TargetKind};
use notion_core::store::{PageEntry, Store};

use crate::edit::EditKey;
use crate::menu::{menu_step, Outcome};
use crate::outline::{self, Marker, Row};
use crate::slash::{self, Menu};
use crate::{keys, md};

/// Where the cursor goes when a row's editor opens.
#[derive(Clone, Copy)]
enum Grab {
    Start,
    End,
}

/// The row being edited: its Markdown source lives here until Esc or a move
/// commits it back into `rows` (in memory only; nothing touches the store).
struct Focus {
    idx: usize,
    text: String,
    menu: Option<Menu>,
    galley: Option<Arc<Galley>>,
    /// Set until the editor has taken keyboard focus and its cursor.
    grab: Option<Grab>,
    prev_len: usize,
    scroll: bool,
}

enum Action {
    Edit(usize, Grab),
    Blur,
    Open(String),
    Toggle(usize),
    Check(usize),
}

pub struct App {
    store: Store,
    pages: Vec<(PageEntry, usize)>,
    page_id: String,
    title: String,
    rows: Vec<Row>,
    shown: Vec<bool>,
    open: HashSet<String>,
    /// Layout of each read-only row, built once (not per frame).
    jobs: HashMap<usize, LayoutJob>,
    /// Table cells by (row, table row, column).
    table_jobs: HashMap<(usize, usize, usize), LayoutJob>,
    focus: Option<Focus>,
    sidebar: bool,
    keylog: bool,
    error: Option<String>,
}

/// The cached page with the most blocks.
pub fn largest_page(store: &Store) -> Option<String> {
    let counts = store.page_tree().ok()?.into_iter().filter_map(|p| {
        let n = outline::flatten(store, &p.id).ok()?.len();
        Some((n, p.id))
    });
    counts
        .max_by_key(|(n, id)| (*n, std::cmp::Reverse(id.clone())))
        .map(|(_, id)| id)
}

/// Depth-first by title; entries whose parent isn't listed are roots.
fn sorted_tree(entries: Vec<PageEntry>) -> Vec<(PageEntry, usize)> {
    let ids: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
    let mut kids: HashMap<Option<String>, Vec<PageEntry>> = HashMap::new();
    for e in entries {
        let key = e.parent.clone().filter(|p| ids.contains(p) && *p != e.id);
        kids.entry(key).or_default().push(e);
    }
    for v in kids.values_mut() {
        v.sort_by_key(|e| e.title.to_lowercase());
    }
    let mut out = Vec::new();
    let mut stack: Vec<(PageEntry, usize)> = kids
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|e| (e, 0))
        .collect();
    while let Some((e, d)) = stack.pop() {
        if let Some(c) = kids.remove(&Some(e.id.clone())) {
            stack.extend(c.into_iter().rev().map(|k| (k, d + 1)));
        }
        out.push((e, d));
    }
    out
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

fn apply_kind(row: &mut Row, kind: TargetKind) {
    row.marker = match kind {
        TargetKind::Paragraph => Marker::None,
        TargetKind::Heading1 => Marker::Heading(1),
        TargetKind::Heading2 => Marker::Heading(2),
        TargetKind::Heading3 => Marker::Heading(3),
        TargetKind::Bulleted => Marker::Bullet,
        TargetKind::Numbered => Marker::Number(1),
        TargetKind::ToDo => Marker::Todo(false),
        TargetKind::Toggle => Marker::Toggle,
        TargetKind::Quote => Marker::Quote,
        TargetKind::Callout => Marker::Callout("💡".into()),
        TargetKind::Code => Marker::Code(String::new()),
        TargetKind::Divider => Marker::Divider,
        TargetKind::Equation => Marker::Equation,
    };
    row.toggle = kind == TargetKind::Toggle;
}

/// Blur first, then the rest in order: a Blur from the edited row must not be
/// lost to (or undo) another row's action in the same frame.
fn blur_first(acts: &mut [Action]) {
    acts.sort_by_key(|a| !matches!(a, Action::Blur));
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

fn set_cursor(ctx: &egui::Context, id: Id, mut st: TextEditState, at: usize) {
    st.cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
    st.store(ctx, id);
}

/// The nearest shown, editable row above (-1) or below (+1) `idx`.
fn neighbour(rows: &[Row], shown: &[bool], idx: usize, dir: isize) -> Option<usize> {
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
    pub fn new(store: Store, page: Option<String>, keylog: bool) -> Self {
        let pages = sorted_tree(store.page_tree().unwrap_or_default());
        let id = page.or_else(|| largest_page(&store)).unwrap_or_default();
        let mut app = App {
            store,
            pages,
            page_id: String::new(),
            title: String::new(),
            rows: Vec::new(),
            shown: Vec::new(),
            open: HashSet::new(),
            jobs: HashMap::new(),
            table_jobs: HashMap::new(),
            focus: None,
            sidebar: true,
            keylog,
            error: None,
        };
        app.open_page(&id);
        app
    }

    fn open_page(&mut self, id: &str) {
        self.commit();
        self.page_id = id.to_string();
        self.jobs.clear();
        self.table_jobs.clear();
        self.error = None;
        self.title = match self.store.get_page(id) {
            Ok(Some(p)) => p.title(),
            _ => self
                .pages
                .iter()
                .find(|(p, _)| p.id == id)
                .map(|(p, _)| p.title.clone())
                .unwrap_or_default(),
        };
        match outline::flatten(&self.store, id) {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                self.rows.clear();
                self.error = Some(format!("cannot read page {id}: {e}"));
            }
        }
        self.shown = outline::shown(&self.rows, &self.open);
    }

    /// Esc or a move: the edited text goes back into the in-memory row.
    fn commit(&mut self) {
        if let Some(f) = self.focus.take() {
            self.rows[f.idx].rich_text = from_markdown(&f.text);
            self.jobs.remove(&f.idx);
        }
    }

    fn apply(&mut self, act: Action) {
        match act {
            Action::Edit(i, _) | Action::Toggle(i) | Action::Check(i) if i >= self.rows.len() => {}
            Action::Edit(i, grab) => {
                self.commit();
                let text = to_markdown(&self.rows[i].rich_text);
                self.focus = Some(Focus {
                    idx: i,
                    prev_len: text.len(),
                    text,
                    menu: None,
                    galley: None,
                    grab: Some(grab),
                    scroll: true,
                });
            }
            Action::Blur => self.commit(),
            Action::Open(id) => self.open_page(&id),
            Action::Toggle(i) => {
                let id = self.rows[i].block_id.clone();
                if !self.open.remove(&id) {
                    self.open.insert(id);
                }
                self.shown = outline::shown(&self.rows, &self.open);
            }
            Action::Check(i) => {
                if let Marker::Todo(c) = &mut self.rows[i].marker {
                    *c = !*c;
                }
            }
        }
    }

    fn sidebar_ui(&self, ui: &mut Ui) -> Option<String> {
        let mut go = None;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (p, depth) in &self.pages {
                ui.horizontal(|ui| {
                    ui.add_space(12.0 * *depth as f32);
                    let title = if p.title.is_empty() {
                        "Untitled"
                    } else {
                        &p.title
                    };
                    let text = format!(
                        "{} {}",
                        p.icon.as_deref().unwrap_or("📄"),
                        title.replace('\n', " ")
                    );
                    if ui.selectable_label(p.id == self.page_id, text).clicked() {
                        go = Some(p.id.clone());
                    }
                });
            }
        });
        go
    }

    fn page_ui(&mut self, ui: &mut Ui) -> Vec<Action> {
        let mut acts = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let w = ui.available_width();
                let col = w.min(900.0);
                ui.horizontal_top(|ui| {
                    ui.add_space((w - col) / 2.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(col);
                        ui.add_space(16.0);
                        ui.add(Label::new(RichText::new(&self.title).size(38.0).strong()).wrap());
                        ui.add_space(12.0);
                        if let Some(e) = &self.error {
                            ui.colored_label(Color32::RED, e);
                        }
                        for idx in 0..self.rows.len() {
                            if self.shown[idx] {
                                acts.extend(self.row_ui(ui, idx));
                            }
                        }
                        ui.add_space(200.0);
                    });
                });
            });
        acts
    }

    fn row_ui(&mut self, ui: &mut Ui, idx: usize) -> Option<Action> {
        if let Marker::Table(t) = &self.rows[idx].marker {
            let indent = 26.0 * self.rows[idx].depth as f32;
            let table_jobs = &mut self.table_jobs;
            ui.horizontal_top(|ui| {
                ui.add_space(indent);
                egui::Grid::new(("table", idx))
                    .striped(true)
                    .show(ui, |ui| {
                        for (ri, r) in t.rows.iter().enumerate() {
                            for (ci, c) in r.cells.iter().enumerate() {
                                let job = table_jobs
                                    .entry((idx, ri, ci))
                                    .or_insert_with(|| md::rich_job(&c.rich_text, 15.0, false));
                                ui.add(Label::new(job.clone()));
                            }
                            ui.end_row();
                        }
                    });
            });
            return None;
        }
        let (depth, editable, toggle, level) = {
            let r = &self.rows[idx];
            (r.depth, r.editable, r.toggle, r.level)
        };
        let marker = self.rows[idx].marker.clone();
        let (size, _) = look(&marker);
        let grey = ui.visuals().weak_text_color();
        let mut act = None;
        ui.horizontal_top(|ui| {
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
                    if ui.add(Label::new(text).sense(Sense::click())).clicked() && page {
                        act = Some(Action::Open(self.rows[idx].block_id.clone()));
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
                    if ui.add(Label::new(text).sense(Sense::click())).clicked() && !database {
                        act = Some(Action::Open(id.clone()));
                    }
                }
                Marker::LinkCard { kind, url, label } => {
                    ui.label(format!(
                        "🔗 [{kind}] {}",
                        if label.is_empty() { url } else { label }
                    ));
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
        act
    }

    /// The row's text: a cached label, or the editor if it has focus.
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
        let mut l = Label::new(job.clone()).wrap();
        if editable {
            l = l.sense(Sense::click()).selectable(false);
        }
        let r = ui.add(l);
        if editable {
            r.on_hover_cursor(egui::CursorIcon::Text)
                .clicked()
                .then_some(Action::Edit(idx, Grab::End))
        } else {
            None
        }
    }

    fn edit_body(&mut self, ui: &mut Ui, idx: usize) -> Option<Action> {
        let ctx = ui.ctx().clone();
        let id = Id::unique(("edit", idx));
        let dim = ui.visuals().weak_text_color();
        let (size, bold) = look(&self.rows[idx].marker);
        let f = self.focus.as_mut()?;
        let mut act = None;

        if let Some(g) = f.grab.take() {
            ctx.memory_mut(|m| m.request_focus(id));
            let at = if matches!(g, Grab::Start) {
                0
            } else {
                f.text.chars().count()
            };
            set_cursor(&ctx, id, TextEditState::default(), at);
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
            take(Key::Tab);
            if take(Key::Escape) > 0 {
                act = Some(Action::Blur);
            }
            // ↑ on the first visual line / ↓ on the last leaves the row
            if let (Some(g), true) = (&f.galley, ctx.input(|i| i.modifiers.is_none())) {
                let row = g.layout_from_cursor(CCursor::new(cur)).row;
                if row == 0 && ctx.input(|i| i.key_pressed(Key::ArrowUp)) {
                    if let Some(j) = neighbour(&self.rows, &self.shown, idx, -1) {
                        take(Key::ArrowUp);
                        act = Some(Action::Edit(j, Grab::End));
                    }
                } else if row + 1 >= g.rows.len() && ctx.input(|i| i.key_pressed(Key::ArrowDown)) {
                    if let Some(j) = neighbour(&self.rows, &self.shown, idx, 1) {
                        take(Key::ArrowDown);
                        act = Some(Action::Edit(j, Grab::Start));
                    }
                }
            }
        }

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
        if let Some(kind) = picked {
            if let Some(m) = &f.menu {
                f.text = m.strip(&f.text, col);
                new_cursor = Some(f.text[..byte_at(&f.text, m.line, m.slash)].chars().count());
            }
            f.menu = None;
            apply_kind(&mut self.rows[idx], kind);
            self.jobs.remove(&idx);
            if kind == TargetKind::Divider {
                act = Some(Action::Blur);
            }
        } else if f.menu.is_none() && typed && f.text[..byte_at(&f.text, line, col)].ends_with('/')
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
            && f.text[..byte_at(&f.text, line, col)].ends_with(' ')
        {
            if let Some((kind, rest)) = slash::shortcut(&f.text, line, col) {
                f.text = rest;
                new_cursor = Some(0);
                apply_kind(&mut self.rows[idx], kind);
                self.jobs.remove(&idx);
            }
        }
        if let Some(at) = new_cursor {
            out.state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(at))));
            out.state.store(&ctx, id);
        }
        f.prev_len = f.text.len();

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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

impl App {
    /// One frame (split from `App::ui` so tests can drive it without a window).
    fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.keylog {
            ctx.input(|i| keys::log(&i.raw.events));
        }
        let ctrl = ctx.input(|i| i.modifiers.ctrl);
        if ctx.input_mut(|i| keys::take_sidebar_toggle(&mut i.events, ctrl)) {
            self.sidebar = !self.sidebar;
        }
        let mut act = None;
        if self.sidebar {
            let go = egui::Panel::left("tree")
                .size_range(180.0..=420.0)
                .default_size(260.0)
                .show(ui, |ui| self.sidebar_ui(ui))
                .inner;
            act = go.map(Action::Open);
        }
        let page = egui::CentralPanel::default()
            .show(ui, |ui| self.page_ui(ui))
            .inner;
        // a Blur from the edited row must not be lost to another row's action
        // (click on row B while A loses focus): Blur first, then the rest
        let mut acts = page;
        acts.extend(act);
        blur_first(&mut acts);
        for a in acts {
            self.apply(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{blk, rt};
    use serde_json::json;

    fn para(id: &str, parent: &str) -> notion_core::model::Block {
        blk(id, parent, "paragraph", json!({ "rich_text": rt(id) }))
    }

    #[test]
    fn largest_page_wins() {
        let store = Store::open_in_memory().unwrap();
        let sub = |id: &str| blk(id, "root", "child_page", json!({ "title": id }));
        store
            .replace_children("root", "root", &[sub("small"), sub("big")])
            .unwrap();
        store
            .replace_children("small", "small", &[para("a", "small")])
            .unwrap();
        let kids = [para("x", "big"), para("y", "big"), para("z", "big")];
        store.replace_children("big", "big", &kids).unwrap();
        assert_eq!(largest_page(&store).as_deref(), Some("big"));
        assert_eq!(largest_page(&Store::open_in_memory().unwrap()), None);
    }

    #[test]
    fn tree_is_depth_first_by_title() {
        let e = |id: &str, parent: Option<&str>| PageEntry {
            id: id.into(),
            title: id.into(),
            parent: parent.map(Into::into),
            icon: None,
        };
        let t = sorted_tree(vec![
            e("c", Some("a")),
            e("b", None),
            e("a", None),
            e("d", Some("zz")),
        ]);
        let got: Vec<(&str, usize)> = t.iter().map(|(p, d)| (p.id.as_str(), *d)).collect();
        assert_eq!(got, [("a", 0), ("c", 1), ("b", 0), ("d", 0)]);
    }

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
        use egui::{Event, Modifiers, RawInput};

        fn key(k: Key) -> Event {
            Event::Key {
                key: k,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }
        }

        struct Win {
            ctx: egui::Context,
            app: App,
        }

        impl Win {
            fn new(texts: &[&str]) -> Win {
                let store = Store::open_in_memory().unwrap();
                let kids: Vec<_> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        blk(
                            &format!("b{i}"),
                            "p",
                            "paragraph",
                            json!({ "rich_text": rt(t) }),
                        )
                    })
                    .collect();
                store.replace_children("p", "p", &kids).unwrap();
                let ctx = egui::Context::default();
                crate::setup_fonts(&ctx);
                let mut w = Win {
                    ctx,
                    app: App::new(store, Some("p".into()), false),
                };
                w.frames(vec![]);
                w
            }

            fn frames(&mut self, events: Vec<Event>) {
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
                let raw = RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                };
                let app = &mut self.app;
                self.ctx
                    .run_ui(raw, |ui| app.frame(ui))
                    .drop_without_applying_deltas();
            }

            /// Starts editing row `i`, with the cursor at the end.
            fn edit(&mut self, i: usize) {
                self.app.apply(Action::Edit(i, Grab::End));
                self.frames(vec![]);
                self.frames(vec![]);
            }

            fn send(&mut self, ev: Event) {
                self.frames(vec![ev]);
                self.frames(vec![]);
            }

            fn text(&self) -> &str {
                &self.app.focus.as_ref().unwrap().text
            }

            fn menu(&self) -> Option<&Menu> {
                self.app.focus.as_ref().and_then(|f| f.menu.as_ref())
            }
        }

        #[test]
        fn slash_menu_takes_the_keys_and_picks() {
            let mut w = Win::new(&[""]);
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
            let mut w = Win::new(&["abc"]);
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
            let mut w = Win::new(&["x\n äö"]);
            w.edit(0);
            let st = TextEditState::default();
            set_cursor(&w.ctx, Id::unique(("edit", 0)), st, 2); // start of line 1
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
            let mut w = Win::new(&[""]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            w.frames(vec![Event::Text("h1".into()), key(Key::Enter)]);
            assert_eq!(w.app.rows[0].marker, Marker::Heading(1));
            assert_eq!(w.text(), "");
        }

        #[test]
        fn unfocused_editor_leaves_menu_keys_alone() {
            let mut w = Win::new(&["", "b"]);
            w.edit(0);
            w.send(Event::Text("/".into()));
            assert!(w.menu().is_some());
            w.ctx
                .memory_mut(|m| m.surrender_focus(Id::unique(("edit", 0))));
            w.send(key(Key::Enter));
            assert_eq!(w.app.rows[0].marker, Marker::None, "no pick without focus");
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
            let mut w = Win::new(&["", "second"]);
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

    #[test]
    fn slash_pick_changes_the_marker() {
        let mut row = Row {
            block_id: "x".into(),
            parent: "p".into(),
            depth: 0,
            marker: Marker::None,
            rich_text: vec![],
            editable: true,
            locked: false,
            toggle: false,
            level: 0,
        };
        apply_kind(&mut row, TargetKind::Heading2);
        assert_eq!(row.marker, Marker::Heading(2));
        apply_kind(&mut row, TargetKind::Toggle);
        assert!(row.toggle);
    }
}
