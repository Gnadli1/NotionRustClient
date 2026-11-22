//! F5 databases, part 2: board, list, calendar and gallery next to the table.
//! They show the table's cached rows (`Db::rows`, in `Db::view` order: the
//! filter and sort apply) and edit through the same `db_write`, so undo, push
//! and conflicts work as for a cell. A view is `Db::defs[Db::cur]`: the data
//! source's saved views when Notion sent them, plus the local kinds they lack.
//! `Db::lanes` is built by `relane` (filter, view or month change), never per frame.

use eframe::egui::{self, vec2, Align2, FontId, Id, Rect, RichText, Sense, Stroke, Ui};
use notion_core::views::SavedView;

use crate::app::Action;
use crate::grid::{Act, Col, Db};
use crate::theme::Colors;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Table,
    Board,
    List,
    Calendar,
    Gallery,
}

const LOCAL: [(Kind, &str); 5] = [
    (Kind::Table, "Table"),
    (Kind::Board, "Board"),
    (Kind::List, "List"),
    (Kind::Calendar, "Calendar"),
    (Kind::Gallery, "Gallery"),
];

impl Kind {
    pub(crate) fn parse(s: &str) -> Option<Kind> {
        LOCAL
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(s))
            .map(|(k, _)| *k)
    }
}

/// What is stored for a view: "<kind>:<name>" (names may repeat, kinds are few).
pub(crate) fn pref(d: &ViewDef) -> String {
    let k = LOCAL
        .iter()
        .find(|(k, _)| *k == d.kind)
        .map_or("", |(_, n)| n);
    format!("{}:{}", k.to_ascii_lowercase(), d.name)
}

/// The view a stored `pref` means: kind and name, else the first of the kind
/// (an old bare label counts as a kind), else the table.
pub(crate) fn pick(defs: &[ViewDef], pref_s: &str) -> usize {
    let (k, n) = pref_s.split_once(':').unwrap_or((pref_s, pref_s));
    let Some(kind) = Kind::parse(k) else { return 0 };
    defs.iter()
        .position(|d| d.kind == kind && d.name == n)
        .or_else(|| defs.iter().position(|d| d.kind == kind))
        .unwrap_or(0)
}

pub(crate) struct ViewDef {
    pub name: String,
    pub kind: Kind,
}

/// The views to offer: the saved ones (kinds we can draw), then the local
/// kinds they don't cover; without saved views the five local ones.
pub(crate) fn defs(saved: Option<&Vec<SavedView>>) -> Vec<ViewDef> {
    let mut out: Vec<ViewDef> = saved
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some(ViewDef {
                name: if v.name.is_empty() {
                    v.kind.clone()
                } else {
                    v.name.clone()
                },
                kind: Kind::parse(&v.kind)?,
            })
        })
        .collect();
    for (kind, name) in LOCAL {
        if !out.iter().any(|d| d.kind == kind) {
            out.push(ViewDef {
                name: name.into(),
                kind,
            });
        }
    }
    out
}

/// One board column, or one calendar day (`day` 1-based, 0 = no date), or the
/// whole list/gallery. `rows` index `Db::rows`.
pub(crate) struct Lane {
    pub label: String,
    /// The board column's option ("" for No value).
    pub value: String,
    pub day: u32,
    pub rows: Vec<usize>,
}

/// The board's property: the first status, else the first select.
pub(crate) fn group_col(cols: &[Col]) -> Option<usize> {
    ["status", "select"]
        .iter()
        .find_map(|k| cols.iter().position(|c| c.kind == *k))
}

pub(crate) fn date_col(cols: &[Col]) -> Option<usize> {
    cols.iter().position(|c| c.kind == "date")
}

// --- dates: days since 1970-01-01 (proleptic Gregorian) ---

fn days(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((i64::from(m) + 9) % 12) + 2) / 5 + i64::from(d) - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

fn month_len(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days(ny, nm, 1) - days(y, m, 1)) as u32
}

/// 0 = Monday.
fn weekday(y: i32, m: u32, d: u32) -> u32 {
    (days(y, m, d) + 3).rem_euclid(7) as u32
}

/// The (year, month) of the day number `z`.
fn month_of(z: i64) -> (i32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((yoe + era * 400) as i32 + i32::from(m <= 2), m as u32)
}

pub(crate) fn this_month() -> (i32, u32) {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    month_of((s / 86400) as i64)
}

/// The day number of the date part of `s` ("2025-03-04" or a datetime).
fn day_of(s: &str) -> Option<i64> {
    let p = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let (y, m, d) = (p(0..4)?, p(5..7)?, p(8..10)?);
    (s.as_bytes().get(4) == Some(&b'-') && (1..=12).contains(&m) && (1..=31).contains(&d))
        .then(|| days(y as i32, m, d))
}

pub(crate) fn step_month((y, m): (i32, u32), by: i32) -> (i32, u32) {
    let n = y * 12 + m as i32 - 1 + by;
    (n.div_euclid(12), n.rem_euclid(12) as u32 + 1)
}

impl Db {
    pub fn mode(&self) -> Kind {
        self.defs.get(self.cur).map_or(Kind::Table, |d| d.kind)
    }

    /// Rebuilds `lanes` from `view` for the current view kind.
    pub fn relane(&mut self) {
        let order = || self.view.iter().copied();
        let lane = |label: &str, value: &str, day: u32, rows: Vec<usize>| Lane {
            label: label.into(),
            value: value.into(),
            day,
            rows,
        };
        self.lanes = match self.mode() {
            Kind::Table => Vec::new(),
            Kind::List | Kind::Gallery => vec![lane("", "", 0, order().collect())],
            Kind::Board => match group_col(&self.cols) {
                None => Vec::new(),
                Some(g) => {
                    // a value that is no option any more (a stale cache) counts as none
                    let mut v = vec![lane("No value", "", 0, Vec::new())];
                    v.extend(self.cols[g].opts.iter().map(|o| lane(o, o, 0, Vec::new())));
                    for i in order() {
                        let k = self.rows[i].cells[g].as_str();
                        let at = v
                            .iter()
                            .skip(1)
                            .position(|l| l.value == k)
                            .map_or(0, |p| p + 1);
                        v[at].rows.push(i);
                    }
                    v
                }
            },
            Kind::Calendar => match date_col(&self.cols) {
                None => Vec::new(),
                Some(dc) => {
                    let (y, m) = self.month;
                    let (first, n) = (days(y, m, 1), month_len(y, m) as usize);
                    let mut by_day = vec![Vec::new(); n];
                    let mut undated = Vec::new();
                    for i in order() {
                        let d = &self.rows[i].props[&self.cols[dc].name]["date"];
                        let Some(s) = d["start"].as_str().and_then(day_of) else {
                            undated.push(i);
                            continue;
                        };
                        let e = d["end"].as_str().and_then(day_of).unwrap_or(s).max(s);
                        for z in s.max(first)..=e.min(first + n as i64 - 1) {
                            by_day[(z - first) as usize].push(i);
                        }
                    }
                    let mut v: Vec<Lane> = by_day
                        .into_iter()
                        .enumerate()
                        .filter(|(_, r)| !r.is_empty())
                        .map(|(d, r)| lane("", "", d as u32 + 1, r))
                        .collect();
                    v.push(lane("No date", "", 0, undated));
                    v
                }
            },
        };
        self.scroll = vec![(0.0, 0.0); self.lanes.len().max(1)];
    }

    /// (lane, position) of row `id`.
    fn find(&self, id: &str) -> Option<(usize, usize)> {
        self.lanes
            .iter()
            .enumerate()
            .find_map(|(l, ln)| Some((l, ln.rows.iter().position(|&i| self.rows[i].id == id)?)))
    }

    /// Moves the selection by `dr` cards within the lane and `dl` lanes
    /// (empty ones are skipped). The first card if nothing is selected.
    pub fn step_lane(&mut self, dr: isize, dl: isize) {
        let sel = self.sel.as_ref().and_then(|(id, _)| self.find(id));
        let (mut l, p) = match sel {
            Some(x) => x,
            None => match self.lanes.iter().position(|l| !l.rows.is_empty()) {
                Some(l) => (l, 0),
                None => return,
            },
        };
        let mut p = p as isize;
        if sel.is_some() {
            let step = dl.signum();
            let mut want = dl.abs();
            let mut j = l as isize;
            while want > 0 {
                j += step;
                match self.lanes.get(usize::try_from(j).unwrap_or(usize::MAX)) {
                    None => break,
                    Some(n) if n.rows.is_empty() => {}
                    Some(_) => {
                        l = j as usize;
                        want -= 1;
                    }
                }
            }
            p = (p + dr).clamp(0, self.lanes[l].rows.len() as isize - 1);
        }
        let p = (p as usize).min(self.lanes[l].rows.len() - 1);
        self.sel = Some((self.rows[self.lanes[l].rows[p]].id.clone(), 0));
        self.reveal = true;
    }

    /// Moving card `id` `dl` columns over: (the board property, the option to
    /// set, "" for No value). None if there is no such column.
    pub fn move_target(&self, id: &str, dl: isize) -> Option<(usize, String)> {
        let g = group_col(&self.cols).filter(|_| self.mode() == Kind::Board)?;
        let (l, _) = self.find(id)?;
        let to = self.lanes.get(usize::try_from(l as isize + dl).ok()?)?;
        Some((g, to.value.clone()))
    }

    /// "name: value" of the first `n` non-empty properties after the title
    /// (not `skip`).
    fn facts(&self, ri: usize, skip: Option<usize>, n: usize) -> Vec<String> {
        let r = &self.rows[ri];
        (1..self.cols.len())
            .filter(|&c| Some(c) != skip && !r.cells[c].is_empty())
            .take(n)
            .map(|c| r.cells[c].clone())
            .collect()
    }
}

// --- drawing ---

const ROW_H: f32 = 26.0;
const GAP: f32 = 6.0;
const CARD_H: f32 = 72.0;
const LANE_W: f32 = 240.0;
const G_W: f32 = 220.0;
const CAL_H: f32 = 96.0;
const CAL_LINE: f32 = 22.0;
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub(crate) fn body(
    ui: &mut Ui,
    c: &Colors,
    db: &mut Db,
    acts: &mut Vec<Action>,
    media: &crate::media::Media,
) {
    ui.add_space(4.0);
    match db.mode() {
        Kind::Board if group_col(&db.cols).is_none() => {
            ui.label(
                RichText::new("A board needs a status or select property.").color(c.secondary),
            );
        }
        Kind::Calendar if date_col(&db.cols).is_none() => {
            ui.label(RichText::new("A calendar needs a date property.").color(c.secondary));
        }
        Kind::Board => board(ui, c, db, acts),
        Kind::List => list(ui, c, db, acts),
        Kind::Gallery => gallery(ui, c, db, acts, media),
        Kind::Calendar => calendar(ui, c, db, acts),
        Kind::Table => {}
    }
    db.reveal = false;
}

/// The offset that brings item `pos` into view, if it isn't.
fn reveal_off(pos: usize, item_h: f32, (off, h): (f32, f32)) -> Option<f32> {
    let (top, h) = (pos as f32 * item_h, h.max(item_h));
    if top < off {
        Some(top)
    } else if top + item_h > off + h {
        Some(top + item_h - h)
    } else {
        None
    }
}

/// Lane `li` as a virtualised vertical list of items `per` cards wide;
/// `draw` gets the visible range of card positions.
#[allow(clippy::too_many_arguments)]
fn rows_area(
    ui: &mut Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    db: &mut Db,
    li: usize,
    per: usize,
    item_h: f32,
    avail: f32,
    mut draw: impl FnMut(&mut Ui, &Db, std::ops::Range<usize>),
) {
    let len = db.lanes[li].rows.len();
    let sel = db
        .sel
        .as_ref()
        .and_then(|(id, _)| db.find(id))
        .filter(|(l, _)| *l == li)
        .map(|(_, p)| p / per);
    let mut area = egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink([false, false])
        .max_height(avail);
    if let Some(o) = sel
        .filter(|_| db.reveal)
        .and_then(|p| reveal_off(p, item_h, db.scroll[li]))
    {
        area = area.vertical_scroll_offset(o);
    }
    let out = ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let shared: &Db = db;
        area.show_rows(ui, item_h, len.div_ceil(per), |ui, rows| {
            draw(ui, shared, rows.start * per..(rows.end * per).min(len))
        })
    });
    let out = out.inner;
    db.scroll[li] = (out.state.clamped_offset().y, out.inner_rect.height());
}

fn title(db: &Db, ri: usize) -> &str {
    crate::view::untitled(db.rows[ri].cells.first().map_or("", String::as_str))
}

fn clicks(resp: &egui::Response, db: &Db, ri: usize, acts: &mut Vec<Action>) {
    let id = db.rows[ri].id.clone();
    if resp.double_clicked() {
        acts.push(Action::Db(Act::OpenRow(id)));
    } else if resp.clicked() {
        acts.push(Action::Db(Act::Card(id)));
    }
}

fn outline(ui: &Ui, c: &Colors, db: &Db, ri: usize, rect: Rect, hovered: bool) {
    let sel = db.sel.as_ref().is_some_and(|(id, _)| *id == db.rows[ri].id);
    ui.painter()
        .rect_filled(rect, 4.0, if hovered { c.hover } else { c.sidebar });
    let (w, col) = if sel {
        (2.0, c.accent)
    } else {
        (1.0, c.border)
    };
    ui.painter()
        .rect_stroke(rect, 4.0, Stroke::new(w, col), egui::StrokeKind::Inside);
}

/// A card: title, then up to `lines` properties.
#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    c: &Colors,
    db: &Db,
    ri: usize,
    rect: Rect,
    salt: u32,
    lines: usize,
    skip: Option<usize>,
    acts: &mut Vec<Action>,
) {
    let resp = ui.interact(
        rect,
        Id::unique(("card", &db.rows[ri].id, salt)),
        Sense::click(),
    );
    outline(ui, c, db, ri, rect, resp.hovered());
    let p = ui.painter().with_clip_rect(rect.shrink(2.0));
    p.text(
        rect.left_top() + vec2(8.0, 6.0),
        Align2::LEFT_TOP,
        title(db, ri),
        FontId::proportional(14.0),
        c.text,
    );
    for (i, f) in db.facts(ri, skip, lines).iter().enumerate() {
        p.text(
            rect.left_top() + vec2(8.0, 26.0 + 15.0 * i as f32),
            Align2::LEFT_TOP,
            f,
            FontId::proportional(12.0),
            c.secondary,
        );
    }
    clicks(&resp, db, ri, acts);
}

/// One line: the title, and (`facts`) the first properties after it.
#[allow(clippy::too_many_arguments)]
fn line(
    ui: &mut Ui,
    c: &Colors,
    db: &Db,
    ri: usize,
    rect: Rect,
    salt: u32,
    facts: bool,
    acts: &mut Vec<Action>,
) {
    let resp = ui.interact(
        rect,
        Id::unique(("line", &db.rows[ri].id, salt)),
        Sense::click(),
    );
    outline(ui, c, db, ri, rect, resp.hovered());
    let p = ui.painter().with_clip_rect(rect.shrink2(vec2(2.0, 0.0)));
    p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        title(db, ri),
        FontId::proportional(13.0),
        c.text,
    );
    if facts {
        let f = db.facts(ri, None, 4).join("  ·  ");
        p.text(
            rect.left_center() + vec2((rect.width() * 0.4).max(200.0), 0.0),
            Align2::LEFT_CENTER,
            f,
            FontId::proportional(12.0),
            c.secondary,
        );
    }
    clicks(&resp, db, ri, acts);
}

fn list(ui: &mut Ui, c: &Colors, db: &mut Db, acts: &mut Vec<Action>) {
    let avail = (ui.available_height() - 10.0).max(100.0);
    let w = ui.available_width();
    let salt = ("list", db.ds.clone());
    rows_area(ui, salt, db, 0, 1, ROW_H, avail, |ui, db, r| {
        for pos in r {
            let ri = db.lanes[0].rows[pos];
            let (rect, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
            line(ui, c, db, ri, rect.shrink2(vec2(0.0, 1.0)), 0, true, acts);
        }
    });
}

/// Offset of the k-th visible card (the range starts at a grid row), and the
/// height of `n` visible cards.
fn gallery_at(k: usize, per: usize) -> egui::Vec2 {
    vec2(
        (G_W + GAP) * (k % per) as f32,
        (CARD_H + GAP) * (k / per) as f32,
    )
}

fn gallery_h(n: usize, per: usize) -> f32 {
    (CARD_H + GAP) * n.div_ceil(per) as f32
}

fn gallery(
    ui: &mut Ui,
    c: &Colors,
    db: &mut Db,
    acts: &mut Vec<Action>,
    media: &crate::media::Media,
) {
    let avail = (ui.available_height() - 10.0).max(100.0);
    let per = ((ui.available_width() - 12.0) / (G_W + GAP))
        .floor()
        .max(1.0) as usize;
    let salt = ("gallery", db.ds.clone());
    db.per = per;
    rows_area(ui, salt, db, 0, per, CARD_H + GAP, avail, |ui, db, r| {
        let (rect, _) = ui.allocate_exact_size(
            vec2((G_W + GAP) * per as f32, gallery_h(r.len(), per)),
            Sense::hover(),
        );
        for (k, pos) in r.clone().enumerate() {
            let at = Rect::from_min_size(rect.min + gallery_at(k, per), vec2(G_W, CARD_H));
            let ri = db.lanes[0].rows[pos];
            card(
                ui,
                c,
                db,
                ri,
                at,
                (r.start / per + k / per) as u32,
                3,
                None,
                acts,
            );
            // the cover, as a thumbnail on the card's right (asked for only while the card is drawn)
            if let Some(src) = &db.rows[ri].cover {
                if let crate::media::Shown::Tex(t) =
                    media.show(ui.ctx(), &db.rows[ri].id, true, src)
                {
                    let thumb = Rect::from_min_size(
                        at.right_top() + vec2(-(CARD_H - 8.0) - 4.0, 4.0),
                        vec2(CARD_H - 8.0, CARD_H - 8.0 - GAP),
                    );
                    crate::media::paint_cover(ui, &t, thumb, 4.0);
                }
            }
        }
    });
}

fn board(ui: &mut Ui, c: &Colors, db: &mut Db, acts: &mut Vec<Action>) {
    let avail = (ui.available_height() - 10.0).max(100.0);
    let skip = group_col(&db.cols);
    let ds = db.ds.clone();
    let sel_lane = db
        .sel
        .as_ref()
        .and_then(|(id, _)| db.find(id))
        .map(|(l, _)| l)
        .filter(|_| db.reveal);
    egui::ScrollArea::horizontal()
        .id_salt(("board-h", ds.clone()))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for li in 0..db.lanes.len() {
                    let head = format!("{}  {}", db.lanes[li].label, db.lanes[li].rows.len());
                    let lane = ui.allocate_ui(vec2(LANE_W, avail), |ui| {
                        ui.set_width(LANE_W);
                        ui.label(RichText::new(head).strong());
                        rows_area(
                            ui,
                            ("lane", &ds, li),
                            db,
                            li,
                            1,
                            CARD_H,
                            avail - 24.0,
                            |ui, db, r| {
                                for pos in r {
                                    let (rect, _) = ui.allocate_exact_size(
                                        vec2(LANE_W - 8.0, CARD_H),
                                        Sense::hover(),
                                    );
                                    let at = Rect::from_min_size(
                                        rect.min,
                                        vec2(rect.width(), CARD_H - GAP),
                                    );
                                    card(ui, c, db, db.lanes[li].rows[pos], at, 0, 3, skip, acts);
                                }
                            },
                        );
                    });
                    if sel_lane == Some(li) {
                        ui.scroll_to_rect(lane.response.rect, None);
                    }
                }
            });
        });
}

fn calendar(ui: &mut Ui, c: &Colors, db: &mut Db, acts: &mut Vec<Action>) {
    let (y, m) = db.month;
    ui.horizontal(|ui| {
        for (glyph, by) in [("◀", -1), ("▶", 1)] {
            if ui.button(glyph).clicked() {
                acts.push(Action::Db(Act::Month(by)));
            }
            if by == -1 {
                ui.label(RichText::new(format!("{} {y}", MONTHS[m as usize - 1])).strong());
            }
        }
        ui.label(
            RichText::new("PageUp / PageDown")
                .small()
                .color(c.secondary),
        );
    });
    let w = ui.available_width();
    let cw = w / 7.0;
    let (off, n) = (weekday(y, m, 1) as i32, month_len(y, m) as i32);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let (head, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        for (i, d) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
            .iter()
            .enumerate()
        {
            ui.painter().text(
                head.left_center() + vec2(cw * (i as f32 + 0.5), 0.0),
                Align2::CENTER_CENTER,
                d,
                FontId::proportional(12.0),
                c.secondary,
            );
        }
        for wk in 0..(off + n + 6) / 7 {
            let (row, _) = ui.allocate_exact_size(vec2(w, CAL_H), Sense::hover());
            for dow in 0..7 {
                let d = wk * 7 + dow - off + 1;
                if d < 1 || d > n {
                    continue;
                }
                let cell =
                    Rect::from_min_size(row.min + vec2(cw * dow as f32, 0.0), vec2(cw, CAL_H));
                ui.painter().rect_stroke(
                    cell,
                    0.0,
                    Stroke::new(1.0, c.border),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    cell.left_top() + vec2(4.0, 2.0),
                    Align2::LEFT_TOP,
                    d.to_string(),
                    FontId::proportional(11.0),
                    c.secondary,
                );
                let Some(lane) = db.lanes.iter().find(|l| l.day == d as u32) else {
                    continue;
                };
                // the first three; the selected one takes the third place if it is further down
                let mut vis: Vec<usize> = lane.rows.iter().copied().take(3).collect();
                if let Some(&s) = lane
                    .rows
                    .iter()
                    .skip(3)
                    .find(|&&i| db.sel.as_ref().is_some_and(|(id, _)| *id == db.rows[i].id))
                {
                    vis[2] = s;
                }
                for (k, &ri) in vis.iter().enumerate() {
                    let r = Rect::from_min_size(
                        cell.min + vec2(2.0, 16.0 + CAL_LINE * k as f32),
                        vec2(cw - 4.0, CAL_LINE - 2.0),
                    );
                    line(ui, c, db, ri, r, d as u32, false, acts);
                }
                if lane.rows.len() > 3 {
                    let more = format!("+{} more", lane.rows.len() - 3);
                    ui.painter().text(
                        cell.left_bottom() + vec2(6.0, -3.0),
                        Align2::LEFT_BOTTOM,
                        more,
                        FontId::proportional(11.0),
                        c.secondary,
                    );
                }
            }
        }
    });
    let li = db.lanes.len() - 1;
    ui.add_space(8.0);
    ui.label(RichText::new(format!("No date ({})", db.lanes[li].rows.len())).strong());
    let avail = ui.available_height().clamp(60.0, 160.0);
    let (salt, w) = (("undated", db.ds.clone()), ui.available_width());
    rows_area(ui, salt, db, li, 1, ROW_H, avail, |ui, db, r| {
        for pos in r {
            let (rect, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
            line(
                ui,
                c,
                db,
                db.lanes[li].rows[pos],
                rect.shrink2(vec2(0.0, 1.0)),
                0,
                true,
                acts,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_math_covers_boundaries_leap_years_and_offsets() {
        assert_eq!(
            (
                month_len(2024, 2),
                month_len(2025, 2),
                month_len(1900, 2),
                month_len(2000, 2)
            ),
            (29, 28, 28, 29)
        );
        assert_eq!((month_len(2025, 12), month_len(2025, 4)), (31, 30));
        assert_eq!(days(1970, 1, 1), 0);
        assert_eq!(weekday(2025, 3, 31), 0, "a Monday");
        assert_eq!(weekday(2024, 2, 29), 3, "a Thursday");
        let z = days(2024, 2, 29);
        assert_eq!(month_of(z), (2024, 2));
        assert_eq!(month_of(z + 1), (2024, 3));
        assert_eq!(month_of(days(2025, 12, 31)), (2025, 12));
        assert_eq!(month_of(days(2026, 1, 1)), (2026, 1));
        assert_eq!(month_of(days(1969, 12, 31)), (1969, 12));
        // the date part of a datetime wins over its offset
        assert_eq!(
            day_of("2025-03-04T23:30:00.000-08:00"),
            Some(days(2025, 3, 4))
        );
        assert_eq!(day_of("2025-03-04T00:10:00+09:00"), Some(days(2025, 3, 4)));
        assert_eq!(day_of("2025-03-04"), Some(days(2025, 3, 4)));
        for bad in [
            "",
            "2025",
            "2025-13-01",
            "2025-00-10",
            "2025-01-00",
            "2025/01/01",
            "abcd-ef-gh",
        ] {
            assert_eq!(day_of(bad), None, "{bad}");
        }
        assert_eq!(step_month((2025, 1), -13), (2023, 12));
        assert_eq!(step_month((2025, 6), 0), (2025, 6));
        assert_eq!(step_month((2025, 11), 26), (2028, 1));
    }

    #[test]
    fn gallery_cards_wrap_into_grid_rows() {
        // 7 visible cards, 3 per row: rows 0, 0, 0, 1, 1, 1, 2
        let at = |k| gallery_at(k, 3);
        assert_eq!((at(0).x, at(0).y), (0.0, 0.0));
        assert_eq!((at(2).x, at(2).y), (2.0 * (G_W + GAP), 0.0));
        assert_eq!((at(3).x, at(3).y), (0.0, CARD_H + GAP));
        assert_eq!((at(6).x, at(6).y), (0.0, 2.0 * (CARD_H + GAP)));
        assert_eq!(gallery_h(7, 3), 3.0 * (CARD_H + GAP));
        assert_eq!(gallery_h(2, 3), CARD_H + GAP);
    }
}
