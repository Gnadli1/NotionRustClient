#!/bin/sh
# Installs Notion Client for the current user (no root needed).
#   ./install.sh              install into ~/.local
#   ./install.sh --uninstall  remove it again (your cache and token stay)
# PREFIX=/some/dir ./install.sh installs the programs into $PREFIX/bin.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor/scalable/apps"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}/notion-client"

refresh_menu() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" >/dev/null 2>&1 || true
    command -v kbuildsycoca6 >/dev/null 2>&1 && kbuildsycoca6 >/dev/null 2>&1 || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "$DATA/icons/hicolor" >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN/notion-egui" "$BIN/notion-cli" "$BIN/notion-client" \
          "$APPS/notion-egui.desktop" "$ICONS/notion-egui.svg"
    refresh_menu
    echo "Removed. Your cache (${DATA}/notion-client) and settings ($CONF) were kept."
    exit 0
fi

install -Dm755 "$HERE/notion-egui" "$BIN/notion-egui"
install -Dm755 "$HERE/notion-cli" "$BIN/notion-cli"
install -Dm755 "$HERE/notion-client" "$BIN/notion-client"
install -Dm644 "$HERE/notion-egui.svg" "$ICONS/notion-egui.svg"
mkdir -p "$APPS"
sed "s|^Exec=.*|Exec=$BIN/notion-client|" "$HERE/notion-egui.desktop" > "$APPS/notion-egui.desktop"
chmod 644 "$APPS/notion-egui.desktop"
refresh_menu
echo "Installed into $BIN and added \"Notion Client\" to the app menu."

# The token: asked once if there is none yet, stored readable only by you.
if [ ! -s "$CONF/token" ] && [ -z "${NOTION_TOKEN:-}" ] && [ -t 0 ]; then
    echo
    echo "Paste your Notion integration secret (see SETUP.md), or press Enter to skip:"
    stty -echo 2>/dev/null || true
    read -r token || token=""
    stty echo 2>/dev/null || true
    echo
    if [ -n "$token" ]; then
        mkdir -p "$CONF" && chmod 700 "$CONF"
        (umask 077 && printf '%s' "$token" > "$CONF/token")
        echo "Saved to $CONF/token."
        printf "Your Notion user id (optional, for the Updates inbox; Enter to skip): "
        read -r uid || uid=""
        [ -n "$uid" ] && (umask 077 && printf '%s' "$uid" > "$CONF/user_id")
    fi
fi
[ -s "$CONF/token" ] || [ -n "${NOTION_TOKEN:-}" ] || \
    echo "No token yet: put it in $CONF/token (chmod 600) or export NOTION_TOKEN."

case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "Note: $BIN is not on your PATH; the app menu entry works anyway." ;;
esac
