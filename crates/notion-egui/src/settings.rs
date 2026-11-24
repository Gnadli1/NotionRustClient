//! App settings (`settings.json` in the config dir): look, scrolling, the AI
//! agent. Key binds stay in `keys.json`, API keys in `ai-keys.json` (ai.rs).

use std::path::{Path, PathBuf};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::ai::AiConfig;

pub const FILE: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "System", or a palette name from `theme::THEMES`.
    pub theme: String,
    /// egui zoom factor, 0.75..=2.0.
    pub zoom: f32,
    /// Multiplier on wheel/touchpad scroll deltas, 0.25..=4.0.
    pub scroll_speed: f32,
    /// "" = the built-in Inter, else a .ttf/.otf path.
    pub font: String,
    /// Pages use the whole window width instead of Notion's 708 pt column.
    pub full_width: bool,
    pub ai: AiConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "System".into(),
            zoom: 1.0,
            scroll_speed: 1.0,
            font: String::new(),
            full_width: false,
            ai: AiConfig::default(),
        }
    }
}

/// `dir/settings.json`; missing → defaults, no problem. Unreadable → defaults
/// and a problem for the status bar.
pub fn load(dir: &Path) -> (Settings, Option<String>) {
    let text = match std::fs::read_to_string(dir.join(FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(e) => {
            return (
                Settings::default(),
                Some(format!("{FILE}: {e}; using defaults")),
            )
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut s) => {
            s.zoom = s.zoom.clamp(0.75, 2.0);
            s.scroll_speed = s.scroll_speed.clamp(0.25, 4.0);
            (s, None)
        }
        Err(e) => (
            Settings::default(),
            Some(format!("{FILE}: {e}; using defaults")),
        ),
    }
}

/// Writes `dir/settings.json` (tmp + rename).
pub fn save(dir: &Path, s: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // a hand-edited file `load` rejected: keep it as settings.json.bad
    if let Ok(old) = std::fs::read(dir.join(FILE)) {
        if serde_json::from_slice::<Settings>(&old).is_err() {
            std::fs::copy(dir.join(FILE), dir.join(format!("{FILE}.bad")))?;
        }
    }
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(tmp, dir.join(FILE))
}

/// The bytes of the font file at `path` if egui can render with it. A file
/// egui can't parse panics in epaint, so it is tried on a scratch context.
fn load_font(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let probe = bytes.clone();
    std::panic::catch_unwind(move || {
        let ctx = egui::Context::default();
        let mut defs = egui::FontDefinitions::default();
        defs.font_data
            .insert("user-font".into(), egui::FontData::from_owned(probe).into());
        defs.families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "user-font".into());
        ctx.set_fonts(defs);
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.label("Aa");
        })
        .drop_without_applying_deltas();
    })
    .ok()?;
    Some(bytes)
}

/// Theme, zoom and font onto `ctx`: call at start and when one of them
/// changed. Returns a problem when the font can't be used (it is then not
/// installed; the caller should reset `s.font`).
pub fn apply(ctx: &egui::Context, s: &Settings) -> Option<String> {
    crate::theme::select(ctx, &s.theme);
    ctx.set_zoom_factor(s.zoom);
    let id = egui::Id::unique("applied-font");
    let last = ctx.data(|d| d.get_temp::<String>(id)).unwrap_or_default();
    if s.font == last {
        return None;
    }
    let bytes = (!s.font.is_empty()).then(|| load_font(&s.font));
    let problem = matches!(bytes, Some(None)).then(|| {
        let name = Path::new(&s.font).file_name().unwrap_or_default();
        format!("font {} can't be used", name.to_string_lossy())
    });
    if ctx.cumulative_pass_nr() == 0 {
        // `ctx.fonts` panics before the first pass (and it applies the fonts
        // `main::setup_fonts` queued, which we extend).
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
    }
    let mut defs = ctx.fonts(|f| f.definitions().clone());
    for fam in [
        egui::FontFamily::Proportional,
        crate::md::bold_family(),
        crate::md::title_family(),
    ] {
        let list = defs.families.entry(fam).or_default();
        list.retain(|n| n != "user-font");
        if let Some(Some(b)) = &bytes {
            defs.font_data
                .insert("user-font".into(), egui::FontData::from_owned(b.clone()).into());
            list.insert(0, "user-font".into());
        }
    }
    ctx.set_fonts(defs);
    let applied = if problem.is_some() { String::new() } else { s.font.clone() };
    ctx.data_mut(|d| d.insert_temp(id, applied));
    problem
}

/// Scales this frame's scroll events by `speed`.
pub fn scale_scroll(raw: &mut egui::RawInput, speed: f32) {
    if speed == 1.0 {
        return;
    }
    for e in &mut raw.events {
        if let egui::Event::MouseWheel { delta, .. } = e {
            *delta *= speed;
        }
    }
}

/// Installed .ttf/.otf files, sorted by file name.
pub fn system_fonts() -> Vec<PathBuf> {
    // symlinked dirs are not followed; depth capped
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u8) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_lowercase);
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth < 6 {
                    walk(&p, out, depth + 1);
                }
            } else if matches!(ext.as_deref(), Some("ttf" | "otf")) {
                out.push(p);
            }
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut v = Vec::new();
    for d in [
        PathBuf::from("/usr/share/fonts"),
        home.join(".local/share/fonts"),
        home.join(".fonts"),
    ] {
        walk(&d, &mut v, 0);
    }
    v.sort_by(|a, b| a.file_name().cmp(&b.file_name()).then_with(|| a.cmp(b)));
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), (Settings::default(), None));
        let s = Settings {
            theme: "Ocean".into(),
            zoom: 1.5,
            full_width: true,
            ..Default::default()
        };
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()), (s, None));
        std::fs::write(dir.path().join(FILE), "{nope").unwrap();
        let (d, p) = load(dir.path());
        assert_eq!(d, Settings::default());
        assert!(p.unwrap().starts_with("settings.json: "));
    }

    #[test]
    fn zoom_and_speed_clamped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), r#"{"zoom":9,"scroll_speed":0}"#).unwrap();
        let (s, p) = load(dir.path());
        assert!(p.is_none());
        assert_eq!((s.zoom, s.scroll_speed), (2.0, 0.25));
    }

    #[test]
    fn scroll_scaled() {
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(1.0, -3.0),
            source: egui::MouseWheelSource::Wheel,
            phase: egui::TouchPhase::Move,
            modifiers: Default::default(),
        });
        scale_scroll(&mut raw, 2.0);
        assert!(
            matches!(raw.events[0], egui::Event::MouseWheel { delta, .. } if delta == egui::vec2(2.0, -6.0))
        );
    }

    #[test]
    fn broken_font_is_refused_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("junk.ttf");
        std::fs::write(&f, b"this is not a font").unwrap();
        let ctx = egui::Context::default();
        let s = Settings {
            font: f.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let p = apply(&ctx, &s).unwrap();
        assert_eq!(p, "font junk.ttf can't be used");
        // the context still works
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.label("x");
        })
        .drop_without_applying_deltas();
        assert_eq!(apply(&ctx, &Settings::default()), None);
        assert_eq!(apply(&ctx, &s).unwrap(), p, "still refused, still no panic");
    }

    #[test]
    fn save_keeps_a_broken_file_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("settings.json.bad");
        std::fs::write(dir.path().join(FILE), "{nope").unwrap();
        save(dir.path(), &Settings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{nope");
        std::fs::remove_file(&bad).unwrap();
        save(dir.path(), &Settings::default()).unwrap();
        assert!(!bad.exists());
    }

    #[test]
    fn apply_installs_user_font() {
        let ctx = egui::Context::default();
        let mut s = Settings {
            theme: "Mint".into(),
            ..Default::default()
        };
        apply(&ctx, &s);
        let f = system_fonts().into_iter().next();
        if let Some(f) = f {
            s.font = f.to_string_lossy().into_owned();
            apply(&ctx, &s);
            apply(&ctx, &s);
        }
    }

    #[test]
    fn unknown_and_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE),
            r#"{"theme":"Dark","future_thing":{"a":1}}"#,
        )
        .unwrap();
        let (s, p) = load(dir.path());
        assert!(p.is_none());
        assert_eq!(
            s,
            Settings { theme: "Dark".into(), ..Default::default() }
        );
        std::fs::write(dir.path().join(FILE), "{}").unwrap();
        assert_eq!(load(dir.path()), (Settings::default(), None));
    }

    #[test]
    fn scroll_scaling_leaves_other_events() {
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::Text("x".into()));
        raw.events.push(egui::Event::Zoom(1.5));
        let before = raw.events.clone();
        scale_scroll(&mut raw, 3.0);
        assert_eq!(raw.events, before);
    }
}
