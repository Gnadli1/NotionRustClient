//! Shim for the keys `slash.rs` routes (the iced crate defines this in edit.rs).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Up,
    Down,
    Enter,
    Backspace,
    Escape,
    Indent,
    Outdent,
    ToggleCheck,
}
