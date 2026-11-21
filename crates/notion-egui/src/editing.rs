//! The editor's keys and the edits they make: Enter / Backspace / Tab on the
//! focused row, keymap commands, the inline title, table cells and links.
//! Logic first; the widgets only collect the keys and hand them over as
//! `Action`s, so tests drive it without a window.

use eframe::egui::{
    self, text_edit::TextEditState, Id, Key, Label, Modifiers, RichText, Sense, Ui,
};
use notion_core::markdown::{from_markdown, to_markdown};
use notion_core::model::{BlockKind, TextBlock};
use notion_core::sync;

use crate::app::{neighbour, set_cursor, Action, App, Focus, Grab, TitleEdit};
use crate::edit::{key_action, Caret, EditKey, KeyAction};
use crate::keys::{self, Command};
use crate::links::{self, Link};
use crate::menu::{Kind, Menu};
use crate::outline::{self, Marker, Row};

const CTRL_SHIFT: Modifiers = Modifiers {
    ctrl: true,
    shift: true,
    ..Modifiers::NONE
};

/// The title's text edit id.
fn title_id() -> Id {
    Id::unique("title-edit")
}

/// The edit id of a table cell.
pub(crate) fn cell_id(row: &str, col: usize) -> Id {
    Id::unique(("cell", row, col))
}

/// The cursor (char index) of the text edit `id`, else the end of `text`.
fn cursor_of(ctx: &egui::Context, id: Id, text: &str) -> usize {
    TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map_or_else(|| text.chars().count(), |r| r.primary.index.0)
}

type Target = fn(&[Row], usize) -> Option<(String, Option<String>)>;

impl App {
    // ------------------------------------------------------------- keys

    /// Takes the keys the focused editor (a row or a cell) acts on from this
    /// frame's input, before its TextEdit runs: keymap commands, Enter, Tab,
    /// Backspace on an empty row, Esc, and the TextEdit's own undo keys (the
    /// page's undo is the keymap's). Enter and Backspace ignore key-repeat.
    /// Marks are applied here; the rest comes back as an `Action`.
    pub(crate) fn editor_keys(&mut self, ctx: &egui::Context, id: Id) -> Option<Action> {
        let ctrl = ctx.input(|i| i.modifiers.ctrl);
        let map = &self.keys;
        let f = self.focus.as_mut()?;
        let row = f.cell.is_none();
        let empty = f.text.is_empty();
        let (cmds, [enter, tab, shift_tab, bs, esc]) = ctx.input_mut(|i| {
            let ev = &mut i.events;
            let cmds = keys::take_commands(ev, ctrl, map, |c| !c.is_global());
            let mut take = |m: Modifiers, k: Key| keys::take_exact(ev, m, k).0;
            let n = [
                if row { take(Modifiers::NONE, Key::Enter) } else { 0 },
                take(Modifiers::NONE, Key::Tab),
                take(Modifiers::SHIFT, Key::Tab),
                if row && empty { take(Modifiers::NONE, Key::Backspace) } else { 0 },
                take(Modifiers::NONE, Key::Escape),
            ];
            // unbound, these would type a newline or fire the TextEdit's undo
            for (m, k) in [
                (Modifiers::ALT, Key::Enter),
                (Modifiers::CTRL, Key::Enter),
                (Modifiers::CTRL, Key::Z),
                (Modifiers::CTRL, Key::Y),
                (CTRL_SHIFT, Key::Z),
            ] {
                take(m, k);
            }
            (cmds, n)
        });
        let mut act = None;
        for (n, a) in [
            (esc, Action::Blur),
            (enter, Action::Key(EditKey::Enter)),
            (tab, Action::Key(EditKey::Indent)),
            (shift_tab, Action::Key(EditKey::Outdent)),
            (bs, Action::Key(EditKey::Backspace)),
        ] {
            if n > 0 {
                act = Some(a);
            }
        }
        let turn_into_open = f.menu.as_ref().is_some_and(|m| m.kind == Kind::TurnInto);
        for c in cmds {
            f.menu = None;
            match c {
                // Ctrl+/ again closes it
                Command::TurnInto if row && !turn_into_open => {
                    f.menu = Some(Menu::new(Kind::TurnInto, 0, 0, Default::default()));
                }
                Command::Mark(m) if !f.code => {
                    let end = f.text.chars().count();
                    let sel = TextEditState::load(ctx, id)
                        .and_then(|s| s.cursor.char_range())
                        .map_or((end, end), |r| (r.secondary.index.0, r.primary.index.0));
                    let (text, (at, _)) = keys::wrap(&f.text, sel, m);
                    f.text = text;
                    f.prev_len = f.text.len();
                    set_cursor(ctx, id, TextEditState::default(), at);
                }
                // same path as a `/` pick, same refusals; a cell is no block
                Command::Block(kind) if row => {
                    act = Some(Action::Convert {
                        kind,
                        text: f.text.clone(),
                        typed: f.text.clone(),
                        at: cursor_of(ctx, id, &f.text),
                    });
                }
                Command::Indent => act = Some(Action::Key(EditKey::Indent)),
                Command::Outdent => act = Some(Action::Key(EditKey::Outdent)),
                Command::ToggleCheck => act = Some(Action::Key(EditKey::ToggleCheck)),
                Command::NewBlock => act = Some(Action::Key(EditKey::Enter)),
                _ => {}
            }
        }
        act
    }

    /// A key the editor took (`Action::Key`): one undo step.
    pub(crate) fn on_edit_key(&mut self, k: EditKey) {
        if !self.pushing() && self.focus.is_some() {
            self.step(|s| s.edit_key(k));
        }
    }

    fn edit_key(&mut self, k: EditKey) {
        let Some(f) = &self.focus else { return };
        let action = key_action(k, Caret::new(&f.text, 0));
        // a table cell: only text editing; the block keys commit and leave
        if f.cell.is_some() {
            if matches!(
                action,
                KeyAction::NewBlockAfter | KeyAction::Leave | KeyAction::Indent | KeyAction::Outdent
            ) {
                self.commit();
            }
            return;
        }
        let idx = f.idx;
        match action {
            KeyAction::NewBlockAfter => self.new_block_after(),
            KeyAction::DeleteRow => self.delete_row(idx),
            KeyAction::Indent => self.move_editing(outline::indent_target),
            KeyAction::Outdent => self.move_editing(outline::outdent_target),
            KeyAction::ToggleCheck => self.check(idx),
            _ => {}
        }
    }

    /// Edits the row `id` if it is editable and shown.
    fn focus_block(&mut self, id: &str, grab: Grab) {
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| r.block_id == id && r.editable)
        {
            self.start_edit(i, grab);
        }
    }

    /// Enter: commit, an empty paragraph after the row, edit it.
    fn new_block_after(&mut self) {
        if !self.flush() {
            return;
        }
        let Some(row) = self.focus.as_ref().and_then(|f| self.rows.get(f.idx)).cloned() else {
            return;
        };
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: Vec::new(),
            color: "default".into(),
        });
        let made = self
            .store
            .resolve_id(&row.parent)
            .and_then(|p| Ok((p, self.store.resolve_id(&row.block_id)?)))
            .and_then(|(p, after)| sync::local_create_block(&self.store, &p, Some(&after), para));
        match made {
            Ok(new) => {
                self.focus = None;
                self.refresh();
                self.focus_block(&new, Grab::Start);
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Backspace on an empty row: delete it, edit the previous one. A row
    /// with children stays (never take a subtree with it).
    fn delete_row(&mut self, idx: usize) {
        if self
            .rows
            .get(idx + 1)
            .is_some_and(|r| r.depth > self.rows[idx].depth)
        {
            return;
        }
        let prev = neighbour(&self.rows, &self.shown, idx, -1).map(|i| self.rows[i].block_id.clone());
        let Some(f) = self.focus.take() else { return };
        let res = self
            .store
            .resolve_id(&f.block_id)
            .and_then(|id| sync::local_delete_block(&self.store, &id));
        if let Err(e) = res {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
        if let Some(p) = prev {
            self.focus_block(&p, Grab::End);
        }
    }

    /// Tab / Shift+Tab: commit, then move the edited block to `target`. The
    /// editor stays on it; its id only changes on push.
    fn move_editing(&mut self, target: Target) {
        if !self.flush() {
            return;
        }
        let Some(f) = &self.focus else { return };
        let (idx, id) = (f.idx, f.block_id.clone());
        let Some((parent, after)) = target(&self.rows, idx) else {
            return;
        };
        // push would refuse it anyway (copy-then-delete can't recreate these)
        if let Some(m) = outline::unmovable_in_subtree(&self.rows, idx) {
            self.last_error = Some(format!("can't move: its subtree contains {}", m.label()));
            return;
        }
        if let Err(e) = sync::local_move_block(&self.store, &id, &parent, after.as_deref()) {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
    }

    // ------------------------------------------------------------ title

    /// Click on the title, or ↑ from the first row.
    pub(crate) fn start_title(&mut self) {
        if !self.title_editable || self.title_edit.is_some() || self.pushing() || !self.commit() {
            return;
        }
        let page = self.current.as_ref().and_then(|id| self.store.get_page(id).ok().flatten());
        if let Some(p) = page {
            let text = p.title();
            self.title_edit = Some(TitleEdit {
                original: text.clone(),
                text,
                grab: true,
            });
        }
    }

    /// Writes the title if it changed (one op, merged with a queued title
    /// op by core), keeping the edit open. True if the store changed. A
    /// failed write drops the edit (shown as an error) rather than block
    /// every commit point.
    pub(crate) fn write_title(&mut self) -> bool {
        let Some(t) = &mut self.title_edit else {
            return false;
        };
        if t.text == t.original {
            return false;
        }
        t.original = t.text.clone();
        let text = t.text.clone();
        let Some(id) = self.current.clone() else {
            return false;
        };
        if let Err(e) = sync::local_set_title(&self.store, &id, &text) {
            self.title_edit = None;
            self.last_error = Some(format!("title not saved: {e}"));
        }
        true
    }

    /// The page title: a label, or the text edit while it is being edited.
    pub(crate) fn title_ui(&mut self, ui: &mut Ui, shown: &str) -> Option<Action> {
        let ctx = ui.ctx().clone();
        let Some(t) = &mut self.title_edit else {
            let mut l = Label::new(RichText::new(shown).size(38.0).strong()).wrap();
            if self.title_editable {
                l = l.sense(Sense::click()).selectable(false);
            }
            let r = ui.add(l);
            return (self.title_editable && r.on_hover_cursor(egui::CursorIcon::Text).clicked())
                .then_some(Action::EditTitle);
        };
        let id = title_id();
        if std::mem::take(&mut t.grab) {
            ctx.memory_mut(|m| m.request_focus(id));
            set_cursor(&ctx, id, TextEditState::default(), t.text.chars().count());
        }
        // the page's undo is the keymap's, not the TextEdit's
        ctx.input_mut(|i| {
            for (m, k) in [(Modifiers::CTRL, Key::Z), (Modifiers::CTRL, Key::Y), (CTRL_SHIFT, Key::Z)] {
                keys::take_exact(&mut i.events, m, k);
            }
        });
        let out = egui::TextEdit::singleline(&mut t.text)
            .id(id)
            .font(egui::FontId::new(38.0, crate::md::bold_family()))
            .frame(egui::Frame::NONE)
            .desired_width(f32::INFINITY)
            .show(ui);
        // Enter, Esc, or a click elsewhere
        out.response.lost_focus().then_some(Action::TitleCommit)
    }

    // ------------------------------------------------------ table cells

    /// Click on a table cell: edit it as Markdown, if it is editable.
    pub(crate) fn start_cell(&mut self, row_id: &str, col: usize) {
        if self.pushing() || !self.commit() {
            return;
        }
        let found = self.rows.iter().enumerate().find_map(|(i, r)| match &r.marker {
            Marker::Table(t) if !r.locked => t
                .rows
                .iter()
                .find(|tr| tr.id == row_id)
                .and_then(|tr| tr.cells.get(col))
                .filter(|c| c.editable)
                .map(|c| (i, r.block_id.clone(), to_markdown(&c.rich_text))),
            _ => None,
        });
        let Some((i, table, md)) = found else { return };
        let mut f = Focus::new(i, table, md.clone(), md, false, Grab::End);
        f.cell = Some((row_id.to_string(), col));
        self.focus = Some(f);
    }

    /// Writes the edited cell; false if it failed.
    pub(crate) fn write_cell(&mut self, row: &str, col: usize, text: &str) -> bool {
        match self
            .store
            .resolve_id(row)
            .and_then(|id| sync::local_edit_table_cell(&self.store, &id, col, from_markdown(text)))
        {
            Ok(_) => true,
            Err(e) => {
                self.last_error = Some(e.to_string());
                false
            }
        }
    }

    /// The text edit of the cell being edited.
    pub(crate) fn cell_edit(&mut self, ui: &mut Ui) -> Vec<Action> {
        let ctx = ui.ctx().clone();
        let dim = ui.visuals().weak_text_color();
        let Some((row, col)) = self.focus.as_ref().and_then(|f| f.cell.clone()) else {
            return Vec::new();
        };
        let id = cell_id(&row, col);
        if let Some(f) = self.focus.as_mut() {
            if let Some(g) = f.take_grab() {
                ctx.memory_mut(|m| m.request_focus(id));
                let at = g.index(&f.text);
                set_cursor(&ctx, id, TextEditState::default(), at);
            }
        }
        let mut acts = Vec::new();
        if ctx.memory(|m| m.has_focus(id)) {
            acts.extend(self.editor_keys(&ctx, id));
        }
        let Some(f) = self.focus.as_mut() else {
            return acts;
        };
        let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, _wrap: f32| {
            let job = crate::md::md_job(text.as_str(), 15.0, false, dim);
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let out = egui::TextEdit::singleline(&mut f.text)
            .id(id)
            .desired_width(140.0)
            .event_filter(egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            })
            .layouter(&mut layouter)
            .show(ui);
        f.prev_len = f.text.len();
        if out.response.response.lost_focus() && acts.is_empty() {
            acts.push(Action::Blur);
        }
        acts
    }

    // ------------------------------------------------------------ links

    /// A click on a link: a Notion page opens here (in a new tab for
    /// `new_tab`), a web link goes to the opener, anything else is refused.
    pub(crate) fn open_link(&mut self, link: Link, new_tab: bool) {
        match link {
            Link::Page(id) => self.pick(&id, new_tab),
            Link::Web(url) if !links::allowed_url(&url) => {
                self.last_error = Some(format!("not opening {url}: only http and https links"));
            }
            Link::Web(url) => {
                if let Err(e) = (self.opener)(&url) {
                    self.last_error = Some(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{blk, rt};
    use crate::testkit::{add_page, page_in, Win};
    use eframe::egui::{Event, PointerButton, Pos2};
    use notion_core::model::plain_text;
    use notion_core::ops::OpKind;
    use notion_core::store::Store;
    use notion_core::sync::PullReport;
    use serde_json::json;

    use crate::worker::{Cmd, Event as WEvent};

    const CS: Modifiers = CTRL_SHIFT;

    fn ids(w: &Win) -> Vec<String> {
        w.app.rows.iter().map(|r| r.block_id.clone()).collect()
    }

    fn focus_idx(w: &Win) -> Option<usize> {
        w.app.focus.as_ref().map(|f| f.idx)
    }

    fn kinds(w: &Win) -> Vec<String> {
        w.app
            .store
            .queued_ops()
            .unwrap()
            .iter()
            .map(|o| serde_json::to_value(&o.kind).unwrap()["kind"].as_str().unwrap().to_string())
            .collect()
    }

    /// Holds `k`: a fresh press, then (without a release) the OS's repeats.
    fn hold(w: &mut Win, k: Key, repeats: usize) {
        w.frame_with(vec![crate::testkit::key(k)], Modifiers::NONE);
        for _ in 0..repeats {
            w.repeat(k);
        }
        w.frames(vec![]);
    }

    // ------------------------------------------------------------ Enter

    #[test]
    fn enter_adds_a_paragraph_after_and_focuses_it_in_one_undo_step() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(0);
        w.type_text("x");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.rows.len(), 3);
        assert_eq!(plain_text(&w.app.rows[0].rich_text), "ax");
        assert_eq!(focus_idx(&w), Some(1), "the new row has the focus");
        assert!(w.app.focus.as_ref().unwrap().block_id.starts_with("local-"));
        assert_eq!(w.text(), "");
        assert_eq!(w.app.undo.len(), 1, "commit and create together");
        w.type_text("y");
        assert_eq!(w.text(), "y", "typing goes to the new row");
        // undo: the typing, then the row and the edit
        w.press(Key::Z, Modifiers::CTRL);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(ids(&w), ["p-0", "p-1"]);
        assert_eq!((w.row_text("p-0"), w.queued()), ("a".into(), 0));
    }

    #[test]
    fn shift_enter_is_a_newline_and_a_repeated_enter_does_nothing() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.press(Key::Enter, Modifiers::SHIFT);
        assert_eq!(w.text(), "a\n");
        assert_eq!(w.app.rows.len(), 1);
        w.press(Key::Backspace, Modifiers::NONE);
        hold(&mut w, Key::Enter, 3);
        assert_eq!(w.app.rows.len(), 2, "one press, its repeats are ignored");
        assert_eq!(focus_idx(&w), Some(1));
        assert_eq!(w.text(), "", "and type no newline");
        // Enter in a code row adds a block too
        w.press(Key::Num8, CS);
        assert!(matches!(w.app.rows[1].marker, crate::outline::Marker::Code(_)));
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.rows.len(), 3);
    }

    // -------------------------------------------------------- Backspace

    #[test]
    fn backspace_on_an_empty_row_deletes_it_and_focuses_the_previous() {
        let mut w = Win::with_texts(&["a", "", "c"]);
        w.edit(1);
        w.press(Key::Backspace, Modifiers::NONE);
        assert_eq!(ids(&w), ["p-0", "p-2"]);
        assert_eq!(focus_idx(&w), Some(0));
        assert_eq!(kinds(&w), ["delete_block"]);
        // a non-empty row: the TextEdit deletes a char instead
        w.press(Key::Backspace, Modifiers::NONE);
        assert_eq!(ids(&w).len(), 2);
        assert_eq!(w.text(), "");
        w.press(Key::Z, Modifiers::CTRL); // the char, then the row
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(ids(&w).len(), 3, "undo brings the row back");
    }

    #[test]
    fn backspace_repeat_and_children_never_delete_more() {
        let mut w = Win::with_texts(&["a", "", ""]);
        w.edit(2);
        hold(&mut w, Key::Backspace, 3);
        assert_eq!(ids(&w), ["p-0", "p-1"], "one press deletes one row, its repeats none");
        assert_eq!(focus_idx(&w), Some(1));
        // a row with children stays
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &[""]);
        store
            .replace_children("p", "p-0", &[blk("k", "p-0", "paragraph", json!({ "rich_text": rt("kid") }))])
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w.edit(0);
        w.press(Key::Backspace, Modifiers::NONE);
        assert_eq!(ids(&w), ["p-0", "k"]);
        assert!(w.app.focus.is_some());
        assert_eq!(w.queued(), 0);
    }

    // -------------------------------------------------------------- Tab

    #[test]
    fn tab_and_shift_tab_queue_a_move_and_undo_restores_it() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(1);
        w.press(Key::Tab, Modifiers::NONE);
        assert_eq!(w.app.rows[1].depth, 1);
        assert_eq!(w.text(), "b", "no tab character typed");
        assert!(w.queued() > 0);
        assert_eq!(focus_idx(&w), Some(1), "the editor stays on the block");
        w.press(Key::Tab, Modifiers::SHIFT);
        assert_eq!(w.app.rows[1].depth, 0);
        assert_eq!(w.app.undo.len(), 2);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.rows[1].depth, 1);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.app.rows[1].depth, w.queued()), (0, 0));
    }

    #[test]
    fn a_refused_move_queues_nothing() {
        // nothing above to nest under
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(0);
        w.press(Key::Tab, Modifiers::NONE);
        w.press(Key::Tab, Modifiers::SHIFT);
        assert_eq!((w.app.rows[0].depth, w.queued(), w.app.undo.len()), (0, 0, 0));
        // a subtree push could not recreate
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["a", "b"]);
        store
            .replace_children(
                "p",
                "p-1",
                &[blk("c", "p-1", "bookmark", json!({ "url": "https://x.example", "caption": [] }))],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        assert_eq!(ids(&w), ["p-0", "p-1", "c"]);
        w.edit(1);
        w.press(Key::Tab, Modifiers::NONE);
        assert!(w.app.last_error.as_deref().unwrap().contains("can't move"));
        assert_eq!((w.app.rows[1].depth, w.queued()), (0, 0));
    }

    // ----------------------------------------------------- keymap, marks

    #[test]
    fn ctrl_enter_toggles_a_todo_and_undo_walks_back() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.press(Key::Num4, CS); // to_do
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(false));
        w.press(Key::Enter, Modifiers::CTRL);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(true));
        assert!(w.app.focus.is_some(), "still editing");
        assert_eq!(w.text(), "a", "no newline typed");
        w.press(Key::Enter, Modifiers::CTRL);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(false));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Todo(true));
    }

    #[test]
    fn ctrl_shift_2_makes_a_heading_and_undo_reverts_the_conversion() {
        let mut w = Win::with_texts(&["abc"]);
        w.edit(0);
        w.press(Key::Num2, CS);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Heading(2));
        assert_eq!(w.text(), "abc");
        assert_eq!(w.app.undo.len(), 1);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::None);
        assert_eq!((w.queued(), w.app.rows.len()), (0, 1));
        // (a conversion replaces the block, so there is no row to keep editing)
        w.press(Key::Z, CS); // redo
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Heading(2));
    }

    #[test]
    fn ctrl_b_wraps_the_selection_or_a_pair_at_the_cursor() {
        let mut w = Win::with_texts(&["abc"]);
        w.edit(0);
        w.select(0, 2);
        w.press(Key::B, Modifiers::CTRL);
        assert_eq!(w.text(), "**ab**c");
        w.press(Key::I, Modifiers::CTRL); // cursor only (after the wrap): a pair
        assert_eq!(w.text(), "**ab****c", "a cursor at 6 puts the pair there");
    }

    #[test]
    fn ctrl_sharp_s_toggles_the_sidebar_as_a_key_or_as_text() {
        let mut w = Win::with_texts(&["a"]);
        assert!(w.app.sidebar);
        w.frame_with(vec![Event::Text("ß".into())], Modifiers::CTRL);
        w.frames(vec![]);
        assert!(!w.app.sidebar);
        w.press(Key::Minus, Modifiers::CTRL);
        assert!(w.app.sidebar);
    }

    #[test]
    fn the_equation_shortcut_and_raw_rows_have_no_slash_menu() {
        let mut w = Win::with_texts(&[""]);
        w.edit(0);
        w.type_text("$$");
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::None);
        w.type_text(" ");
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::Equation, "`$$ ` is in iced's list");
        w.type_text("/");
        assert!(w.menu().is_none(), "no slash menu in an equation row");
        // Ctrl+Shift+0 turns it back into text
        w.press(Key::Num0, CS);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::None);
        w.press(Key::Num8, CS);
        w.type_text("/");
        assert!(w.menu().is_none(), "nor in a code row");
    }

    // ------------------------------------------------------------- undo

    #[test]
    fn undo_and_redo_of_an_edit_and_undo_commits_typing_first() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("1");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!((w.row_text("p-0"), w.app.undo.len()), ("a1".into(), 1));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.row_text("p-0"), w.queued()), ("a".into(), 0));
        assert_eq!((w.app.undo.len(), w.app.redo.len()), (0, 1));
        w.press(Key::Z, CS);
        assert_eq!((w.row_text("p-0"), w.queued()), ("a1".into(), 1));
        // typing not yet written: undo writes it (its own step), then undoes it
        w.edit(0);
        w.type_text("2");
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.text(), "a1", "typing undone");
        assert!(w.app.focus.is_some(), "still editing");
        assert_eq!((w.app.undo.len(), w.app.redo.len()), (1, 1));
        w.press(Key::Z, CS);
        assert_eq!(w.text(), "a12", "and redone");
        // a new edit clears redo
        w.press(Key::Z, Modifiers::CTRL);
        w.type_text("3");
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.redo.is_empty());
    }

    #[test]
    fn undo_is_blocked_in_flight_and_while_the_switcher_is_open() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("1");
        w.press(Key::Escape, Modifiers::NONE);
        w.app.sent.push_back(Cmd::Search(String::new()));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.row_text("p-0"), "a1", "a command is in flight");
        w.app.sent.clear();
        w.press(Key::K, Modifiers::CTRL);
        assert!(w.app.switcher.is_some());
        w.app.undo_redo(true);
        assert_eq!(w.row_text("p-0"), "a1", "the switcher is open");
        assert_eq!(w.app.undo.len(), 1);
    }

    #[test]
    fn a_push_and_a_pull_clear_both_stacks_but_a_skipped_pull_does_not() {
        let mut w = Win::with_texts(&["a"]);
        w.edit(0);
        w.type_text("1");
        w.press(Key::Escape, Modifiers::NONE);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.app.undo.len(), w.app.redo.len()), (0, 1));
        // pull, skipped: nothing was written
        w.press(Key::R, Modifiers::CTRL);
        w.answer(WEvent::Pulled {
            id: "p".into(),
            report: PullReport {
                skipped: true,
                ..Default::default()
            },
        });
        assert_eq!(w.app.redo.len(), 1);
        w.press(Key::R, Modifiers::CTRL);
        w.answer(WEvent::Pulled {
            id: "p".into(),
            report: PullReport::default(),
        });
        assert!(w.app.undo.is_empty() && w.app.redo.is_empty(), "a pull rewrote rows");
        // push
        w.press(Key::Z, CS);
        w.edit(0);
        w.type_text("2");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!(w.app.undo.len(), 1);
        w.press(Key::S, Modifiers::CTRL);
        w.answer(WEvent::Pushed(Default::default()));
        assert!(w.app.undo.is_empty() && w.app.redo.is_empty());
        w.cmds();
    }

    // ------------------------------------------------------------ title

    #[test]
    fn title_edits_queue_one_op_merge_with_a_queued_one_and_undo() {
        let mut w = Win::with_texts(&["a"]);
        assert!(w.app.title_editable);
        w.app.apply(Action::EditTitle);
        w.frames(vec![]);
        w.frames(vec![]);
        w.type_text("x");
        assert_eq!(w.app.title_edit.as_ref().unwrap().text, "px");
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.title_edit.is_none());
        assert_eq!((w.app.title.as_str(), w.queued()), ("px", 1));
        // Esc commits too; the second edit merges with the queued op
        w.app.apply(Action::EditTitle);
        w.frames(vec![]);
        w.frames(vec![]);
        w.type_text("y");
        w.press(Key::Escape, Modifiers::NONE);
        assert_eq!((w.app.title.as_str(), w.queued()), ("pxy", 1));
        assert_eq!(w.app.undo.len(), 2);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(w.app.title, "px");
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.app.title.as_str(), w.queued()), ("p", 0));
    }

    #[test]
    fn a_formatted_or_linked_title_is_not_edited_in_place() {
        for run in [
            json!({ "bold": true }),
            json!({ "color": "red" }),
        ] {
            let mut pg = page_in("p", None);
            let mut v = serde_json::to_value(&pg).unwrap();
            v["properties"]["Name"]["title"][0]["annotations"] = run;
            pg = serde_json::from_value(v).unwrap();
            let store = Store::open_in_memory().unwrap();
            store.upsert_page(&pg).unwrap();
            let mut w = Win::new(store);
            w.app.apply(Action::Open("p".into()));
            w.frames(vec![]);
            assert!(!w.app.title_editable);
            w.app.apply(Action::EditTitle);
            assert!(w.app.title_edit.is_none(), "it would flatten the formatting");
        }
        let mut v = serde_json::to_value(page_in("p", None)).unwrap();
        v["properties"]["Name"]["title"][0]["href"] = json!("https://e.x/");
        let store = Store::open_in_memory().unwrap();
        store.upsert_page(&serde_json::from_value(v).unwrap()).unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        assert!(!w.app.title_editable, "a link");
    }

    #[test]
    fn up_from_the_first_row_edits_the_title() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(1);
        w.press(Key::ArrowUp, Modifiers::NONE);
        assert_eq!(focus_idx(&w), Some(0));
        w.press(Key::ArrowUp, Modifiers::NONE);
        assert!(w.app.focus.is_none());
        assert!(w.app.title_edit.is_some());
    }

    // ------------------------------------------------------ table cells

    fn table_win() -> Win {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &[]);
        let mention = json!([{ "type": "mention", "mention": { "type": "link_preview", "link_preview": { "url": "https://x.test" } },
            "plain_text": "d", "annotations": {} }]);
        let table = |id: &str, parent: &str| {
            blk(id, parent, "table", json!({ "table_width": 2, "has_column_header": true, "has_row_header": false }))
        };
        let trow = |id: &str, p: &str, cells: serde_json::Value| blk(id, p, "table_row", json!({ "cells": cells }));
        store.replace_children("p", "p", &[table("tbl", "p")]).unwrap();
        store
            .replace_children(
                "p",
                "tbl",
                &[
                    trow("r1", "tbl", json!([rt("a"), rt("b")])),
                    trow("r2", "tbl", json!([mention, rt("c")])),
                ],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w
    }

    fn cell_text(w: &Win, row: &str, col: usize) -> String {
        let Marker::Table(t) = &w.app.rows[0].marker else { panic!() };
        plain_text(&t.rows.iter().find(|r| r.id == row).unwrap().cells[col].rich_text)
    }

    #[test]
    fn a_table_cell_is_edited_as_markdown_and_tab_commits_it() {
        let mut w = table_win();
        w.app.apply(Action::EditCell("r2".into(), 0));
        assert!(w.app.focus.is_none(), "a mention cell is read-only");
        w.app.apply(Action::EditCell("r1".into(), 1));
        w.frames(vec![]);
        w.frames(vec![]);
        assert_eq!(w.app.focus.as_ref().unwrap().block_id, "tbl");
        // no slash menu, no conversion, Backspace never deletes the table
        w.type_text("/");
        assert!(w.menu().is_none());
        w.press(Key::Num1, CS);
        assert_eq!(w.app.rows[0].marker.label(), "");
        for _ in 0..3 {
            w.press(Key::Backspace, Modifiers::NONE);
        }
        assert_eq!(w.text(), "");
        w.type_text("**B**");
        assert_eq!(w.queued(), 0, "typing alone writes nothing");
        w.press(Key::Tab, Modifiers::NONE);
        assert!(w.app.focus.is_none(), "Tab leaves the cell");
        assert_eq!(cell_text(&w, "r1", 1), "B");
        assert_eq!(kinds(&w), ["update_block"]);
        let OpKind::UpdateBlock { block_id, body } = &w.app.store.queued_ops().unwrap()[0].kind else {
            panic!()
        };
        assert_eq!(block_id, "r1");
        assert_eq!(body["table_row"]["cells"][0][0]["plain_text"], "a");
        assert_eq!(body["table_row"]["cells"][1][0]["annotations"]["bold"], true);
        // Enter commits too
        w.app.apply(Action::EditCell("r1".into(), 0));
        w.frames(vec![]);
        w.frames(vec![]);
        w.type_text("!");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!((cell_text(&w, "r1", 0), w.app.focus.is_none()), ("a!".into(), true));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(cell_text(&w, "r1", 0), "a");
    }

    #[test]
    fn locked_tables_and_vanished_cells() {
        let mut w = table_win();
        w.app.rows[0].locked = true;
        w.app.apply(Action::EditCell("r1".into(), 0));
        assert!(w.app.focus.is_none(), "a synced table is read-only");
        w.app.refresh();
        w.app.apply(Action::EditCell("r1".into(), 0));
        w.type_text("z");
        w.app.store.replace_children("p", "tbl", &[]).unwrap();
        w.app.refresh();
        assert!(w.app.focus.is_none());
        assert!(w.app.last_error.as_deref().unwrap().contains("gone"));
    }

    // ------------------------------------------------------------ links

    const PID: &str = "0123abcd-0123-4567-89ab-0123456789ab";

    fn go(w: &mut Win, link: Link, new_tab: bool) {
        w.app.apply(Action::Link { link, new_tab });
        w.frames(vec![]);
    }

    #[test]
    fn notion_links_open_in_the_app_pulled_if_uncached() {
        let mut w = Win::with_texts(&["a"]);
        go(&mut w, links::route(&format!("/{PID}")), false);
        assert_eq!(w.cmds(), [Cmd::Pull { id: PID.into(), force: false }]);
        w.app.sent.clear();
        w.app.store.upsert_page(&page_in(PID, None)).unwrap();
        go(&mut w, links::route(&format!("https://acme.notion.site/X-{PID}")), false);
        assert_eq!(w.app.current.as_deref(), Some(PID));
        assert_eq!(w.app.tabs.len(), 1);
        // Ctrl+click / middle click: a new tab
        go(&mut w, Link::Page("p".into()), true);
        assert_eq!((w.app.tabs.len(), w.app.current.as_deref()), (2, Some("p")));
        assert!(w.opened.borrow().is_empty());
    }

    #[test]
    fn a_link_card_to_a_notion_page_opens_in_the_app() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_page(&page_in("p", None)).unwrap();
        store.upsert_page(&page_in(PID, None)).unwrap();
        let card = |id: &str, url: String| blk(id, "p", "bookmark", json!({ "url": url, "caption": [] }));
        store
            .replace_children(
                "p",
                "p",
                &[card("b", format!("https://notion.so/{PID}")), card("c", "https://e.x/page".into())],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        // the layout is unknown: scan the first card down the page
        for y in (60..200).step_by(4) {
            let pos = Pos2::new(330.0, y as f32);
            let ev = |pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            w.frames(vec![Event::PointerMoved(pos)]);
            w.frames(vec![ev(true)]);
            w.frames(vec![ev(false)]);
            w.frames(vec![]);
            if w.app.current.as_deref() == Some(PID) || !w.opened.borrow().is_empty() {
                break;
            }
        }
        assert_eq!(w.app.current.as_deref(), Some(PID));
        assert!(w.opened.borrow().is_empty(), "not the browser");
    }

    #[test]
    fn web_links_go_to_the_opener_and_other_schemes_are_refused() {
        let mut w = Win::with_texts(&["a"]);
        go(&mut w, links::route("https://example.com/a"), false);
        assert_eq!(*w.opened.borrow(), ["https://example.com/a"]);
        assert_eq!(w.app.last_error, None);
        for bad in ["file:///etc/passwd", "mailto:x@y", "javascript:alert(1)"] {
            go(&mut w, links::route(bad), false);
            assert!(w.app.last_error.as_deref().unwrap().contains("only http"), "{bad}");
        }
        assert_eq!(w.opened.borrow().len(), 1, "nothing else was opened");
        assert_eq!(w.app.current.as_deref(), Some("p"));
    }

    /// A page whose only row is one long link; clicks the row's text by
    /// scanning down the page (the layout is not known to the test).
    fn click_link(button: PointerButton, mods: Modifiers) -> Win {
        let store = Store::open_in_memory().unwrap();
        store.upsert_page(&page_in("p", None)).unwrap();
        store.upsert_page(&page_in(PID, None)).unwrap();
        let text = "link link link link link link link link link link link link";
        let run = |url: &str| {
            json!([{ "type": "text", "text": { "content": text, "link": { "url": url } },
                "plain_text": text, "href": url, "annotations": {} }])
        };
        store
            .replace_children(
                "p",
                "p",
                &[
                    blk("web", "p", "paragraph", json!({ "rich_text": run(&format!("https://notion.so/{PID}")) })),
                ],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        for y in (60..300).step_by(5) {
            let pos = Pos2::new(330.0, y as f32);
            let ev = |pressed| Event::PointerButton { pos, button, pressed, modifiers: mods };
            w.frame_with(vec![Event::PointerMoved(pos)], mods);
            w.frame_with(vec![ev(true)], mods);
            w.frame_with(vec![ev(false)], mods);
            w.frames(vec![]);
            if w.app.current.as_deref() == Some(PID)
                || w.app.tabs.len() > 1
                || w.app.focus.is_some()
                || w.app.title_edit.is_some()
            {
                if w.app.title_edit.is_some() || w.app.focus.is_some() {
                    // hit the title or the row's editor instead: keep scanning
                    w.app.apply(Action::Blur);
                    w.app.title_edit = None;
                    w.frames(vec![]);
                    continue;
                }
                break;
            }
        }
        w
    }

    #[test]
    fn clicking_a_link_in_a_row_follows_it() {
        let w = click_link(PointerButton::Primary, Modifiers::NONE);
        assert_eq!(w.app.current.as_deref(), Some(PID), "opened in this tab");
        assert!(w.app.focus.is_none(), "not the editor");
        // (a real Ctrl press sets `command` too)
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        let w = click_link(PointerButton::Primary, ctrl);
        assert_eq!((w.app.tabs.len(), w.app.current.as_deref()), (2, Some(PID)));
        let w = click_link(PointerButton::Middle, Modifiers::NONE);
        assert_eq!(w.app.tabs.len(), 2);
    }
}
