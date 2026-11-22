//! egui port of the Notion client, shell stage (E1): sidebar tree, tabs,
//! Ctrl+K switcher, sync actions, status bar, session. The page body is the
//! spike's. Usage: `notion-egui [DB_PATH]`; DB_PATH defaults to
//! $XDG_DATA_HOME/notion-client/cache.db, else ~/.local/share/..., the cache
//! notion-cli and notion-gui share. `NOTION_EGUI_KEYLOG=1` prints raw
//! key/text/IME events to stderr.

mod app;
mod comments;
#[allow(dead_code)]
mod edit;
mod editing;
mod grid;
mod inbox;
mod keys;
mod links;
mod md;
mod media;
mod menu;
mod nav;
mod pages;
mod people;
mod problems;
mod resolve;
mod shell;
mod switcher;
mod theme;
mod undo;
mod view;
mod views;
mod worker;
// shared with notion-gui for now; a crate of their own later
#[allow(dead_code)]
#[path = "../../notion-gui/src/outline.rs"]
mod outline;
#[allow(dead_code)]
#[path = "../../notion-gui/src/session.rs"]
mod session;
#[allow(dead_code)]
#[path = "../../notion-gui/src/sidebar.rs"]
mod sidebar;
#[allow(dead_code)]
#[path = "../../notion-gui/src/slash.rs"]
mod slash;
#[allow(dead_code)]
#[path = "../../notion-gui/src/tabs.rs"]
mod tabs;
#[allow(dead_code)]
#[path = "../../notion-gui/src/tex.rs"]
mod tex;

use std::path::{Path, PathBuf};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
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

/// Noto Sans first (Latin, Greek, umlauts; a bold face for `md::bold_family`),
/// then Noto Sans Math for the TeX output (∑ ∫ ≤ ...). Every file is optional.
/// Colour emoji and any other missing glyph come from eframe's system-font
/// provider (feature `system_fonts`, on by default), which loads Noto Color
/// Emoji through fontconfig; epaint's `color_fonts` is enabled by that crate.
fn setup_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let add = |defs: &mut FontDefinitions, name: &str, data: Option<FontData>, front: bool| {
        let Some(data) = data else { return };
        defs.font_data.insert(name.into(), data.into());
        for fam in [FontFamily::Proportional, FontFamily::Monospace] {
            let list = defs.families.entry(fam.clone()).or_default();
            if front && fam == FontFamily::Proportional {
                list.insert(0, name.into());
            } else if !front {
                list.push(name.into());
            }
        }
    };
    let dirs = [
        "/usr/share/fonts/noto",
        "/usr/share/fonts/TTF",
        "/usr/share/fonts/truetype/noto",
    ];
    let find = |file: &str| {
        dirs.iter()
            .find_map(|d| std::fs::read(format!("{d}/{file}")).ok())
            .map(FontData::from_owned)
    };
    add(&mut defs, "noto-sans", find("NotoSans-Regular.ttf"), true);
    // math only: more symbol fonts would also catch emoji codepoints in
    // monochrome before the system provider's Noto Color Emoji gets a chance
    add(
        &mut defs,
        "noto-math",
        find("NotoSansMath-Regular.ttf"),
        false,
    );
    // bold: its own face if there is one, else the normal text
    let mut bold = defs.families[&FontFamily::Proportional].clone();
    if let Some(b) = find("NotoSans-Bold.ttf") {
        defs.font_data.insert("noto-sans-bold".into(), b.into());
        bold.insert(0, "noto-sans-bold".into());
    }
    defs.families.insert(md::bold_family(), bold);
    ctx.set_fonts(defs);
}

fn main() -> eframe::Result {
    let Some(db) = db_path(std::env::args_os().nth(1).map(PathBuf::from)) else {
        eprintln!("HOME not set; pass the cache path as the first argument");
        std::process::exit(2);
    };
    let store = open_store(&db).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", db.display());
        std::process::exit(1);
    });
    let keylog = std::env::var_os("NOTION_EGUI_KEYLOG").is_some_and(|v| v == "1");
    let dir = config_dir();
    let (sess, problem) = dir
        .as_deref()
        .map(|d| session::load_from(d, shell::SESSION_FILE))
        .unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([sess.window.0, sess.window.1])
            .with_title(env!("CARGO_PKG_NAME")),
        ..Default::default()
    };
    eframe::run_native(
        "notion-egui",
        options,
        Box::new(move |cc| {
            // Ctrl+Plus/Minus would zoom; Ctrl+ß / Ctrl+\ are ours
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            setup_fonts(&cc.egui_ctx);
            theme::install(&cc.egui_ctx);
            let (cmds, events, worker) = worker::spawn(db, cc.egui_ctx.clone());
            let mut app = app::App::new(store, cmds, events, keylog);
            app.set_worker(worker);
            if let Some(dir) = dir {
                let (keys, notes) = keys::load(&dir);
                app.keys = keys;
                app.last_error = (!notes.is_empty()).then(|| notes.join("; "));
                app.restore(dir, sess, problem);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod testkit;
