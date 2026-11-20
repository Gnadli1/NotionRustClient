//! Sync worker: a std thread with a current-thread tokio runtime, its own
//! Store connection and Client (the core's sync futures are `!Send`).

use std::path::PathBuf;

use iced::futures::channel::mpsc as fmpsc;
use notion_core::api::Client;
use notion_core::model::SearchResult;
use notion_core::store::Store;
use notion_core::sync::{self, PullReport, PushReport};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Pull { id: String, force: bool },
    Push,
    Search(String),
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
    Error(String),
}

/// Answers commands until the sender is dropped. Without a client every
/// command gets an Error, so the app still works offline.
pub async fn run(
    store: &Store,
    client: Option<&Client>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
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

/// Starts the worker thread on `db`. It stops when the command sender is dropped.
/// Join the handle on exit so an in-flight push can finish.
pub fn spawn(
    db: PathBuf,
) -> (
    mpsc::UnboundedSender<Cmd>,
    fmpsc::UnboundedReceiver<Event>,
    std::thread::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = fmpsc::unbounded();
    let handle = std::thread::spawn(move || {
        let emit = |e| {
            let _ = ev_tx.unbounded_send(e);
        };
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return emit(Event::Error(format!("worker: {e}"))),
        };
        let client = Client::from_env().ok();
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
    use notion_core::api::ClientConfig;
    use serde_json::{json, Value};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const T: &str = "2025-01-01T00:00:00.000Z";

    fn client(s: &MockServer) -> Client {
        Client::new(
            "t".into(),
            ClientConfig {
                base_url: s.uri(),
                requests_per_sec: 1000.0,
                burst: 1000,
                max_retries: 0,
                max_backoff: Duration::from_secs(1),
            },
        )
    }

    fn page(id: &str, title: &str) -> Value {
        json!({
            "object": "page", "id": id, "parent": { "type": "workspace", "workspace": true },
            "created_time": T, "last_edited_time": T, "in_trash": false, "url": "https://notion.so/x",
            "properties": { "Name": { "id": "title", "type": "title", "title": [
                { "type": "text", "text": { "content": title }, "plain_text": title, "annotations": {} }] } },
        })
    }

    fn list(results: Vec<Value>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(
            json!({ "object": "list", "results": results, "has_more": false, "next_cursor": null }),
        )
    }

    async fn drive(store: &Store, client: Option<&Client>, cmds: Vec<Cmd>) -> Vec<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        for c in cmds {
            tx.send(c).unwrap();
        }
        drop(tx);
        let mut out = Vec::new();
        run(store, client, rx, |e| out.push(e)).await;
        out
    }

    #[tokio::test]
    async fn pull_writes_store_and_search_returns_pages() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/pages/{PID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(PID, "Home")))
            .mount(&s)
            .await;
        let para = json!({
            "object": "block", "id": "b1", "parent": { "type": "page_id", "page_id": PID },
            "type": "paragraph", "paragraph": { "rich_text": [{ "type": "text", "text": { "content": "hi" },
                "plain_text": "hi", "annotations": {} }], "color": "default" },
            "has_children": false, "in_trash": false, "created_time": T, "last_edited_time": T,
        });
        Mock::given(method("GET"))
            .and(path(format!("/v1/blocks/{PID}/children")))
            .respond_with(list(vec![para]))
            .mount(&s)
            .await;
        let ds = json!({ "object": "data_source", "id": "ds1", "title": [],
            "parent": { "type": "database_id", "database_id": "db1" },
            "created_time": T, "last_edited_time": T, "properties": {} });
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(list(vec![page(PID, "Home"), ds]))
            .mount(&s)
            .await;

        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        let evs = drive(
            &store,
            Some(&c),
            vec![
                Cmd::Pull {
                    id: PID.into(),
                    force: false,
                },
                Cmd::Search("ho".into()),
            ],
        )
        .await;
        assert_eq!(
            evs,
            vec![
                Event::Pulled {
                    id: PID.into(),
                    report: PullReport {
                        fetched: 1,
                        updated: 1,
                        skipped: false
                    }
                },
                Event::SearchResults(vec![(PID.into(), "Home".into())]),
            ]
        );
        let tree = store.page_tree().unwrap();
        assert_eq!(
            tree.iter()
                .map(|e| (e.id.as_str(), e.title.as_str()))
                .collect::<Vec<_>>(),
            [(PID, "Home")]
        );
        assert_eq!(store.children(PID).unwrap()[0].id, "b1");
    }

    #[tokio::test]
    async fn client_errors_and_offline_emit_error() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                json!({ "object": "error", "status": 401, "code": "unauthorized", "message": "bad token" }),
            ))
            .mount(&s)
            .await;
        let store = Store::open_in_memory().unwrap();
        let c = client(&s);
        let evs = drive(
            &store,
            Some(&c),
            vec![Cmd::Pull {
                id: PID.into(),
                force: true,
            }],
        )
        .await;
        assert!(
            matches!(&evs[..], [Event::Error(m)] if m.contains("unauthorized")),
            "{evs:?}"
        );

        let evs = drive(&store, None, vec![Cmd::Push, Cmd::Search("x".into())]).await;
        assert_eq!(evs.len(), 2);
        assert!(evs.iter().all(|e| matches!(e, Event::Error(_))));
    }
}
