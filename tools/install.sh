#!/usr/bin/env bash
# Install gitwall for the current user: binary on PATH, icons in the hicolor
# theme, and a desktop entry so it shows up in the app menu.
#
#   ./tools/install.sh            build release, then install
#   ./tools/install.sh --no-build use whatever is already in target/release
#   ./tools/install.sh --uninstall
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
DESKTOP="$APP_DIR/gitwall.desktop"

uninstall() {
  rm -f "$BIN_DIR/gitwall" "$DESKTOP"
  for size in 32x32 128x128 256x256 512x512; do
    rm -f "$ICON_ROOT/$size/apps/gitwall.png"
  done
  refresh
  echo "removed gitwall"
}

refresh() {
  command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$ICON_ROOT" 2>/dev/null || true
}

case "${1:-}" in
  --uninstall) uninstall; exit 0 ;;
  --no-build) BUILD=0 ;;
  "") BUILD=1 ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac

if [ "$BUILD" = 1 ]; then
  echo "building release…"
  (cd "$REPO" && cargo build --release -p gitwall)
fi

SRC="$REPO/target/release/gitwall"
if [ ! -x "$SRC" ]; then
  echo "no release binary at $SRC — run without --no-build" >&2
  exit 1
fi

mkdir -p "$BIN_DIR" "$APP_DIR"
install -m755 "$SRC" "$BIN_DIR/gitwall"
echo "binary   -> $BIN_DIR/gitwall"

# Icon names map to the sizes tools/make_icons.py emits.
declare -A ICONS=(
  [32x32]=32x32.png
  [128x128]=128x128.png
  [256x256]=128x128@2x.png
  [512x512]=icon.png
)
for size in "${!ICONS[@]}"; do
  src="$REPO/assets/icons/${ICONS[$size]}"
  [ -f "$src" ] || continue
  mkdir -p "$ICON_ROOT/$size/apps"
  install -m644 "$src" "$ICON_ROOT/$size/apps/gitwall.png"
done
echo "icons    -> $ICON_ROOT/*/apps/gitwall.png"

# Absolute Exec path: a desktop session's PATH does not reliably include
# ~/.local/bin, so relying on the bare name can silently fail to launch.
cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=gitwall
GenericName=Wallpaper Picker
Comment=Browse a GitHub repo of wallpapers and set one
Exec=$BIN_DIR/gitwall
Icon=gitwall
Terminal=false
Categories=Utility;
Keywords=wallpaper;background;desktop;github;rice;
StartupNotify=true
EOF
chmod 644 "$DESKTOP"
echo "launcher -> $DESKTOP"

refresh

echo
echo "Launch with 'gitwall' or from the app menu."
echo "To bind it to a key in GNOME, see the README."
