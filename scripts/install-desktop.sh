#!/usr/bin/env bash
# Install the desktop entry, icon and a `brumm` launcher symlink for the current user.
#
# Wayland compositors never look at the icon a client sets on its window; they match the
# window's app_id against an installed `<app_id>.desktop` and load that entry's `Icon=`
# from the icon theme. So without this step a native Wayland window always shows the
# generic fallback icon.
#
# Usage: scripts/install-desktop.sh [path-to-binary]   (default: target/release/brumm)
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${1:-$repo_root/target/release/brumm}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_dir="${BRUMM_BIN_DIR:-$HOME/.local/bin}"
apps_dir="$data_home/applications"
icon_dir="$data_home/icons/hicolor/256x256/apps"

if [[ ! -x "$binary" ]]; then
    echo "install-desktop: no executable at $binary — run 'cargo build --release' first" >&2
    exit 1
fi

# Resolve to an absolute path first: both the desktop entry and the symlink must work
# regardless of the caller's working directory.
binary="$(realpath -- "$binary")"

# The install rewrites `Exec=` to the absolute binary path: the shell's PATH is the
# session's, not the login shell's, so a bare `Exec=brumm` is unlikely to resolve and
# GNOME will then neither show nor launch the entry. The repo asset stays `Exec=brumm`
# for real packaging, where the binary lives on the system PATH.
sed "s|^Exec=.*|Exec=$binary|" \
    "$repo_root/crates/brumm-app/assets/brumm.desktop" > "$apps_dir/brumm.desktop"
chmod 644 "$apps_dir/brumm.desktop"
install -Dm644 "$repo_root/crates/brumm-app/assets/icon.png" "$icon_dir/brumm.png"

mkdir -p "$bin_dir"
ln -sfn -- "$binary" "$bin_dir/brumm"

# Refresh the caches the shell reads; both tools are optional.
command -v update-desktop-database >/dev/null && update-desktop-database "$apps_dir"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache --force --ignore-theme-index "$data_home/icons/hicolor"

echo "installed: $apps_dir/brumm.desktop, $icon_dir/brumm.png, $bin_dir/brumm -> $binary"

if [[ ":$PATH:" != *":$bin_dir:"* ]]; then
    echo "note: $bin_dir is not on PATH; the symlink is only for shell use, the entry uses the absolute path" >&2
fi
