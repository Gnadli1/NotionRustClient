//! Raw-event helpers: the sidebar shortcut and the key log.

use eframe::egui::{Event, Key};

/// True (and the events removed, so a focused text edit can't type them) if
/// `events` hold Ctrl+\ . On the Austrian layout `\` is AltGr+ß, and winit may
/// report it as logical Backslash, as Text "\\", or fall back to the physical
/// key under ß (Minus), so all of those count while Ctrl is held. Ctrl+Minus
/// is free: the zoom shortcut is disabled in main.rs.
pub fn take_sidebar_toggle(events: &mut Vec<Event>, ctrl_held: bool) -> bool {
    let hit = |e: &Event| match e {
        Event::Key {
            key,
            physical_key,
            pressed: true,
            repeat: false,
            modifiers,
        } => {
            modifiers.ctrl
                && (matches!(key, Key::Backslash | Key::Minus)
                    || matches!(physical_key, Some(Key::Backslash | Key::Minus)))
        }
        Event::Text(t) => ctrl_held && matches!(t.as_str(), "\\" | "ß"),
        _ => false,
    };
    let found = events.iter().any(hit);
    if found {
        events.retain(|e| !hit(e));
    }
    found
}

/// `NOTION_EGUI_KEYLOG=1`: print raw Key/Text/Ime events to stderr.
pub fn log(events: &[Event]) {
    for e in events {
        if matches!(e, Event::Key { .. } | Event::Text(_) | Event::Ime(_)) {
            eprintln!("{e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Modifiers;

    fn key(key: Key, physical: Option<Key>, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: physical,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn ctrl_backslash_variants_toggle() {
        let ctrl = Modifiers::CTRL;
        for ev in [
            key(Key::Backslash, None, ctrl),
            key(Key::Minus, Some(Key::Minus), ctrl), // Ctrl+ß, no logical key
            key(Key::A, Some(Key::Backslash), ctrl),
        ] {
            let mut v = vec![ev, Event::Text("x".into())];
            assert!(take_sidebar_toggle(&mut v, true));
            assert_eq!(
                v,
                vec![Event::Text("x".into())],
                "only the shortcut is eaten"
            );
        }
        let mut v = vec![Event::Text("ß".into())];
        assert!(take_sidebar_toggle(&mut v, true));
        assert!(v.is_empty());
    }

    #[test]
    fn plain_typing_does_not_toggle() {
        let mut v = vec![
            key(Key::Backslash, None, Modifiers::NONE),
            Event::Text("ß".into()), // typing ß without Ctrl
            Event::Text("\\".into()),
        ];
        assert!(!take_sidebar_toggle(&mut v, false));
        assert_eq!(v.len(), 3);
    }
}
