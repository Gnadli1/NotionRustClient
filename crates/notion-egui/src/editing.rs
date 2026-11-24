//! The editor's keys and the edits they make: Enter / Backspace / Tab on the
//! focused row, keymap commands, the inline title, table cells and links.
//! Logic first; the widgets only collect the keys and hand them over as
//! `Action`s, so tests drive it without a window.

use eframe::egui::{
    self, text_edit::TextEditState, Id, Key, Label, Modifiers, RichText, Sense, Ui,
};
use notion_core::markdown::{from_markdown, markdown_shortcut, to_markdown};
use notion_core::model::{plain_text, BlockKind, TargetKind, TextBlock};
use notion_core::sync;

use crate::app::{neighbour, set_cursor, Action, App, Focus, Grab, TitleEdit};
use crate::edit::{key_action, Caret, EditKey, KeyAction};
use crate::keys::{self, Command};
use crate::links::{self, Link};
use crate::menu::{Kind, Menu};
use crate::outline::{self, Marker, Row, Slot};

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
        f.cursor = cursor_of(ctx, id, &f.text);
        let mut paste = None;
        let pending = std::mem::take(&mut f.pending);
        // a menu pick runs on the selection the menu was opened on
        if let (false, Some(r), Some(mut s)) = (pending.is_empty(), f.sel, TextEditState::load(ctx, id)) {
            s.cursor.set_char_range(Some(r));
            s.store(ctx, id);
        }
        let (mut cmds, [enter, tab, shift_tab, bs, esc]) = ctx.input_mut(|i| {
            let ev = &mut i.events;
            // several lines pasted into an empty row become one block each
            if row && empty && !f.code {
                ev.retain(|e| match e {
                    egui::Event::Paste(t) if t.trim_end().contains('\n') => {
                        paste = Some(t.clone());
                        false
                    }
                    _ => true,
                });
            }
            let cmds = keys::take_commands(ev, ctrl, map, |c| !c.is_global());
            let mut take = |m: Modifiers, k: Key| keys::take_exact(ev, m, k).0;
            let n = [
                if row {
                    take(Modifiers::NONE, Key::Enter)
                } else {
                    0
                },
                take(Modifiers::NONE, Key::Tab),
                take(Modifiers::SHIFT, Key::Tab),
                if row && empty {
                    take(Modifiers::NONE, Key::Backspace)
                } else {
                    0
                },
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
        cmds.extend(pending);
        let mut act = paste.map(Action::Paste);
        for (n, a) in [
            (esc, if row { Action::Select } else { Action::Blur }),
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
            // the block commands never act while a menu is open
            if f.menu.is_some()
                && matches!(c, Command::Duplicate | Command::MoveUp | Command::MoveDown)
            {
                continue;
            }
            f.menu = None;
            match c {
                Command::Duplicate | Command::MoveUp | Command::MoveDown if row => {
                    act = Some(Action::Run(c));
                }
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
                KeyAction::NewBlockAfter
                    | KeyAction::Leave
                    | KeyAction::Indent
                    | KeyAction::Outdent
            ) {
                self.commit();
            }
            return;
        }
        let idx = f.idx;
        match action {
            KeyAction::NewBlockAfter => self.split_row(),
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

    /// Enter: the text after the cursor moves to a new block (a list item
    /// continues the list, anything else gives a paragraph) and is edited; an
    /// empty list item becomes a paragraph. Code rows just get an empty block.
    fn split_row(&mut self) {
        let Some(f) = &self.focus else { return };
        let row = &self.rows[f.idx];
        let list = matches!(
            row.marker,
            Marker::Bullet | Marker::Number(_) | Marker::Todo(_)
        );
        if f.code || row.locked || !row.editable {
            return self.new_block_after();
        }
        if f.text.is_empty() && list {
            return self.convert(TargetKind::Paragraph, String::new(), String::new(), 0);
        }
        let (id, text, at) = (
            f.block_id.clone(),
            f.text.clone(),
            f.cursor.min(f.text.chars().count()),
        );
        match self
            .store
            .resolve_id(&id)
            .and_then(|id| sync::local_split_block(&self.store, &id, &text, at))
        {
            Ok(new) => {
                self.focus = None;
                self.refresh();
                self.focus_block(&new, Grab::Start);
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// The hover "+": commit, then an empty paragraph after row `idx`, edited.
    pub(crate) fn insert_block_after(&mut self, idx: usize) {
        let Some(row) = self.rows.get(idx).cloned() else {
            return;
        };
        if row.locked || !row.editable || !self.flush() {
            return;
        }
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

    /// A click below the last block, as in Notion: the page's trailing empty
    /// paragraph is edited, else a new one is added at the end.
    pub(crate) fn append_block_at_end(&mut self) {
        if let Some(i) = self.rows.iter().rposition(|r| r.depth == 0) {
            let r = &self.rows[i];
            if r.editable && matches!(r.marker, Marker::None) && r.rich_text.is_empty() {
                return self.start_edit(i, Grab::End);
            }
        }
        let Some(page) = self.current.clone() else {
            return;
        };
        if self.meta.locked || self.pushing() || !self.flush() {
            return;
        }
        let para = BlockKind::Paragraph(TextBlock {
            rich_text: Vec::new(),
            color: "default".into(),
        });
        let made = self
            .store
            .resolve_id(&page)
            .and_then(|p| sync::local_create_block(&self.store, &p, None, para));
        match made {
            Ok(new) => {
                self.focus = None;
                self.refresh();
                self.focus_block(&new, Grab::Start);
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Enter: commit, an empty paragraph after the row, edit it.
    fn new_block_after(&mut self) {
        if !self.flush() {
            return;
        }
        let Some(row) = self
            .focus
            .as_ref()
            .and_then(|f| self.rows.get(f.idx))
            .cloned()
        else {
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
        let prev =
            neighbour(&self.rows, &self.shown, idx, -1).map(|i| self.rows[i].block_id.clone());
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

    /// A block dropped by its grip: commit any edit, then move it (with its
    /// subtree) under `parent` after `after`, and bring it into view.
    pub(crate) fn drop_block(&mut self, id: &str, parent: &str, slot: &Slot) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        if !self.flush() {
            return;
        }
        let Some(i) = self.rows.iter().position(|r| r.block_id == id) else {
            return;
        };
        if let Some(m) = outline::unmovable_in_subtree(&self.rows, i) {
            self.last_error = Some(format!("can't move: its subtree contains {}", m.label()));
            return;
        }
        let res = self.store.atomic(|| {
            let (id, parent) = (self.resolved(i)?, self.store.resolve_id(parent)?);
            match slot {
                Slot::After(a) => {
                    let a = self.store.resolve_id(a)?;
                    sync::local_move_block(&self.store, &id, &parent, Some(&a)).map(drop)
                }
                // nothing can be put first: after the first sibling S, then S after it
                Slot::First => {
                    let kids = self.store.children(&parent)?;
                    if kids.first().is_some_and(|b| b.id == id) {
                        return Ok(()); // already first
                    }
                    let first = kids.into_iter().find(|b| b.id != id);
                    let Some(s) = first.map(|b| b.id) else {
                        return sync::local_move_block(&self.store, &id, &parent, None).map(drop);
                    };
                    if let Some(m) = self
                        .rows
                        .iter()
                        .position(|r| r.block_id == s)
                        .and_then(|k| outline::unmovable_in_subtree(&self.rows, k))
                    {
                        return Err(notion_core::Error::Invalid(format!(
                            "can't move: its subtree contains {}",
                            m.label()
                        )));
                    }
                    sync::local_move_block(&self.store, &id, &parent, Some(&s))?;
                    sync::local_move_block(&self.store, &s, &parent, Some(&id)).map(drop)
                }
            }
        });
        if let Err(e) = res {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
        if let Some(i) = self.rows.iter().position(|r| r.block_id == id) {
            self.show_row(i);
        }
    }

    // ---------------------------------------------------- block editing

    /// Row indices of the selected run of siblings.
    pub(crate) fn sel_idx(&self) -> Vec<usize> {
        let Some((a, b)) = &self.sel else {
            return Vec::new();
        };
        let pos = |id: &str| self.rows.iter().position(|r| r.block_id == id);
        let (Some(a), Some(b)) = (pos(a), pos(b)) else {
            return Vec::new();
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let parent = &self.rows[lo].parent;
        (lo..=hi)
            .filter(|&i| self.rows[i].parent == *parent)
            .collect()
    }

    /// The rows to highlight: the selected blocks and their subtrees.
    pub(crate) fn sel_rows(&self) -> std::ops::Range<usize> {
        let idx = self.sel_idx();
        match (idx.first(), idx.last()) {
            (Some(&a), Some(&b)) => a..b + 1 + outline::subtree(&self.rows, b).len(),
            _ => 0..0,
        }
    }

    /// The next (`dir` 1) or previous (-1) row with the same parent.
    fn sibling(&self, i: usize, dir: isize) -> Option<usize> {
        let same = |&j: &usize| self.rows[j].parent == self.rows[i].parent;
        if dir < 0 {
            (0..i).rev().find(same)
        } else {
            (i + 1..self.rows.len()).find(same)
        }
    }

    /// The keys of a block selection, taken before any widget runs: Esc
    /// clears it, Shift+Up/Down extend it, Delete / Backspace / Ctrl+X remove
    /// it, Ctrl+C copies it as Markdown, Enter edits its first block and the
    /// keymap's duplicate / move commands act on it. Only with no editor, no
    /// other text box and no overlay wanting the keys.
    pub(crate) fn selection_keys(&mut self, ctx: &egui::Context) {
        if self.sel.is_none()
            || self.focus.is_some()
            || self.title_edit.is_some()
            || self.switcher.is_some()
            || self.dlg.is_some()
            || self.inbox.is_some()
            || self.conflict.is_some()
            || self.problems_open
            || self.comments.is_some()
            || self.settings_ui.is_some()
            || ctx.egui_wants_keyboard_input()
        {
            return;
        }
        let ctrl = ctx.input(|i| i.modifiers.ctrl);
        let map = &self.keys;
        let (cmds, [esc, up, down, del, enter, copy, cut]) = ctx.input_mut(|i| {
            let cmds = keys::take_commands(&mut i.events, ctrl, map, |c| {
                matches!(c, Command::Duplicate | Command::MoveUp | Command::MoveDown)
            });
            let (mut copy, mut cut) = (0, 0);
            i.events.retain(|e| match e {
                egui::Event::Copy => {
                    copy += 1;
                    false
                }
                egui::Event::Cut => {
                    cut += 1;
                    false
                }
                _ => true,
            });
            let mut take = |m: Modifiers, k: Key| {
                let (fresh, repeats) = keys::take_exact(&mut i.events, m, k);
                fresh + repeats
            };
            let n = [
                take(Modifiers::NONE, Key::Escape),
                take(Modifiers::SHIFT, Key::ArrowUp),
                take(Modifiers::SHIFT, Key::ArrowDown),
                take(Modifiers::NONE, Key::Delete) + take(Modifiers::NONE, Key::Backspace),
                take(Modifiers::NONE, Key::Enter),
                copy,
                cut,
            ];
            (cmds, n)
        });
        for (n, dir) in [(up, -1), (down, 1)] {
            for _ in 0..n {
                let head = self
                    .sel
                    .as_ref()
                    .and_then(|(_, h)| self.rows.iter().position(|r| r.block_id == *h));
                if let Some(j) = head.and_then(|h| self.sibling(h, dir)) {
                    let id = self.rows[j].block_id.clone();
                    self.sel.as_mut().expect("checked").1 = id;
                }
            }
        }
        if copy > 0 {
            ctx.copy_text(self.sel_markdown());
        }
        if del + cut > 0 && self.sel_deletable() {
            if cut > 0 {
                ctx.copy_text(self.sel_markdown());
            }
            self.step(Self::delete_selected);
        }
        for c in cmds {
            self.step(|s| s.block_cmd(c));
        }
        if enter > 0 {
            if let Some(&i) = self.sel_idx().first().filter(|&&i| self.rows[i].editable) {
                self.start_edit(i, Grab::End);
            }
        }
        if esc > 0 {
            self.sel = None;
        }
    }

    /// The selected blocks and their children as Markdown, one block per line
    /// (children indented two spaces per level).
    fn sel_markdown(&self) -> String {
        self.rows_markdown(&self.sel_idx())
    }

    /// The whole open page as Markdown (same format as `sel_markdown`).
    pub(crate) fn page_markdown(&self) -> String {
        let tops: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].depth == 0)
            .collect();
        self.rows_markdown(&tops)
    }

    /// Appends Markdown at the end of the page as blocks.
    pub(crate) fn insert_markdown(&mut self, md: &str) {
        self.append_block_at_end();
        let before = self.last_error.clone();
        self.paste_lines(md);
        if self.last_error == before {
            self.info = Some("inserted".into());
        }
    }

    fn rows_markdown(&self, idx: &[usize]) -> String {
        let mut out = Vec::new();
        for &i in idx {
            let d0 = self.rows[i].depth;
            for r in std::iter::once(&self.rows[i]).chain(outline::subtree(&self.rows, i)) {
                let md = to_markdown(&r.rich_text);
                let line = match &r.marker {
                    Marker::Heading(n) => format!("{} {md}", "#".repeat(usize::from(*n))),
                    Marker::Bullet => format!("- {md}"),
                    Marker::Number(n) => format!("{n}. {md}"),
                    Marker::Todo(c) => format!("[{}] {md}", if *c { "x" } else { " " }),
                    Marker::Quote => format!("> {md}"),
                    Marker::Divider => "---".into(),
                    // ponytail: one line per block, so a multi-line code block is not fenced
                    Marker::Code(_) => format!("```{}", plain_text(&r.rich_text)),
                    Marker::Equation => format!("$$ {}", plain_text(&r.rich_text)),
                    _ => md,
                };
                out.push(format!("{}{line}", "  ".repeat(r.depth - d0)));
            }
        }
        out.join("\n")
    }

    fn resolved(&self, i: usize) -> notion_core::Result<String> {
        self.store.resolve_id(&self.rows[i].block_id)
    }

    /// Delete and cut only take plain text-like blocks (and subtrees of them):
    /// a child page, table or synced block would be lost with no way back.
    fn sel_deletable(&mut self) -> bool {
        let bad = self
            .sel_idx()
            .into_iter()
            .find_map(|i| outline::unmovable_in_subtree(&self.rows, i));
        if let Some(m) = bad {
            self.last_error = Some(format!(
                "not deleting: the selection contains {}",
                m.label()
            ));
        }
        bad.is_none()
    }

    /// Deletes the selected blocks (one undo step).
    fn delete_selected(&mut self) {
        let idx = self.sel_idx();
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let res = self.store.atomic(|| {
            for &i in &idx {
                sync::local_delete_block(&self.store, &self.resolved(i)?)?;
            }
            Ok(())
        });
        if let Err(e) = res {
            self.last_error = Some(e.to_string());
        }
        self.sel = None;
        self.refresh();
    }

    /// Ctrl+D / Ctrl+Shift+Up / Ctrl+Shift+Down on the edited row (kept
    /// open) or the selected blocks.
    pub(crate) fn block_cmd(&mut self, c: Command) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let editing = self
            .focus
            .as_ref()
            .filter(|f| f.cell.is_none())
            .map(|f| f.block_id.clone());
        if editing.is_some() && !self.flush() {
            return;
        }
        let idx = match editing {
            Some(id) => self
                .rows
                .iter()
                .position(|r| r.block_id == id)
                .into_iter()
                .collect(),
            None => self.sel_idx(),
        };
        let (Some(&first), Some(&last)) = (idx.first(), idx.last()) else {
            return;
        };
        let res = self.store.atomic(|| match c {
            Command::Duplicate => {
                // copies go after the last block, in order
                let mut after = self.resolved(last)?;
                let mut copies = Vec::new();
                for &i in &idx {
                    after =
                        sync::local_duplicate_block(&self.store, &self.resolved(i)?, Some(&after))?;
                    copies.push(after.clone());
                }
                Ok(Some(copies))
            }
            _ => {
                // up: the sibling before swaps past the run; down: the run
                // goes after the sibling behind it (nothing can be put first)
                let (moved, anchor): (Vec<usize>, usize) = if c == Command::MoveUp {
                    match self.sibling(first, -1) {
                        Some(p) => (vec![p], last),
                        None => return Ok(None),
                    }
                } else {
                    match self.sibling(last, 1) {
                        Some(n) => (idx.clone(), n),
                        None => return Ok(None),
                    }
                };
                let parent = self.store.resolve_id(&self.rows[first].parent)?;
                let mut anchor = self.resolved(anchor)?;
                if c == Command::MoveUp
                    && self.store.subtree_has_comments(&self.resolved(moved[0])?)?
                {
                    return Err(notion_core::Error::Invalid(
                        "the block above has comments; moving it would lose them".into(),
                    ));
                }
                for i in moved {
                    if let Some(m) = outline::unmovable_in_subtree(&self.rows, i) {
                        return Err(notion_core::Error::Invalid(format!(
                            "can't move: its subtree contains {}",
                            m.label()
                        )));
                    }
                    let id = self.resolved(i)?;
                    sync::local_move_block(&self.store, &id, &parent, Some(&anchor))?;
                    anchor = id;
                }
                Ok(None)
            }
        });
        match res {
            Ok(copies) => {
                if let (Some(copies), true) = (copies, self.sel.is_some()) {
                    self.sel = copies.first().cloned().zip(copies.last().cloned());
                }
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
        self.refresh();
    }

    /// A `/` colour pick: writes the text without the trigger, then the colour.
    pub(crate) fn set_color(&mut self, text: String, color: String) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let Some(f) = &mut self.focus else { return };
        f.text = text;
        if !self.flush() {
            return;
        }
        let Some(f) = &self.focus else { return };
        if self.rows[f.idx].locked {
            self.last_error = Some("can't edit inside a synced block".into());
            return;
        }
        let id = f.block_id.clone();
        if let Err(e) = self
            .store
            .resolve_id(&id)
            .and_then(|id| sync::local_set_color(&self.store, &id, &color))
        {
            self.last_error = Some(e.to_string());
        }
        self.refresh();
    }

    /// A multi-line paste into the empty edited row: one block per non-blank
    /// line after it (Markdown shortcuts pick the types), then the empty row
    /// goes, unless it has children. The last new block is edited.
    // ponytail: nesting is flattened and a fenced code block is one line per block
    pub(crate) fn paste_lines(&mut self, text: &str) {
        if self.pushing() {
            self.last_error = Some("editing is paused while a push runs".into());
            return;
        }
        let Some(idx) = self.focus.as_ref().map(|f| f.idx) else {
            return;
        };
        // the emptied text is written first: the row may stay (children)
        if !self.flush() {
            return;
        }
        let kids = !outline::subtree(&self.rows, idx).is_empty();
        let res = self.store.atomic(|| {
            let parent = self.store.resolve_id(&self.rows[idx].parent)?;
            let old = self.resolved(idx)?;
            let mut after = old.clone();
            for line in text
                .lines()
                .map(str::trim_start)
                .filter(|l| !l.trim().is_empty())
            {
                let (kind, rest) = markdown_shortcut(line).unwrap_or((TargetKind::Paragraph, line));
                let body = TextBlock {
                    rich_text: from_markdown(rest),
                    color: "default".into(),
                };
                after = sync::local_create_block(
                    &self.store,
                    &parent,
                    Some(&after),
                    kind.block_kind(body),
                )?;
            }
            if !kids {
                sync::local_delete_block(&self.store, &old)?;
            }
            Ok(after)
        });
        self.focus = None;
        self.refresh();
        match res {
            Ok(last) => self.focus_block(&last, Grab::End),
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Ctrl+Alt+T: opens every toggle, or closes them all when all are open.
    pub(crate) fn toggle_all(&mut self) {
        let all: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.toggle)
            .map(|r| r.block_id.clone())
            .collect();
        if all.iter().any(|id| !self.open.contains(id)) {
            self.open.extend(all);
        } else {
            self.open.retain(|id| !all.contains(id));
            // the edited row stays in sight
            if let Some(f) = &self.focus {
                self.open.extend(
                    outline::ancestors(&self.rows, f.idx)
                        .into_iter()
                        .filter(|a| all.iter().any(|x| x == a))
                        .map(String::from),
                );
            }
        }
        self.shown = outline::shown(&self.rows, &self.open);
    }

    // ------------------------------------------------------------ title

    /// Click on the title, or ↑ from the first row.
    pub(crate) fn start_title(&mut self) {
        if !self.title_editable || self.title_edit.is_some() || self.pushing() || !self.commit() {
            return;
        }
        let page = self
            .current
            .as_ref()
            .and_then(|id| self.store.get_page(id).ok().flatten());
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
            let mut l = Label::new(
                RichText::new(shown)
                    .font(egui::FontId::new(
                        crate::theme::TITLE_SIZE,
                        crate::md::title_family(),
                    ))
                    .color(ui.visuals().text_color()),
            )
            .wrap();
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
            for (m, k) in [
                (Modifiers::CTRL, Key::Z),
                (Modifiers::CTRL, Key::Y),
                (CTRL_SHIFT, Key::Z),
            ] {
                keys::take_exact(&mut i.events, m, k);
            }
        });
        let out = egui::TextEdit::singleline(&mut t.text)
            .id(id)
            .font(egui::FontId::new(
                crate::theme::TITLE_SIZE,
                crate::md::title_family(),
            ))
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
        let found = self
            .rows
            .iter()
            .enumerate()
            .find_map(|(i, r)| match &r.marker {
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
            .map(|o| {
                serde_json::to_value(&o.kind).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
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

    #[test]
    fn dropping_a_dragged_block_moves_it() {
        let mut w = Win::with_texts(&["a", "b", "c"]);
        w.app.apply(Action::Drop("p-0".into(), "p".into(), Slot::After("p-2".into())));
        w.frames(vec![]);
        assert_eq!(ids(&w), ["p-1", "p-2", "p-0"]);
        assert!(kinds(&w).contains(&"move_block".to_string()), "{:?}", kinds(&w));
    }

    /// page p: a (kids a1, a2), b (toggle, empty), c
    fn nested() -> Win {
        let store = Store::open_in_memory().unwrap();
        store.upsert_page(&page_in("p", None)).unwrap();
        let para = |id: &str, parent: &str| blk(id, parent, "paragraph", json!({ "rich_text": rt(id) }));
        let top = [
            para("a", "p"),
            blk("b", "p", "toggle", json!({ "rich_text": rt("b") })),
            para("c", "p"),
        ];
        store.replace_children("p", "p", &top).unwrap();
        store.replace_children("p", "a", &[para("a1", "a"), para("a2", "a")]).unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w
    }

    fn comment_on(w: &mut Win, block: &str) {
        let c: notion_core::model::Comment = serde_json::from_value(json!({
            "object": "comment", "id": "c1", "discussion_id": "d1",
            "parent": { "type": "block_id", "block_id": block },
            "created_time": "2025-01-01T00:00:00.000Z", "last_edited_time": "2025-01-01T00:00:00.000Z",
            "created_by": { "object": "user", "id": "u" },
            "rich_text": [{ "type": "text", "text": { "content": "x" }, "plain_text": "x", "annotations": {} }],
        }))
        .unwrap();
        w.app.store.put_comment(&c).unwrap();
        w.app.refresh();
    }

    #[test]
    fn dropping_a_block_into_its_own_subtree_is_refused() {
        let mut w = nested();
        let before = ids(&w);
        assert_eq!(before, ["a", "a1", "a2", "b", "c"]);
        for (parent, after) in [("a", Slot::First), ("a", Slot::After("a1".into())), ("a1", Slot::First)] {
            w.app.last_error = None;
            w.app.drop_block("a", parent, &after);
            assert!(w.app.last_error.is_some(), "{parent} {after:?}");
            assert_eq!((ids(&w), w.queued()), (before.clone(), 0));
        }
    }

    #[test]
    fn dropping_a_block_with_children_moves_the_whole_subtree() {
        let mut w = nested();
        w.app.drop_block("a", "p", &Slot::After("c".into()));
        w.frames(vec![]);
        assert!(w.app.last_error.is_none(), "{:?}", w.app.last_error);
        assert_eq!(ids(&w), ["b", "c", "a", "a1", "a2"]);
        assert_eq!(w.app.rows[3].depth, 1);
        assert_eq!(kinds(&w), ["move_block"]);
    }

    #[test]
    fn dropping_a_commented_block_is_refused_without_an_op() {
        let mut w = nested();
        comment_on(&mut w, "c");
        w.app.drop_block("c", "p", &Slot::First);
        assert!(w.app.last_error.as_deref().unwrap_or("").contains("comments"));
        assert_eq!((ids(&w), w.queued()), (vec!["a", "a1", "a2", "b", "c"].into_iter().map(String::from).collect::<Vec<_>>(), 0));
    }

    #[test]
    fn dropping_as_first_child_of_a_toggle_and_at_the_end() {
        let mut w = nested();
        w.app.drop_block("c", "b", &Slot::First);
        w.frames(vec![]);
        assert!(w.app.last_error.is_none(), "{:?}", w.app.last_error);
        assert_eq!(ids(&w), ["a", "a1", "a2", "b", "c"]);
        assert_eq!((w.app.rows[4].depth, w.app.rows[4].parent.as_str()), (1, "b"));
        w.app.drop_block("a", "p", &Slot::After("b".into()));
        w.frames(vec![]);
        assert_eq!(ids(&w), ["b", "c", "a", "a1", "a2"]);
        assert_eq!(w.app.rows[2].parent, "p");
        // very end of the page: after the last top-level block
        w.app.drop_block("b", "p", &Slot::After("a".into()));
        w.frames(vec![]);
        assert_eq!(ids(&w), ["a", "a1", "a2", "b", "c"]);
        assert_eq!(w.app.rows.last().unwrap().parent, "b");
        assert_eq!(w.app.rows[3].parent, "p");
    }

    #[test]
    fn dropping_first_puts_the_block_before_existing_siblings() {
        let mut w = nested();
        // top of the page
        w.app.drop_block("c", "p", &Slot::First);
        w.frames(vec![]);
        assert!(w.app.last_error.is_none(), "{:?}", w.app.last_error);
        assert_eq!(ids(&w), ["c", "a", "a1", "a2", "b"]);
        // first child of a block that already has children
        w.app.drop_block("c", "a", &Slot::First);
        w.frames(vec![]);
        assert_eq!(ids(&w), ["a", "c", "a1", "a2", "b"]);
        assert_eq!(w.app.rows[1].parent, "a");
        // a toggle without children: first = only child
        w.app.drop_block("a1", "b", &Slot::First);
        w.frames(vec![]);
        assert_eq!(ids(&w), ["a", "c", "a2", "b", "a1"]);
        assert_eq!(w.app.rows[4].parent, "b");
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
    fn a_click_below_the_last_block_adds_a_line_or_reuses_an_empty_one() {
        let mut w = Win::with_texts(&["a"]);
        w.app.apply(Action::AppendEnd);
        assert_eq!(w.app.rows.len(), 2);
        assert_eq!(focus_idx(&w), Some(1), "the new last line is edited");
        assert_eq!(w.queued(), 1);
        // its click again: the empty last line is reused, nothing new
        w.app.apply(Action::AppendEnd);
        assert_eq!((w.app.rows.len(), w.queued()), (2, 1));
        assert_eq!(focus_idx(&w), Some(1));
        w.type_text("b");
        assert_eq!(w.text(), "b");
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
        assert!(matches!(
            w.app.rows[1].marker,
            crate::outline::Marker::Code(_)
        ));
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
        assert_eq!(
            ids(&w),
            ["p-0", "p-1"],
            "one press deletes one row, its repeats none"
        );
        assert_eq!(focus_idx(&w), Some(1));
        // a row with children stays
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &[""]);
        store
            .replace_children(
                "p",
                "p-0",
                &[blk(
                    "k",
                    "p-0",
                    "paragraph",
                    json!({ "rich_text": rt("kid") }),
                )],
            )
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
        assert_eq!(
            (w.app.rows[0].depth, w.queued(), w.app.undo.len()),
            (0, 0, 0)
        );
        // a subtree push could not recreate
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["a", "b"]);
        store
            .replace_children(
                "p",
                "p-1",
                &[blk(
                    "c",
                    "p-1",
                    "bookmark",
                    json!({ "url": "https://x.example", "caption": [] }),
                )],
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
    fn resize_image_caches_the_width_and_queues_one_update() {
        let mut w = Win::with_texts(&["a"]);
        let img = blk("img", "p", "image", json!({ "type": "external", "external": { "url": "u" } }));
        w.app.store.replace_children("p", "p", &[img]).unwrap();
        w.app.refresh();
        w.app.apply(Action::ResizeImage { id: "img".into(), width: 320.0 });
        let b = w.app.store.get_block("img").unwrap().unwrap();
        assert_eq!(notion_core::model::image_width(&b.raw), Some(320.0));
        assert_eq!(kinds(&w), ["update_block"]);
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
        assert_eq!(
            w.app.rows[0].marker,
            crate::outline::Marker::Equation,
            "`$$ ` is in iced's list"
        );
        w.type_text("/");
        assert!(w.menu().is_none(), "no slash menu in an equation row");
        // Ctrl+Shift+0 turns it back into text
        w.press(Key::Num0, CS);
        assert_eq!(w.app.rows[0].marker, crate::outline::Marker::None);
        w.press(Key::Num8, CS);
        w.type_text("/");
        assert!(w.menu().is_none(), "nor in a code row");
    }

    fn img_with(w: &mut Win, id: &str, ty: &str, payload: serde_json::Value) {
        let b = blk(id, "p", ty, payload);
        w.app.store.replace_children("p", "p", &[b]).unwrap();
        w.app.refresh();
    }

    fn width_of(w: &Win, id: &str) -> Option<f32> {
        let b = w.app.store.get_block(id).unwrap().unwrap();
        notion_core::model::image_width(&b.raw)
    }

    #[test]
    fn resizing_twice_before_a_push_keeps_the_last_width_in_one_op() {
        let mut w = Win::with_texts(&["a"]);
        img_with(&mut w, "img", "image", json!({ "type": "external", "external": { "url": "u" } }));
        w.app.apply(Action::ResizeImage { id: "img".into(), width: 200.0 });
        w.app.apply(Action::ResizeImage { id: "img".into(), width: 340.0 });
        assert_eq!(width_of(&w, "img"), Some(340.0));
        let ops = w.app.store.queued_ops().unwrap();
        assert_eq!(ops.len(), 1, "{ops:?}");
        let OpKind::UpdateBlock { body, .. } = &ops[0].kind else { panic!() };
        assert_eq!(body["image"]["_width"], json!(340.0));
    }

    #[test]
    fn undo_of_a_resize_restores_the_previous_width() {
        let mut w = Win::with_texts(&["a"]);
        img_with(&mut w, "img", "image", json!({ "type": "external", "external": { "url": "u" }, "_width": 150 }));
        assert_eq!(width_of(&w, "img"), Some(150.0));
        w.app.apply(Action::ResizeImage { id: "img".into(), width: 400.0 });
        assert_eq!(width_of(&w, "img"), Some(400.0));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((width_of(&w, "img"), w.queued()), (Some(150.0), 0));
    }

    #[test]
    fn undo_of_the_first_resize_removes_the_width() {
        let mut w = Win::with_texts(&["a"]);
        img_with(&mut w, "img", "image", json!({ "type": "external", "external": { "url": "u" } }));
        w.app.apply(Action::ResizeImage { id: "img".into(), width: 400.0 });
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(width_of(&w, "img"), None);
    }

    #[test]
    fn video_and_pdf_resize_without_trouble() {
        for ty in ["video", "pdf"] {
            let mut w = Win::with_texts(&["a"]);
            img_with(&mut w, "m", ty, json!({ "type": "external", "external": { "url": "u" } }));
            w.app.apply(Action::ResizeImage { id: "m".into(), width: 300.0 });
            assert_eq!(width_of(&w, "m"), Some(300.0), "{ty}");
            assert!(w.app.last_error.is_none(), "{ty}: {:?}", w.app.last_error);
        }
    }

    #[test]
    fn width_on_a_block_whose_create_is_queued_folds_into_the_create_and_never_reaches_the_public_body() {
        let mut w = Win::with_texts(&["a"]);
        let b = blk("local-img", "p", "image", json!({ "type": "external", "external": { "url": "u" } }));
        w.app.store.replace_children("p", "p", std::slice::from_ref(&b)).unwrap();
        w.app
            .store
            .enqueue(
                OpKind::CreateBlock { parent_id: "p".into(), after: None, block: serde_json::to_value(&b).unwrap() },
                None,
                None,
            )
            .unwrap();
        w.app.refresh();
        w.app.apply(Action::ResizeImage { id: "local-img".into(), width: 250.0 });
        let ops = w.app.store.queued_ops().unwrap();
        assert_eq!(ops.len(), 1, "{ops:?}");
        let OpKind::CreateBlock { block, .. } = &ops[0].kind else { panic!("{ops:?}") };
        // the create op keeps the width in its cached copy ...
        assert_eq!(block["image"]["_width"], json!(250.0));
        // ... but the public request body is built from the typed kind, which can't write media
        let local: notion_core::model::Block = serde_json::from_value(block.clone()).unwrap();
        match local.kind.to_request_json() {
            Err(_) => {}
            Ok(v) => assert!(v["image"].get("_width").is_none(), "_width leaks into the CreateBlock body: {v}"),
        }
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
        assert!(
            w.app.undo.is_empty() && w.app.redo.is_empty(),
            "a pull rewrote rows"
        );
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
        for run in [json!({ "bold": true }), json!({ "color": "red" })] {
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
            assert!(
                w.app.title_edit.is_none(),
                "it would flatten the formatting"
            );
        }
        let mut v = serde_json::to_value(page_in("p", None)).unwrap();
        v["properties"]["Name"]["title"][0]["href"] = json!("https://e.x/");
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_page(&serde_json::from_value(v).unwrap())
            .unwrap();
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
            blk(
                id,
                parent,
                "table",
                json!({ "table_width": 2, "has_column_header": true, "has_row_header": false }),
            )
        };
        let trow = |id: &str, p: &str, cells: serde_json::Value| {
            blk(id, p, "table_row", json!({ "cells": cells }))
        };
        store
            .replace_children("p", "p", &[table("tbl", "p")])
            .unwrap();
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
        let Marker::Table(t) = &w.app.rows[0].marker else {
            panic!()
        };
        plain_text(&t.rows.iter().find(|r| r.id == row).unwrap().cells[col].rich_text)
    }

    #[test]
    fn tables_share_the_text_left_edge_at_every_depth() {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &[]);
        let mk = |id: &str, parent: &str, t: &str, v: serde_json::Value| {
            let mut b = blk(id, parent, t, v);
            b.has_children = true;
            b
        };
        let table = |id: &str, parent: &str| {
            blk(
                id,
                parent,
                "table",
                json!({ "table_width": 1, "has_column_header": false, "has_row_header": false }),
            )
        };
        let trow =
            |id: &str, p: &str, c: &str| blk(id, p, "table_row", json!({ "cells": [rt(c)] }));
        store
            .replace_children(
                "p",
                "p",
                &[
                    blk("t0", "p", "paragraph", json!({ "rich_text": rt("TXT0") })),
                    table("tb0", "p"),
                    mk(
                        "par",
                        "p",
                        "bulleted_list_item",
                        json!({ "rich_text": rt("PAR") }),
                    ),
                ],
            )
            .unwrap();
        store
            .replace_children("p", "tb0", &[trow("r0", "tb0", "CELL0")])
            .unwrap();
        store
            .replace_children(
                "p",
                "par",
                &[
                    blk("t1", "par", "paragraph", json!({ "rich_text": rt("TXT1") })),
                    table("tb1", "par"),
                ],
            )
            .unwrap();
        store
            .replace_children("p", "tb1", &[trow("r1", "tb1", "CELL1")])
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w.frames(vec![]);
        let x = |s: &str| {
            w.text_at
                .iter()
                .find(|(t, _)| t == s)
                .unwrap_or_else(|| panic!("{s} not drawn: {:?}", w.text_at))
                .1
                .x
        };
        let pad = f32::from(crate::theme::TABLE_PAD_X);
        let (t0, c0, t1, c1) = (x("TXT0"), x("CELL0") - pad, x("TXT1"), x("CELL1") - pad);
        assert!((t0 - c0).abs() <= 1.0, "depth 0: text {t0} vs table {c0}");
        assert!((t1 - c1).abs() <= 1.0, "depth 1: text {t1} vs table {c1}");
        assert!(
            (t1 - t0 - (c1 - c0)).abs() <= 1.0,
            "same indent step for text and table"
        );
    }

    #[test]
    fn title_breadcrumb_heading_and_paragraph_share_the_left_edge() {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, &["PARA"]);
        store
            .replace_children(
                "p",
                "p",
                &[
                    blk("h", "p", "heading_1", json!({ "rich_text": rt("HEAD") })),
                    blk("t", "p", "paragraph", json!({ "rich_text": rt("PARA") })),
                ],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w.frames(vec![]);
        let title = crate::view::untitled(&w.app.title).to_string();
        let xs = |s: &str| {
            w.text_at
                .iter()
                .filter(|(t, _)| t == s)
                .map(|(_, p)| p.x)
                .collect::<Vec<_>>()
        };
        // the title (in the page and in its tab) and the breadcrumb's
        // current-page label draw the same text
        let (t, h, p) = (xs(&title), xs("HEAD"), xs("PARA"));
        assert!(
            t.len() >= 2 && !h.is_empty() && !p.is_empty(),
            "{:?}",
            w.text_at
        );
        assert!(
            t.iter().any(|x| (x - p[0]).abs() <= 1.0),
            "the page title shares the paragraph's left edge: {:?}",
            w.text_at
        );
        for x in &h {
            assert!(
                (x - p[0]).abs() <= 1.0,
                "{x} vs paragraph {}: {:?}",
                p[0],
                w.text_at
            );
        }
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
        let OpKind::UpdateBlock { block_id, body } = &w.app.store.queued_ops().unwrap()[0].kind
        else {
            panic!()
        };
        assert_eq!(block_id, "r1");
        assert_eq!(body["table_row"]["cells"][0][0]["plain_text"], "a");
        assert_eq!(
            body["table_row"]["cells"][1][0]["annotations"]["bold"],
            true
        );
        // Enter commits too
        w.app.apply(Action::EditCell("r1".into(), 0));
        w.frames(vec![]);
        w.frames(vec![]);
        w.type_text("!");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(
            (cell_text(&w, "r1", 0), w.app.focus.is_none()),
            ("a!".into(), true)
        );
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
        assert_eq!(
            w.cmds(),
            [Cmd::Pull {
                id: PID.into(),
                force: false
            }]
        );
        w.app.sent.clear();
        w.app.store.upsert_page(&page_in(PID, None)).unwrap();
        go(
            &mut w,
            links::route(&format!("https://acme.notion.site/X-{PID}")),
            false,
        );
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
        let card =
            |id: &str, url: String| blk(id, "p", "bookmark", json!({ "url": url, "caption": [] }));
        store
            .replace_children(
                "p",
                "p",
                &[
                    card("b", format!("https://notion.so/{PID}")),
                    card("c", "https://e.x/page".into()),
                ],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        // the layout is unknown: scan the first card down the page
        for y in (60..700).step_by(4) {
            let pos = Pos2::new(400.0, y as f32);
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
            assert!(
                w.app.last_error.as_deref().unwrap().contains("only http"),
                "{bad}"
            );
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
                &[blk(
                    "web",
                    "p",
                    "paragraph",
                    json!({ "rich_text": run(&format!("https://notion.so/{PID}")) }),
                )],
            )
            .unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        for y in (60..700).step_by(5) {
            let pos = Pos2::new(400.0, y as f32);
            let ev = |pressed| Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: mods,
            };
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
        let ctrl = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let w = click_link(PointerButton::Primary, ctrl);
        assert_eq!((w.app.tabs.len(), w.app.current.as_deref()), (2, Some(PID)));
        let w = click_link(PointerButton::Middle, Modifiers::NONE);
        assert_eq!(w.app.tabs.len(), 2);
    }
    // ---------------------------------------------------- block editing

    fn win_with(kids: &[(&str, &str, serde_json::Value)]) -> Win {
        let store = Store::open_in_memory().unwrap();
        store.upsert_page(&page_in("p", None)).unwrap();
        let blocks: Vec<_> = kids
            .iter()
            .map(|(id, t, pl)| blk(id, "p", t, pl.clone()))
            .collect();
        store.replace_children("p", "p", &blocks).unwrap();
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w
    }

    fn texts(w: &Win) -> Vec<String> {
        w.app
            .rows
            .iter()
            .map(|r| plain_text(&r.rich_text))
            .collect()
    }

    fn sel(w: &Win) -> Vec<usize> {
        w.app.sel_idx()
    }

    const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::NONE
    };

    #[test]
    fn esc_selects_shift_extends_and_the_selection_acts() {
        let mut w = Win::with_texts(&["a", "b", "c", "d"]);
        w.edit(1);
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.focus.is_none(), "the editor closed");
        assert_eq!(sel(&w), [1]);
        w.frames(vec![]);
        assert_eq!(sel(&w), [1], "and the selection stays");
        w.press(Key::ArrowDown, Modifiers::SHIFT);
        assert_eq!(sel(&w), [1, 2]);
        w.press(Key::ArrowDown, Modifiers::SHIFT);
        w.press(Key::ArrowDown, Modifiers::SHIFT);
        assert_eq!(sel(&w), [1, 2, 3], "stops at the last sibling");
        w.press(Key::ArrowUp, Modifiers::SHIFT);
        assert_eq!(sel(&w), [1, 2]);
        // Ctrl+C: Markdown on the clipboard, nothing changed
        w.send(Event::Copy);
        assert_eq!(w.copied, ["b\nc"]);
        assert_eq!((w.app.rows.len(), w.queued()), (4, 0));
        // Delete: one undo step
        w.press(Key::Delete, Modifiers::NONE);
        assert_eq!(ids(&w), ["p-0", "p-3"]);
        assert_eq!((w.app.sel.is_none(), w.app.undo.len()), (true, 1));
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(ids(&w), ["p-0", "p-1", "p-2", "p-3"]);
        // Ctrl+X copies, then deletes
        w.app.sel = Some(("p-2".into(), "p-3".into()));
        w.send(Event::Cut);
        assert_eq!(w.copied, ["b\nc", "c\nd"]);
        assert_eq!(ids(&w), ["p-0", "p-1"]);
        assert_eq!(w.app.undo.len(), 1);
        // Esc clears; typing and Backspace then do nothing
        w.app.sel = Some(("p-0".into(), "p-0".into()));
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.sel.is_none());
        w.press(Key::Backspace, Modifiers::NONE);
        assert_eq!(ids(&w).len(), 2);
    }

    #[test]
    fn enter_edits_the_first_selected_block_and_backspace_deletes() {
        let mut w = Win::with_texts(&["a", "b", "c"]);
        w.app.sel = Some(("p-2".into(), "p-1".into()));
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(focus_idx(&w), Some(1));
        assert!(w.app.sel.is_none());
        w.press(Key::Escape, Modifiers::NONE);
        w.press(Key::Backspace, Modifiers::NONE);
        assert_eq!(ids(&w), ["p-0", "p-2"]);
    }

    #[test]
    fn esc_closes_a_menu_first_and_block_keys_wait_for_it() {
        let mut w = Win::with_texts(&["", "b"]);
        w.edit(0);
        w.type_text("/");
        assert!(w.menu().is_some());
        w.press(Key::D, Modifiers::CTRL);
        w.press(Key::ArrowDown, CS);
        w.press(Key::T, CTRL_ALT);
        assert_eq!(ids(&w), ["p-0", "p-1"]);
        assert_eq!(w.queued(), 0);
        assert!(w.menu().is_some(), "still open");
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.menu().is_none() && w.app.focus.is_some() && w.app.sel.is_none());
        w.press(Key::Escape, Modifiers::NONE);
        assert!(w.app.focus.is_none() && w.app.sel.is_some());
    }

    #[test]
    fn ctrl_d_duplicates_the_edited_or_selected_blocks_one_step_each() {
        let mut w = Win::with_texts(&["a", "b"]);
        w.edit(0);
        w.press(Key::D, Modifiers::CTRL);
        assert_eq!(texts(&w), ["a", "a", "b"]);
        assert_eq!(focus_idx(&w), Some(0), "the editor stays on the original");
        assert_eq!(w.app.undo.len(), 1);
        assert_eq!(kinds(&w), ["create_block"]);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(texts(&w), ["a", "b"]);
        // a selection: copies follow the whole run, in order
        w.press(Key::Escape, Modifiers::NONE);
        w.press(Key::ArrowDown, Modifiers::SHIFT);
        assert_eq!(sel(&w), [0, 1]);
        w.press(Key::D, Modifiers::CTRL);
        assert_eq!(texts(&w), ["a", "b", "a", "b"]);
        assert_eq!(sel(&w), [2, 3], "the copies are selected");
        assert_eq!(w.app.undo.len(), 1);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(texts(&w), ["a", "b"]);
    }

    #[test]
    fn duplicate_takes_the_children_along() {
        let mut w = win_with(&[("t", "toggle", json!({ "rich_text": rt("T") }))]);
        let kid = blk("k", "t", "paragraph", json!({ "rich_text": rt("kid") }));
        w.app.store.replace_children("t", "t", &[kid]).unwrap();
        w.app.refresh();
        w.app.sel = Some(("t".into(), "t".into()));
        w.press(Key::D, Modifiers::CTRL);
        assert_eq!(texts(&w), ["T", "kid", "T", "kid"]);
        assert_eq!(kinds(&w), ["create_block", "create_block"]);
    }

    #[test]
    fn ctrl_shift_arrows_move_among_siblings_one_step_each() {
        let mut w = Win::with_texts(&["a", "b", "c"]);
        w.edit(2);
        w.press(Key::ArrowUp, CS);
        assert_eq!(texts(&w), ["a", "c", "b"]);
        assert_eq!(focus_idx(&w), Some(1), "the editor goes along");
        assert_eq!(w.app.undo.len(), 1);
        assert_eq!(kinds(&w), ["move_block"]);
        w.press(Key::ArrowUp, CS);
        assert_eq!(texts(&w), ["c", "a", "b"]);
        let n = (w.app.undo.len(), w.queued());
        w.press(Key::ArrowUp, CS);
        assert_eq!((w.app.undo.len(), w.queued()), n, "at the top: nothing");
        assert_eq!(texts(&w), ["c", "a", "b"]);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(texts(&w), ["a", "c", "b"]);
        // a selected run moves down as one
        w.press(Key::Escape, Modifiers::NONE);
        w.app.sel = Some((
            w.app.rows[0].block_id.clone(),
            w.app.rows[1].block_id.clone(),
        ));
        w.press(Key::ArrowDown, CS);
        assert_eq!(texts(&w), ["b", "a", "c"]);
        assert_eq!(sel(&w), [1, 2]);
        w.press(Key::ArrowDown, CS);
        assert_eq!(texts(&w), ["b", "a", "c"], "at the bottom: nothing");
    }

    #[test]
    fn a_commented_block_is_not_moved() {
        let mut w = Win::with_texts(&["a", "b"]);
        let c: notion_core::model::Comment = serde_json::from_value(json!({
            "object": "comment", "id": "c1", "discussion_id": "d1",
            "parent": { "type": "block_id", "block_id": "p-0" },
            "created_time": "2025-01-01T00:00:00.000Z", "last_edited_time": "2025-01-01T00:00:00.000Z",
            "created_by": { "object": "user", "id": "u" },
            "rich_text": [{ "type": "text", "text": { "content": "x" }, "plain_text": "x", "annotations": {} }],
        }))
        .unwrap();
        w.app.store.put_comment(&c).unwrap();
        w.app.refresh();
        w.app.sel = Some(("p-0".into(), "p-0".into()));
        w.press(Key::ArrowDown, CS);
        assert_eq!(texts(&w), ["a", "b"]);
        assert!(w
            .app
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("comments"));
        // up: the block above is the one that would be re-created
        w.app.last_error = None;
        w.app.sel = Some(("p-1".into(), "p-1".into()));
        w.press(Key::ArrowUp, CS);
        assert!(w
            .app
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("the block above has comments"));
        assert_eq!((w.queued(), w.app.undo.len()), (0, 0));
    }

    #[test]
    fn unmovable_neighbours_and_undeletable_selections_are_refused() {
        let img = json!({ "type": "external", "external": { "url": "https://e.com/a.png" } });
        let mut w = win_with(&[
            ("i", "image", img),
            ("a", "paragraph", json!({ "rich_text": rt("a") })),
            ("c", "child_page", json!({ "title": "Sub" })),
        ]);
        // up past an image: nothing queued
        w.app.sel = Some(("a".into(), "a".into()));
        w.press(Key::ArrowUp, CS);
        assert!(w
            .app
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("can't move"));
        assert_eq!(
            (ids(&w), w.queued(), w.app.undo.len()),
            (vec!["i".to_string(), "a".into(), "c".into()], 0, 0)
        );
        // a run holding a child page is neither deleted nor cut
        w.app.last_error = None;
        w.app.sel = Some(("a".into(), "c".into()));
        w.press(Key::Delete, Modifiers::NONE);
        w.send(Event::Cut);
        assert_eq!((w.app.rows.len(), w.queued(), w.copied.len()), (3, 0, 0));
        assert!(w
            .app
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("not deleting"));
        // copy still works
        w.send(Event::Copy);
        assert_eq!(w.copied.len(), 1);
        // Enter on a selection that starts at a non-editable block edits nothing
        w.app.sel = Some(("i".into(), "i".into()));
        w.press(Key::Enter, Modifiers::NONE);
        assert!(w.app.focus.is_none());
    }

    #[test]
    fn colours_via_the_slash_menu_render_in_both_themes_and_undo() {
        let mut w = Win::with_texts(&[""]);
        w.edit(0);
        w.type_text("/");
        w.type_text("color red");
        assert_eq!(
            w.menu().map(|m| m.kind),
            Some(crate::menu::Kind::Slash),
            "the colour entries keep the menu open"
        );
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.rows[0].color, "red");
        assert_eq!(w.text(), "", "the trigger text is gone");
        assert_eq!(kinds(&w), ["update_block"]);
        let body = serde_json::to_value(&w.app.store.queued_ops().unwrap()[0].kind).unwrap();
        assert_eq!(body["body"]["paragraph"]["color"], "red");
        w.press(Key::Escape, Modifiers::NONE);
        // a background tint, German name
        w.edit(0);
        w.type_text("/");
        w.type_text("farbe blau hinter");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.rows[0].color, "blue_background");
        for theme in [egui::ThemePreference::Dark, egui::ThemePreference::Light] {
            w.ctx.set_theme(theme);
            w.frames(vec![]);
            w.frames(vec![]);
            w.frames(vec![]);
        }
        w.press(Key::Escape, Modifiers::NONE);
        w.press(Key::Z, Modifiers::CTRL);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!((w.app.rows[0].color.as_str(), w.queued()), ("default", 0));
    }

    #[test]
    fn toggle_headings_collapse_and_heading_4_renders() {
        let mut w = win_with(&[
            (
                "h",
                "heading_2",
                json!({ "rich_text": rt("Head"), "is_toggleable": true }),
            ),
            (
                "h4",
                "heading_4",
                json!({ "rich_text": rt("Four"), "is_toggleable": false }),
            ),
        ]);
        w.app
            .store
            .replace_children(
                "h",
                "h",
                &[blk(
                    "k",
                    "h",
                    "paragraph",
                    json!({ "rich_text": rt("kid") }),
                )],
            )
            .unwrap();
        w.app.refresh();
        w.frames(vec![]);
        assert!(w.app.rows[0].toggle);
        assert_eq!(w.app.rows[2].marker, Marker::Heading(4));
        assert!(w.shown.contains("Four") && !w.shown.contains("kid"));
        w.app.apply(Action::Toggle(0));
        w.frames(vec![]);
        assert!(w.shown.contains("kid"), "{}", w.shown);
        // and the `/` menu makes one from a paragraph
        let mut w = Win::with_texts(&[""]);
        w.edit(0);
        w.type_text("/");
        w.type_text("th3");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(
            (w.app.rows[0].marker.clone(), w.app.rows[0].toggle),
            (Marker::Heading(3), true)
        );
    }

    #[test]
    fn enter_splits_at_the_cursor_and_keeps_marks() {
        let mut w = win_with(&[(
            "a",
            "paragraph",
            json!({ "rich_text": [{ "type": "text", "text": { "content": "bold tail" },
                "plain_text": "bold tail", "annotations": { "bold": true } }] }),
        )]);
        w.edit(0);
        w.select(6, 6); // **bold| tail**
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(texts(&w), ["bold", " tail"]);
        assert!(w.app.rows.iter().all(|r| r.rich_text[0].annotations.bold));
        assert_eq!(focus_idx(&w), Some(1));
        assert_eq!(w.app.undo.len(), 1);
        assert_eq!(kinds(&w), ["update_block", "create_block"]);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(texts(&w), ["bold tail"]);
    }

    #[test]
    fn enter_continues_lists_ends_headings_and_empties_a_list_item() {
        let mut w = win_with(&[
            ("b", "bulleted_list_item", json!({ "rich_text": rt("ab") })),
            ("h", "heading_1", json!({ "rich_text": rt("Head") })),
        ]);
        w.edit(0);
        w.select(1, 1);
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(texts(&w), ["a", "b", "Head"]);
        assert_eq!(
            (w.app.rows[0].marker.clone(), w.app.rows[1].marker.clone()),
            (Marker::Bullet, Marker::Bullet)
        );
        // Enter on the now-empty item turns it into a paragraph
        w.press(Key::Delete, Modifiers::NONE);
        assert_eq!(w.text(), "");
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(
            (texts(&w).len(), w.app.rows[1].marker.clone()),
            (3, Marker::None)
        );
        // a heading's tail goes to a paragraph
        w.app.focus = None;
        w.edit(2);
        w.press(Key::Enter, Modifiers::NONE);
        assert_eq!(w.app.rows[2].marker, Marker::Heading(1));
        assert_eq!(
            (w.app.rows[3].marker.clone(), texts(&w)[3].as_str()),
            (Marker::None, "")
        );
    }

    #[test]
    fn pasting_lines_into_an_empty_row_makes_a_block_each() {
        let mut w = Win::with_texts(&["", "z"]);
        w.edit(0);
        w.send(Event::Paste("# Title\n- one\n\nplain **x**".into()));
        assert_eq!(texts(&w), ["Title", "one", "plain x", "z"]);
        let m: Vec<_> = w.app.rows.iter().map(|r| r.marker.clone()).collect();
        assert_eq!(m[..3], [Marker::Heading(1), Marker::Bullet, Marker::None]);
        assert_eq!(w.app.undo.len(), 1);
        w.press(Key::Z, Modifiers::CTRL);
        assert_eq!(texts(&w), ["", "z"]);
        // one line, or a non-empty row: plain text for the editor
        w.edit(1);
        w.send(Event::Paste("a\nb".into()));
        assert_eq!(ids(&w).len(), 2);
    }

    #[test]
    fn ctrl_alt_t_opens_all_toggles_or_closes_them() {
        let mut w = win_with(&[
            ("t1", "toggle", json!({ "rich_text": rt("T1") })),
            (
                "t2",
                "heading_1",
                json!({ "rich_text": rt("T2"), "is_toggleable": true }),
            ),
        ]);
        w.press(Key::T, CTRL_ALT);
        assert!(w.app.open.contains("t1") && w.app.open.contains("t2"));
        w.press(Key::T, CTRL_ALT);
        assert!(w.app.open.is_empty());
        w.app.open.insert("t1".into());
        w.press(Key::T, CTRL_ALT);
        assert_eq!(w.app.open.len(), 2, "any closed: open all");
        // AltGr+T types a letter: no Ctrl or Alt reaches the app
        w.press(Key::T, Modifiers::NONE);
        assert_eq!(w.app.open.len(), 2);
        w.edit(0);
        w.type_text("t");
        assert!(w.text().ends_with('t'));
    }

    // -------------------------------------------------------------- AI helpers

    #[test]
    fn page_markdown_includes_nested_children() {
        let mut w = Win::with_texts(&["a", "b", "c"]);
        w.edit(1);
        w.press(Key::Tab, Modifiers::NONE);
        w.edit(2);
        w.press(Key::Tab, Modifiers::NONE);
        w.press(Key::Tab, Modifiers::NONE);
        assert_eq!(w.app.rows[2].depth, 2);
        assert_eq!(w.app.page_markdown(), "a\n  b\n    c");
    }

    #[test]
    fn insert_markdown_into_empty_and_nonempty_pages() {
        let mut w = Win::with_texts(&[]);
        w.app.insert_markdown("# H\n- x");
        assert_eq!(w.app.page_markdown(), "# H\n- x");
        let mut w = Win::with_texts(&["keep"]);
        w.app.insert_markdown("new");
        assert_eq!(w.app.page_markdown(), "keep\nnew");
        assert_eq!(w.app.info.as_deref(), Some("inserted"));
    }
}
