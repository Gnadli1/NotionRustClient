//! Keyboard-driven Notion client (iced). Usage: `notion-gui [DB_PATH]`;
//! DB_PATH defaults to $XDG_DATA_HOME/notion-client/cache.db, else
//! ~/.local/share/notion-client/cache.db (same cache as notion-cli).

mod app;
mod edit;
mod keys;
mod outline;
mod slash;
mod tex;
mod view;
mod worker;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;
use notion_core::store::Store;

fn db_path(arg: Option<PathBuf>) -> Option<PathBuf> {
    if arg.is_some() {
        return arg;
    }
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("notion-client/cache.db"))
}

/// $XDG_CONFIG_HOME/notion-client, else ~/.config/notion-client.
fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("notion-client"))
}

fn open_store(db: &Path) -> Result<Store, String> {
    if let Some(dir) = db.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Store::open(db).map_err(|e| e.to_string())
}

fn main() -> iced::Result {
    let Some(db) = db_path(std::env::args_os().nth(1).map(PathBuf::from)) else {
        eprintln!("HOME not set; pass the cache path as the first argument");
        std::process::exit(2);
    };
    let store = match open_store(&db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open {}: {e}", db.display());
            std::process::exit(1);
        }
    };
    let (cmds, events, worker) = worker::spawn(db);
    let init = Cell::new(Some((store, cmds, events)));
    let boot = move || {
        let (store, cmds, events) = init.take().expect("boot runs once");
        let mut state = app::State::new(store, cmds);
        if let Some(dir) = config_dir() {
            let (keys, problems) = keys::load(&dir);
            state.set_keys(keys, problems);
        }
        (
            state,
            Task::batch([
                iced::system::theme().map(app::Message::ThemeChanged),
                Task::run(events, |e| app::Message::Worker(Arc::new(e)))
                    .chain(Task::done(app::Message::WorkerGone)),
            ]),
        )
    };
    let r = iced::application(boot, app::update, view::view)
        .title(app::State::window_title)
        .subscription(app::subscription)
        .theme(app::theme)
        .run();
    // State (and the command sender) is gone now, so this only waits for the
    // command in flight: a push killed mid-way would re-send applied ops.
    let _ = worker.join();
    r
}
