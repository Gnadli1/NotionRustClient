# NotionRustClient

A native, keyboard-driven Notion client for Linux, written in Rust. It is local-first: everything you open is cached in SQLite, so you can read and edit offline and push your changes when you're ready.

It talks to Notion only through the official [Notion API](https://developers.notion.com/) (version `2025-09-03`) with your own internal integration token.

> **Status:** personal project, works day to day on Linux. Expect rough edges. It is not affiliated with Notion Labs.

## Features

- **Fast and offline.** Pages, databases and images are cached locally. Search, browse and edit work without a connection.
- **Editing** with Markdown-style formatting (`**bold**`, `*italic*`, `` `code` ``, `[links](…)`), headings, to-dos, lists, toggles, code and equation blocks, tables, indenting, moving and duplicating blocks.
- **Explicit sync.** Edits are queued locally. `Ctrl+S` pushes them, `Ctrl+R` pulls. A conflict panel shows when someone changed the same block in Notion meanwhile (keep mine, keep theirs, or merge).
- **Near-live collaboration.** The open page refreshes every 3 s while the window is focused, and changed blocks are highlighted briefly. Comment threads refresh every 10 s.
- **Comments and an Updates inbox** (`Ctrl+Shift+M`, `Ctrl+Shift+J`).
- **Databases** as table, board, list, calendar and gallery views, with property editing.
- **Images, covers and file uploads.**
- **Tabs, history, a `Ctrl+K` quick switcher**, page find (`Ctrl+F`), and keybindings you can change.

## Quick start

```bash
git clone https://github.com/Gnadli1/NotionRustClient.git
cd NotionRustClient
export NOTION_TOKEN="your-integration-token"   # see SETUP.md
cargo run --release -p notion-egui
```

You need a Notion integration, and pages shared with it, before anything shows up. **[SETUP.md](SETUP.md)** walks through everything: system packages, the integration and its capabilities, sharing pages, the environment variables and the first sync.

## The crates

| Crate | What it is |
|---|---|
| `notion-egui` | **The app.** The desktop client (egui/eframe). |
| `notion-core` | Notion API client, SQLite cache, sync, ops queue and conflict handling. |
| `notion-cli` | A small command-line tool: `search`, `pull`, `sync-all`, `cat`, `edit-block`, `push`, `status`. |
| `notion-gui` | The older iced prototype, kept for reference. Use `notion-egui`. |

## Limitations

Some things aren't possible through Notion's public API:
- presence and live cursors (who is on the page right now);
- sharing and permissions;
- Notion's own inbox and notifications;
- keystroke-level live updates. Changes appear once Notion saves them, which is usually within seconds.

Known gaps for each feature are listed in [`docs/phase3-plan.md`](docs/phase3-plan.md).

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The tests never call the real API; they use a mock server. A single live smoke test exists and is `#[ignore]`d. See [SETUP.md](SETUP.md#running-the-tests).

## Security

Your token is read only from the `NOTION_TOKEN` environment variable. It is never stored, logged or written to the cache. The cache (`~/.local/share/notion-client/cache.db`) holds your page contents in plain text, so treat it like the pages themselves.

## License

[MIT](LICENSE)
