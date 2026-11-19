# Phase 2 plan: first GUI slice (`notion-gui`, iced)

Status: **Approved 2026-11-19 with all defaults (iced 0.14, manual Ctrl+S push, Tab/Shift+Tab in G3, mention/equation blocks read-only).** This plan builds on Phase 1 (`docs/plan.md`). The core stays UI-agnostic: the GUI is just one more client of `notion-core`.

## Goal of this slice

A keyboard-driven desktop window where you can:
- open a cached page (or find one via search);
- read it as an outline;
- edit block text with Markdown-style inline formatting;
- add, delete and indent blocks;
- push and pull.

Not in this slice: databases and data-source views, page creation, a conflict-resolution UI beyond listing conflicts, images and files, drag-and-drop (never), and theming beyond iced's light/dark.

## New dependency (needs your OK)

- **`iced = "0.14"`** (current release) with default features: wgpu plus tiny-skia fallback, X11 and Wayland. It's a large dependency tree (wgpu), but iced is the GUI toolkit you already chose, so there's nothing else to add.

## Architecture

```
notion-gui (iced, UI thread) ──reads/local edits──► Store (connection #1)
        │  ▲
  cmds  │  │ events (Subscription)
        ▼  │
sync worker thread: current-thread tokio runtime + Client + Store (connection #2)
        └── pull_page / push / search ──► Notion API
```

- **A dedicated worker thread for sync.** The Phase 1 sync futures are `!Send` (known gap §7.6), so they can't run on iced's executor.
  - The worker thread owns its own `Store` connection and `Client`, and runs a current-thread tokio runtime.
  - The GUI sends `Cmd::{Pull(id, force), Push, Search(q)}` over a channel. Events (`Pulled`, `Pushed(report)`, `SearchResults`, `Error`) come back through an iced `Subscription`.
- **Two SQLite connections on one file.** WAL mode is already on. The core gets a `busy_timeout` so the two connections don't fail with `SQLITE_BUSY`.
- **GUI-side state.** The GUI keeps a flattened outline: `Vec<Row { block_id, depth, kind, markdown }>`. It rebuilds it from the store after every pull or push and after each local edit.

## Core additions (task C1, in `notion-core`)

- **`markdown` module:** `to_markdown(&[RichText]) -> String` and `from_markdown(&str) -> Vec<RichText>`.
  - Supported syntax: `**bold**`, `*italic*`, `` `code` ``, `~~strike~~` and `[text](url)`. Colours and underline are dropped, because Markdown has no syntax for them.
  - A block whose rich text contains a mention or an equation is shown **read-only**, so edits can't silently lose it.
  - Round-trip tests.
- **`sync` local-edit helpers:**
  - `local_edit_rich(store, id, Vec<RichText>)`; the existing `local_edit_text` becomes a wrapper around it.
  - `local_delete_block`.
  - `local_move_block(store, id, new_parent, after)`. This updates the cache immediately and enqueues `MoveBlock`.
  - `local_set_checked` for to-dos.
  - `local_create_block` will respect `after` for the cached position (fixes gap §7.10).
- **`Store` additions:**
  - `pages() -> Vec<(id, title)>`, for the sidebar.
  - `busy_timeout(5s)` set in `open`.
  - `move_block_local(id, new_parent, position)`.

## GUI (`crates/notion-gui`)

**Layout:**
- **Left sidebar:** cached pages, plus a search box (`Ctrl+K`) that queries the API and pulls the chosen page.
- **Main area:** page title, then the outline. Each row is indented by depth and has a marker (`•`, `1.`, `☐`/`☑`, `▸`, `#` sizes).
- **Status bar:** queued, conflicted and failed op counts, plus the last sync error.

**Editing model:**
- Only the focused row is an `iced::widget::text_editor` holding Markdown source. Other rows render as styled `rich_text` spans.
- Leaving a row, or pressing Enter, commits the edit with `local_edit_rich`, but only if the text changed.
- Unsupported blocks show as a grey `[image]` and can't be focused for editing.

**Keys:**

| Key | Action |
|---|---|
| ↑ / ↓ | Move to the previous or next row (from the first or last line of the editor) |
| Enter | Commit, then create a paragraph after it under the same parent and focus it |
| Shift+Enter | Newline inside the block |
| Backspace on an empty row | Delete the block, focus the previous row |
| Tab / Shift+Tab | Indent (become the last child of the previous sibling) / outdent (go right after the parent) |
| Ctrl+Enter | Toggle a to-do's checkbox |
| Ctrl+S | Push |
| Ctrl+R | Pull the current page (`Ctrl+Shift+R` = force) |
| Ctrl+K | Search / switch page |
| Esc | Leave edit mode (commit) |

**Push behaviour (default):** manual `Ctrl+S`. Each push is followed by a re-pull of the page, so the ids of moved blocks become real.

**Indent caveat:** Notion can't move blocks, so Tab and Shift+Tab are copy-then-delete on push. The block gets a new id, and the move is refused if its subtree contains unsupported blocks or child pages.

**Testability:**
- Key handling and outline logic live in plain functions, `fn update(&mut State, Message) -> Vec<Effect>`.
- These are unit-tested on an in-memory store without opening a window.
- Rendering is checked manually.

## Tasks

| # | Task | Files | Acceptance |
|---|---|---|---|
| C1 | Core additions (markdown, local ops, store helpers) | `notion-core/src/{markdown.rs, sync.rs, store.rs, lib.rs}` | markdown round-trip incl. nesting edge cases; mention block is read-only; local move/delete/checked update the cache and enqueue the right op; `after` respected; busy_timeout set; all Phase 1 tests still pass |
| G1 | GUI skeleton: window, sidebar, read-only outline, worker thread (pull/push/search), status bar | `notion-gui/` (new crate), workspace `Cargo.toml` | builds; unit tests for outline flattening and worker command round-trip (wiremock); manual: opens the real page from the cache |
| G2 | Editing: focus, text_editor with Markdown, commit, Enter/Backspace/arrows, Ctrl+S/R | `notion-gui/src/*` | unit tests for key → effect logic; manual: edit, push, see it in Notion |
| G3 | Tab/Shift+Tab, to-do toggle, Ctrl+K switcher | `notion-gui/src/*` | unit tests for indent/outdent target computation; manual check |

**Order:** C1, then G1, G2, G3, all sequential. They share files, so nothing runs in parallel. Each task is implemented by one agent and reviewed by a separate one, with at most 2 rounds, then committed. The gate is `cargo test` plus `cargo clippy --all-targets -- -D warnings`.

## Decisions for you (defaults in bold)

1. Add **iced 0.14**: **yes**.
2. Push: **manual Ctrl+S**, or automatic (e.g. 5 s after the last edit).
3. Tab/Shift+Tab via copy-then-delete in this slice: **yes, in G3**, or defer.
4. Blocks with mentions or equations: **read-only in the editor**, or editable at the risk of flattening the mention to plain text.
