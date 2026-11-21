# Phase 3 plan: egui port and Notion feature parity

Status: **Approved 2026-11-20 with all defaults (eframe 0.37, API 2025-09-03, Ctrl+Shift+0–9, German aliases, keep iced until E2, F5/F7 later, 15 s / 60 s live updates).** It builds on Phase 2b (`docs/phase2b-plan.md`). Your screenshots of the Notion desktop app (sidebar, `/Glei` slash menu, Ctrl+T switcher) are the reference.

## Goal
- **egui:** move the GUI from iced to egui. It's the only pure-Rust toolkit with styled, editable text.
- **Keyboard-first:** the `/` menu, the `@` menu and the switcher work fully from the keyboard.
- **Feature parity:** add the Notion features the public API allows, and list the ones it doesn't.
- **Collaboration first:** comments, people and mentions, near-live updates while a page is open, conflicts you can actually resolve, and an activity inbox.

## Why egui (research summary)
| Toolkit | Styled editable text | Notes |
|---|---|---|
| **egui / eframe 0.37** | Yes. `TextEdit` with a custom layouter | Colour emoji since 0.37. Works on X11 and Wayland. Wayland IME is unverified. |
| iced 0.14 (now) | No. `text_editor` is plain text; `rich_text` is display only | What we have today |
| Slint 1.19 | No. Styled text is display only | |
| gpui 0.2 | Unclear | Before 1.0; breaks often |
| Xilem, Blitz | — | Experimental or beta |
| Tauri | Best (a web editor) | The editor would be JavaScript, not Rust |

You're on **Wayland**, so the spike tests IME and dead keys there.

## Approach: a spike first, then a port next to the iced app
The iced crate `notion-gui` stays and keeps working until `notion-egui` covers it. Deleting the iced crate is a separate decision, and I'll ask before doing it.

**Most of the code carries over unchanged.** It's plain Rust with no iced in it:
- **Core:** all of `notion-core`, including sync, undo checkpoints and conflicts.
- **GUI logic:** `outline.rs`, `slash.rs`, `tabs.rs`, `sidebar.rs`, `session.rs`, `tex.rs` and `edit.rs`.
- **Worker:** the worker thread stays. The iced `Subscription` becomes a channel that is polled every frame, plus `ctx.request_repaint()` when an event arrives.
- **Keys:** `keys.rs` gets an adapter from egui's key type. egui's `Key` enum is a fixed set and may have no `ß`. If so, Ctrl+ß is matched on the text event, as an AltGr workaround.
- **New code:** the app state, the update logic (`app.rs`) and the view layer.

**Editing model:** same as now. Only the focused row is a `TextEdit` over its Markdown source. Its layouter styles the text as you type: bold, italic, code, links and `$…$`. All other rows are painted as styled text from the cached rich text.

## Tasks

| # | Task | Files | Acceptance |
|---|---|---|---|
| E0 | **Spike, then stop for your check.** A new crate, `notion-egui`, opens one real cached page (the longest one) from the store, read-only apart from the focused row. **Must have:** a focused-row `TextEdit` with Markdown styling; colour emoji in icons; a `/` menu that works with **no mouse at all** (typing filters, ↑/↓ moves, Enter picks, Esc or Backspace past `/` closes); dead keys and IME composition on Wayland; Ctrl+\ and Ctrl+ß; smooth scrolling on the long page | `crates/notion-egui/*`, workspace `Cargo.toml` | builds; unit test that menu keys filter, move, pick and close; **you check it on screen → go / no-go** |
| E1 | **Shell port:** window, Notion theme (light and dark, following the system), sidebar page tree, tabs, breadcrumb, status bar with sync dots and the problem list, worker channel, session restore | `notion-egui/src/*` | existing tests for the logic modules pass from the new crate; manual check |
| E2 | **Editor port:** every block type from S3 (tables, columns, synced, link cards, callouts, toggles, code, equations), the editing keys, slash menu, Markdown shortcuts, keybinds, undo and redo, inline title edit | `notion-egui/src/*` | port of the iced crate's app tests; manual check |
| E3 | **Keyboard-first menus:** the `/` menu gains German aliases (`Gleichung`, `Überschrift`, `Aufzählung`, `Zitat`, `Tabelle` and so on), so `/Glei` finds both equation items. An `@` menu for page mentions and today's date. `[[` opens a page link picker. Ctrl+/ opens a "turn into / colour" block menu. Every menu runs on the keyboard alone | `notion-egui/src/*`, `slash.rs` | filter tests incl. German; key-only tests |
| F1 | **Navigation:** back and forward (Ctrl+[ / Ctrl+], mouse buttons 4/5); Ctrl+Shift+U goes up a level; Ctrl+F finds in the page; Ctrl+L copies the Notion URL; the Ctrl+K/Ctrl+P switcher looks like your screenshot (recent pages grouped Heute / Gestern / Vergangene Woche, each with its path, a preview pane, Ctrl+Enter opens in a new tab); an outline on the right (the heading ticks in your screenshot) | `notion-egui/src/*` | unit tests for history and grouping |
| F2 | **Block editing:** Ctrl+D duplicates a block; Ctrl+Shift+↑/↓ moves it (copy-then-delete, as with indent); Esc selects the block and Shift+↑/↓ extends the selection; delete, copy and paste work on selected blocks; text colour and background colour (19 Notion colours); `heading_4`; toggle headings; split the block at the cursor on Enter (closes a G2 gap); Ctrl+Alt+T opens or closes all toggles | `notion-core/src/{sync,model}.rs`, `notion-egui/src/*` | core tests through wiremock for duplicate and move; GUI tests |
| F3 | **Pages:** a new page (Ctrl+N, or `/page` creates a child page); rename; change the icon (emoji picker) and cover; move the page (`POST /v1/pages/{id}/move`); trash and restore; a Trash view; lock and unlock | `notion-core/src/{api,sync,ops}.rs`, `notion-egui/src/*` | wiremock tests for each op; manual check |
| F4 | **Databases, part 1:** open a data source (Timetable, Grades, Assignments and so on) as a **table** view; pull rows with filter and sort; edit cells for title, text, number, select, multi-select, status, date, checkbox, url, email, phone and people; formula, rollup and relation are read-only; add and delete rows | `notion-core/*`, `notion-egui/src/*` | wiremock tests for query and update; manual check against your Timetable |
| F5 | **Databases, part 2:** board, list, calendar and gallery views. The API's saved views are read when the API version allows it, otherwise a local view is used | `notion-egui/src/*`, maybe core | unit tests for grouping |
| C1 | **Comments:** a right-hand panel with page and block comments (Ctrl+Shift+M) and a comment count on each block. Reply in a thread (`discussion_id`). Comment on a page or a block. Edit and delete your own comments. Mention people and pages in comments. Comments are cached and written offline, then sent on push like other ops | `notion-core/src/{api,model,store,ops,sync}.rs`, `notion-egui/src/*` | wiremock tests for list, create, reply, edit and delete; queued offline comment is pushed |
| C2 | **People:** fetch and cache the workspace users (`/v1/users`). Show avatars and names. An `@` menu for user mentions. Show "edited by X, 5 min ago" on hover for each block and in the page header (`last_edited_by`). Edit people properties in database rows | `notion-core/*`, `notion-egui/src/*` | tests for the user cache and mention round-trip |
| C3 | **Live updates (near real time):** while a page is open, check it for changes every 15 s, using a cheap check of `last_edited_time` first and a full pull only when it changed. Check the other open tabs every 60 s. Remote changes merge into blocks you aren't editing, without a reload and with a short highlight on what changed. Stay within Notion's 180 requests per minute | `notion-core/src/sync.rs`, `notion-egui/src/*` | tests for rate budget and merge-while-editing |
| C4 | **Conflicts you can resolve:** for a conflict, show your text next to Notion's, with the differences marked. Choose **Keep mine** (re-queue your edit against the new remote version), **Keep theirs**, or edit a merged version. This closes the Phase 2b gap where Discard was the only choice | `notion-core/src/sync.rs`, `notion-egui/src/*` | core tests for keep-mine re-basing; GUI tests |
| C5 | **Activity inbox:** a local "Updates" list built from what each pull sees. It shows new comments, comments that mention you, and pages others edited since you last opened them, with unread markers. Notion's own inbox isn't in the API, so this is built locally | `notion-core/src/store.rs`, `notion-egui/src/*` | unit tests for unread tracking |
| F7 | **Media:** inline images (download, cache, show; the links expire after 1 h, so pull again first); upload an image or file (File Upload API, up to 20 MB) | `notion-core/*`, `notion-egui/src/*` | tests for upload and caching |
| F8 | **Full-workspace sync:** a "Sync everything" command and `notion-cli sync-all` pull every shared page and data source. Pages whose parent is a data source appear under their database in the sidebar | `notion-core/src/sync.rs`, `notion-cli`, sidebar | wiremock test for pagination; manual check |

**Order:** E0 → (your check) → E1 → E2 → E3 → C1 → C2 → C3 → C4 → F1 → F8 → F2 → F3 → F4 → C5 → F5 → F7. Collaboration (C1–C4) comes right after the port, as you asked. They run in sequence because they share `app.rs`. Core-only pieces of C1, C2, F3 and F4 can run in parallel with GUI tasks when their files don't overlap. As before, every task is implemented by one agent and reviewed by a separate one, with at most 2 rounds, then committed. **Subagents run on Sonnet 5.5.** The gate is `cargo test --workspace` plus clippy with `-D warnings`.

## Can't be built: the public API doesn't expose it
These are parts of your screenshot or the Notion app that the API doesn't give us:
- **Sidebar sections:** "Favoriten", "Privat", "Geteilt" and teamspaces. The sidebar will show "Kürzlich verwendet" (kept locally) and the page tree instead.
- **Page metadata:** the avatars of who else is on the page, page history and restoring old versions, backlinks.
- **Page settings:** full width, small text and font aren't in the API. Full width can be a local setting, per page.
- **Comments:** starting a comment on selected text, resolving threads, and emoji reactions on comments.
- **Collaboration:** live cursors and presence (who is on the page now), sharing and permissions ("Teilen"), invites, guests, Notion's own inbox and notifications, and @-reminders. Real-time push is also out: webhooks need a public HTTPS server, so C3 polls instead.
- **Other:** buttons, forms and "Neuer Chat" (Notion AI); duplicating a page server-side (it would be a local copy of its blocks).
- **Search:** the API searches titles only. Full-text search runs locally over the cache instead (part of F1's Ctrl+F and the switcher).

## Decisions for you (defaults in bold)
1. **Add `eframe = "0.37"` (which brings in egui):** **yes**. This is the one new dependency for the port. The spike might also need `egui_extras` for images; I'll ask then.
2. **API version:** **stay on 2025-09-03**, which already has the Views API, page move and tab blocks. The alternative is to move to 2026-03-11, which renames `after` to `position` and `archived` to `in_trash` throughout core and is needed for `heading_4`. With the default, `heading_4` shows as a read-only placeholder.
3. **Block-type shortcuts:** Notion on Linux uses **Ctrl+Shift+0–9**. K1's defaults are Ctrl+Alt+0–9. Default: **switch to Notion's**. Ctrl+Shift+E (inline equation) and Ctrl+Shift+S (strike) stay.
4. **German slash aliases:** **yes, German and English both match**. Menu labels stay in English, or switch to German.
5. **The iced crate:** **keep it until E2 is done**, then ask you about deleting it.
6. **The cut line:** E0–E3, C1–C5, F1–F4 and F8 are this phase; **F5 and F7 come later**. Or include all of them now.
7. **Live update interval:** **15 s for the open page, 60 s for other tabs**. Faster uses more of Notion's rate limit, which is shared with anyone else using the integration.

## Before you approve: the page import
I pulled all 164 pages shared with the integration into the cache. One failed with a Notion 502 error and succeeded on retry. The 13 data sources (Timetable, Grades and so on) aren't imported as tables until F4/F8. Their rows are cached as plain pages but don't appear in the sidebar yet.

## Known gaps after E1 (reviewed, deferred)
1. **No request timeout in core.** A stalled connection leaves "syncing…" on screen until restart. On exit the app waits at most 10 s for it. Add a reqwest timeout in a later task.
2. **The slash menu is off in equation and code rows**, so you can't turn them back into text from the editor yet (E2).
3. **Editor parity is E2:** Enter and Backspace don't create or delete blocks yet. Tab indent, undo, keybinds, table cells and title editing are also still missing.

## Known gaps after E2 (reviewed, deferred)
1. **Shared `keys.json`:** the old Ctrl+Alt+0–9 keys from the iced app still work alongside Ctrl+Shift+0–9. Delete them from `~/.config/notion-client/keys.json` if you only want Notion's keys.
2. **Ctrl+Minus on US layouts** also toggles the sidebar, a side effect of the Ctrl+ß workaround.
3. **Tab always indents**, even if `keys.json` unbinds it.
4. **Titles with formatting** (bold, colour, link) or mentions can't be edited in place, so they aren't flattened.
5. **After undoing a block conversion** the editor loses focus.
6. **No way back from the title** with ↓ or Enter.
7. **Not clickable yet:** databases (F4) and links inside table cells.

## Known gaps after C1 (reviewed, deferred)
1. **The integration needs comment access.** Turn on "Read comments" and "Insert comments" under the integration's Capabilities at notion.so/my-integrations. Without them, Notion answers 403. Checked live on 2026-11-21: listing comments returned 403 `restricted_resource`.
2. **Block comments are fetched only when the panel shows that block.** A pull fetches just the page's own threads, because Notion has no endpoint that lists every comment on a page. Until the panel has shown a block, its 💬 count is missing, and moving or converting it isn't refused even if it has comments on Notion. Those comments would be lost on Notion.
3. **Editing and deleting comments are documented only for API version 2026-03-11.** Under our 2025-09-03 they're untested. If Notion refuses them, they show as failed in the problem list.
4. **The comment box doesn't have the `@` menu.** To mention a page there, you type the `@[label](page:id)` token yourself.
5. **A block creation that fails right after it was sent can still post the block twice.** The same thing was fixed for comments in C1.

## Known gaps after C2 (reviewed, deferred)
1. **Users need a capability.** The integration needs "Read user information" (with or without email) at notion.so/my-integrations. Without it, a 403 shows one sticky message and user pulls stop.
2. **Avatars are initials only.** Avatar images are F7.
3. **People properties** in databases arrive with F4.
4. **"Edited by" is Notion's last value.** It doesn't change after your local edits until the next pull.
5. **No `@` menu in the comment box.** Mentions in comments are plain text for now.

## Known gaps after C3 (reviewed, deferred)
1. **One op per poll flush.** While typing, each 15 s poll writes the edit as its own queued UpdateBlock, so a push sends a few PATCHes for one block. Undo stays one step.
2. **Typing during a poll's round trip.** If you start typing in a block while a poll is in flight and that same block was edited in Notion, your push can overwrite it. The status bar warns ("this block was edited in Notion meanwhile").
3. **Same-minute edits.** On a recently edited page, a remote edit in the same minute can take up to ~60 s to show (one full pull per minute per page).
4. **Undo tail.** Ctrl+Z after a poll flush first undoes the typing since that flush, then the earlier part.
5. **Highlight** ignores remote deletions and reordering; the tint colour is untuned.
6. **Pushes wait behind a running poll** (the worker is serial).

## Known gaps after C4 (reviewed, deferred)
1. **Two-way diff.** The panel compares your text with Notion's, not against the common base.
2. **Merge needs Markdown-safe text.** Blocks with mentions Markdown can't carry (link previews, templates) get Keep mine / Keep theirs only. Underline and colour are still lost if you edit the merged text (same limit as the editor).
3. **Keep mine re-sends the whole queued body.** It can undo a remote colour change on that block.
4. **Failed ops** (not conflicts) still only have Discard.
5. **No push after resolving.** Press Ctrl+S to send the re-queued op.
6. **Title ops writing other properties** offer Keep mine / Keep theirs only.

## Known gaps after F1 (reviewed, deferred)
1. **History is in memory only.** Back/forward history is lost on restart.
2. **Switcher dates after a restart.** Visit times aren't saved, so older pages are grouped by Notion's last edit date.
3. **Find** searches block text only, not table cells, link cards or child-page titles.
4. **Find and outline jumps open collapsed toggles** and they stay open.
5. **Untested on screen:** Ctrl+[ / Ctrl+] on the Austrian layout (Alt+←/→ is the safe default), mouse buttons 4/5, the look of the outline ticks and preview.
