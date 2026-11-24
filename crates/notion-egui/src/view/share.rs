//! The top-right access bar ("Edited 17 h ago  (G)(P)(P) +2  Share") and the
//! Share popover: who can open the page, and adding workspace members.

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Area, Button, ComboBox, Frame, Layout, Order, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder,
};
use notion_core::model::{Access, User};

use crate::app::Action;
use crate::theme::Colors;

const FACE: f32 = 24.0;
const STEP: f32 = 16.0; // 8 px overlap
const SHARE_W: f32 = 64.0;
const ROLES: [(&str, &str); 4] = [
    ("editor", "Full access"),
    ("read_and_write", "Can edit"),
    ("comment_only", "Can comment"),
    ("reader", "Can view"),
];

pub fn role_label(role: &str) -> &str {
    ROLES.iter().find(|r| r.0 == role).map_or(role, |r| r.1)
}

/// The popover's own state, plus the hit rects of the last frame.
#[derive(Default)]
pub struct ShareUi {
    pub open: bool,
    query: String,
    /// "" means the default, read_and_write.
    role: String,
    /// The Share button, and the clickable member rows, as last drawn.
    pub button: Option<Rect>,
    pub hits: Vec<(String, Rect)>,
}

/// What the bar shows; `access` is None while the page's answer is pending.
pub struct Bar<'a> {
    pub edited: Option<String>,
    pub access: Option<&'a Result<Access, String>>,
    pub users: &'a [User],
    pub media: &'a crate::media::Media,
    /// A Share is in flight.
    pub waiting: bool,
}

/// The width the cluster needs, so the tabs can leave room for it.
pub fn width(b: &Bar) -> f32 {
    let n = b
        .access
        .and_then(|a| a.as_ref().ok())
        .map_or(0, |a| a.people.len());
    let faces = if n == 0 {
        0.0
    } else {
        FACE + STEP * (n.min(3) - 1) as f32 + 6.0
    };
    let more = if n > 3 { 28.0 } else { 0.0 };
    let edited = if b.edited.is_some() { 120.0 } else { 0.0 };
    edited + faces + more + SHARE_W + 12.0
}

/// ONE place to swap in the photo avatar (`people::face`).
fn face(ui: &mut Ui, b: &Bar, u: Option<&User>, id: &str, size: f32) {
    match u {
        Some(u) => {
            crate::people::face(ui, b.media, u, size);
        }
        None => {
            crate::people::avatar(ui, id, &crate::people::name_of(b.users, id), size);
        }
    }
}

fn user<'a>(id: &str, a: &'a Access, users: &'a [User]) -> Option<&'a User> {
    crate::people::find(&a.users, id).or_else(|| crate::people::find(users, id))
}

/// Draws the cluster into `ui` (right to left) and the popover below it.
pub fn show(ui: &mut Ui, c: &Colors, st: &mut ShareUi, b: &Bar) -> Option<Action> {
    let mut act = None;
    let ok = b.access.and_then(|a| a.as_ref().ok());
    let off_tip = match b.access {
        None => Some("Loading…".to_string()),
        Some(Err(e)) if e.starts_with("offline") => Some("offline".to_string()),
        Some(Err(e)) if e.contains("browser-session") => {
            Some("Sharing needs a browser session (sign in with the browser)".into())
        }
        Some(Err(e)) => Some(e.clone()),
        Some(Ok(_)) => None,
    };
    let mut toggle = false;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let btn = Button::new(RichText::new("Share").size(13.0).color(c.text)).fill(c.hover);
        let r = ui.add_enabled(off_tip.is_none(), btn);
        st.button = Some(r.rect);
        if let Some(t) = &off_tip {
            r.on_disabled_hover_text(t);
        } else if r.clicked() {
            toggle = true;
        }
        if let Some(a) = ok.filter(|a| !a.people.is_empty()) {
            let n = a.people.len();
            if n > 3 {
                ui.label(
                    RichText::new(format!("+{}", n - 3))
                        .size(12.0)
                        .color(c.secondary),
                );
            }
            let shown = n.min(3);
            let (faces, r) = ui
                .allocate_exact_size(vec2(FACE + STEP * (shown - 1) as f32, FACE), Sense::click());
            for (i, (id, _)) in a.people.iter().take(3).enumerate() {
                let rect =
                    Rect::from_min_size(faces.min + vec2(STEP * i as f32, 0.0), vec2(FACE, FACE));
                // a child, not a scope: a scope moves the cursor back over the faces
                let mut one = ui.new_child(UiBuilder::new().max_rect(rect));
                face(&mut one, b, user(id, a, b.users), id, FACE);
                ui.painter()
                    .circle_stroke(rect.center(), FACE / 2.0, Stroke::new(1.5, c.sidebar));
            }
            toggle |= r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
            // the faces open it too: a click on them is not "outside"
            st.button = st.button.map(|b| b.union(faces));
        }
        if let Some(t) = &b.edited {
            ui.add(egui::Label::new(RichText::new(t).size(12.0).color(c.secondary)).truncate());
        }
    });
    if toggle {
        st.open = !st.open;
        st.query.clear();
        ui.ctx().request_repaint();
    }
    st.hits.clear();
    match ok.filter(|_| st.open) {
        Some(a) => act = popover(ui, c, st, b, a),
        None => st.open = false,
    }
    act
}

fn popover(ui: &mut Ui, c: &Colors, st: &mut ShareUi, b: &Bar, a: &Access) -> Option<Action> {
    let mut act = None;
    let ctx = ui.ctx().clone();
    let btn = st.button.unwrap_or(Rect::ZERO);
    let pos = pos2(btn.right(), btn.bottom() + 6.0);
    let area = Area::new(egui::Id::unique("share-pop"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(pos)
        .show(&ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_repaint(); // a new Area is invisible on its sizing pass
            }
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(340.0);
                for (id, role) in &a.people {
                    let u = user(id, a, b.users);
                    ui.horizontal(|ui| {
                        face(ui, b, u, id, 28.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(crate::people::name_of(&a.users, id));
                            if let Some(e) = u.and_then(|u| u.email.as_deref()) {
                                ui.label(RichText::new(e).size(11.0).color(c.secondary));
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(role_label(role)).color(c.secondary));
                        });
                    });
                }
                for (who, role) in [
                    ("Everyone in workspace", &a.workspace),
                    ("Anyone with the link", &a.public),
                ] {
                    if let Some(r) = role {
                        ui.label(
                            RichText::new(format!("{who} — {}", role_label(r))).color(c.secondary),
                        );
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Add people").strong());
                    let cur = if st.role.is_empty() {
                        "read_and_write".to_string()
                    } else {
                        st.role.clone()
                    };
                    ComboBox::from_id_salt("share-role")
                        .selected_text(role_label(&cur))
                        .show_ui(ui, |ui| {
                            for &(k, l) in &ROLES {
                                if ui.selectable_label(cur == k, l).clicked() {
                                    st.role = k.to_string();
                                }
                            }
                        });
                });
                ui.add(
                    TextEdit::singleline(&mut st.query)
                        .hint_text("Search members")
                        .desired_width(f32::INFINITY),
                );
                if b.waiting {
                    ui.label(RichText::new("Sharing…").color(c.secondary));
                }
                let q = st.query.to_lowercase();
                let cands = b.users.iter().filter(|u| {
                    u.kind == "person"
                        && !a.people.iter().any(|(id, _)| *id == u.id)
                        && (u.name.to_lowercase().contains(&q)
                            || u.email
                                .as_deref()
                                .is_some_and(|e| e.to_lowercase().contains(&q)))
                });
                ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    for u in cands.take(50) {
                        let r = super::item(ui, c, false, 32.0, |ui| {
                            face(ui, b, Some(u), &u.id, 22.0);
                            ui.label(&u.name);
                            if let Some(e) = &u.email {
                                ui.label(RichText::new(e).size(11.0).color(c.secondary));
                            }
                        });
                        st.hits.push((u.id.clone(), r.rect));
                        if r.clicked() && !b.waiting {
                            let role = if st.role.is_empty() {
                                "read_and_write"
                            } else {
                                &st.role
                            };
                            act = Some(Action::Share {
                                user: u.id.clone(),
                                role: role.to_string(),
                            });
                        }
                    }
                });
            });
        });
    // a click outside closes it (the button toggles on its own)
    // (nor is one in the role list, which pops up outside the Area)
    let out = !egui::Popup::is_any_open(&ctx)
        && ctx.input(|i| i.pointer.any_pressed())
        && ctx
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|p| !area.response.rect.contains(p) && !btn.contains(p));
    if out || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        st.open = false;
    }
    act
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::Shape;

    /// The texts painted for a bar with `n` people.
    fn texts(n: usize) -> Vec<String> {
        painted(n).into_iter().map(|(t, _)| t).collect()
    }

    /// The texts painted for a bar with `n` people, with their left/right x.
    fn painted(n: usize) -> Vec<(String, (f32, f32))> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let acc = Ok(Access {
            people: (0..n)
                .map(|i| (format!("u{i}"), "editor".to_string()))
                .collect(),
            ..Default::default()
        });
        let media = crate::media::Media::default();
        let bar = Bar {
            edited: Some("Edited 1 h ago".into()),
            access: Some(&acc),
            users: &[],
            media: &media,
            waiting: false,
        };
        let mut st = ShareUi::default();
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 100.0))),
            ..Default::default()
        };
        let out = ctx.run_ui(raw, |ui| {
            let c = crate::theme::colors(ui);
            let mut child =
                ui.new_child(UiBuilder::new().layout(Layout::right_to_left(Align::Center)));
            show(&mut child, c, &mut st, &bar);
        });
        let mut t = Vec::new();
        for s in &out.shapes {
            if let Shape::Text(x) = &s.shape {
                let l = x.pos.x + x.galley.rect.left();
                t.push((x.galley.text().to_string(), (l, l + x.galley.rect.width())));
            }
        }
        out.drop_without_applying_deltas();
        t
    }

    #[test]
    fn three_faces_then_a_plus_count() {
        let faces = |t: &[String]| {
            t.iter()
                .filter(|s| s.chars().count() <= 2 && !s.starts_with('+'))
                .count()
        };
        let t = texts(5);
        assert!(t.contains(&"+2".to_string()), "{t:?}");
        assert_eq!(faces(&t), 3, "{t:?}");
        let t = texts(2);
        assert!(!t.iter().any(|s| s.starts_with('+')), "{t:?}");
        assert_eq!(faces(&t), 2, "{t:?}");
    }

    #[test]
    fn the_edited_text_stays_left_of_the_faces() {
        let p = painted(5);
        let edited = p.iter().find(|(t, _)| t.starts_with("Edited")).unwrap().1;
        for (t, (l, _)) in &p {
            if !t.starts_with("Edited") {
                assert!(edited.1 <= *l, "{t} at {l} under the Edited text ending at {}", edited.1);
            }
        }
    }
}
