//! egui port of the Notion client: sidebar tree, tabs, Ctrl+K switcher, sync
//! actions, status bar, workspace profiles, session. Usage:
//! `notion-egui [DB_PATH]`; without a path it uses the active workspace
//! profile in `profiles.json` (config folder), else `NOTION_TOKEN` on the
//! shared `cache.db` in the data folder. `NOTION_EGUI_KEYLOG=1` prints raw
//! key/text/IME events to stderr.

mod ai;
mod app;
mod browser_login;
mod comments;
#[allow(dead_code)]
mod edit;
mod editing;
mod grid;
mod icons;
mod inbox;
mod keys;
mod links;
mod md;
mod media;
mod menu;
mod nav;
#[allow(dead_code)]
mod outline;
mod pages;
mod people;
mod problems;
mod profiles;
mod resolve;
#[allow(dead_code)]
mod session;
mod settings;
mod shell;
#[allow(dead_code)]
mod sidebar;
#[allow(dead_code)]
mod slash;
mod switcher;
#[allow(dead_code)]
mod tabs;
#[allow(dead_code)]
mod tex;
mod theme;
mod undo;
mod update;
mod view;
mod views;
mod worker;

use std::path::{Path, PathBuf};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use notion_core::paths;
use notion_core::store::Store;

/// Creates `dir` and makes it yours alone (0700): tokens, keys, cached pages,
/// images and sessions all live under it, so no file inside is readable by
/// other users whatever its own mode.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

fn open_store(db: &Path) -> Result<Store, String> {
    if let Some(dir) = db.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Store::open(db).map_err(|e| e.to_string())
}

/// Inter is embedded so the type matches on every platform (Notion itself
/// uses the platform UI sans, which Inter is modelled on). Regular, semibold
/// and bold map to body, emphasis/headings and the page title. Noto Sans Math
/// is appended when present for the TeX output (∑ ∫ ≤ ...); every other
/// missing glyph — including colour emoji — falls back to eframe's system-font
/// provider, exactly as before.
fn setup_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    for (name, data) in [
        (
            "inter",
            FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf")),
        ),
        (
            "inter-semibold",
            FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
        ),
        (
            "inter-bold",
            FontData::from_static(include_bytes!("../assets/fonts/Inter-Bold.ttf")),
        ),
    ] {
        defs.font_data.insert(name.into(), data.into());
    }
    let fallback = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let dirs = [
        "/usr/share/fonts/noto",
        "/usr/share/fonts/TTF",
        "/usr/share/fonts/truetype/noto",
        r"C:\Windows\Fonts",
    ];
    let mut regular = vec!["inter".to_owned()];
    if let Some(math) = dirs.iter().find_map(|d| {
        std::fs::read(format!("{d}/NotoSansMath-Regular.ttf"))
            .ok()
            .map(FontData::from_owned)
    }) {
        defs.font_data.insert("noto-math".into(), math.into());
        regular.push("noto-math".into());
    }
    // Segoe UI Symbol has the arrows and letterlike symbols TeX output needs
    // (⟹, ℤ, ...) but Inter lacks.
    if let Some(sym) = std::fs::read(r"C:\Windows\Fonts\seguisym.ttf")
        .ok()
        .map(FontData::from_owned)
    {
        defs.font_data.insert("segoe-symbol".into(), sym.into());
        regular.push("segoe-symbol".into());
    }
    regular.extend(fallback.iter().cloned());
    defs.families
        .insert(FontFamily::Proportional, regular.clone());
    let family = |head: &str| {
        let mut list = vec![head.to_owned()];
        list.extend(regular.iter().cloned());
        list
    };
    defs.families
        .insert(md::bold_family(), family("inter-semibold"));
    defs.families
        .insert(md::title_family(), family("inter-bold"));
    // Equations read as math: a serif face when the system has one, else Inter.
    let mut math_head = "inter";
    if let Some(serif) = [
        r"C:\Windows\Fonts\times.ttf",
        r"C:\Windows\Fonts\georgia.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
        "/usr/share/fonts/dejavu/DejaVuSerif.ttf",
    ]
    .iter()
    .find_map(|p| std::fs::read(p).ok().map(FontData::from_owned))
    {
        defs.font_data.insert("math-serif".into(), serif.into());
        math_head = "math-serif";
    }
    defs.families.insert(md::math_family(), family(math_head));
    ctx.set_fonts(defs);
}

fn main() -> eframe::Result {
    let data = paths::data_dir();
    let dir = paths::config_dir();
    for d in data.iter().chain(dir.iter()) {
        if let Err(e) = private_dir(d) {
            eprintln!("can't make {} private: {e}", d.display());
        }
    }
    let (profiles, profiles_problem) = dir
        .as_deref()
        .map(profiles::Profiles::load)
        .unwrap_or_default();
    let active = profiles.active_profile().cloned();
    let db_arg = std::env::args_os().nth(1).map(PathBuf::from);
    let Some(db) = db_arg
        .clone()
        .or_else(|| active.as_ref().and_then(|p| p.cache_path(data.as_deref())))
        .or_else(paths::default_db)
    else {
        eprintln!("cannot find a cache folder; set LOCALAPPDATA/APPDATA or pass DB_PATH");
        std::process::exit(2);
    };
    let store = open_store(&db).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", db.display());
        std::process::exit(1);
    });
    let keylog = std::env::var_os("NOTION_EGUI_KEYLOG").is_some_and(|v| v == "1");
    let token = active
        .as_ref()
        .map(|p| p.token.trim().to_string())
        .filter(|t| !t.is_empty());
    let browser_auth = active
        .as_ref()
        .filter(|p| p.kind == profiles::AuthKind::Browser)
        .and_then(|p| {
            profiles.browser.as_ref().and_then(|b| {
                p.space_id.as_ref().map(|space| {
                    (
                        b.token_v2.clone(),
                        space.clone(),
                        b.user_id.clone(),
                        b.cookies.clone(),
                    )
                })
            })
        });
    let user_id = active
        .as_ref()
        .and_then(|p| p.user_id.clone())
        .or_else(|| browser_auth.as_ref().and_then(|(_, _, uid, _)| uid.clone()))
        .or_else(|| {
            std::env::var("NOTION_USER_ID")
                .ok()
                .filter(|u| !u.trim().is_empty())
        });
    let session_file = active
        .as_ref()
        .map(profiles::Profile::session_file)
        .unwrap_or_else(|| shell::SESSION_FILE.to_string());
    let (sess, session_problem) = dir
        .as_deref()
        .map(|d| session::load_from(d, &session_file))
        .unwrap_or_default();
    let problem = match (profiles_problem, session_problem) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
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
            let (cmds, events, worker) = match browser_auth {
                Some((token_v2, space_id, _, cookies)) if !token_v2.trim().is_empty() => {
                    worker::spawn_browser(
                        db,
                        cc.egui_ctx.clone(),
                        token_v2,
                        space_id,
                        user_id,
                        cookies,
                    )
                }
                _ => worker::spawn_profile(db, cc.egui_ctx.clone(), token, user_id),
            };
            let mut app = app::App::new(store, cmds, events, keylog);
            app.set_worker(worker);
            app.set_profiles(profiles, dir.clone(), data.clone(), session_file.clone());
            app.start_space_refresh();
            app.first_run();
            if let Some(dir) = dir {
                let (keys, mut notes) = keys::load(&dir);
                let (mut settings, settings_problem) = settings::load(&dir);
                notes.extend(settings_problem);
                if let Some(p) = settings::apply(&cc.egui_ctx, &settings) {
                    settings.font.clear();
                    notes.push(p);
                }
                app.settings = settings;
                app.ai_secrets = ai::load_secrets(&dir);
                app.keys = keys;
                if !notes.is_empty() {
                    let note = notes.join("; ");
                    app.last_error = Some(match problem {
                        Some(p) => format!("{note}; {p}"),
                        None => note,
                    });
                } else {
                    app.last_error = problem;
                }
                app.restore(dir, sess, None);
            }
            if app.tree.is_empty()
                && app
                    .profiles
                    .active_profile()
                    .is_some_and(|p| p.kind == profiles::AuthKind::Browser)
            {
                app.sync_all();
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod testkit;

#[cfg(test)]
mod private_dir_tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_dir_creates_and_tightens_to_0700() {
        let d = std::env::temp_dir().join(format!("nc-private-{}", std::process::id()));
        let inner = d.join("a/b");
        super::private_dir(&inner).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
        super::private_dir(&inner).unwrap(); // an existing open dir is tightened too
        let mode = std::fs::metadata(&inner).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(mode, 0o700);
    }
}
