#!/usr/bin/env bash
# Display Widget installer for Ubuntu (GNOME, X11 or Wayland).
#
# - builds the release binary (or reuses target/release/display-widget)
# - grants your user access to /dev/i2c-* (required for DDC/CI)
# - installs the app to ~/.local/bin with a desktop entry
#
# The i2c step is the only one that needs sudo.

set -euo pipefail

# Keep user files and group membership attached to the caller, not root.
if [[ "$EUID" -eq 0 ]]; then
    if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != root ]]; then
        exec sudo -H -u "$SUDO_USER" -- bash "$0" "$@"
    fi
    echo "Run ./install.sh as your desktop user; it requests sudo when needed." >&2
    exit 1
fi

cd "$(dirname "$0")"

BIN="target/release/display-widget"

if [[ ! -x "$BIN" ]]; then
    echo "==> Building release binary (first build takes a few minutes)…"
    if command -v cargo >/dev/null 2>&1; then
        cargo build --release
    else
        "$HOME/.cargo/bin/cargo" build --release
    fi
fi

echo "==> Installing binary to $HOME/.local/bin"
mkdir -p "$HOME/.local/bin"
cp -f "$BIN" "$HOME/.local/bin/display-widget"

echo "==> Installing desktop entry + icon"
mkdir -p "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/scalable/apps"
cp -f packaging/display-widget.desktop "$HOME/.local/share/applications/"
cp -f packaging/display-widget.svg "$HOME/.local/share/icons/hicolor/scalable/apps/"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true

echo "==> DDC/CI needs access to /dev/i2c-* devices."
echo "    This adds you to the 'i2c' group and installs a udev rule so it works"
echo "    immediately (and after reboot)."
sudo -v
if ! getent group i2c >/dev/null; then
    sudo groupadd i2c
fi
sudo usermod -aG i2c "$(id -un)"
sudo tee /etc/udev/rules.d/71-display-widget-i2c.rules >/dev/null <<'RULE'
# Allow members of the 'i2c' group to talk DDC/CI to monitors
KERNEL=="i2c-[0-9]*", GROUP="i2c", MODE="0660"
RULE
sudo udevadm control --reload-rules
sudo udevadm trigger
sudo udevadm trigger --name-match="i2c-*" 2>/dev/null || true

echo
echo "==> Done."
echo "    If 'i2c' group membership was just added, log out and back in once."
echo "    The udev rule sets device permissions but does not refresh your groups."
echo
echo "    Run it:  display-widget   (or find 'Display Widget' in the app grid)"
echo "    Demo UI without hardware:  DISPLAY_WIDGET_DEMO=1 display-widget"
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo
    echo "    ~/.local/bin is not on your PATH. Add this to your shell configuration:"
    echo '    export PATH="$HOME/.local/bin:$PATH"'
    echo "    Or run directly: $HOME/.local/bin/display-widget"
fi
