//! Sync worker: a std thread with a current-thread tokio runtime, its own
//! Store connection and Client (the core's sync futures are `!Send`). Events
//! go back over a std channel; each send wakes the UI with `request_repaint`.

use std::path::PathBuf;

use std::sync::mpsc;

use eframe::egui;
use notion_core::api::Client;
use notion_core::model::SearchResult;
use notion_core::store::Store;
use notion_core::views::SavedView;
use notion_core::sync::{self, PullReport, PushReport, Skip, SyncAllReport};
use notion_core::Error;
use tokio::sync::mpsc as tmpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Pull { id: String, force: bool },
    /// A background check of a page: like a pull, but may skip (see `Skip`).
    Poll { id: String, skip: Skip },
    Push,
    Search(String),
    /// Fetches the threads of a page or block (`sync::pull_comments`).
    PullComments { id: String },
    /// Asks for the integration's own user id (to tell its comments apart).
    Me,
    /// Fetches the workspace's users (`sync::pull_users`) into the cache.
    PullUsers,
    /// Gets Notion's version of a conflict op's object, for the conflict panel.
    Fetch { op: i64 },
    /// Keep mine, or Merge with Markdown `merged`: re-queues the conflict op
    /// against Notion's version (`sync::resolve_mine`), if its text is still
    /// `seen`, the Notion text the panel showed.
    Resolve { op: i64, merged: Option<String>, seen: Option<String> },
    /// Pulls every shared page and data source (`sync::pull_all`); answered by
    /// one `SyncedAll` or `Error`, after any number of `Progress`.
    SyncAll,
    /// The saved views of a data source (`Client::list_views`).
    Views { ds: String },
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
    UsersPulled,
    /// `GET /v1/users` said 403: the integration lacks "read user information".
    UsersDenied,
    Fetched { op: i64, what: sync::Fetched },
    Resolved(i64),
    Views { ds: String, views: Vec<SavedView> },
    SyncedAll(SyncAllReport),
    /// (done, total) of a running `SyncAll`. Not an answer: the app must not
    /// pop `sent` for it.
    Progress { done: usize, total: usize },
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
    // who "you" are, for the Updates inbox: `users/me` the first time a
    // command that records activity runs, retried at most every 5 minutes
    // until it works; NOTION_USER_ID (a user id, not a secret) names the person
    let env_user = std::env::var("NOTION_USER_ID").ok().filter(|u| !u.is_empty());
    // refetched once per session so a removed or changed NOTION_USER_ID takes effect
    let mut identity_done = false;
    let mut identity_try: Option<std::time::Instant> = None;
    while let Some(cmd) = cmds.recv().await {
        let Some(client) = client else {
            emit(Event::Error("offline: NOTION_TOKEN not set".into()));
            continue;
        };
        if !identity_done
            && matches!(cmd, Cmd::Pull { .. } | Cmd::Poll { .. } | Cmd::PullComments { .. } | Cmd::SyncAll)
            && identity_try.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(300))
        {
            identity_try = Some(std::time::Instant::now());
            if let Ok((bot, owner)) = client.me_identity().await {
                identity_done = store.set_identity(&bot, owner.as_deref(), env_user.as_deref()).is_ok();
            }
        }
        let ev = match cmd {
            Cmd::Pull { id, force } => sync::pull_any(client, store, &id, force)
                .await
                .map(|report| Event::Pulled { id, report }),
            Cmd::Poll { id, skip } => sync::poll_any(client, store, &id, skip)
                .await
                .map(|report| Event::Pulled { id, report }),
            Cmd::Push => sync::push(client, store).await.map(Event::Pushed),
            Cmd::PullComments { id } => sync::pull_comments(client, store, &id)
                .await
                .map(|_| Event::CommentsPulled),
            Cmd::Me => client.me_identity().await.map(|(bot, owner)| {
                identity_done |= store.set_identity(&bot, owner.as_deref(), env_user.as_deref()).is_ok();
                Event::Me(bot)
            }),
            Cmd::PullUsers => match sync::pull_users(client, store).await {
                Ok(_) => Ok(Event::UsersPulled),
                Err(Error::Api { status: 403, .. }) => Ok(Event::UsersDenied),
                Err(e) => Err(e),
            },
            Cmd::Fetch { op } => sync::fetch_conflict(client, store, op)
                .await
                .map(|what| Event::Fetched { op, what }),
            Cmd::Resolve { op, merged, seen } => {
                sync::resolve_mine(client, store, op, merged.as_deref(), seen.as_deref())
                    .await
                    .map(|_| Event::Resolved(op))
            }
            Cmd::SyncAll => {
                sync::pull_all(client, store, |done, total, _| emit(Event::Progress { done, total }))
                    .await
                    .map(Event::SyncedAll)
            }
            Cmd::Views { ds } => client.list_views(&ds).await.map(|vs| Event::Views {
                views: vs.iter().filter_map(SavedView::from_value).collect(),
                ds,
            }),
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
    /// Each Views gets exactly one event: Error offline and on a 404, Views on success.
    #[test]
    fn views_is_answered_by_exactly_one_event() {
        use notion_core::api::ClientConfig;
        let ok = r#"{"object":"list","results":[{"object":"view","id":"v1","name":"All","type":"table"}],"has_more":false,"next_cursor":null}"#;
        let nf = r#"{"object":"error","status":404,"code":"x","message":"m"}"#;
        for (status, body) in [(0, ""), (404, nf), (200, ok)] {
            let dir = tempfile::tempdir().unwrap();
            let url = if status == 0 { String::new() } else { serve(status, body) };
            let (tx, rx, handle) = spawn_with(dir.path().join("cache.db"), egui::Context::default(), move || {
                (status != 0).then(|| Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
            });
            tx.send(Cmd::Views { ds: "ds".into() }).unwrap();
            let ev = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match status {
                0 => assert!(matches!(&ev, Event::Error(m) if m.starts_with("offline")), "{ev:?}"),
                404 => assert!(matches!(&ev, Event::Error(m) if m.contains("404")), "{ev:?}"),
                _ => assert!(matches!(&ev, Event::Views { ds, .. } if ds == "ds"), "{ev:?}"),
            }
            drop(tx);
            handle.join().unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no stray event");
        }
    }

    /// A one-thread HTTP server that answers every request with `status` and `body`.
    fn serve(status: u16, body: &'static str) -> String {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                let _ = write!(
                    c,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    /// Each PullUsers gets exactly one event: UsersPulled (cached), UsersDenied
    /// for a 403, Error otherwise.
    #[test]
    fn pull_users_is_answered_by_exactly_one_event() {
        use notion_core::api::ClientConfig;
        let list = r#"{"object":"list","results":[{"object":"user","id":"u1","type":"person","name":"Ada"}],"has_more":false,"next_cursor":null}"#;
        let err = |s: &'static str| format!(r#"{{"object":"error","status":{s},"code":"x","message":"m"}}"#);
        let err: &'static str = Box::leak(err("403").into_boxed_str());
        for (status, body) in [(200, list), (403, err), (500, err)] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("cache.db");
            let url = serve(status, body);
            let (tx, rx, handle) = spawn_with(db.clone(), egui::Context::default(), move || {
                Some(Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
            });
            tx.send(Cmd::PullUsers).unwrap();
            tx.send(Cmd::Me).unwrap(); // the next event is Me's: PullUsers made just one
            let ev = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match status {
                200 => {
                    assert_eq!(ev, Event::UsersPulled);
                    assert_eq!(Store::open(&db).unwrap().user("u1").unwrap().unwrap().name, "Ada");
                }
                403 => assert_eq!(ev, Event::UsersDenied),
                _ => assert!(matches!(&ev, Event::Error(m) if m.contains("500")), "{ev:?}"),
            }
            assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Event::Me(_) | Event::Error(_)));
            drop(tx);
            handle.join().unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no stray event");
        }
        // no client: the offline error, as for every command
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx, handle) = spawn_with(dir.path().join("cache.db"), egui::Context::default(), || None);
        tx.send(Cmd::PullUsers).unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Event::Error(m) if m.starts_with("offline")));
        drop(tx);
        handle.join().unwrap();
    }

    /// SyncAll is answered by exactly one event besides its Progress ones, whatever Notion says.
    #[test]
    fn sync_all_is_answered_by_exactly_one_event_besides_progress() {
        use notion_core::api::ClientConfig;
        let page = r#"{"object":"page","id":"p","parent":{"type":"workspace","workspace":true},"created_time":"2025-01-01T00:00:00.000Z","last_edited_time":"2025-01-01T00:00:00.000Z","in_trash":false,"url":"u","properties":{}}"#;
        let list = |r: &str| -> &'static str {
            Box::leak(format!(r#"{{"object":"list","results":[{r}],"has_more":false,"next_cursor":null}}"#).into_boxed_str())
        };
        let err = |s: &str| -> &'static str {
            Box::leak(format!(r#"{{"object":"error","status":{s},"code":"x","message":"m"}}"#).into_boxed_str())
        };
        // (status, body, Progress events expected)
        for (status, body, progress) in [(200, list(""), 0), (200, list(page), 1), (401, err("401"), 0), (500, err("500"), 0)] {
            let dir = tempfile::tempdir().unwrap();
            let url = serve(status, body);
            let (tx, rx, handle) = spawn_with(dir.path().join("cache.db"), egui::Context::default(), move || {
                Some(Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
            });
            tx.send(Cmd::SyncAll).unwrap();
            tx.send(Cmd::Me).unwrap(); // its answer is the next non-progress event
            drop(tx);
            handle.join().unwrap();
            let evs: Vec<Event> = rx.try_iter().collect();
            let (prog, answers): (Vec<_>, Vec<_>) = evs.iter().partition(|e| matches!(e, Event::Progress { .. }));
            assert_eq!(prog.len(), progress, "{evs:?}");
            assert_eq!(answers.len(), 2, "{evs:?}");
            match (status, answers[0]) {
                (200, Event::SyncedAll(r)) => assert_eq!(r.errors.len(), progress),
                (401 | 500, Event::Error(m)) => assert!(m.contains(&status.to_string()), "{m}"),
                (_, e) => panic!("{e:?}"),
            }
        }
        // no client: the offline error, and nothing else
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx, handle) = spawn_with(dir.path().join("cache.db"), egui::Context::default(), || None);
        tx.send(Cmd::SyncAll).unwrap();
        drop(tx);
        handle.join().unwrap();
        let evs: Vec<Event> = rx.try_iter().collect();
        assert!(matches!(&evs[..], [Event::Error(m)] if m.starts_with("offline")), "{evs:?}");
    }

    /// Fetch and Resolve are each answered by exactly one event, whatever
    /// Notion says: the app's `sent` queue pairs commands with events.
    #[test]
    fn fetch_and_resolve_are_answered_by_exactly_one_event() {
        use notion_core::api::ClientConfig;
        use notion_core::ops::{OpKind, OpStatus};
        let block = r#"{"object":"block","id":"b","parent":{"type":"page_id","page_id":"p"},"type":"paragraph","paragraph":{"rich_text":[{"type":"text","text":{"content":"theirs"},"plain_text":"theirs","annotations":{}}]},"has_children":false,"in_trash":false,"created_time":"2025-01-01T00:00:00.000Z","last_edited_time":"2025-01-02T00:00:00.000Z"}"#;
        let err = r#"{"object":"error","status":500,"code":"x","message":"m"}"#;
        let gone = r#"{"object":"error","status":404,"code":"object_not_found","message":"m"}"#;
        for (status, body) in [(200, block), (404, gone), (500, err)] {
            for resolve in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let db = dir.path().join("cache.db");
                let store = Store::open(&db).unwrap();
                let rt = serde_json::json!([{"type":"text","text":{"content":"mine"},"plain_text":"mine","annotations":{}}]);
                let op = store
                    .enqueue(OpKind::UpdateBlock { block_id: "b".into(), body: serde_json::json!({ "paragraph": { "rich_text": rt } }) }, None, None)
                    .unwrap();
                store.mark_op(op, OpStatus::Conflict, Some("remote edited")).unwrap();
                let url = serve(status, body);
                let (tx, rx, handle) = spawn_with(db.clone(), egui::Context::default(), move || {
                    Some(Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
                });
                tx.send(if resolve { Cmd::Resolve { op, merged: None, seen: None } } else { Cmd::Fetch { op } }).unwrap();
                tx.send(Cmd::Me).unwrap(); // the next event is Me's: the first made just one
                let ev = rx.recv_timeout(Duration::from_secs(10)).unwrap();
                match (resolve, status) {
                    (false, 200) => assert!(matches!(&ev, Event::Fetched { what: sync::Fetched::Text { theirs, .. }, .. } if theirs == "theirs"), "{ev:?}"),
                    (false, 404) => assert_eq!(ev, Event::Fetched { op, what: sync::Fetched::Gone }),
                    (true, 200) => {
                        assert_eq!(ev, Event::Resolved(op));
                        assert_eq!(store.queued_ops().unwrap().len(), 1, "re-queued");
                    }
                    (true, 404) => assert!(matches!(&ev, Event::Error(m) if m.contains("gone")), "{ev:?}"),
                    _ => assert!(matches!(&ev, Event::Error(m) if m.contains("500")), "{ev:?}"),
                }
                assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Event::Me(_) | Event::Error(_)));
                drop(tx);
                handle.join().unwrap();
                assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no stray event");
            }
        }
        // a conflict that is gone from the queue is an error too, not silence
        let dir = tempfile::tempdir().unwrap();
        let url = serve(200, block);
        let (tx, rx, handle) = spawn_with(dir.path().join("cache.db"), egui::Context::default(), move || {
            Some(Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
        });
        tx.send(Cmd::Fetch { op: 99 }).unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Event::Error(m) if m.contains("not a conflict")));
        drop(tx);
        handle.join().unwrap();
    }

    /// The users/me identity fetch happens at most once per session (even when
    /// it fails), is saved, and adds no Event: one answer per command.
    #[test]
    fn identity_is_fetched_once_and_emits_no_extra_event() {
        use notion_core::api::ClientConfig;
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::sync::Arc;
        for me_ok in [true, false] {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let hits = Arc::new(AtomicUsize::new(0));
            let h = hits.clone();
            std::thread::spawn(move || {
                for mut c in l.incoming().flatten() {
                    let mut buf = [0u8; 4096];
                    let n = c.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (status, body) = if req.contains("/v1/users/me") {
                        h.fetch_add(1, SeqCst);
                        if me_ok {
                            (200, r#"{"id":"bot1","bot":{"owner":{"type":"user","user":{"id":"owner1"}}}}"#)
                        } else {
                            (500, r#"{"object":"error","status":500,"code":"x","message":"m"}"#)
                        }
                    } else {
                        (500, r#"{"object":"error","status":500,"code":"x","message":"m"}"#)
                    };
                    let _ = write!(
                        c,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("cache.db");
            let (tx, rx, handle) = spawn_with(db.clone(), egui::Context::default(), move || {
                Some(Client::new("t".into(), ClientConfig { base_url: url, max_retries: 0, ..Default::default() }))
            });
            tx.send(Cmd::Pull { id: "p".into(), force: false }).unwrap();
            tx.send(Cmd::PullComments { id: "p".into() }).unwrap();
            tx.send(Cmd::Poll { id: "p".into(), skip: sync::Skip::Never }).unwrap();
            tx.send(Cmd::SyncAll).unwrap();
            drop(tx);
            handle.join().unwrap();
            let evs: Vec<Event> = rx.try_iter().filter(|e| !matches!(e, Event::Progress { .. })).collect();
            assert_eq!(evs.len(), 4, "exactly one event per command: {evs:?}");
            assert!(!evs.iter().any(|e| matches!(e, Event::Me(_))), "{evs:?}");
            assert_eq!(hits.load(SeqCst), 1, "users/me once (me_ok={me_ok})");
            let id = Store::open(&db).unwrap().identity().unwrap();
            assert_eq!(id.is_some(), me_ok);
        }
    }
}
