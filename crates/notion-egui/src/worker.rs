//! Sync worker: a std thread with a current-thread tokio runtime, its own
//! Store connection and Client (the core's sync futures are `!Send`). Events
//! go back over a std channel; each send wakes the UI with `request_repaint`.

use std::path::PathBuf;

use std::sync::mpsc;

use eframe::egui;
use notion_core::api::Client;
use notion_core::model::SearchResult;
use notion_core::store::Store;
use notion_core::sync::{self, PullReport, PushReport};
use tokio::sync::mpsc as tmpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Pull { id: String, force: bool },
    Push,
    Search(String),
    /// Fetches the threads of a page or block (`sync::pull_comments`).
    PullComments { id: String },
    /// Asks for the integration's own user id (to tell its comments apart).
    Me,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Pulled {
        id: String,
        report: PullReport,
    },
    Pushed(PushReport),
    /// (page id, title); data sources are left out.
    SearchResults(Vec<(String, String)>),
    CommentsPulled,
    Me(String),
    Error(String),
}

/// Answers commands until the sender is dropped. Without a client every
/// command gets an Error, so the app still works offline.
pub async fn run(
    store: &Store,
    client: Option<&Client>,
    mut cmds: tmpsc::UnboundedReceiver<Cmd>,
    mut emit: impl FnMut(Event),
) {
    while let Some(cmd) = cmds.recv().await {
        let Some(client) = client else {
            emit(Event::Error("offline: NOTION_TOKEN not set".into()));
            continue;
        };
        let ev = match cmd {
            Cmd::Pull { id, force } => sync::pull_page(client, store, &id, force)
                .await
                .map(|report| Event::Pulled { id, report }),
            Cmd::Push => sync::push(client, store).await.map(Event::Pushed),
            Cmd::PullComments { id } => sync::pull_comments(client, store, &id)
                .await
                .map(|_| Event::CommentsPulled),
            Cmd::Me => client.me().await.map(Event::Me),
            Cmd::Search(q) => client.search(&q).await.map(|rs| {
                Event::SearchResults(
                    rs.into_iter()
                        .filter_map(|r| match r {
                            SearchResult::Page(p) => Some((p.id.clone(), p.title())),
                            SearchResult::DataSource(_) => None,
                        })
                        .collect(),
                )
            }),
        };
        emit(ev.unwrap_or_else(|e| Event::Error(e.to_string())));
    }
}

/// Starts the worker thread on `db`. It stops when the command sender is
/// dropped. Join the handle on exit so an in-flight push can finish. The
/// token is read only here, by `Client::from_env` inside the thread.
pub fn spawn(
    db: PathBuf,
    ctx: egui::Context,
) -> (
    tmpsc::UnboundedSender<Cmd>,
    mpsc::Receiver<Event>,
    std::thread::JoinHandle<()>,
) {
    spawn_with(db, ctx, || Client::from_env().ok())
}

/// [`spawn`] with the client made by `make_client` (inside the thread), so
/// tests can run offline whatever the environment holds.
/// Requests a repaint when the worker thread ends, however it ends.
struct Repaint(egui::Context);

impl Drop for Repaint {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

pub fn spawn_with(
    db: PathBuf,
    ctx: egui::Context,
    make_client: impl FnOnce() -> Option<Client> + Send + 'static,
) -> (
    tmpsc::UnboundedSender<Cmd>,
    mpsc::Receiver<Event>,
    std::thread::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = tmpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        // dropped last, panic or not: wakes the app to see the closed channel
        let guard = Repaint(ctx);
        let ev_tx = ev_tx;
        let emit = |e| {
            let _ = ev_tx.send(e);
            guard.0.request_repaint();
        };
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return emit(Event::Error(format!("worker: {e}"))),
        };
        let client = make_client();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(run(&store, client.as_ref(), cmd_rx, emit));
    });
    (cmd_tx, ev_rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// No client: every command is answered with an Error over the channel.
    #[test]
    fn offline_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx, handle) = spawn_with(
            dir.path().join("cache.db"),
            egui::Context::default(),
            || None,
        );
        tx.send(Cmd::Pull {
            id: "p".into(),
            force: false,
        })
        .unwrap();
        tx.send(Cmd::Push).unwrap();
        for _ in 0..2 {
            let ev = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(matches!(&ev, Event::Error(m) if m.starts_with("offline")), "{ev:?}");
        }
        drop(tx);
        handle.join().unwrap();
        // the worker is gone: the channel says so
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_err());
    }
}
