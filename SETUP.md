# Setup

This guide takes you from nothing to a synced client. It takes about 10 minutes.

## Quick install from a release (no Rust needed)

1. Download `notion-client-<version>-x86_64-linux.tar.gz` from the [Releases page](https://github.com/Gnadli1/NotionRustClient/releases). It runs on x86_64 Linux with glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+, Fedora 36+, Arch and similar.
2. Unpack it and run the installer:

   ```bash
   tar xzf notion-client-*-x86_64-linux.tar.gz
   cd notion-client-*-x86_64-linux
   ./install.sh
   ```

   This puts `notion-egui`, `notion-cli` and the `notion-client` launcher into `~/.local/bin`, and adds **Notion Client** to your app menu. It then asks for your integration token, which you create in [step 2](#2-create-a-notion-integration), and stores it in `~/.config/notion-client/token`, readable only by you. Your user id is optional and goes to `~/.config/notion-client/user_id`.
3. Share pages with the integration ([step 3](#3-share-pages-with-the-integration)), start **Notion Client** from the menu, and press `Ctrl+Shift+Y` for the first sync.

`./install.sh --uninstall` removes the app again and keeps your cache and token. The launcher prefers `NOTION_TOKEN` / `NOTION_USER_ID` from the environment if they're set.

The rest of this guide builds from source.

1. [Install the build tools](#1-install-the-build-tools)
2. [Create a Notion integration](#2-create-a-notion-integration)
3. [Share pages with the integration](#3-share-pages-with-the-integration)
4. [Set the environment variables](#4-set-the-environment-variables)
5. [Build and run](#5-build-and-run)
6. [First sync](#6-first-sync)

Other sections: [Configuration](#configuration) · [The command-line tool](#the-command-line-tool) · [Running the tests](#running-the-tests) · [Troubleshooting](#troubleshooting)

---

## 1. Install the build tools

**Rust.** Install the current stable toolchain with [rustup](https://rustup.rs):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

If you already have Rust, update it with `rustup update stable`. Old toolchains can fail to build egui.

**System libraries.** The window and graphics code needs a C compiler, OpenGL and the X11/Wayland libraries. SQLite is compiled in and TLS uses rustls, so you need neither SQLite nor OpenSSL.

| Distro | Command |
|---|---|
| Arch, CachyOS, Manjaro | `sudo pacman -S --needed base-devel libxkbcommon wayland libglvnd libx11 libxcursor libxrandr libxi` |
| Debian, Ubuntu | `sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libgl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config libxkbcommon-devel wayland-devel mesa-libGL-devel libX11-devel libXcursor-devel libXrandr-devel libXi-devel` |

## 2. Create a Notion integration

The client signs in as an **internal integration** that you create for your own workspace.

1. Open <https://www.notion.so/profile/integrations> and click **New integration**.
2. Give it a name (e.g. "Rust client"), pick your workspace, and choose type **Internal**.
3. Under **Capabilities**, enable:

   | Capability | Needed for |
   |---|---|
   | Read content | everything |
   | Update content | editing and pushing |
   | Insert content | new blocks, pages, rows and uploads |
   | Read comments | the comment panel, the Updates inbox |
   | Insert comments | writing and replying to comments |
   | Read user information (without email is fine) | names on comments and in people properties, "edited by" |

4. Save, then copy the **Internal Integration Secret**. It starts with `ntn_` or `secret_`. This is your `NOTION_TOKEN`.

> The token gives access to everything you share with the integration. Don't commit it, paste it into issues, or put it in shell history you sync anywhere. If it leaks, open the integration page and click **Refresh** to make a new one.

## 3. Share pages with the integration

An integration only sees pages that are explicitly shared with it.

1. In Notion, open a top-level page (or a teamspace page) you want in the client.
2. Click **•••** (top right) → **Connections** → find your integration → **Confirm**.
3. Repeat for each top-level page. Sub-pages and databases inside a shared page are included automatically.

## 4. Set the environment variables

| Variable | Required | What it does |
|---|---|---|
| `NOTION_TOKEN` | **yes** | The integration secret from step 2. Without it the app starts in offline mode, showing whatever is cached. |
| `NOTION_USER_ID` | recommended | Your own Notion user id. The Updates inbox uses it to tell your edits and comments apart from other people's. |
| `XDG_DATA_HOME` / `XDG_CONFIG_HOME` | no | These move the cache and the config folder away from the defaults (see [Configuration](#configuration)). |

**Finding your user id.** List the workspace's users with the token. This needs the "Read user information" capability:

```bash
curl -s https://api.notion.com/v1/users -H "Authorization: Bearer $NOTION_TOKEN" -H "Notion-Version: 2025-09-03" | grep -o '"id":"[^"]*","name":"[^"]*"'
```

Pick the `id` next to your name.

**Keeping the token out of your history.** Put the token in a file only you can read and load it from there, instead of typing it into the shell:

```bash
mkdir -p ~/.config/notion-client && chmod 700 ~/.config/notion-client
printf '%s' 'ntn_your_token_here' > ~/.config/notion-client/token && chmod 600 ~/.config/notion-client/token
```

Then add this to `~/.bashrc` or `~/.zshrc`, or into a launcher script:

```bash
export NOTION_TOKEN="$(cat ~/.config/notion-client/token)"
export NOTION_USER_ID="your-user-id"
```

## 5. Build and run

```bash
git clone https://github.com/Gnadli1/NotionRustClient.git
cd NotionRustClient
cargo run --release -p notion-egui
```

The first build takes a few minutes. After that, the binary is at `target/release/notion-egui`. You can copy it somewhere on your `PATH`:

```bash
install -Dm755 target/release/notion-egui ~/.local/bin/notion-egui
```

The app takes one optional argument, the path of a cache file: `notion-egui /path/to/cache.db`.

## 6. First sync

1. Press **`Ctrl+Shift+Y`** (sync all). It pulls every page and database shared with the integration. The status bar shows `syncing 12/80…`. The first run can take a minute or two, because Notion allows about 3 requests per second.
2. Press **`Ctrl+K`** to search and open a page, or pick one in the sidebar.
3. Edit. Changes are queued locally. Press **`Ctrl+S`** to push them to Notion.

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

| What | Where |
|---|---|
| Cache (pages, queued edits, images) | `$XDG_DATA_HOME/notion-client/cache.db`, by default `~/.local/share/notion-client/` |
| Keybindings | `$XDG_CONFIG_HOME/notion-client/keys.json`, by default `~/.config/notion-client/` |

**Keybindings.** `keys.json` is written with the defaults on first start, and existing files are never overwritten. It maps a chord to a command name, for example:

```json
{ "ctrl+shift+1": "heading_1", "ctrl+k": "search", "ctrl+p": "search" }
```

Map a chord to `"none"` to unbind it. Mistakes are reported in the status bar, and the defaults stay in effect for those entries.

**Live updates.** While the window is focused, the open page is checked every 3 s. Without focus it's every 15 s, and other tabs are checked every 60 s. Comment threads in the open comment panel refresh every 10 s. Nothing needs to be configured.

**Resetting.** Delete `cache.db` to start over. Queued edits that weren't pushed are lost with it.

## The command-line tool

`notion-cli` uses the same cache as the app:

```bash
cargo run --release -p notion-cli -- sync-all      # pull everything
cargo run --release -p notion-cli -- search "meeting"
cargo run --release -p notion-cli -- cat <page-id> # print a cached page
cargo run --release -p notion-cli -- status        # queued / conflicting / failed edits
cargo run --release -p notion-cli -- push
```

`--db <path>` picks a different cache file.

## Running the tests

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The tests use a local mock server and never contact Notion. There is one read-only live smoke test, which is skipped unless you ask for it:

```bash
NOTION_TEST_PAGE_ID=<a page id> cargo test -p notion-core --test live_smoke -- --ignored
```

It needs `NOTION_TOKEN` set, and the page must be shared with the integration.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Status bar says `offline: NOTION_TOKEN not set` | Export `NOTION_TOKEN` in the shell (or launcher) that starts the app. |
| `401 unauthorized` | The token is wrong or was refreshed. Copy it again from the integration page. |
| A page or database is missing, or you see `404 object_not_found` | It isn't shared with the integration. See [step 3](#3-share-pages-with-the-integration). |
| One message about users, then no names | Enable "Read user information" on the integration. |
| Comments panel stays empty / `403` on comments | Enable "Read comments" (and "Insert comments" to write). |
| The Updates inbox shows nothing | Set `NOTION_USER_ID`, or make sure the integration has "Read user information". |
| Sync is slow | Notion allows about 3 requests per second. A large first sync takes a while, and later syncs skip pages that haven't changed. |
| Build fails in a `wayland`, `xkbcommon` or `GL` crate | Install the system libraries from [step 1](#1-install-the-build-tools). |
| Odd keyboard behaviour | Run with `NOTION_EGUI_KEYLOG=1` to print raw key events to the terminal. |
