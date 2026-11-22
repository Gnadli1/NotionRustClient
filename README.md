# NotionRustClient

A native, keyboard-driven Notion client for Linux, written in Rust (macOS and Windows are untested). It is local-first: everything you open is cached in SQLite, so you can read and edit offline and push your changes when you're ready.

It has two native Rust backends: a browser-session backend that uses the `token_v2` cookie against Notion's own web API (the recommended way to sign in), and the official [Notion API](https://developers.notion.com/) (version `2025-09-03`) with an integration token. Several workspaces are kept side by side and switched from the top bar.

> **Status:** personal project, works day to day on Linux. Expect rough edges. It is not affiliated with Notion Labs.

## Features

- **Fast and offline.** Pages, databases and images are cached locally. Search, browse and edit work without a connection.
- **Editing** with Markdown-style formatting (`**bold**`, `*italic*`, `` `code` ``, `[links](…)`), headings, to-dos, lists, toggles, code and equation blocks, tables, indenting, moving and duplicating blocks.
- **Explicit sync.** Edits are queued locally. `Ctrl+S` pushes them, `Ctrl+R` pulls. A conflict panel shows when someone changed the same block in Notion meanwhile (keep mine, keep theirs, or merge).
- **Near-live collaboration.** The open page refreshes every 3 s while the window is focused, and changed blocks are highlighted briefly. Comment threads refresh every 10 s.
- **Comments and an Updates inbox** (`Ctrl+Shift+M`, `Ctrl+Shift+J`).
- **Databases** as table, board, list, calendar and gallery views, with property editing.
- **Images, covers and file uploads.**
- **Browser-session sign-in (recommended).** Paste the `token_v2` cookie from a logged-in browser (a whole `Cookie` header also works); the client talks to Notion's web API itself, in Rust, discovers every workspace, including pages other people shared with you, and downloads private images, covers and files by exchanging the token for a short-lived file session. No WebView and no browser window.
- **Workspace profiles.** One profile per workspace, a separate cache and session for each, and a switcher in the top bar. Integration-token profiles use the official API and keep working.
- **Tabs, history, a `Ctrl+K` quick switcher**, page find (`Ctrl+F`), and keybindings you can change.

## Quick start

**From a release:** download the tarball from [Releases](https://github.com/Gnadli1/NotionRustClient/releases), unpack it and run `./install.sh`. It adds **Notion Client** to your app menu and optionally asks for an integration token (Enter skips it; you can sign in from the app instead).

**From source:**

```powershell
git clone https://github.com/Gnadli1/NotionRustClient.git
cd NotionRustClient
cargo run --release -p notion-egui
```

Then open the workspace picker → **Browser session (token_v2)…** and paste the `token_v2` cookie from your logged-in browser (DevTools → Application/Storage → Cookies → `https://www.notion.so`). It finds every workspace and creates a native profile for each. **Add workspace with a token…** adds an official integration-token profile instead.

When no `profiles.json` exists, `NOTION_TOKEN` still works as a `Default` profile. **[SETUP.md](SETUP.md)** walks through everything: build tools on Windows and Linux, browser sessions, tokens, sharing pages, profile files and the first sync.

> **Risk:** the browser-session mode uses Notion's private, undocumented web API, which can change or break without notice. The `token_v2` cookie is full access to your account: treat it like a password. For a stable, documented path use an integration token.

## The crates

| Crate | What it is |
|---|---|
| `notion-egui` | **The app.** The desktop client (egui/eframe). |
| `notion-core` | Notion API client, SQLite cache, sync, ops queue and conflict handling. |
| `notion-cli` | A small command-line tool: `search`, `pull`, `sync-all`, `cat`, `edit-block`, `push`, `status`. |

## Limitations

Some things aren't possible through Notion's public API:
- presence and live cursors (who is on the page right now);
- sharing and permissions;
- Notion's own inbox and notifications;
- keystroke-level live updates. Changes appear once Notion saves them, which is usually within seconds.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The tests never call the real API; they use a mock server. A single live smoke test exists and is `#[ignore]`d. See [SETUP.md](SETUP.md#running-the-tests).

## Security

Tokens are read from `profiles.json` in your config folder (or from `NOTION_TOKEN` when no profile file exists). They are never logged or written to the page cache. `profiles.json` holds them in plain text: on Linux/macOS it is written with mode `600`, and on Windows it is protected by your user account's folder permissions. The cache holds your page contents in plain text, so treat it like the pages themselves.

Browser-session mode stores the account-wide `token_v2` cookie in `profiles.json` (under `browser`). It is a full session credential, equivalent to being logged in: never share the file. Sign out in Notion or change the password to invalidate it.

## Fonts

The app embeds the Inter font (Bold, SemiBold, Regular), copyright 2016 The Inter Project Authors, under the SIL Open Font License 1.1. The licence text is in [`crates/notion-egui/assets/fonts/OFL.txt`](crates/notion-egui/assets/fonts/OFL.txt) and ships in the release tarball as `OFL-Inter.txt`.

## License

[MIT](LICENSE)
