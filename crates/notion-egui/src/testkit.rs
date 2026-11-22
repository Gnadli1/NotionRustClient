//! Test helpers: a headless egui window around an `App`, with its command
//! channel in reach and a fake worker end for events.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, RawInput, ViewportCommand, ViewportId};
use notion_core::model::Page;
use notion_core::store::Store;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::app::{Action, App, Grab};
use crate::menu::Menu;
use crate::outline::tests::{blk, rt};
use crate::worker::{Cmd, Event as WEvent};

pub fn page(id: &str, parent: Value) -> Page {
    let t = "2025-01-01T00:00:00.000Z";
    serde_json::from_value(json!({
        "object": "page", "id": id, "parent": parent, "created_time": t,
        "last_edited_time": t, "in_trash": false, "url": "https://notion.so/x",
        "properties": { "Name": { "id": "title", "type": "title", "title": [
            { "type": "text", "text": { "content": id }, "plain_text": id, "annotations": {} }] } },
    }))
    .unwrap()
}

/// A page whose title is its id, under page `parent` (or the workspace).
pub fn page_in(id: &str, parent: Option<&str>) -> Page {
    page(
        id,
        match parent {
            Some(p) => json!({ "type": "page_id", "page_id": p }),
            None => json!({ "type": "workspace", "workspace": true }),
        },
    )
}

/// Adds page `id` with one paragraph per text (blocks `{id}-0`, `{id}-1`, ...).
pub fn add_page(store: &Store, id: &str, parent: Option<&str>, texts: &[&str]) {
    store.upsert_page(&page_in(id, parent)).unwrap();
    let kids: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            blk(
                &format!("{id}-{i}"),
                id,
                "paragraph",
                json!({ "rich_text": rt(t) }),
            )
        })
        .collect();
    store.replace_children(id, id, &kids).unwrap();
}

pub fn key(k: Key) -> Event {
    key_mod(k, Modifiers::NONE)
}

pub fn key_mod(k: Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key: k,
        physical_key: Some(k),
        pressed: true,
        repeat: false,
        modifiers,
    }
}

pub struct Win {
    pub ctx: egui::Context,
    pub app: App,
    /// Commands the app sent to the worker.
    pub rx: UnboundedReceiver<Cmd>,
    /// The worker's end: send events as if it had answered.
    pub ev: Sender<WEvent>,
    /// The URLs the app handed to its opener (nothing is ever spawned).
    pub opened: Rc<RefCell<Vec<String>>>,
    /// Keys pressed in the last frame, released at the start of the next.
    down: Vec<(Key, Modifiers)>,
    /// The text the last frame drew, one line per label.
    pub shown: String,
    /// Each text the last frame drew, with its top-left corner.
    pub text_at: Vec<(String, egui::Pos2)>,
    /// Texts the app put on the clipboard (all frames so far).
    pub copied: Vec<String>,
}

impl Win {
    pub fn new(store: Store) -> Win {
        let (tx, rx) = unbounded_channel();
        let (ev, events) = channel();
        let ctx = egui::Context::default();
        crate::setup_fonts(&ctx);
        crate::theme::install(&ctx);
        let mut app = App::new(store, tx, events, false);
        // the first frame has the focus already: no focus-gain auto-pull
        app.was_focused = true;
        // users pulled a while ago: not due for the tick, past the on-demand throttle
        app.users_at = Instant::now().checked_sub(Duration::from_secs(120));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = opened.clone();
        app.opener = Box::new(move |url| {
            sink.borrow_mut().push(url.to_string());
            Ok(())
        });
        let mut w = Win {
            ctx,
            app,
            rx,
            ev,
            opened,
            down: Vec::new(),
            shown: String::new(),
            text_at: Vec::new(),
            copied: Vec::new(),
        };
        w.frames(vec![]);
        w
    }

    /// One page "p" with a paragraph per text (blocks `p-0`, ...), open.
    pub fn with_texts(texts: &[&str]) -> Win {
        let store = Store::open_in_memory().unwrap();
        add_page(&store, "p", None, texts);
        let mut w = Win::new(store);
        w.app.apply(Action::Open("p".into()));
        w.frames(vec![]);
        w
    }

    /// Runs one frame; returns the viewport commands it sent.
    pub fn frame_with(&mut self, events: Vec<Event>, modifiers: Modifiers) -> Vec<ViewportCommand> {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
        // egui flags a press of a key still down as a repeat: let go of what
        // the last frame pressed
        let mut all: Vec<Event> = self
            .down
            .drain(..)
            .map(|(key, modifiers)| Event::Key {
                key,
                physical_key: Some(key),
                pressed: false,
                repeat: false,
                modifiers,
            })
            .collect();
        all.push(Event::ModifiersChanged(modifiers));
        for e in &events {
            if let Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = e
            {
                self.down.push((*key, *modifiers));
            }
        }
        all.extend(events);
        let raw = RawInput {
            screen_rect: Some(screen),
            events: all,
            ..Default::default()
        };
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui));
        let cmds = out
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default();
        self.copied
            .extend(out.platform_output.commands.iter().filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            }));
        self.text_at = out
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::epaint::Shape::Text(t) => Some((t.galley.text().to_string(), t.pos)),
                _ => None,
            })
            .collect();
        self.shown = out
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.drop_without_applying_deltas();
        cmds
    }

    pub fn frames(&mut self, events: Vec<Event>) -> Vec<ViewportCommand> {
        self.frame_with(events, Modifiers::NONE)
    }

    /// An OS key-repeat of `k`, which the last frame pressed: it is still down.
    pub fn repeat(&mut self, k: Key) {
        self.down.clear();
        self.frames(vec![Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: true,
            modifiers: Modifiers::NONE,
        }]);
    }

    pub fn send(&mut self, ev: Event) {
        self.frames(vec![ev]);
        self.frames(vec![]);
    }

    /// A shortcut: the key with `modifiers` held for one frame, then a quiet one.
    pub fn press(&mut self, k: Key, modifiers: Modifiers) {
        self.frame_with(vec![key_mod(k, modifiers)], modifiers);
        self.frames(vec![]);
    }

    pub fn type_text(&mut self, t: &str) {
        self.send(Event::Text(t.into()));
    }

    /// Commands the app sent since the last call.
    pub fn cmds(&mut self) -> Vec<Cmd> {
        std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
    }

    /// The worker answers: the event is handled on the next frame.
    pub fn answer(&mut self, ev: WEvent) {
        self.ev.send(ev).unwrap();
        self.frames(vec![]);
    }

    /// Starts editing row `i`, with the cursor at the end.
    pub fn edit(&mut self, i: usize) {
        self.app.apply(Action::Edit(i, Grab::End));
        self.frames(vec![]);
        self.frames(vec![]);
    }

    pub fn text(&self) -> &str {
        &self.app.focus.as_ref().unwrap().text
    }

    pub fn menu(&self) -> Option<&Menu> {
        self.app.focus.as_ref().and_then(|f| f.menu.as_ref())
    }

    /// Selects chars `a..b` of the focused editor (cursor at `b`).
    pub fn select(&mut self, a: usize, b: usize) {
        let id = crate::app::edit_id(&self.app.focus.as_ref().unwrap().block_id);
        let mut st = egui::text_edit::TextEditState::load(&self.ctx, id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(a),
            egui::text::CCursor::new(b),
        )));
        st.store(&self.ctx, id);
        self.frames(vec![]);
    }

    /// The text of row `id` as stored (plain).
    pub fn row_text(&self, id: &str) -> String {
        let r = self.app.rows.iter().find(|r| r.block_id == id).unwrap();
        notion_core::model::plain_text(&r.rich_text)
    }

    pub fn queued(&self) -> usize {
        self.app.store.queued_ops().unwrap().len()
    }
}
