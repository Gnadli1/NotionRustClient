# Phase 2b plan: slash commands and a Notion-like look

Status: **Approved 2026-11-19.** Decisions: converting a block with children is refused; the sidebar shows every cached page, nested; QoL features a, b, c, d, e, f and g are in (no drag-and-drop); equations render as Unicode pretty-print. Your additions (shared-page compatibility, keybinds, KaTeX) are tasks S3, K1 and M1. It builds on the first GUI slice: C1 and G1–G3 in `docs/phase2-plan.md`.

## Goal
- **Block conversion:** turn any block into another type, through a `/` menu or Markdown shortcuts.
- **Look:** make the window look and feel like the Notion desktop app. That means tabs at the top, a page tree in the sidebar, a centred page column, Notion's light and dark palette, and Notion-style blocks.
- **Quality-of-life features:** whichever ones you pick from the list at the end.

We copy Notion's layout conventions, not its logo, name or branding assets. The window title stays our own.

## The API constraint behind block conversion
Notion can't change a block's type: a PATCH has to keep the same type. Conversion therefore works in one of three ways:
- **Unpushed `local-*` block** (the usual case: Enter, then `/h1`). The queued CreateBlock is rewritten in place. This costs nothing, and the id is kept.
- **Existing block without children.** A new block of the target type, with the same rich text, is created right after the old one, and the old one is deleted. The block gets a **new id** and loses its comments and backlinks. This works the same way as an indent.
- **Existing block with children.** Default: **refused**, with a message in the status bar. The alternative is copy-then-delete of the whole subtree, the same as a move.
- Converting *to* a divider drops the text. A code block keeps its text as plain text.

## Tasks

| # | Task | Files | Acceptance |
|---|---|---|---|
| S1 | Core `local_change_type(store, id, new_kind)`: rewrite a local create, or replace an existing block (create after it, then delete it); refuse when the block has children. Pure fn `markdown_shortcut(text) -> Option<(kind, rest)>` for `# `, `## `, `### `, `- `/`* `, `1. `, `[] `, `> `, `` ``` ``, `---` | `notion-core/src/{sync,markdown}.rs` | unit tests for each path, including push through wiremock (new id, old one deleted, edit after conversion lands on the new id) |
| S2 | GUI `/` menu: typing `/` in a block opens a filterable popup at the cursor (Text, H1–H3, Bullet, Numbered, To-do, Toggle, Quote, Callout, Code, Divider). ↑/↓/Enter picks, Esc closes, Backspace past `/` closes. Markdown shortcuts fire on Space at the start of a block | `notion-gui/src/*` | unit tests for filter, keys and shortcut firing; manual check |
| L1 | Theme and page layout: Notion light and dark palettes, following the system theme (`iced::system::theme_changes`); a centred page column (max ~900 px, generous margins); a large page title; Notion-like font sizes, weights and line heights; row hover highlight | `notion-gui/src/{app,theme}.rs` | builds; screenshot check by you |
| L2 | Block styling: a grey bar on quotes, a tinted rounded box with an icon for callouts, a monospace block with a background and language label for code, a thin line for dividers, Notion-style bullets (• ◦ ▪ by depth), checkboxes, ▸/▾ toggles that **collapse** their children, and columns (`column_list`) shown side by side | `notion-gui/src/{app,outline}.rs` | unit tests for toggle collapse in the outline; screenshot check |
| L3 | Sidebar: a **page tree** (child pages nested under their parents, expand/collapse), page icons (emoji from the page's `icon`), the current page highlighted, hover states, and a collapsible sidebar (Ctrl+\\). Search moves to a Notion-style "Search" entry that opens the Ctrl+K switcher as a centred overlay | `notion-gui/src/*`, maybe `notion-core/src/store.rs` (a query for the parent of each page) | unit tests for building the tree; manual check |
| L4 | Tabs: several open pages as tabs across the top. Ctrl+T opens the switcher in a new tab, Ctrl+W closes a tab, Ctrl+Tab / Ctrl+Shift+Tab cycle, middle-click closes. Breadcrumb (parent › page) above the title. Each tab keeps its own scroll position and focus | `notion-gui/src/*` | unit tests for the tab state machine; manual check |
| S3 | **Shared-page compatibility.** Render and preserve every block type the API returns. Tables (`table`/`table_row`) become a grid you can edit cell by cell. `synced_block` shows its source content as read-only, with a marker. `column_list`/`column` render side by side. `bookmark`, `embed`, `link_preview`, `file`, `pdf`, `video` and `image` become a link card that opens with `xdg-open`. `link_to_page` and `child_database` become clickable links. `child_page` opens the page in a tab. `table_of_contents` and `breadcrumb` render as themselves. Unknown types stay a grey placeholder and are never sent back modified | `notion-core/src/model.rs` (typed kinds, only where editing needs them), `notion-gui/src/*` | snapshot tests of the outline for each type against API-shaped JSON fixtures; nothing that is read-only can be edited |
| M1 | **Equations (KaTeX).** Markdown `$…$` maps to inline `equation` rich text, both ways. An `equation` block is edited as `$$…$$` source. Blocks with equations become editable (this reverses the earlier read-only default for equations; mentions stay read-only). Display uses a pure function `pretty_tex(src) -> String`: Greek letters, `^`/`_` with Unicode super- and subscripts where they exist, `\frac`, `\sqrt`, `\sum`, `\int`, `\prod`, `\lim`, relations, arrows, `\cdot`, `\infty`, `\mathbb`, `\text`. Anything it can't render falls back to the source in monospace | `notion-core/src/markdown.rs`, a new `notion-gui/src/tex.rs` | round-trip tests for `$…$`; a table of `pretty_tex` cases; fallback test |
| K1 | **Keybinds.** `~/.config/notion-client/keys.json` maps a key chord to any `/` command or app action. It is created with Notion's defaults: Ctrl+Alt+0 text, Ctrl+Alt+1/2/3 headings, Ctrl+Alt+4 to-do, Ctrl+Alt+5 bullet, Ctrl+Alt+6 numbered, Ctrl+Alt+7 toggle, Ctrl+Alt+8 code, Ctrl+Alt+E equation block, Ctrl+B/I/U/E/Shift+S for inline marks on the selection. An invalid file falls back to the defaults and shows an error in the status bar | `notion-gui/src/keys.rs`, `app.rs` | parse, merge and conflict tests; unit test that a binding fires its command |
| Q1 | **Quality-of-life features:** (a) click a link to open it with `xdg-open`; (b) edit the page title inline (`UpdatePageProperties`); (c) auto-pull every 5 minutes and on window focus, never while editing or pushing; (d) Ctrl+Z / Ctrl+Shift+Z undo and redo for local edits that haven't been pushed, by reverting the cache and the queued op; (e) a recent-pages switcher; (f) a dot on rows with queued ops, plus a conflict list in the status bar that you can click; (g) restore open tabs, sidebar state and window size from `~/.config/notion-client/session.json` | `notion-gui/src/*`, small core helpers as needed | unit tests for each; manual check |

**Order:** S1 → S2 → M1 → K1 → S3 → L1 → L2 → L3 → L4 → Q1. The editing features come first because they shape the row model, and the visual pass styles the final set of block types. They are sequential because they share `app.rs`. As before, each task is implemented by one agent, reviewed by a separate one, gets at most 2 rounds, and is committed.

`app.rs` is getting large (about 1100 lines), so L1 starts by splitting the view code into `view/`.

## Limits to expect
- **Emoji:** colour emoji depend on iced/cosmic-text font fallback. If the system has no emoji font (e.g. `noto-fonts-emoji`), page icons fall back to the generic page glyph.
- **Font:** Notion uses a system-UI sans font. We use the system default sans through iced. No fonts are bundled and no dependencies are added.
- **No new dependencies.** Opening links in the browser uses `xdg-open` through `std::process`.

## Decisions for you (defaults in bold)
1. Converting an existing block that has children: **refuse**, or copy-then-delete the whole subtree.
2. Sidebar tree contents: **all cached pages, nested by parent**, or only pages you've opened.
3. Quality-of-life features (pick any):
   - a. Click a link to open it in the browser (`xdg-open`)
   - b. Edit the page title inline
   - c. Background auto-pull every N minutes, and on window focus
   - d. Ctrl+Z undo for local edits that haven't been pushed yet
   - e. Recent pages / quick switcher ranked by last visit
   - f. Sync indicator per block (a dot on rows with queued ops) and a conflict list you can click
   - g. Remember open tabs, sidebar state and window size between runs
   - h. Drag rows to reorder them. The plan says "drag-and-drop (never)", so this would reverse that.

## Shared pages: the access limit
An internal integration only sees pages that have been shared with it: open the page or a parent page in Notion, then ••• → Connections → your integration. A page that someone else's workspace shares with you, such as a teacher's, is only reachable if someone in that workspace adds the integration. That is a limit of Notion's API, not of this app. A **public** integration with OAuth can't get around it either; it only makes adding the integration easier.

## Known gaps after S3 (reviewed, deferred)
1. **Notion-hosted file links expire.** Links to files stored in Notion use signed URLs that expire after about an hour. Pull the page again before opening an old one.
2. **Only http and https links open.** Other link types, such as `file://`, are refused for safety.
3. **Tab in a table cell leaves the cell.** It commits the edit and doesn't move to the next cell. You can't add or remove table rows or columns.
4. **Columns aren't side by side yet.** Column content shows as nested rows; L2 puts it side by side.
5. **Databases don't open.** Clicking a `child_database` shows "databases not supported yet".

## Known gaps after L1–Q1 (reviewed, deferred)
1. **Conflicts:** the only way to resolve one is to discard your change and keep Notion's version. There is no "keep mine".
2. **Undo:** undo is cleared by any push, and by any pull that brings in changes. While a pull or push is running, undo does nothing.
3. **Session:** the session is saved only when the app closes normally. If it is killed or crashes, the last session is lost.
4. **Auto-pull without a token:** the background auto-pull keeps retrying quietly, at most every 30 s, until a manual pull shows the "offline" message.
5. **Sidebar order:** pages are sorted alphabetically at every level. Notion uses each page's position on its parent page.
6. **Nested blocks:** blocks inside a quote or callout show under its box, not inside it. Columns don't stack in narrow windows.
7. **Scrolling:** every scroll redraws the whole view, because tabs remember their scroll position. Fix this if long pages stutter.
8. **Shortcut:** Ctrl+\ is also bound as Ctrl+ß, because on German/Austrian layouts it arrives as ß.
