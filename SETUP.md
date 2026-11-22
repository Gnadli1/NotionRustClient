# Setup

This guide takes you from nothing to a synced client with several workspaces.
It takes about 10 minutes, plus the first sync.

## Quick install from a release (no Rust needed)

1. Download `notion-client-<version>-x86_64-linux.tar.gz` from the [Releases page](https://github.com/Gnadli1/NotionRustClient/releases). It runs on x86_64 Linux with glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+, Fedora 36+, Arch and similar.
2. Unpack it and run the installer:

   ```bash
   tar xzf notion-client-*-x86_64-linux.tar.gz
   cd notion-client-*-x86_64-linux
   ./install.sh
   ```

   This puts `notion-egui`, `notion-cli` and the `notion-client` launcher into `~/.local/bin`, and adds **Notion Client** to your app menu. It then optionally asks for a Notion integration token (press Enter to skip) and stores it in `~/.config/notion-client/token`, readable only by you; the launcher passes it as `NOTION_TOKEN`, so it works as the `Default` profile. Your user id is optional and goes to `~/.config/notion-client/user_id`.
3. Start **Notion Client** from the menu, then sign in with your browser session ([step 2](#2-log-in)), or use the stored integration token. Press `Ctrl+Shift+Y` for the first sync.

`./install.sh --uninstall` removes the app again and keeps your cache and token. The launcher prefers `NOTION_TOKEN` / `NOTION_USER_ID` from the environment if they're set.

The rest of this guide builds from source.

1. [Install the build tools](#1-install-the-build-tools)
2. [Log in](#2-log-in)
3. [Get access to a shared workspace](#3-get-access-to-a-shared-workspace)
4. [Add your workspaces](#4-add-your-workspaces)
5. [Build and run](#5-build-and-run)
6. [First sync](#6-first-sync)

Other sections: [Configuration](#configuration) · [The command-line tool](#the-command-line-tool) · [Running the tests](#running-the-tests) · [Troubleshooting](#troubleshooting)

---

## 1. Install the build tools

**Rust.** Install the current stable toolchain with [rustup](https://rustup.rs).
eframe 0.37 needs Rust **1.95 or newer**; `rustup update stable` upgrades an
existing install.

**Windows.** Install [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
and select **Desktop development with C++** (MSVC and the Windows SDK). Rust
uses `link.exe` from it. Restart the terminal afterwards; no other libraries
are needed. SQLite is compiled in and TLS uses rustls, so there is no SQLite
or OpenSSL dependency.

```powershell
rustup default stable
```

**Linux.** The window and graphics code needs a C compiler, OpenGL and the
X11/Wayland libraries.

| Distro | Command |
|---|---|
| Arch, CachyOS, Manjaro | `sudo pacman -S --needed base-devel libxkbcommon wayland libglvnd libx11 libxcursor libxrandr libxi` |
| Debian, Ubuntu | `sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libgl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config libxkbcommon-devel wayland-devel mesa-libGL-devel libX11-devel libXcursor-devel libXrandr-devel libXi-devel` |

**macOS (untested).** `xcode-select --install`.

## 2. Log in

There are two native backends. **Browser session** is the recommended way to
sign in: it sees every workspace your account can see, including pages other
people shared with you. **Integration tokens** use the public, documented API.

> **Risk.** The browser session uses Notion's private, undocumented web API
> with your account cookie, so it can break whenever Notion changes it. The
> `token_v2` cookie is full access to your account: treat it like a password.
> `profiles.json` stores it in plain text (mode `600` on Linux/macOS).

### Browser session (recommended)

This is the workaround that other Notion clients use: the app authenticates
with the same cookie as Notion's web app and calls Notion's own API in Rust.
No WebView, no browser window, no per-workspace key.

1. Open <https://www.notion.so> in your browser and make sure you are logged in.
2. Open DevTools (`F12`) → **Application** (Chrome/Edge) or **Storage**
   (Firefox) → **Cookies** → `https://www.notion.so`.
3. Copy the value of the cookie named **`token_v2`**.
4. In the app: workspace picker → **Browser session (token_v2)…** → paste the
   value (a whole `Cookie` header also works) → **Connect**.
5. The app asks Notion for every workspace the account can see and creates one
   native profile per workspace. Switch with the picker; each has its own
   cache and session.
6. Private images, covers and files work with the same token: the app
   exchanges it for a short-lived Notion file token when a picture is first
   downloaded. No second cookie is needed.

The cookie expires when you log out, change your password, or revoke the
session. If a pull starts returning `401`, repeat these steps with a fresh
cookie.

### Alternative: an integration token

You can add a profile with an official token instead.

**Personal access token.** Create one at
<https://www.notion.so/developers/tokens>: choose the workspace, enable the
**Notion API** capability, and copy the `ntn_…` value. It acts as you and uses
your existing page permissions, so no page-by-page sharing is needed. It only
reaches the selected workspace, and **guests/restricted members cannot create
one** (only full members; on Free/Business the workspace policy may limit it).

**Internal-connection token.** The workspace owner creates an **Internal**
connection at <https://www.notion.so/profile/integrations>, enables the same
capabilities, then shares each top-level page with it: **••• → Connections →**
the connection **→ Confirm**. This is the least-privilege route and works for
guests.

## 3. Get access to a shared workspace

This is the part that surprises people: **a page link alone is not access** for
any third-party API client. Notion's own app uses your logged-in account
session; the public API needs an authorization for that workspace.

Try these in order:

1. **Use the browser session.** Paste `token_v2` as described in step 2. The
   app then talks to Notion's own web API with your account: every workspace
   and page that the official app can see, including guest pages and pages
   shared by other people, appears as a native profile. No key from the owner
   is needed. This is the path that solves the shared-workspace problem.
2. **You are a full member of that workspace.** Create a personal access token
   for it. It needs no page-by-page sharing and no key from anyone else.
3. **The workspace owner creates an internal-connection token** and shares the
   page with it. This works for guests and is the least-privilege route.
4. **Own/admin the workspace:** a personal access token or internal connection
   both work.

A page link with no account access is still not readable by any API client;
the browser session only sees what your Notion account can see.

## 4. Add your workspaces

**Browser session** creates one profile per workspace automatically after you
paste `token_v2`; the cookie itself is stored once under `browser`, not copied
into every profile. It needs nothing else.

To add a public-API profile with a token instead, use **＋ Add workspace with a
token…** in the top bar and give it:

- a **name** (for example `Personal` or `Acme team`),
- the **token** from step 2,
- optionally a **user id** (see below).

Switch with the same picker. Each profile gets its own cache and session, so
tabs and queued edits do not mix.

**User id.** Needed for the Updates inbox when the token is a bot
(internal-connection) token. A personal access token knows its owner, so you
can leave this empty. To find a user id, with an internal token that can read
users:

```bash
curl -s https://api.notion.com/v1/users \
  -H "Authorization: Bearer $NOTION_TOKEN" \
  -H "Notion-Version: 2025-09-03" \
  | grep -o '"id":"[^"]*","name":"[^"]*"'
```

**Or edit the file directly.** `profiles.json` is created on the first save:

```json
{
  "active": "Acme team",
  "browser": { "token_v2": "the browser cookie", "user_id": "…" },
  "profiles": [
    { "name": "Acme team", "kind": "browser", "space_id": "…" },
    { "name": "Personal", "token": "ntn_personal", "user_id": null }
  ]
}
```

`kind` defaults to `official`; a `browser` profile needs `space_id` and uses
the account-wide `browser.token_v2`. Official profiles need `name` and `token`.
`cache_file` and `session_file` are optional overrides; by default each profile
uses `cache-<slug>.db` and `session-egui-<slug>.json`.

**Old single-token setup.** With no `profiles.json`, `NOTION_TOKEN` (and
`NOTION_USER_ID`) still works as a `Default` profile on the shared `cache.db`
and `session-egui.json`. The app never rewrites that mode on its own.

## 5. Build and run

```powershell
git clone https://github.com/Gnadli1/NotionRustClient.git
cd NotionRustClient
cargo build --release -p notion-egui
.\target\release\notion-egui.exe
```

On Linux/macOS use `cargo run --release -p notion-egui`; the binary is at
`target/release/notion-egui`. The first build takes a few minutes.

To put it on the Windows Start menu, copy `notion-egui.exe` to
`%LOCALAPPDATA%\Programs\notion-egui\` and create a shortcut there; the app
finds its config and cache through the environment, not the working directory.

The app takes one optional argument, the path of a cache file:
`notion-egui C:\path\to\cache.db`. Custom profiles still use their own cache
files when you switch; the argument just picks the first one.

## 6. First sync

1. Select a workspace with a token in the top bar.
2. Press **`Ctrl+Shift+Y`** (sync all). It pulls every page and database the
   token can see. The status bar shows `syncing 12/80…`. The first run can
   take a minute or two, because Notion allows about 3 requests per second.
3. Press **`Ctrl+K`** to search and open a page, or pick one in the sidebar.
4. Edit. Changes are queued locally. Press **`Ctrl+S`** to push them to Notion.

Switch workspaces whenever no push is running. The app saves the current
session, stops its worker, opens the other workspace's cache, and starts a new
worker with that workspace's token.

The essential keys:

| Key | Action |
|---|---|
| `Ctrl+K` | quick switcher / search |
| `Ctrl+S` / `Ctrl+R` | push / pull the open page |
| `Ctrl+Shift+Y` | sync everything |
| `Ctrl+Z` / `Ctrl+Shift+Z` | undo / redo |
| `Ctrl+T` / `Ctrl+W` / `Ctrl+Tab` | new tab / close tab / next tab |
| `Ctrl+F` | find on page |
| `Ctrl+/` | turn the block into another type |
| `Ctrl+Shift+0`…`8` | paragraph, headings 1–3, to-do, bullet, number, toggle, code |
| `Ctrl+Shift+M` | comments |
| `Ctrl+Shift+J` / `Ctrl+Shift+A` | Updates inbox / mark all read |
| `Ctrl+N` | new page |
| `Ctrl+Shift+F` | upload a file |
| `Ctrl+\` | show/hide the sidebar |

Every key can be changed, see below.

## Configuration

| What | Windows | Linux | macOS |
|---|---|---|---|
| Profiles (tokens) | `%APPDATA%\notion-client\profiles.json` | `${XDG_CONFIG_HOME:-~/.config}/notion-client/profiles.json` | `~/Library/Application Support/notion-client/profiles.json` |
| Keybindings | `%APPDATA%\notion-client\keys.json` | `${XDG_CONFIG_HOME:-~/.config}/notion-client/keys.json` | `~/Library/Application Support/notion-client/keys.json` |
| Cache | `%LOCALAPPDATA%\notion-client\` | `${XDG_DATA_HOME:-~/.local/share}/notion-client/` | `~/Library/Application Support/notion-client/` |

**Keybindings.** `keys.json` is written with the defaults on first start, and
existing files are never overwritten. It maps a chord to a command name, for
example:

```json
{ "ctrl+shift+1": "heading_1", "ctrl+k": "search", "ctrl+p": "search" }
```

Map a chord to `"none"` to unbind it. Mistakes are reported in the status bar,
and the defaults stay in effect for those entries.

**Live updates.** While the window is focused, the open page is checked every
3 s. Without focus it's every 15 s, and other tabs are checked every 60 s.
Comment threads in the open comment panel refresh every 10 s. Nothing needs to
be configured.

**Resetting.** Delete a profile's cache file to start that workspace over.
Queued edits that weren't pushed are lost with it. Delete `profiles.json` to
forget all workspaces (and their tokens).

## The command-line tool

`notion-cli` uses the same cache as the app, and `NOTION_TOKEN`:

```bash
cargo run --release -p notion-cli -- sync-all      # pull everything
cargo run --release -p notion-cli -- search "meeting"
cargo run --release -p notion-cli -- cat <page-id> # print a cached page
cargo run --release -p notion-cli -- status        # queued / conflicting / failed edits
cargo run --release -p notion-cli -- push
```

`--db <path>` picks a different cache file, for example a profile's
`cache-<slug>.db`.

To use a browser-session profile from the CLI, set `NOTION_TOKEN_V2` and
`NOTION_SPACE_ID` (from `profiles.json`) and pass `--browser`:

```powershell
$env:NOTION_TOKEN_V2 = "the token_v2 cookie"
$env:NOTION_SPACE_ID  = "the browser profile's space_id"
cargo run --release -p notion-cli -- --browser --db "$env:LOCALAPPDATA\notion-client\cache-<slug>.db" sync-all
```

## Running the tests

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The tests use a local mock server and never contact Notion. There is one
read-only live smoke test, which is skipped unless you ask for it:

```bash
NOTION_TEST_PAGE_ID=<a page id> cargo test -p notion-core --test live_smoke -- --ignored
```

It needs `NOTION_TOKEN` set, and the page must be visible to that token.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Status bar says `no token for this workspace` | The active profile has an empty token. Edit it in the top-bar picker. |
| Browser session finds no workspaces | The `token_v2` cookie is stale or copied from the wrong site. Log in at `https://www.notion.so` again and copy it from that exact domain. |
| Browser session fails with `401` / `403` | The cookie expired or was signed out. Open the browser session dialog and paste a fresh `token_v2`. |
| A page pulls but looks incomplete (databases, files) | The private backend is best-effort; Notion changes it without notice. Use an integration-token profile for anything that must stay stable. |
| Creating tables or some database properties fails | The browser backend does not yet implement every private transaction type; the public API profile remains the full-featured path for those operations. |
| `401 unauthorized` | The token is wrong, revoked or expired. Copy/create a new token and edit the profile. |
| A page or database is missing, or `404 object_not_found` | An internal connection only sees pages shared with it. A personal access token sees what you see. See [step 3](#3-get-access-to-a-shared-workspace). |
| I only have a page link | That is not API access. The workspace owner must give you full membership (then use a personal token) or an internal-connection token for that page. |
| One message about users, then no names | Personal access tokens cannot list all workspace users. Names may show as ids; this is expected. For an internal connection, enable "Read user information". |
| Comments panel stays empty / `403` on comments | Enable "Read comments" (and "Insert comments" to write) on an internal connection. |
| The Updates inbox shows nothing | Set the profile's user id, or use a personal access token (it knows its owner), or enable "Read user information". |
| Switching says to wait | A push is in flight. Let it finish; the switch waits for an in-progress push only at exit. |
| Build fails with `link.exe not found` | Install the C++ Build Tools and Windows SDK, then restart the terminal. |
| Build says the Rust version is too old | `rustup update stable` (eframe 0.37 needs 1.95+). |
| Build fails in a `wayland`, `xkbcommon` or `GL` crate | On Linux, install the system libraries from [step 1](#1-install-the-build-tools). |
| Odd keyboard behaviour | Run with `NOTION_EGUI_KEYLOG=1` to print raw key events to the terminal. |




