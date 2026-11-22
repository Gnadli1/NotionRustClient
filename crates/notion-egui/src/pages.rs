//! F3 pages: new subpage, icon, cover, move, trash and restore, lock. Every
//! change is a local edit through core (queued, pushed with Ctrl+S, one undo
//! step). The dialogs are modals whose keys are taken before any widget, like
//! the switcher's.

use eframe::egui::{self, Align2, Color32, Context, Id, Key, Label, Modal, Modifiers, RichText, Sense, TextEdit, Ui, Vec2};
use notion_core::media::{self, MediaKind};
use notion_core::model::{Page, Parent};
use notion_core::sync;

use crate::app::{Action, App};
use crate::keys::take_exact;
use crate::outline;
use crate::theme::Colors;

pub(crate) const EMOJI: [&str; 60] = [
    "📄", "📝", "📚", "📌", "📎", "📅", "📊", "📈", "📁", "📂",
    "💡", "🔥", "⭐", "✅", "❗", "❓", "🎯", "🚀", "🧪", "🔧",
    "💻", "🖥", "📱", "🔒", "🔑", "🌍", "🏠", "🏫", "🎓", "🧠",
    "❤", "💬", "📧", "📞", "🕐", "💰", "🛒", "🍎", "☕", "🍕",
    "🎵", "🎮", "🎬", "📷", "🎨", "✏", "🔬", "⚙", "🧩", "🏆",
    "🌱", "🌞", "🌙", "⚡", "❄", "🐱", "🐶", "🦊", "🐍", "🦀",
];
const COLS: usize = 10;

use crate::media::Shown;

pub(crate) enum Dlg {
    Icon { text: String, sel: usize },
    Cover { text: String },
    /// A file path to upload as an image or file block.
    Upload { text: String },
    /// Trash page (id, title): Enter = yes, Esc = no.
    Trash(String, String),
    /// The Trash view: cached pages in the trash, Enter restores the selected one.
    Bin { sel: usize },
}

/// What a dialog decided.
pub(crate) enum Done {
    Close,
    Icon(Option<String>),
    Cover(Option<String>),
    Upload(String),
    Trash(String),
    Restore(String),
}

/// The open page's icon, cover URL and lock, for the header.
#[derive(Default)]
pub(crate) struct Meta {
    pub icon: Option<String>,
    pub cover: Option<String>,
    pub locked: bool,
}

impl Meta {
    pub fn of(p: &Page) -> Meta {
        let s = |v: &serde_json::Value| v.as_str().map(String::from);
        Meta {
            icon: s(&p.raw["icon"]["emoji"]),
            cover: s(&p.raw["cover"]["external"]["url"]).or_else(|| s(&p.raw["cover"]["file"]["url"])),
            locked: p.raw["is_locked"].as_bool().unwrap_or(false),
        }
    }
}

/// The cover (its picture once loaded, else a band in a colour from the host),
/// the icon and the lock mark.
pub(crate) fn header(ui: &mut Ui, c: &Colors, m: &Meta, pic: Option<Shown>) -> Option<Action> {
    let mut act = None;
    if let (Some(url), Some(Shown::Tex(t))) = (&m.cover, &pic) {
        let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 200.0), Sense::click());
        crate::media::paint_cover(ui, t, rect, 6.0);
        if r.on_hover_text("Click to change the cover").clicked() {
            act = Some(Action::Dlg(Dlg::Cover { text: url.clone() }));
        }
        ui.add_space(6.0);
    } else if let Some(url) = &m.cover {
        let host = sync::cover_host(url).unwrap_or_else(|_| "image".into());
        let hue = host.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
        let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 72.0), Sense::click());
        let fill: Color32 = egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.35, if ui.visuals().dark_mode { 0.35 } else { 0.9 }, 1.0).into();
        ui.painter().rect_filled(rect, 6.0, fill);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("Cover: {host}  (click to change)"),
            egui::FontId::proportional(13.0),
            c.secondary,
        );
        if r.on_hover_text(url).clicked() {
            act = Some(Action::Dlg(Dlg::Cover { text: url.clone() }));
        }
        ui.add_space(6.0);
    }
    ui.horizontal(|ui| {
        let icon = m.icon.as_deref().unwrap_or("＋");
        let size = if m.icon.is_some() { 44.0 } else { 16.0 };
        let l = Label::new(RichText::new(icon).size(size).color(if m.icon.is_some() { c.text } else { c.secondary })).sense(Sense::click());
        if ui.add(l).on_hover_text("Page icon (Ctrl+Shift+I); cover: Ctrl+Shift+C").clicked() {
            act = Some(Action::Dlg(Dlg::Icon { text: String::new(), sel: 0 }));
        }
        if m.locked {
            ui.label(RichText::new("🔒 locked").small().color(c.secondary));
        }
    });
    act
}

impl App {
    /// The open page's cover picture, if it has a cover.
    pub(crate) fn cover_shown(&self, ui: &Ui, id: &str) -> Option<Shown> {
        self.meta.cover.as_ref()?;
        let src = self.media.src(&self.store, id, true)?;
        Some(self.media.show(ui.ctx(), id, true, &src))
    }

    /// Runs a page edit as one undo step. Not while a push runs.
    fn page_edit(&mut self, page: Option<String>, also: Vec<String>, f: impl FnOnce(&notion_core::store::Store, &str) -> notion_core::Result<i64>) -> bool {
        let Some(id) = page.or_else(|| self.current.clone()) else { return false };
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return false;
        }
        if !self.flush() {
            return false;
        }
        let r = self.step_page([vec![id.clone()], also].concat(), |s| f(&s.store, &id));
        self.refresh();
        match r {
            Ok(_) => true,
            Err(e) => {
                self.last_error = Some(e.to_string());
                false
            }
        }
    }

    /// Ctrl+N and `/page`: a new subpage of the open page, opened with its
    /// title in edit mode. `text` is the editor's text with the `/page`
    /// trigger removed: it is written first and the new page goes under that
    /// row's top-level block (Notion may put it last when it is pushed).
    pub(crate) fn new_page(&mut self, text: Option<String>) {
        let Some(parent) = self.current.clone() else {
            self.last_error = Some("open a page first: pages are created inside a page (an integration can't create workspace pages)".into());
            return;
        };
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let at = self.focus.as_ref().filter(|f| f.cell.is_none()).map(|f| f.idx);
        if let (Some(t), Some(f)) = (text, &mut self.focus) {
            f.text = t;
        }
        if !self.flush() {
            return;
        }
        let after = at.and_then(|i| {
            let top = outline::ancestors(&self.rows, i).last().copied().unwrap_or(&self.rows[i].block_id).to_string();
            self.store.resolve_id(&top).ok()
        });
        let made = self.step(|s| {
            let id = sync::local_create_page(&s.store, &parent, after.as_deref(), "")?;
            s.commit();
            s.pending_open = None;
            s.set_current(&id);
            s.refresh();
            s.reveal_current();
            s.start_title();
            Ok::<_, notion_core::Error>(())
        });
        if let Err(e) = made {
            self.last_error = Some(e.to_string());
        }
    }

    /// Queues the file at `path` as an image (png, jpg, gif, webp) or file
    /// block after the edited row's top-level block, else at the end of the page.
    pub(crate) fn upload_media(&mut self, path: &str) -> bool {
        let Some(parent) = self.current.clone() else {
            self.last_error = Some("open a page first".into());
            return false;
        };
        if self.db.is_some() {
            self.last_error = Some("close the database view to upload a file".into());
            return false;
        }
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return false;
        }
        let at = self.focus.as_ref().filter(|f| f.cell.is_none()).map(|f| f.idx);
        if !self.flush() {
            return false;
        }
        let after = at.and_then(|i| {
            let top = outline::ancestors(&self.rows, i).last().copied().unwrap_or(&self.rows[i].block_id).to_string();
            self.store.resolve_id(&top).ok()
        });
        let p = path.trim().trim_matches(['"', '\'']);
        let p = match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => std::path::Path::new(&home).join(rest),
            _ => p.into(),
        };
        let kind = match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("png" | "jpg" | "jpeg" | "gif" | "webp") => MediaKind::Image,
            _ => MediaKind::File,
        };
        let made = self.step(|s| media::local_create_media(&s.store, &parent, after.as_deref(), &p, kind));
        self.refresh();
        match made {
            Ok(_) => {
                self.info = Some("queued (Ctrl+S to upload and push)".into());
                true
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
                false
            }
        }
    }

    pub(crate) fn set_icon(&mut self, emoji: Option<String>) -> bool {
        self.page_edit(None, vec![], |st, id| sync::local_set_icon(st, id, emoji.as_deref()))
    }

    pub(crate) fn set_cover(&mut self, url: Option<String>) -> bool {
        self.page_edit(None, vec![], |st, id| sync::local_set_cover(st, id, url.as_deref()))
    }

    pub(crate) fn toggle_lock(&mut self) {
        let lock = !self.meta.locked;
        self.page_edit(None, vec![], |st, id| sync::local_set_locked(st, id, lock));
    }

    /// Queues the trashing of `id` and leaves it: its parent if cached, else
    /// the tab closes.
    fn trash_page(&mut self, id: String) {
        let parent = match self.store.get_page(&id) {
            Ok(Some(Page { parent: Parent::Page(p) | Parent::DataSource { id: p, .. }, .. })) => Some(p),
            _ => None,
        };
        if !self.page_edit(Some(id.clone()), vec![], |st, id| sync::local_trash_page(st, id, true)) {
            return;
        }
        self.info = Some("trashed: restore it from the Trash view".into());
        if self.current.as_deref() != Some(id.as_str()) {
            return;
        }
        match parent.filter(|p| self.cached(p)) {
            Some(p) => self.open_or_pull(&p),
            None if self.tabs.len() > 1 => self.close_tab(self.active),
            None => {
                self.current = None;
                self.refresh();
            }
        }
    }

    fn restore_page(&mut self, id: String) {
        if self.page_edit(Some(id), vec![], |st, id| sync::local_trash_page(st, id, false)) {
            self.info = Some("restored".into());
        }
    }

    /// The picked page is the new parent of `page` (the switcher's move mode).
    pub(crate) fn move_page(&mut self, page: &str, parent: &str) {
        // the page's rows in the old and the new parent move with it
        let old = match self.store.get_page(page) {
            Ok(Some(Page { parent: Parent::Page(p), .. })) => vec![p, parent.to_string()],
            _ => vec![parent.to_string()],
        };
        if self.page_edit(Some(page.to_string()), old, |st, id| sync::local_move_page(st, id, parent)) {
            self.info = Some("moved (Ctrl+S to push)".into());
            self.reveal_current();
        }
    }

    /// A key-map command of the pages feature (see `run_shortcut`).
    pub(crate) fn page_command(&mut self, c: crate::keys::Command) {
        use crate::keys::Command as C;
        if self.db.is_some() {
            return self.db_command(c);
        }
        if c == C::NewPage {
            return self.new_page(None);
        }
        if !self.commit() {
            return;
        }
        let Some(id) = self.current.clone() else {
            if c == C::TrashView {
                self.dlg = Some(Dlg::Bin { sel: 0 });
            } else {
                self.info = Some("open a page first".into());
            }
            return;
        };
        match c {
            C::TrashView => self.dlg = Some(Dlg::Bin { sel: 0 }),
            C::PageIcon => self.dlg = Some(Dlg::Icon { text: String::new(), sel: 0 }),
            C::PageCover => self.dlg = Some(Dlg::Cover { text: self.meta.cover.clone().unwrap_or_default() }),
            C::UploadFile => self.dlg = Some(Dlg::Upload { text: String::new() }),
            C::ToggleLock => self.toggle_lock(),
            C::TrashPage if id.starts_with("local-") => {
                self.last_error = Some("this page isn't pushed yet: push it (Ctrl+S) first".into());
            }
            C::TrashPage => self.dlg = Some(Dlg::Trash(id, self.title.clone())),
            C::MovePage => {
                self.open_switcher();
                if let Some(sw) = &mut self.switcher {
                    sw.moving = Some(id);
                }
            }
            _ => {}
        }
    }

    /// Applies what a dialog decided.
    pub(crate) fn dlg_done(&mut self, d: Done) {
        let ok = match d {
            Done::Close => true,
            Done::Icon(e) => self.set_icon(e),
            Done::Cover(u) => self.set_cover(u),
            Done::Upload(p) => self.upload_media(&p),
            Done::Trash(id) if self.db_has_row(&id) => {
                self.trash_row(id);
                true
            }
            Done::Trash(id) => {
                self.trash_page(id);
                true
            }
            Done::Restore(id) => {
                self.restore_page(id);
                false // the Trash view stays
            }
        };
        // a refused icon or URL keeps its box open, with the error shown
        if ok {
            self.dlg = None;
        }
    }

    /// The trashed pages the Trash view lists.
    fn bin(&self) -> Vec<Page> {
        self.store.trashed_pages().unwrap_or_default()
    }

    /// The dialog's keys, taken before any widget sees them.
    pub(crate) fn dlg_keys(&mut self, ctx: &Context) -> Option<Done> {
        let bin = self.bin();
        let dlg = self.dlg.as_mut()?;
        let (esc, enter, rm, l, r, u, d) = ctx.input_mut(|i| {
            // a held Enter (key repeats) must not restore page after page
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                take_exact(&mut i.events, Modifiers::NONE, Key::Enter).0 > 0,
                take_exact(&mut i.events, Modifiers::CTRL, Key::Delete).0 > 0,
                i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(Modifiers::NONE, Key::ArrowRight),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
            )
        });
        if esc {
            return Some(Done::Close);
        }
        match dlg {
            Dlg::Icon { text, sel } => {
                let step = r as isize - l as isize + COLS as isize * (d as isize - u as isize);
                *sel = (*sel as isize + step).rem_euclid(EMOJI.len() as isize) as usize;
                if rm {
                    return Some(Done::Icon(None));
                }
                let typed = text.trim();
                enter.then(|| Done::Icon(Some(if typed.is_empty() { EMOJI[*sel] } else { typed }.to_string())))
            }
            Dlg::Cover { text } => {
                if rm {
                    return Some(Done::Cover(None));
                }
                enter.then(|| Done::Cover(Some(text.trim().to_string())))
            }
            Dlg::Upload { text } => enter.then(|| Done::Upload(text.clone())),
            Dlg::Trash(id, _) => enter.then(|| Done::Trash(id.clone())),
            Dlg::Bin { sel } => {
                *sel = (*sel as isize + d as isize - u as isize).clamp(0, bin.len().max(1) as isize - 1) as usize;
                let id = bin.get(*sel).map(|p| p.id.clone());
                id.filter(|_| enter).map(Done::Restore)
            }
        }
    }

    /// Draws the dialog; its mouse result.
    pub(crate) fn dlg_ui(&mut self, ctx: &Context, c: &Colors) -> Option<Done> {
        let bin = self.bin();
        let dlg = self.dlg.as_mut()?;
        let mut out = None;
        let id = Id::unique("page_dialog");
        let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 120.0));
        let modal = Modal::new(id).area(area).show(ctx, |ui| {
            ui.set_width(460.0);
            match dlg {
                Dlg::Icon { text, sel } => {
                    ui.label(RichText::new("Page icon: arrows + Enter, or type any emoji. Ctrl+Delete removes it.").small().color(c.secondary));
                    let f = ui.add(TextEdit::singleline(text).hint_text("Any emoji…").desired_width(f32::INFINITY));
                    if !f.has_focus() {
                        f.request_focus();
                    }
                    for (row, chunk) in EMOJI.chunks(COLS).enumerate() {
                        ui.horizontal(|ui| {
                            for (col, e) in chunk.iter().enumerate() {
                                let i = row * COLS + col;
                                let r = ui.selectable_label(i == *sel, RichText::new(*e).size(22.0));
                                if r.clicked() {
                                    out = Some(Done::Icon(Some(e.to_string())));
                                }
                            }
                        });
                    }
                    if ui.button("Remove").clicked() {
                        out = Some(Done::Icon(None));
                    }
                }
                Dlg::Cover { text } => {
                    ui.label(RichText::new("Cover: an http(s) image URL. Enter sets it.").small().color(c.secondary));
                    let f = ui.add(TextEdit::singleline(text).hint_text("https://…").desired_width(f32::INFINITY));
                    if !f.has_focus() {
                        f.request_focus();
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Remove").clicked() {
                            out = Some(Done::Cover(None));
                        }
                    });
                }
                Dlg::Upload { text } => {
                    ui.label(RichText::new("Upload an image or file (up to 20 MB): type or paste its path. Enter uploads.").small().color(c.secondary));
                    let f = ui.add(TextEdit::singleline(text).hint_text("/home/you/picture.png").desired_width(f32::INFINITY));
                    if !f.has_focus() {
                        f.request_focus();
                    }
                }
                Dlg::Trash(id, title) => {
                    ui.label(RichText::new(format!("Trash '{}'? Enter = yes, Esc = no", crate::view::untitled(title))).size(16.0));
                    if ui.button("Trash").clicked() {
                        out = Some(Done::Trash(id.clone()));
                    }
                }
                Dlg::Bin { sel } => {
                    ui.label(RichText::new("Trash: Up/Down, Enter = restore, Esc closes").small().color(c.secondary));
                    if bin.is_empty() {
                        ui.label("Nothing in the trash.");
                    }
                    for (i, p) in bin.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let label = format!("{} {}", Meta::of(p).icon.as_deref().unwrap_or("📄"), crate::view::untitled(&p.title()));
                            if ui.selectable_label(i == *sel, label).clicked() {
                                *sel = i;
                            }
                            if ui.button("Restore").clicked() {
                                out = Some(Done::Restore(p.id.clone()));
                            }
                        });
                    }
                }
            }
        });
        out.or_else(|| modal.backdrop_response.clicked().then_some(Done::Close))
    }
}

impl App {
    /// A never-created page was discarded: its tabs go to `up` (its parent), or empty.
    pub(crate) fn leave_gone(&mut self, id: &str, up: Option<String>) {
        for t in &mut self.tabs {
            if t.page.as_deref() == Some(id) {
                t.page = up.clone();
            }
        }
        self.recent.retain(|r| r != id);
        if self.current.as_deref() == Some(id) {
            self.current = up;
        }
    }

    /// After a push: a new page's `local-…` id is its real one (the pages
    /// were re-keyed in the store).
    pub(crate) fn remap_ids(&mut self) {
        let store = &self.store;
        let r = |id: &str| if id.starts_with("local-") { store.resolve_id(id).unwrap_or_else(|_| id.into()) } else { id.into() };
        self.current = self.current.take().map(|p| r(&p));
        for t in &mut self.tabs {
            t.page = t.page.take().map(|p| r(&p));
        }
        self.recent.iter_mut().for_each(|p| *p = r(p));
        self.tree_open = self.tree_open.drain().map(|p| r(&p)).collect();
        self.hists.iter_mut().for_each(|h| h.remap(r));
        if let Some(d) = &mut self.db {
            d.remap(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{add_page, key, Win};
    use crate::worker::Event as WEvent;
    use eframe::egui::Event;
    use notion_core::ops::OpKind;
    use notion_core::store::Store;

    const CS: Modifiers = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };

    fn child_rows(w: &Win, parent: &str) -> Vec<String> {
        w.app.store.children(parent).unwrap().into_iter().map(|b| b.id).collect()
    }

    #[test]
    fn ctrl_n_opens_a_new_subpage_with_its_title_in_edit_mode_and_undo_removes_it() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::N, Modifiers::CTRL);
        let id = w.app.current.clone().unwrap();
        assert!(id.starts_with("local-"));
        assert!(w.app.title_edit.is_some(), "the title is being edited");
        assert_eq!(child_rows(&w, "p"), ["p-0", id.as_str()], "a child_page row");
        let ops = w.app.store.queued_ops().unwrap();
        assert!(matches!(&ops[..], [o] if matches!(&o.kind, OpKind::CreatePage { parent_id, .. } if parent_id == "p")));
        // typing the title rewrites the one create
        w.type_text("Ideas");
        w.frames(vec![key(Key::Enter)]);
        w.frames(vec![]);
        assert_eq!(w.app.title, "Ideas");
        assert_eq!(w.queued(), 1);
        // undo: the title, then the page; back on the parent
        w.press(Key::Z, Modifiers::CTRL);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.current.as_deref(), Some("p"));
        assert_eq!(w.queued(), 0);
        assert_eq!(child_rows(&w, "p"), ["p-0"]);
        assert!(w.app.store.get_page(&id).unwrap().is_none());
        w.press(Key::Z, CS);
        assert_eq!(child_rows(&w, "p").len(), 2, "redo brings the row back");
    }

    #[test]
    fn ctrl_n_without_a_page_says_why() {
        let mut w = Win::new(Store::open_in_memory().unwrap());
        w.press(Key::N, Modifiers::CTRL);
        assert!(w.app.last_error.as_deref().unwrap().contains("open a page first"));
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn slash_page_creates_a_child_page_row_at_the_cursor() {
        let mut w = Win::with_texts(&["", "b"]);
        w.edit(0);
        w.send(Event::Text("/".into()));
        w.send(Event::Text("pag".into()));
        assert!(w.menu().is_some());
        w.frames(vec![key(Key::Enter)]);
        w.frames(vec![]);
        let id = w.app.current.clone().unwrap();
        assert!(id.starts_with("local-"), "the new page is open");
        assert!(w.app.title_edit.is_some());
        let rows = w.app.store.children("p").unwrap();
        assert_eq!(rows.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), ["p-0", id.as_str(), "p-1"]);
        assert!(matches!(rows[1].kind, notion_core::model::BlockKind::ChildPage { .. }));
        assert_eq!(w.queued(), 1, "the row text was unchanged: only the create");
        // German alias
        let mut w = Win::with_texts(&[""]);
        w.edit(0);
        w.send(Event::Text("/".into()));
        w.send(Event::Text("seit".into()));
        assert!(w.menu().is_some());
    }

    #[test]
    fn the_emoji_picker_is_keyboard_driven() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::I, CS);
        assert!(matches!(w.app.dlg, Some(Dlg::Icon { .. })));
        for k in [Key::ArrowRight, Key::ArrowRight, Key::ArrowDown] {
            w.press(k, Modifiers::NONE);
        }
        assert!(matches!(w.app.dlg, Some(Dlg::Icon { sel: 12, .. })));
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.dlg.is_none());
        assert_eq!(w.app.meta.icon.as_deref(), Some(EMOJI[12]));
        assert_eq!(w.queued(), 1);
        // Esc changes nothing
        w.press(Key::I, CS);
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.dlg.is_none());
        assert_eq!(w.app.meta.icon.as_deref(), Some(EMOJI[12]));
        // any emoji, typed
        w.press(Key::I, CS);
        w.type_text("🦄");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.meta.icon.as_deref(), Some("🦄"));
        assert_eq!(w.queued(), 1, "one PATCH, merged");
        // Remove
        w.press(Key::I, CS);
        w.press(Key::Delete, Modifiers::CTRL);
        assert_eq!(w.app.meta.icon, None);
        // one undo step each
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.meta.icon.as_deref(), Some("🦄"));
    }

    #[test]
    fn upload_queues_an_image_block_shows_it_and_undoes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pic.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255])).save(&f).unwrap();
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::F, CS);
        w.type_text(&format!("  \"{}\"", f.display()));
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!((w.queued(), w.app.rows.len()), (1, 2));
        assert_eq!(w.app.rows[1].marker, crate::outline::Marker::Unsupported("image".into()));
        w.frames(vec![]);
        w.frames(vec![]);
        let asked: Vec<_> = w.cmds().into_iter().filter(|c| matches!(c, crate::worker::Cmd::Media { page: false, .. })).collect();
        assert_eq!(asked.len(), 1, "the picture is asked for once: {asked:?}");
        // the worker answers with the file itself: no cache, no network
        let crate::worker::Cmd::Media { id, .. } = asked[0].clone() else { unreachable!() };
        w.answer(crate::worker::Event::Media { id, path: Ok(f.clone()) });
        assert!(w.app.sent.iter().all(|c| !matches!(c, crate::worker::Cmd::Media { .. })));
        // a refused file queues nothing and says why
        w.press(Key::F, CS);
        w.type_text("/nonexistent/x.png");
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.last_error.as_deref().is_some_and(|e| e.contains("cannot read")));
        assert_eq!(w.queued(), 1);
        w.press(Key::Escape, Modifiers::NONE);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.queued(), w.app.rows.len()), (0, 1));
    }

    #[test]
    fn cover_takes_http_urls_only_and_shows_the_host() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::C, CS);
        w.type_text("ftp://x.example/a.png");
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.last_error.is_some());
        assert!(matches!(w.app.dlg, Some(Dlg::Cover { .. })), "the box stays open");
        assert_eq!(w.queued(), 0);
        if let Some(Dlg::Cover { text }) = &mut w.app.dlg {
            text.clear();
        }
        w.type_text("https://img.example.com/a.png");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.meta.cover.as_deref(), Some("https://img.example.com/a.png"));
        assert!(w.shown.contains("img.example.com"), "{}", w.shown);
        w.press(Key::C, CS);
        w.press(Key::Delete, Modifiers::CTRL);
        assert_eq!(w.app.meta.cover, None);
    }

    #[test]
    fn trashing_asks_first_esc_does_nothing_and_the_trash_view_restores() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::Backspace, CS);
        w.frames(vec![]);
        assert!(matches!(w.app.dlg, Some(Dlg::Trash(..))));
        assert!(w.shown.contains("Trash 'p'? Enter = yes, Esc = no"), "{}", w.shown);
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.dlg.is_none());
        assert_eq!(w.queued(), 0);
        assert!(!w.app.store.get_page("p").unwrap().unwrap().in_trash);
        assert_eq!(w.app.current.as_deref(), Some("p"));

        w.press(Key::Backspace, CS);
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.store.get_page("p").unwrap().unwrap().in_trash);
        assert!(w.app.tree.is_empty(), "hidden from the sidebar");
        assert_eq!(w.app.current, None, "its tab left the page");
        assert_eq!(w.queued(), 1);

        // the Trash view lists it; Enter restores
        w.press(Key::Delete, CS);
        w.frames(vec![]);
        assert!(matches!(w.app.dlg, Some(Dlg::Bin { .. })));
        assert!(w.shown.contains("📄 p"), "{}", w.shown);
        w.press(Key::Enter, Modifiers::NONE);
        assert!(!w.app.store.get_page("p").unwrap().unwrap().in_trash);
        assert_eq!(w.app.tree.len(), 1);
        assert_eq!(w.queued(), 1, "trash and restore merge into one PATCH");
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.dlg.is_none());
        // undo of the restore does not open the trashed page
        w.press(Key::Z, Modifiers::CTRL);
        assert!(w.app.store.get_page("p").unwrap().unwrap().in_trash);
        assert_eq!(w.app.current, None);
    }

    #[test]
    fn the_sidebar_has_a_trash_entry() {
        let mut w = Win::with_texts(&["a"]);
        assert!(w.shown.contains("Trash"));
        w.app.apply(Action::Dlg(Dlg::Bin { sel: 0 }));
        w.frames(vec![]);
        w.frames(vec![]);
        assert!(w.shown.contains("Nothing in the trash."));
    }

    #[test]
    fn move_to_queues_a_move_and_refuses_the_page_itself_and_its_descendants() {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["a"]);
        add_page(&store, "c", Some("p"), &["x"]);
        add_page(&store, "q", None, &["y"]);
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w.press(Key::P, CS);
        assert_eq!(w.app.switcher.as_ref().and_then(|s| s.moving.clone()).as_deref(), Some("p"));
        w.app.pick("c", false);
        assert!(w.app.last_error.as_deref().unwrap().contains("itself or its own subpages"));
        w.app.last_error = None;
        w.app.move_page("p", "p");
        assert!(w.app.last_error.is_some());
        assert_eq!(w.queued(), 0);
        // a real move
        w.app.last_error = None;
        w.press(Key::P, CS);
        w.app.pick("q", false);
        assert!(w.app.last_error.is_none(), "{:?}", w.app.last_error);
        assert_eq!(w.queued(), 1);
        let p = w.app.store.get_page("p").unwrap().unwrap();
        assert_eq!(p.parent, notion_core::model::Parent::Page("q".into()));
        assert!(w.app.switcher.is_none());
    }

    #[test]
    fn a_pushed_page_gets_its_real_id_everywhere() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::N, Modifiers::CTRL);
        let local = w.app.current.clone().unwrap();
        w.press(Key::S, Modifiers::CTRL);
        w.cmds();
        // the worker created it: the store re-keyed the page
        w.app.store.rename_page(&local, "real1").unwrap();
        w.app.store.map_id(&local, "real1").unwrap();
        w.answer(WEvent::Pushed(Default::default()));
        assert_eq!(w.app.current.as_deref(), Some("real1"));
        assert!(!w.app.recent.contains(&local) && w.app.recent.iter().any(|r| r == "real1"));
        assert_eq!(w.cmds(), [crate::worker::Cmd::Pull { id: "real1".into(), force: true }]);
    }

    #[test]
    fn a_local_page_is_never_polled_or_pulled() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::N, Modifiers::CTRL);
        w.press(Key::R, Modifiers::CTRL);
        assert!(w.cmds().is_empty());
        assert!(w.app.poll_pages().is_empty());
    }

    #[test]
    fn selection_keys_do_not_leak_through_a_dialog() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.app.sel = Some(("p-0".into(), "p-0".into()));
        w.press(Key::Backspace, CS);
        assert!(matches!(w.app.dlg, Some(Dlg::Trash(..))));
        w.press(Key::Delete, Modifiers::NONE);
        assert_eq!(w.app.rows.len(), 2, "Delete deleted nothing");
        assert_eq!(w.queued(), 0);
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.dlg.is_none() && w.app.focus.is_none(), "Enter confirmed the dialog, edited no block");
        assert!(w.app.store.get_page("p").unwrap().unwrap().in_trash);
    }

    #[test]
    fn discarding_a_failed_new_page_leaves_it_without_a_pull() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::N, Modifiers::CTRL);
        let id = w.app.current.clone().unwrap();
        let op = w.app.store.queued_ops().unwrap()[0].id;
        w.app.store.mark_op(op, notion_core::ops::OpStatus::Failed, Some("400")).unwrap();
        w.app.refresh();
        w.cmds();
        w.app.apply(Action::Discard(op));
        assert_eq!(w.app.current.as_deref(), Some("p"), "back on the parent");
        assert!(w.app.store.get_page(&id).unwrap().is_none());
        assert_eq!(child_rows(&w, "p"), ["p-0"]);
        assert!(w.cmds().is_empty(), "no pull of a page Notion never saw");
        assert!(w.app.tabs.iter().all(|t| t.page.as_deref() != Some(id.as_str())));
    }

    #[test]
    fn undo_of_a_move_puts_the_row_back_among_its_siblings() {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["a", "b"]);
        add_page(&store, "q", None, &["y"]);
        add_page(&store, "c", Some("p"), &[]);
        let row = crate::outline::tests::blk("c", "p", "child_page", serde_json::json!({ "title": "c" }));
        let kids = store.children("p").unwrap();
        store.replace_children("p", "p", &[kids[0].clone(), row, kids[1].clone()]).unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w.app.move_page("c", "q");
        assert_eq!(child_rows(&w, "p"), ["p-0", "p-1"]);
        assert_eq!(child_rows(&w, "q"), ["q-0", "c"]);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(child_rows(&w, "p"), ["p-0", "c", "p-1"]);
        assert_eq!(child_rows(&w, "q"), ["q-0"]);
        assert_eq!(w.queued(), 0);
    }

    #[test]
    fn a_held_enter_restores_one_page_only() {
        let store = Store::open_in_memory().unwrap();
        for id in ["p", "q"] {
            add_page(&store, id, None, &["a"]);
            let mut pg = store.get_page(id).unwrap().unwrap();
            pg.in_trash = true;
            store.upsert_page(&pg).unwrap();
        }
        let mut w = Win::new(store);
        w.app.apply(Action::Dlg(Dlg::Bin { sel: 0 }));
        w.frames(vec![]);
        w.frames(vec![key(Key::Enter)]);
        assert_eq!(w.app.store.trashed_pages().unwrap().len(), 1);
        w.repeat(Key::Enter);
        assert_eq!(w.app.store.trashed_pages().unwrap().len(), 1);
    }
}
