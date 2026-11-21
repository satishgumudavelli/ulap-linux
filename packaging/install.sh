#!/bin/sh
# Installs Ulap for the current user from an unpacked release tarball.
set -e
d=$(cd "$(dirname "$0")" && pwd)
mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/512x512/apps
install -m 755 "$d/ulap-linux" "$d/ulap-account" ~/.local/bin/
install -m 644 "$d/ulap.png" ~/.local/share/icons/hicolor/512x512/apps/ulap.png
sed "s|^Exec=.*|Exec=$HOME/.local/bin/ulap-linux|" "$d/ulap.desktop" > ~/.local/share/applications/ulap.desktop
echo "Installed. Launch Ulap from your app menu."
