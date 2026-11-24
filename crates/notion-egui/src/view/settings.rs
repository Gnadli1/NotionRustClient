//! The Settings window (Ctrl+,): Appearance, Keys, AI Agent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Event, Key, Modifiers, RichText, Vec2};

use crate::ai::{self, Backend, Secrets};
use crate::app::App;
use crate::keys::{self, Chord, Command, Keymap};
use crate::settings::{self, Settings};
use crate::theme;

type Test = Arc<Mutex<Option<Result<String, String>>>>;

/// The window's own state while it's open.
#[derive(Default)]
pub struct SettingsUi {
    pub tab: usize,
    /// (file stem, path) of the system fonts, listed on first use.
    fonts: Option<Vec<(String, PathBuf)>>,
    font_filter: String,
    /// Zoom (percent) while the slider is dragged; applied on release.
    zoom: Option<f32>,
    key_filter: String,
    /// The command waiting for a key press.
    capture: Option<Command>,
    note: String,
    /// A running connection test, and the last result.
    test: Option<Test>,
    test_result: Option<Result<String, String>>,
}

impl SettingsUi {
    /// Waiting for a key press: the global shortcuts must stay quiet.
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }
}

const TABS: [&str; 3] = ["Appearance", "Keys", "AI Agent"];

const PRESETS: &[(&str, &str)] = &[
    ("OpenAI", "https://api.openai.com/v1"),
    ("OpenRouter", "https://openrouter.ai/api/v1"),
    ("Mistral", "https://api.mistral.ai/v1"),
    ("Groq", "https://api.groq.com/openai/v1"),
    ("Gemini", "https://generativelanguage.googleapis.com/v1beta/openai"),
    ("Ollama (local)", "http://localhost:11434/v1"),
];

/// The chord of the first fresh key press in `events` (modifier keys alone
/// and repeats don't count).
pub fn capture(events: &[Event]) -> Option<Chord> {
    events.iter().find_map(|e| match e {
        Event::Key {
            key,
            physical_key,
            pressed: true,
            repeat: false,
            modifiers,
        } => keys::chords(*key, *physical_key, *modifiers).into_iter().next(),
        _ => None,
    })
}

/// Binds `chord` to `cmd`. Ok holds the other command it took the chord from.
/// Keys that type text or that the editor owns need ctrl, alt or super.
pub fn bind(map: &mut Keymap, chord: Chord, cmd: Command) -> Result<Option<Command>, String> {
    if !chord.is_bindable() {
        return Err(format!("{chord} types text or is the editor's own key: add Ctrl, Alt or Super"));
    }
    let prev = map.get(&chord).filter(|p| *p != cmd);
    map.set(chord, Some(cmd));
    Ok(prev)
}

/// "heading_1" -> "Heading 1".
fn label(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

fn save_keys(app: &mut App) {
    if let Some(dir) = app.profiles_dir.clone() {
        if let Err(e) = app.keys.save(&dir) {
            app.last_error = Some(format!("keys.json: {e}"));
        }
    }
}

fn save_secrets(app: &mut App) {
    if let Some(dir) = app.profiles_dir.clone() {
        if let Err(e) = ai::save_secrets(&dir, &app.ai_secrets) {
            app.last_error = Some(format!("{}: {e}", ai::KEYS_FILE));
        }
    }
}

/// Shows the window when `app.settings_ui` is Some; edits `app.settings`,
/// `app.keys`, `app.ai_secrets` and saves them on change.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut st) = app.settings_ui.take() else {
        return;
    };
    // Esc cancels a key capture, else closes; an open combo keeps its own Esc
    let mut open = true;
    if (st.capture.is_some() || !ctx.any_popup_open())
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        open = st.capture.take().is_some();
    }
    let (before, tab) = (app.settings.clone(), st.tab);
    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(true)
        .default_size(Vec2::new(560.0, 480.0))
        .open(&mut open)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_repaint(); // a new window is invisible until the next frame
            }
            ui.horizontal(|ui| {
                for (i, t) in TABS.iter().enumerate() {
                    ui.selectable_value(&mut st.tab, i, *t);
                }
            });
            ui.separator();
            match st.tab {
                0 => appearance(app, &mut st, ui),
                1 => keys_tab(app, &mut st, ui),
                _ => ai_tab(app, &mut st, ui),
            }
        });
    if st.tab != tab {
        st.capture = None;
    }
    if app.settings != before {
        let s = &app.settings;
        if (&s.theme, s.zoom, &s.font) != (&before.theme, before.zoom, &before.font) {
            if let Some(p) = settings::apply(ctx, s) {
                app.settings.font.clear();
                app.last_error = Some(p);
            }
        }
        if let Some(dir) = app.profiles_dir.clone() {
            if let Err(e) = settings::save(&dir, &app.settings) {
                app.last_error = Some(format!("{}: {e}", settings::FILE));
            }
        }
    }
    if open {
        app.settings_ui = Some(st);
    }
}

fn swatch(ui: &mut egui::Ui, p: &theme::Palette) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 2.0, p.colors.bg);
    ui.painter().rect_filled(r.shrink(4.0), 1.0, p.colors.accent);
}

fn appearance(app: &mut App, st: &mut SettingsUi, ui: &mut egui::Ui) {
    let s = &mut app.settings;
    ui.horizontal(|ui| {
        ui.label("Theme");
        egui::ComboBox::from_id_salt("theme")
            .selected_text(s.theme.clone())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.theme, "System".to_string(), "System");
                for p in theme::THEMES {
                    ui.horizontal(|ui| {
                        swatch(ui, p);
                        ui.selectable_value(&mut s.theme, p.name.to_string(), p.name);
                    });
                }
            });
    });

    ui.horizontal(|ui| {
        ui.label("Text size");
        let mut pct = st.zoom.unwrap_or(s.zoom * 100.0);
        let r = ui.add(egui::Slider::new(&mut pct, 75.0..=200.0).suffix(" %").step_by(5.0));
        if r.changed() {
            st.zoom = Some(pct);
        }
        // zoom rescales the window: apply on release so the slider stays put
        if (r.drag_stopped() || r.lost_focus()) && st.zoom.is_some() {
            s.zoom = pct / 100.0;
            st.zoom = None;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Scroll speed");
        ui.add(egui::Slider::new(&mut s.scroll_speed, 0.25..=4.0).suffix("×"));
    });

    let fonts = st.fonts.get_or_insert_with(|| {
        let mut v: Vec<_> = settings::system_fonts()
            .into_iter()
            .map(|p| (p.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into()), p))
            .collect();
        v.sort();
        v
    });
    let current = if s.font.is_empty() {
        "Inter (built-in)".to_string()
    } else {
        fonts
            .iter()
            .find(|(_, p)| p.to_string_lossy() == s.font)
            .map_or_else(|| s.font.clone(), |(n, _)| n.clone())
    };
    ui.horizontal(|ui| {
        ui.label("Font");
        egui::ComboBox::from_id_salt("font")
            .selected_text(current)
            .show_ui(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut st.font_filter).hint_text("Filter"));
                ui.selectable_value(&mut s.font, String::new(), "Inter (built-in)");
                let f = st.font_filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    for (n, p) in fonts.iter().filter(|(n, _)| n.to_lowercase().contains(&f)) {
                        ui.selectable_value(&mut s.font, p.to_string_lossy().into(), n);
                    }
                });
            });
    });

    ui.checkbox(&mut s.full_width, "Full-width pages");
    ui.add_space(8.0);
    if ui.button("Reset to defaults").clicked() {
        let ai = s.ai.clone();
        *s = Settings {
            ai,
            ..Settings::default()
        };
        st.zoom = None;
    }
}

fn keys_tab(app: &mut App, st: &mut SettingsUi, ui: &mut egui::Ui) {
    if let Some(cmd) = st.capture {
        let chord = ui.ctx().input_mut(|i| {
            let c = capture(&i.events);
            if c.is_some() {
                i.events.retain(|e| !matches!(e, Event::Key { .. } | Event::Text(_)));
            }
            c
        });
        if let Some(chord) = chord {
            let shown = chord.to_string();
            match bind(&mut app.keys, chord, cmd) {
                Ok(prev) => {
                    st.note = match prev {
                        Some(o) => format!("{shown} replaces {}", label(keys::name(o))),
                        None => String::new(),
                    };
                    st.capture = None;
                    save_keys(app);
                }
                Err(m) => st.note = m,
            }
        }
    }
    ui.label("/ commands (Text, Heading 1, To-do, Callout, …) can be bound like any other action");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.key_filter).hint_text("Filter"));
        if ui.button("Reset all to defaults").clicked() {
            app.keys = Keymap::default();
            st.capture = None;
            st.note.clear();
            save_keys(app);
        }
    });
    if !st.note.is_empty() {
        ui.label(&st.note);
    }
    let f = st.key_filter.to_lowercase();
    let bound = app.keys.bindings();
    let (mut unbind, mut start) = (None, None);
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("keys").num_columns(2).striped(true).show(ui, |ui| {
            for (name, cmd) in keys::NAMES {
                let text = label(name);
                let chords: Vec<&Chord> =
                    bound.iter().filter(|(_, c)| c == cmd).map(|(k, _)| k).collect();
                let hit = text.to_lowercase().contains(&f)
                    || chords.iter().any(|k| k.to_string().contains(&f));
                if !hit {
                    continue;
                }
                ui.label(text);
                ui.horizontal_wrapped(|ui| {
                    for k in chords {
                        if ui.small_button(format!("{k} ×")).on_hover_text("Unbind").clicked() {
                            unbind = Some(k.clone());
                        }
                    }
                    if st.capture == Some(*cmd) {
                        ui.label("Press keys… Esc cancels");
                    } else if ui.small_button("+").clicked() {
                        start = Some(*cmd);
                    }
                });
                ui.end_row();
            }
        });
    });
    if let Some(k) = unbind {
        app.keys.set(k, None);
        st.note.clear();
        save_keys(app);
    }
    if start.is_some() {
        st.capture = start;
        st.note.clear();
    }
}

fn ai_tab(app: &mut App, st: &mut SettingsUi, ui: &mut egui::Ui) {
    let cfg = &mut app.settings.ai;
    for (b, t) in [
        (Backend::ClaudeCli, "Claude — your subscription (Claude Code)"),
        (Backend::Anthropic, "Anthropic API key"),
        (Backend::OpenAiCompatible, "Other AI — OpenAI-compatible API key"),
    ] {
        ui.radio_value(&mut cfg.backend, b, t);
    }
    ui.add_space(6.0);
    let mut key_changed = false;
    let slot = Secrets::slot(cfg);
    let mut key = slot
        .as_ref()
        .and_then(|s| app.ai_secrets.keys.get(s))
        .cloned()
        .unwrap_or_default();
    match cfg.backend {
        Backend::ClaudeCli => {
            ui.horizontal(|ui| {
                ui.label("Claude path");
                ui.text_edit_singleline(&mut cfg.claude_path);
            });
            ui.label("Uses your Claude Code sign-in (Pro/Max). Run `claude` once in a terminal to sign in.");
        }
        Backend::Anthropic => {
            ui.horizontal(|ui| {
                ui.label("API key");
                key_changed = ui.add(egui::TextEdit::singleline(&mut key).password(true)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Model");
                ui.add(egui::TextEdit::singleline(&mut cfg.model).hint_text("claude-opus-5-5"));
            });
        }
        Backend::OpenAiCompatible => {
            let cur = PRESETS
                .iter()
                .find(|(_, u)| *u == cfg.base_url.trim_end_matches('/'))
                .map_or("Custom", |(n, _)| n);
            ui.horizontal(|ui| {
                ui.label("Service");
                egui::ComboBox::from_id_salt("preset").selected_text(cur).show_ui(ui, |ui| {
                    for (n, u) in PRESETS {
                        if ui.selectable_label(cur == *n, *n).clicked() {
                            cfg.base_url = (*u).into();
                        }
                    }
                    let _ = ui.selectable_label(cur == "Custom", "Custom"); // keeps the URL
                });
            });
            ui.horizontal(|ui| {
                ui.label("Base URL");
                ui.text_edit_singleline(&mut cfg.base_url);
            });
            ui.horizontal(|ui| {
                ui.label("API key");
                key_changed = ui.add(egui::TextEdit::singleline(&mut key).password(true)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("Model");
                ui.add(egui::TextEdit::singleline(&mut cfg.model).hint_text("required, e.g. gpt-4o"));
            });
        }
    }
    if key_changed {
        if let Some(s) = slot.clone() {
            if key.is_empty() {
                app.ai_secrets.keys.remove(&s);
            } else {
                app.ai_secrets.keys.insert(s, key.clone());
            }
            save_secrets(app);
        }
    }
    let cfg = app.settings.ai.clone();
    ui.add_space(6.0);
    let pending = st.test.is_some();
    if ui.add_enabled(!pending, egui::Button::new("Test connection")).clicked() {
        let k = (!key.is_empty()).then_some(key);
        let out: Test = Arc::default();
        let (o, ctx) = (out.clone(), ui.ctx().clone());
        std::thread::spawn(move || {
            let r = ai::test(&cfg, k.as_deref());
            *o.lock().unwrap() = Some(r);
            ctx.request_repaint();
        });
        st.test = Some(out);
        st.test_result = None;
    }
    let done = st.test.as_ref().and_then(|t| t.lock().unwrap().take());
    if done.is_some() {
        st.test = None;
        st.test_result = done;
    }
    if st.test.is_some() {
        ui.label("Testing…");
    }
    match &st.test_result {
        Some(Ok(m)) => {
            ui.label(m);
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
        }
        None => {}
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Open AI panel: Ctrl+Shift+H").weak());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{key_mod, Win};
    use eframe::egui::RawInput;

    fn run(w: &mut Win, events: Vec<Event>) {
        let raw = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))),
            events,
            ..Default::default()
        };
        let app = &mut w.app;
        let out = w.ctx.run_ui(raw, |ui| show(app, &ui.ctx().clone()));
        out.drop_without_applying_deltas();
    }

    fn ch(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn renders_all_tabs() {
        let mut w = Win::with_texts(&["a"]);
        w.app.settings_ui = Some(Default::default());
        for tab in 0..3 {
            w.app.settings_ui.as_mut().unwrap().tab = tab;
            run(&mut w, vec![]);
            run(&mut w, vec![]);
        }
        assert!(w.app.settings_ui.is_some());
    }

    #[test]
    fn escape_closes() {
        let mut w = Win::with_texts(&["a"]);
        w.app.settings_ui = Some(Default::default());
        run(&mut w, vec![]);
        run(&mut w, vec![key_mod(Key::Escape, Modifiers::NONE)]);
        assert!(w.app.settings_ui.is_none());
    }

    #[test]
    fn capture_takes_the_chord_and_rebinds() {
        assert_eq!(capture(&[Event::Text("x".into())]), None);
        assert_eq!(capture(&[key_mod(Key::Copy, Modifiers::CTRL)]), None);
        let cs = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };
        let c = capture(&[key_mod(Key::Num9, cs)]).unwrap();
        assert_eq!(c, ch("ctrl+shift+9"));
        let callout = keys::NAMES.iter().find(|(n, _)| *n == "callout").unwrap().1;
        let mut m = Keymap::default();
        assert_eq!(bind(&mut m, c.clone(), callout), Ok(None));
        assert_eq!(m.get(&c), Some(callout));
        // taking a bound chord reports who lost it
        let prev = bind(&mut m, ch("ctrl+b"), callout).unwrap();
        assert_eq!(prev.map(keys::name), Some("bold"));
        // editor keys need a modifier
        assert!(bind(&mut m, ch("enter"), callout).is_err());
        assert!(bind(&mut m, ch("ctrl+enter"), callout).is_ok());
    }

    #[test]
    fn capture_mode_binds_through_the_window() {
        let mut w = Win::with_texts(&["a"]);
        let callout = keys::NAMES.iter().find(|(n, _)| *n == "callout").unwrap().1;
        w.app.settings_ui = Some(SettingsUi {
            tab: 1,
            capture: Some(callout),
            ..Default::default()
        });
        run(&mut w, vec![]);
        let cs = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };
        run(&mut w, vec![key_mod(Key::Num9, cs)]);
        assert_eq!(w.app.keys.get(&ch("ctrl+shift+9")), Some(callout));
        assert!(!w.app.settings_ui.as_ref().unwrap().capturing());
    }

    #[test]
    fn labels() {
        assert_eq!(label("heading_1"), "Heading 1");
    }

    #[test]
    fn ctrl_comma_toggles_the_window() {
        let mut w = Win::with_texts(&["a"]);
        w.press(Key::Comma, Modifiers::CTRL);
        assert!(w.app.settings_ui.is_some());
        w.press(Key::Comma, Modifiers::CTRL);
        assert!(w.app.settings_ui.is_none());
        // also while a block is being edited
        w.edit(0);
        w.press(Key::Comma, Modifiers::CTRL);
        w.frames(vec![]);
        assert!(w.app.settings_ui.is_some());
    }

    #[test]
    fn sidebar_buttons_open_settings_and_ai() {
        let mut w = Win::with_texts(&["a"]);
        w.app.apply(crate::app::Action::Settings);
        w.app.apply(crate::app::Action::Ai);
        w.frames(vec![]);
        assert!(w.app.settings_ui.is_some() && w.app.ai.is_some());
        w.app.apply(crate::app::Action::Settings);
        assert!(w.app.settings_ui.is_none());
    }

    #[test]
    fn capturing_swallows_ctrl_b() {
        let mut w = Win::with_texts(&["a"]);
        let callout = keys::NAMES.iter().find(|(n, _)| *n == "callout").unwrap().1;
        w.app.settings_ui = Some(SettingsUi { tab: 1, capture: Some(callout), ..Default::default() });
        w.edit(0);
        w.select(0, 1);
        w.press(Key::B, Modifiers::CTRL);
        assert_eq!(w.text(), "a", "ctrl+b must not bold");
        assert_eq!(w.app.keys.get(&ch("ctrl+b")), Some(callout));
        assert!(!w.app.settings_ui.as_ref().unwrap().capturing());
    }

    #[test]
    fn bare_editor_keys_rejected() {
        let callout = keys::NAMES.iter().find(|(n, _)| *n == "callout").unwrap().1;
        let mut m = Keymap::default();
        let before = m.clone();
        for k in ["enter", "up", "escape", "a", "shift+a", "1", "space", "+", "shift+space"] {
            assert!(bind(&mut m, ch(k), callout).is_err(), "{k}");
        }
        assert_eq!(m, before);
    }
}
