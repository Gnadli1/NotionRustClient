//! App state, update and view. `update` only touches the store and sends
//! commands to the worker, so it is testable without a window.

use std::sync::Arc;

use iced::font::{Style, Weight};
use iced::keyboard::{self, Key};
use iced::widget::text::Span;
use iced::widget::{button, column, container, rich_text, row, scrollable, span, text, text_input};
use iced::{event, Color, Element, Fill, Font, Subscription};
use notion_core::model::{Parent, RichText};
use notion_core::ops::OpStatus;
use notion_core::store::Store;
use tokio::sync::mpsc::UnboundedSender;

use crate::outline::{self, Marker, Row};
use crate::worker::{Cmd, Event};

#[derive(Debug, Clone)]
pub enum Message {
    /// Show a cached page (no network).
    Open(String),
    Pull {
        force: bool,
    },
    Push,
    SearchInput(String),
    SearchSubmit,
    /// Search result chosen: pull it, then open it.
    PickResult(String),
    Worker(Arc<Event>),
    /// The worker's event stream ended (thread exited or panicked).
    WorkerGone,
}

pub struct State {
    store: Store,
    cmds: UnboundedSender<Cmd>,
    pages: Vec<(String, String)>,
    current: Option<String>,
    title: String,
    rows: Vec<Row>,
    /// queued, conflict, failed
    counts: [usize; 3],
    last_error: Option<String>,
    search: String,
    results: Vec<(String, String)>,
    /// Picked search result waiting for its pull.
    pending_open: Option<String>,
    in_flight: usize,
}

impl State {
    pub fn new(store: Store, cmds: UnboundedSender<Cmd>) -> Self {
        let mut s = State {
            store,
            cmds,
            pages: Vec::new(),
            current: None,
            title: String::new(),
            rows: Vec::new(),
            counts: [0; 3],
            last_error: None,
            search: String::new(),
            results: Vec::new(),
            pending_open: None,
            in_flight: 0,
        };
        s.refresh();
        s
    }

    fn send(&mut self, cmd: Cmd) {
        match self.cmds.send(cmd) {
            Ok(()) => self.in_flight += 1,
            Err(_) => self.last_error = Some("sync worker stopped".into()),
        }
    }

    /// Reloads sidebar, outline and op counts from the store.
    fn refresh(&mut self) {
        if let Err(e) = self.try_refresh() {
            self.last_error = Some(e.to_string());
        }
    }

    fn try_refresh(&mut self) -> notion_core::Result<()> {
        // ponytail: one get_page per cached page; fine for hundreds, add a
        // Store query if the cache grows to thousands
        self.pages.clear();
        for (id, title) in self.store.pages()? {
            let row = self
                .store
                .get_page(&id)?
                .is_some_and(|p| matches!(p.parent, Parent::DataSource { .. }));
            if !row {
                self.pages.push((id, title));
            }
        }
        if let Some(id) = &self.current {
            self.title = self
                .store
                .get_page(id)?
                .map_or_else(|| "(not cached)".into(), |p| untitled(&p.title()).into());
            self.rows = outline::flatten(&self.store, id)?;
        }
        self.counts = [
            self.store.queued_ops()?.len(),
            self.store.ops_by_status(OpStatus::Conflict)?.len(),
            self.store.ops_by_status(OpStatus::Failed)?.len(),
        ];
        Ok(())
    }
}

pub fn update(s: &mut State, msg: Message) {
    match msg {
        Message::Open(id) => {
            s.pending_open = None;
            s.current = Some(id);
            s.refresh();
        }
        Message::Pull { force } => {
            if let Some(id) = s.current.clone() {
                s.send(Cmd::Pull { id, force });
            }
        }
        Message::Push => s.send(Cmd::Push),
        Message::SearchInput(q) => s.search = q,
        Message::SearchSubmit => {
            let q = s.search.trim().to_string();
            if !q.is_empty() {
                s.send(Cmd::Search(q));
            }
        }
        Message::PickResult(id) => {
            if matches!(s.store.get_page(&id), Ok(Some(_))) {
                s.current = Some(id.clone());
                s.refresh();
            }
            s.pending_open = Some(id.clone());
            s.send(Cmd::Pull { id, force: false });
        }
        Message::WorkerGone => {
            s.in_flight = 0;
            s.last_error = Some("sync worker stopped".into());
        }
        Message::Worker(ev) => {
            s.in_flight = s.in_flight.saturating_sub(1);
            match &*ev {
                Event::Pulled { id, .. } => {
                    if s.pending_open.as_ref() == Some(id) {
                        s.pending_open = None;
                        s.current = Some(id.clone());
                    }
                    s.last_error = None;
                    s.refresh();
                }
                Event::Pushed(_) => {
                    s.last_error = None;
                    s.refresh();
                    // force: the page's edit time may match the cache, and the
                    // re-pull is what turns local ids into real ones
                    if let Some(id) = s.current.clone() {
                        s.send(Cmd::Pull { id, force: true });
                    }
                }
                Event::SearchResults(r) => s.results = r.clone(),
                Event::Error(e) => {
                    s.pending_open = None;
                    // a push can fail after applying some ops
                    s.refresh();
                    s.last_error = Some(e.clone());
                }
            }
        }
    }
}

/// Ctrl+R pull, Ctrl+Shift+R force pull, Ctrl+S push. Sees events even when a
/// widget (the search box) captured them.
fn shortcut(ev: iced::Event, _: event::Status, _: iced::window::Id) -> Option<Message> {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modifiers,
        repeat: false,
        ..
    }) = ev
    else {
        return None;
    };
    let Key::Character(c) = key.as_ref() else {
        return None;
    };
    if !modifiers.command() {
        return None;
    }
    if c.eq_ignore_ascii_case("r") {
        Some(Message::Pull {
            force: modifiers.shift(),
        })
    } else if c.eq_ignore_ascii_case("s") {
        Some(Message::Push)
    } else {
        None
    }
}

pub fn subscription(_: &State) -> Subscription<Message> {
    event::listen_with(shortcut)
}

fn untitled(t: &str) -> &str {
    if t.is_empty() {
        "Untitled"
    } else {
        t
    }
}

const GREY: Color = Color::from_rgb(0.55, 0.55, 0.55);
const LINK: Color = Color::from_rgb(0.25, 0.45, 0.9);

fn styled(t: &RichText) -> Span<'_, ()> {
    let a = &t.annotations;
    let mut font = if a.code {
        Font::MONOSPACE
    } else {
        Font::DEFAULT
    };
    if a.bold {
        font.weight = Weight::Bold;
    }
    if a.italic {
        font.style = Style::Italic;
    }
    span(t.plain_text.as_str())
        .font(font)
        .strikethrough(a.strikethrough)
        .underline(a.underline || t.href.is_some())
        .color_maybe(t.href.as_ref().map(|_| LINK))
}

fn row_view(r: &Row) -> Element<'_, Message> {
    let size = match r.marker {
        Marker::Heading(1) => 28,
        Marker::Heading(2) => 22,
        Marker::Heading(3) => 18,
        _ => 15,
    };
    let grey = matches!(
        r.marker,
        Marker::Unsupported(_) | Marker::ChildPage(_) | Marker::ChildDatabase(_) | Marker::Divider
    );
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    let label = r.marker.label();
    if !label.is_empty() {
        let sep = if matches!(r.marker, Marker::Code(_)) {
            "\n"
        } else {
            " "
        };
        spans.push(span(format!("{label}{sep}")).color_maybe(grey.then_some(GREY)));
    }
    spans.extend(r.rich_text.iter().map(styled));
    container(rich_text(spans).size(size))
        .padding(iced::Padding::ZERO.left(r.depth as f32 * 24.0))
        .into()
}

pub fn view(s: &State) -> Element<'_, Message> {
    let mut side = column![text_input("Search Notion…", &s.search)
        .on_input(Message::SearchInput)
        .on_submit(Message::SearchSubmit)]
    .spacing(4)
    .width(240);
    for (id, title) in &s.results {
        side = side.push(
            button(text(untitled(title)))
                .on_press(Message::PickResult(id.clone()))
                .style(button::text),
        );
    }
    if !s.results.is_empty() {
        side = side.push(text("Cached").color(GREY));
    }
    for (id, title) in &s.pages {
        let style = if s.current.as_ref() == Some(id) {
            button::secondary
        } else {
            button::text
        };
        side = side.push(
            button(text(untitled(title)))
                .on_press(Message::Open(id.clone()))
                .style(style),
        );
    }

    let main: Element<_> = match &s.current {
        None => text("Pick a page on the left, or search.")
            .color(GREY)
            .into(),
        Some(_) => column![
            text(&s.title).size(32),
            scrollable(column(s.rows.iter().map(row_view)).spacing(6)).height(Fill)
        ]
        .spacing(12)
        .into(),
    };

    let [queued, conflicts, failed] = s.counts;
    let mut status = format!("queued {queued} · conflicts {conflicts} · failed {failed}");
    if s.in_flight > 0 {
        status.push_str(" · syncing…");
    }
    if let Some(e) = &s.last_error {
        status.push_str(&format!(" · error: {e}"));
    }

    column![
        row![scrollable(side), container(main).width(Fill).height(Fill)]
            .spacing(16)
            .height(Fill),
        text(status).size(13).color(GREY),
    ]
    .padding(12)
    .spacing(8)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notion_core::model::Page;
    use serde_json::json;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn page(id: &str, parent: serde_json::Value) -> Page {
        let t = "2025-01-01T00:00:00.000Z";
        serde_json::from_value(json!({
            "object": "page", "id": id, "parent": parent, "created_time": t,
            "last_edited_time": t, "in_trash": false, "url": "https://notion.so/x",
            "properties": { "Name": { "id": "title", "type": "title", "title": [
                { "type": "text", "text": { "content": id }, "plain_text": id, "annotations": {} }] } },
        }))
        .unwrap()
    }

    #[test]
    fn sidebar_skips_data_source_rows() {
        let (mut s, _rx) = state();
        s.store
            .upsert_page(&page(
                "a",
                json!({ "type": "workspace", "workspace": true }),
            ))
            .unwrap();
        s.store
            .upsert_page(&page(
                "row",
                json!({ "type": "data_source_id", "data_source_id": "ds", "database_id": "db" }),
            ))
            .unwrap();
        s.refresh();
        assert_eq!(s.pages, vec![("a".into(), "a".into())]);
    }

    #[test]
    fn pending_open_is_dropped_and_worker_loss_shown() {
        let (mut s, _rx) = state();
        update(&mut s, Message::PickResult("p1".into()));
        update(&mut s, Message::Open("p2".into()));
        let pulled = || {
            Message::Worker(Arc::new(Event::Pulled {
                id: "p1".into(),
                report: Default::default(),
            }))
        };
        update(&mut s, pulled());
        assert_eq!(s.current.as_deref(), Some("p2"), "user's choice wins");

        update(&mut s, Message::PickResult("p1".into()));
        update(
            &mut s,
            Message::Worker(Arc::new(Event::Error("offline".into()))),
        );
        update(&mut s, pulled());
        assert_eq!(s.current.as_deref(), Some("p2"), "failed pick is forgotten");

        update(&mut s, Message::Pull { force: false });
        update(&mut s, Message::WorkerGone);
        assert_eq!(s.in_flight, 0);
        assert_eq!(s.last_error.as_deref(), Some("sync worker stopped"));
    }

    fn state() -> (State, UnboundedReceiver<Cmd>) {
        let (tx, rx) = unbounded_channel();
        (State::new(Store::open_in_memory().unwrap(), tx), rx)
    }

    fn key(c: &str, ctrl: bool, shift: bool) -> iced::Event {
        let mut m = keyboard::Modifiers::empty();
        m.set(keyboard::Modifiers::CTRL, ctrl);
        m.set(keyboard::Modifiers::SHIFT, shift);
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character(c.into()),
            modified_key: Key::Character(c.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: m,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn shortcuts() {
        let id = iced::window::Id::unique();
        let st = event::Status::Captured;
        let k = |c, ctrl, shift| shortcut(key(c, ctrl, shift), st, id);
        assert!(matches!(
            k("r", true, false),
            Some(Message::Pull { force: false })
        ));
        assert!(matches!(
            k("R", true, true),
            Some(Message::Pull { force: true })
        ));
        assert!(matches!(k("s", true, false), Some(Message::Push)));
        assert!(k("s", false, false).is_none());
        let mut held = key("s", true, false);
        if let iced::Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut held {
            *repeat = true;
        }
        assert!(shortcut(held, st, id).is_none(), "key repeat is ignored");
    }

    #[test]
    fn push_then_repull_and_pick_result() {
        let (mut s, mut rx) = state();
        update(&mut s, Message::Pull { force: false });
        assert!(rx.try_recv().is_err(), "no page open: nothing to pull");

        update(&mut s, Message::PickResult("p1".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "p1".into(),
                force: false
            }
        );
        assert_eq!(s.current, None, "not cached yet");
        let pulled = Event::Pulled {
            id: "p1".into(),
            report: Default::default(),
        };
        update(&mut s, Message::Worker(Arc::new(pulled)));
        assert_eq!(s.current.as_deref(), Some("p1"));

        update(&mut s, Message::Push);
        assert_eq!(rx.try_recv().unwrap(), Cmd::Push);
        update(
            &mut s,
            Message::Worker(Arc::new(Event::Pushed(Default::default()))),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Cmd::Pull {
                id: "p1".into(),
                force: true
            }
        );
        assert_eq!(s.in_flight, 1, "the re-pull is out");

        update(
            &mut s,
            Message::Worker(Arc::new(Event::Error("boom".into()))),
        );
        assert_eq!(s.last_error.as_deref(), Some("boom"));
        assert_eq!(s.in_flight, 0);
    }
}
