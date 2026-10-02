#!/bin/sh
# Installs SereChat for the current user: the binary, plus the launcher entry
# and icon desktops use to show it in menus, docks and task switchers.
set -eu
cd "$(dirname "$0")"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$bin" "$data/applications" "$data/icons/hicolor/256x256/apps"
cp serechat "$bin/serechat"
chmod 755 "$bin/serechat"
cp serechat.png "$data/icons/hicolor/256x256/apps/serechat.png"
# An absolute Exec works even when the bin folder is not on PATH.
sed "s|^Exec=.*|Exec=$bin/serechat|" serechat.desktop > "$data/applications/serechat.desktop"
echo "Installed SereChat to $bin/serechat; it is now in your applications menu."
