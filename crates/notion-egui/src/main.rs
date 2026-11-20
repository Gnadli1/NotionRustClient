//! egui spike: shows one cached page read-only on disk (edits live in memory
//! only). Usage: `notion-egui [PAGE_ID] [DB_PATH]`; PAGE_ID defaults to the
//! cached page with the most blocks, DB_PATH to the notion-cli cache.
//! `NOTION_EGUI_KEYLOG=1` prints raw key/text/IME events to stderr.

mod app;
#[allow(dead_code)]
mod edit;
mod keys;
mod md;
mod menu;
// shared with notion-gui for now; E1 moves them into a crate of their own
#[allow(dead_code)]
#[path = "../../notion-gui/src/outline.rs"]
mod outline;
#[allow(dead_code)]
#[path = "../../notion-gui/src/slash.rs"]
mod slash;
#[allow(dead_code)]
#[path = "../../notion-gui/src/tex.rs"]
mod tex;

use std::path::PathBuf;

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
    let mut args = std::env::args_os().skip(1);
    let page = args.next().and_then(|a| a.into_string().ok());
    let Some(db) = db_path(args.next().map(PathBuf::from)) else {
        eprintln!("HOME not set; pass the cache path as the second argument");
        std::process::exit(2);
    };
    if !db.exists() {
        eprintln!("no cache at {}", db.display());
        std::process::exit(1);
    }
    let store = Store::open(&db).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", db.display());
        std::process::exit(1);
    });
    let keylog = std::env::var_os("NOTION_EGUI_KEYLOG").is_some_and(|v| v == "1");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 800.0])
            .with_title("Notion (egui spike)"),
        ..Default::default()
    };
    eframe::run_native(
        "notion-egui",
        options,
        Box::new(move |cc| {
            // Ctrl+Plus/Minus would zoom; Ctrl+ß / Ctrl+\ are ours
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(app::App::new(store, page, keylog)))
        }),
    )
}
