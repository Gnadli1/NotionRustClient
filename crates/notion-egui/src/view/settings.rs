//! The Settings window (Ctrl+,): Appearance, Keys, AI Agent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, Event, FontId, Frame, Key, Layout, Modifiers,
    Rect, RichText, ScrollArea, Sense, Ui, UiBuilder, Vec2,
};

use crate::ai::{self, Backend, Secrets};
use crate::app::App;
use crate::icons::{self, Icon};
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

const PRESETS: &[(&str, &str)] = &[
    ("OpenAI", "https://api.openai.com/v1"),
    ("OpenRouter", "https://openrouter.ai/api/v1"),
    ("Mistral", "https://api.mistral.ai/v1"),
    ("Groq", "https://api.groq.com/openai/v1"),
    (
        "Gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
    ),
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
        } => keys::chords(*key, *physical_key, *modifiers)
            .into_iter()
            .next(),
        _ => None,
    })
}

/// Binds `chord` to `cmd`. Ok holds the other command it took the chord from.
/// Keys that type text or that the editor owns need ctrl, alt or super.
pub fn bind(map: &mut Keymap, chord: Chord, cmd: Command) -> Result<Option<Command>, String> {
    if !chord.is_bindable() {
        return Err(format!(
            "{chord} types text or is the editor's own key: add Ctrl, Alt or Super"
        ));
    }
    let prev = map.get(&chord).filter(|p| *p != cmd);
    map.set(chord, Some(cmd));
    Ok(prev)
}

/// "heading_1" -> "Heading 1".
fn label(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next()
        .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
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

/// The left column: (name, heading subtitle, icon) per tab.
const TABS: [(&str, &str, Icon); 3] = [
    ("Appearance", "Choose how the app looks and feels on this computer", Icon::Sidebar),
    (
        "Keyboard shortcuts",
        "Bind any key to any action, including every / command",
        Icon::Table,
    ),
    (
        "AI Agent",
        "Connect Claude with your subscription, or any AI with an API key",
        Icon::Sparkle,
    ),
];

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
    // a big window in the middle of the screen, like Notion's settings
    let screen = ctx.content_rect();
    let size = Vec2::new(
        (screen.width() - 80.0).clamp(420.0, 1100.0),
        (screen.height() - 80.0).clamp(320.0, 760.0),
    );
    let vis = ctx.global_style().visuals.clone();
    let frame = Frame::new()
        .fill(vis.window_fill)
        .stroke(vis.window_stroke)
        .corner_radius(12)
        .shadow(vis.window_shadow);
    let mut close = false;
    let m = egui::Modal::new(egui::Id::unique("settings"))
        .frame(frame)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_repaint(); // a new window is invisible until the next frame
            }
            let c = theme::colors(ui);
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let (nav, body) = rect.split_left_right_at_x(rect.left() + 230.0);
            let round = CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 };
            ui.painter().rect_filled(nav, round, c.sidebar);
            let mut nui = ui.new_child(
                UiBuilder::new()
                    .max_rect(nav.shrink(10.0))
                    .layout(Layout::top_down(Align::Min)),
            );
            nav_list(&mut nui, c, &mut st.tab);
            let x = Rect::from_center_size(body.right_top() + vec2(-24.0, 24.0), Vec2::splat(28.0));
            let mut xui = ui.new_child(UiBuilder::new().max_rect(x));
            close = icons::button(&mut xui, 28.0, Icon::Close, c.secondary, c.hover, Some("Close (Esc)"))
                .clicked();
            let inner = Rect::from_min_max(body.min + vec2(56.0, 40.0), body.max - vec2(40.0, 12.0));
            let mut bui = ui.new_child(
                UiBuilder::new()
                    .max_rect(inner)
                    .layout(Layout::top_down(Align::Min)),
            );
            ScrollArea::vertical()
                .auto_shrink(false)
                .show(&mut bui, |ui| {
                    ui.set_max_width(760.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let (title, sub, _) = TABS[st.tab.min(2)];
                    ui.label(RichText::new(title).size(28.0).strong().color(c.text));
                    ui.add_space(6.0);
                    ui.label(RichText::new(sub).size(15.0).color(c.secondary));
                    match st.tab {
                        0 => appearance(app, &mut st, ui),
                        1 => keys_tab(app, &mut st, ui),
                        _ => ai_tab(app, &mut st, ui),
                    }
                    ui.add_space(24.0);
                });
        });
    if close || m.backdrop_response.clicked() {
        open = false;
    }
    if st.tab != tab {
        ctx.request_repaint(); // the new tab is laid out in the next frame
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

fn nav_list(ui: &mut Ui, c: &theme::Colors, tab: &mut usize) {
    ui.add_space(8.0);
    ui.label(RichText::new("  Settings").size(12.5).color(c.secondary));
    ui.add_space(4.0);
    for (i, (name, _, icon)) in TABS.iter().enumerate() {
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
        if i == *tab {
            ui.painter().rect_filled(r, 6.0, c.hover.gamma_multiply(2.0));
        } else if resp.hovered() {
            ui.painter().rect_filled(r, 6.0, c.hover);
        }
        let ir = Rect::from_center_size(egui::pos2(r.left() + 18.0, r.center().y), Vec2::splat(18.0));
        icons::paint(ui, ir, *icon, c.secondary);
        ui.painter().text(
            egui::pos2(r.left() + 38.0, r.center().y),
            Align2::LEFT_CENTER,
            *name,
            FontId::proportional(14.5),
            c.text,
        );
        if resp.clicked() {
            *tab = i;
        }
    }
    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        ui.label(RichText::new("  Ctrl+, opens · Esc closes").size(11.5).color(c.secondary));
    });
}

/// A section heading with a rule under it.
fn section(ui: &mut Ui, c: &theme::Colors, title: &str) {
    ui.add_space(30.0);
    ui.label(RichText::new(title).size(16.5).color(c.text));
    ui.add_space(10.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), (1.0, c.border));
    ui.add_space(4.0);
}

/// One setting: title and description on the left, its control on the right.
fn row(ui: &mut Ui, c: &theme::Colors, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w * 0.58, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.label(RichText::new(title).size(14.5).color(c.text));
            if !desc.is_empty() {
                ui.label(RichText::new(desc).size(12.5).color(c.secondary));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

/// An on/off switch.
fn toggle(ui: &mut Ui, c: &theme::Colors, on: &mut bool) {
    let (r, resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let fill = if *on { c.accent } else { c.hover.gamma_multiply(3.0) };
    ui.painter().rect_filled(r, 11.0, fill);
    let x = egui::lerp((r.left() + 11.0)..=(r.right() - 11.0), t);
    ui.painter().circle_filled(egui::pos2(x, r.center().y), 8.5, Color32::WHITE);
}

fn swatch(ui: &mut egui::Ui, p: &theme::Palette) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 2.0, p.colors.bg);
    ui.painter().rect_filled(r.shrink(4.0), 1.0, p.colors.accent);
}

fn appearance(app: &mut App, st: &mut SettingsUi, ui: &mut egui::Ui) {
    let c = theme::colors(ui);
    let s = &mut app.settings;
    section(ui, c, "Theme");
    row(ui, c, "Design", "A light, dark or colourful palette, or follow the system", |ui| {
        egui::ComboBox::from_id_salt("theme")
            .selected_text(s.theme.clone())
            .width(180.0)
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

    section(ui, c, "Text");
    row(ui, c, "Text size", "Scales the whole window, like zoom in a browser", |ui| {
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
    let font_filter = &mut st.font_filter;
    row(ui, c, "Font", "Inter, or any font installed on this computer", |ui| {
        egui::ComboBox::from_id_salt("font")
            .selected_text(current)
            .width(180.0)
            .show_ui(ui, |ui| {
                ui.add(egui::TextEdit::singleline(font_filter).hint_text("Filter"));
                ui.selectable_value(&mut s.font, String::new(), "Inter (built-in)");
                let f = font_filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    for (n, p) in fonts.iter().filter(|(n, _)| n.to_lowercase().contains(&f)) {
                        ui.selectable_value(&mut s.font, p.to_string_lossy().into(), n);
                    }
                });
            });
    });
    row(ui, c, "Full-width pages", "Use the whole window for page content", |ui| {
        toggle(ui, c, &mut s.full_width)
    });

    section(ui, c, "Scrolling");
    row(ui, c, "Scroll speed", "Multiplier for the mouse wheel and touchpad", |ui| {
        ui.add(egui::Slider::new(&mut s.scroll_speed, 0.25..=4.0).suffix("×"));
    });

    section(ui, c, "Reset");
    let mut reset = false;
    row(ui, c, "Reset appearance", "Back to the defaults; AI settings stay", |ui| {
        reset = ui.button("Reset to defaults").clicked();
    });
    if reset {
        let ai = s.ai.clone();
        *s = Settings {
            ai,
            ..Settings::default()
        };
        st.zoom = None;
    }
}

fn keys_tab(app: &mut App, st: &mut SettingsUi, ui: &mut egui::Ui) {
    let c = theme::colors(ui);
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
    ui.add_space(22.0);
    let mut reset = false;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.key_filter)
                .hint_text("Search shortcuts")
                .desired_width(280.0),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            reset = ui.button("Reset all to defaults").clicked();
        });
    });
    if reset {
        app.keys = Keymap::default();
        st.capture = None;
        st.note.clear();
        save_keys(app);
    }
    if !st.note.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new(&st.note).color(c.accent));
    }
    section(ui, c, "Actions and / commands");
    let f = st.key_filter.to_lowercase();
    let bound = app.keys.bindings();
    let (mut unbind, mut start) = (None, None);
    for (name, cmd) in keys::NAMES {
        let text = label(name);
        let chords: Vec<&Chord> = bound.iter().filter(|(_, c)| c == cmd).map(|(k, _)| k).collect();
        let hit = text.to_lowercase().contains(&f) || chords.iter().any(|k| k.to_string().contains(&f));
        if !hit {
            continue;
        }
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            ui.label(RichText::new(text).size(14.0).color(c.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if st.capture == Some(*cmd) {
                    ui.label(RichText::new("Press keys… Esc cancels").color(c.accent));
                } else if icons::button(ui, 24.0, Icon::Plus, c.secondary, c.hover, Some("Add a key"))
                    .clicked()
                {
                    start = Some(*cmd);
                }
                for k in chords.iter().rev() {
                    let chip = egui::Button::new(RichText::new(format!("{k}  ×")).size(12.5).color(c.text))
                        .fill(c.hover.gamma_multiply(1.6))
                        .corner_radius(6);
                    if ui.add(chip).on_hover_text("Unbind").clicked() {
                        unbind = Some((*k).clone());
                    }
                }
            });
        });
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), (1.0, c.border.gamma_multiply(0.6)));
    }
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
    let c = theme::colors(ui);
    let cfg = &mut app.settings.ai;
    section(ui, c, "Provider");
    for (b, t, d) in [
        (
            Backend::ClaudeCli,
            "Claude — your subscription",
            "Runs your installed Claude Code with your Pro/Max login; no API key",
        ),
        (Backend::Anthropic, "Anthropic API", "Pay-as-you-go with an Anthropic API key"),
        (
            Backend::OpenAiCompatible,
            "Other AI",
            "Any OpenAI-compatible API: OpenAI, OpenRouter, Mistral, Groq, Gemini, Ollama",
        ),
    ] {
        row(ui, c, t, d, |ui| {
            if ui.radio(cfg.backend == b, "").clicked() {
                cfg.backend = b;
            }
        });
    }
    let mut key_changed = false;
    let slot = Secrets::slot(cfg);
    let mut key = slot
        .as_ref()
        .and_then(|s| app.ai_secrets.keys.get(s))
        .cloned()
        .unwrap_or_default();
    let field = |ui: &mut Ui, v: &mut String, hint: &str, secret: bool| {
        ui.add(
            egui::TextEdit::singleline(v)
                .hint_text(hint)
                .password(secret)
                .desired_width(300.0),
        )
        .changed()
    };
    section(ui, c, "Connection");
    let key_desc = "Saved only on this computer (ai-keys.json, readable by you alone)";
    match cfg.backend {
        Backend::ClaudeCli => {
            row(ui, c, "Claude path", "Run `claude` once in a terminal to sign in", |ui| {
                field(ui, &mut cfg.claude_path, "claude", false);
            });
            row(ui, c, "Model", "Empty uses Claude Code's default", |ui| {
                field(ui, &mut cfg.model, "default", false);
            });
        }
        Backend::Anthropic => {
            row(ui, c, "API key", key_desc, |ui| {
                key_changed = field(ui, &mut key, "sk-ant-…", true);
            });
            row(ui, c, "Model", "Empty uses claude-opus-5-5", |ui| {
                field(ui, &mut cfg.model, "claude-opus-5-5", false);
            });
        }
        Backend::OpenAiCompatible => {
            let cur = PRESETS
                .iter()
                .find(|(_, u)| *u == cfg.base_url.trim_end_matches('/'))
                .map_or("Custom", |(n, _)| n);
            row(ui, c, "Service", "Fills in the base URL", |ui| {
                egui::ComboBox::from_id_salt("preset")
                    .selected_text(cur)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (n, u) in PRESETS {
                            if ui.selectable_label(cur == *n, *n).clicked() {
                                cfg.base_url = (*u).into();
                            }
                        }
                        let _ = ui.selectable_label(cur == "Custom", "Custom"); // keeps the URL
                    });
            });
            row(ui, c, "Base URL", "https, or http to this computer only", |ui| {
                field(ui, &mut cfg.base_url, "https://…/v1", false);
            });
            row(ui, c, "API key", key_desc, |ui| {
                key_changed = field(ui, &mut key, "empty for a local server", true);
            });
            row(ui, c, "Model", "Required, e.g. gpt-4o", |ui| {
                field(ui, &mut cfg.model, "model name", false);
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
    let done = st.test.as_ref().and_then(|t| t.lock().unwrap().take());
    if done.is_some() {
        st.test = None;
        st.test_result = done;
    }
    section(ui, c, "Check");
    let pending = st.test.is_some();
    let mut go = false;
    row(ui, c, "Test connection", "Sends one tiny request with these settings", |ui| {
        let text = if pending { "Testing…" } else { "Test connection" };
        go = ui.add_enabled(!pending, egui::Button::new(text)).clicked();
    });
    if go {
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
    match &st.test_result {
        Some(Ok(m)) => {
            ui.add_space(6.0);
            ui.label(RichText::new(m).color(c.accent));
        }
        Some(Err(e)) => {
            ui.add_space(6.0);
            ui.label(RichText::new(e).color(c.error));
        }
        None => {}
    }
    ui.add_space(18.0);
    ui.label(
        RichText::new("Open the AI panel with Ctrl+Shift+H or the ✦ button at the bottom of the sidebar.")
            .size(12.5)
            .color(c.secondary),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{key_mod, Win};
    use eframe::egui::RawInput;

    fn run(w: &mut Win, events: Vec<Event>) {
        let raw = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
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
        let cs = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
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
        let cs = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        };
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
        // editing first: the settings window is modal and blocks the page behind it
        w.edit(0);
        w.select(0, 1);
        w.app.settings_ui = Some(SettingsUi {
            tab: 1,
            capture: Some(callout),
            ..Default::default()
        });
        w.press(Key::B, Modifiers::CTRL);
        let text = w.app.focus.as_ref().map_or("a", |f| f.text.as_str());
        assert_eq!(text, "a", "ctrl+b must not bold");
        assert_eq!(w.app.keys.get(&ch("ctrl+b")), Some(callout));
        assert!(!w.app.settings_ui.as_ref().unwrap().capturing());
    }

    #[test]
    fn bare_editor_keys_rejected() {
        let callout = keys::NAMES.iter().find(|(n, _)| *n == "callout").unwrap().1;
        let mut m = Keymap::default();
        let before = m.clone();
        for k in [
            "enter",
            "up",
            "escape",
            "a",
            "shift+a",
            "1",
            "space",
            "+",
            "shift+space",
        ] {
            assert!(bind(&mut m, ch(k), callout).is_err(), "{k}");
        }
        assert_eq!(m, before);
    }
}
