# Display Widget

Monitor control for Ubuntu GNOME, written in Rust with egui. It covers the same ground as Acer Display Widget and Omarchy's displays plugin, but runs on a stock Ubuntu session. One binary, no daemon, and no ddcutil dependency.

![Image, Layout, and Profiles tabs](packaging/display-widget.svg)

## Why this exists

Vendor monitor utilities are usually Windows-only, and GNOME's display settings stop at layout. This app handles both the monitor's image controls and the desktop layout in one window.

## What it does

### Image (DDC/CI)

Talks to the monitor directly over `/dev/i2c-*`:

- Brightness, contrast, audio volume, and sharpness sliders
- Color preset, input source, and gamma. Options come from the monitor's own capabilities string instead of a hardcoded list
- Power mode (on, standby, off)
- An advanced panel with every VCP feature the monitor reports, including read and set controls
- Laptop panels are handled through GNOME's power daemon, since eDP panels don't speak DDC/CI

### Layout (GNOME)

Uses mutter's `org.gnome.Mutter.DisplayConfig` DBus API, the same one GNOME Settings uses. Changes are real and stay consistent with the rest of the desktop:

- Resolution, refresh rate, scale, and rotation
- A mini-map for arrangement. Drag tiles, snap to edges, choose the primary monitor, set positions in pixels
- Mirror two monitors, or turn a connected but powered-off monitor on and off
- Safe apply. Configurations are verified first, then applied. "Apply (revert in 15s)" shows a keep/revert banner like GNOME Settings
- X11 and Wayland

### Profiles

Save per-monitor image settings along with mode, scale, rotation, and position. Profiles are keyed by make, model, and serial, so they follow the monitor across cables and docks. Apply with one click or automatically at startup.

### Monitor modes

One-click buttons for the picture presets stored in the display itself, the ones behind the monitor's Mode button. Switching goes over the vendor DDC control. Each mode keeps its own brightness, so the app re-reads image values after a switch. Saving a profile captures the current mode; applying a profile switches the mode first, then restores the saved values.

Keyboard: `1`, `2`, and `3` switch tabs.

## Requirements

- Ubuntu with GNOME on X11 or Wayland
- Rust 1.85 or newer (the crate uses edition 2024)
- A monitor with DDC/CI support. Most desktop monitors have it; internal laptop panels usually don't

## Build and install

```sh
git clone https://github.com/sarvkk/AcerDisplay.git
cd AcerDisplay
cargo build --release
./install.sh
```

`install.sh` installs the binary to `~/.local/bin` and a desktop entry to `~/.local/share/applications`. It requests sudo only for device permissions: it adds you to the `i2c` group and installs a udev rule at `/etc/udev/rules.d/71-display-widget-i2c.rules` that gives the group read/write access to `/dev/i2c-*`. Run it as your normal user. If you invoke it with sudo, it switches back to the invoking user before installing. Log out and back in if DDC access still fails.

Run it without installing:

```sh
cargo run --release
```

Try the interface without hardware or permissions:

```sh
DISPLAY_WIDGET_DEMO=1 cargo run --release
```

## CLI diagnostics

```sh
display-widget --probe
```

Prints i2c permission status, every DDC monitor it finds with its EDID identity, and the current GNOME/mutter display state.

## Troubleshooting

- `display-widget: command not found`: rerun `./install.sh` without sudo. If `~/.local/bin` isn't on your PATH, add `export PATH="$HOME/.local/bin:$PATH"` to your shell startup file and open a new terminal. Running `~/.local/bin/display-widget` directly also works.
- No DDC monitors in `--probe`: check i2c permissions first. If they look correct, compare with `ddcutil detect`. On hybrid graphics systems the external monitor can hang off the discrete GPU. Recent proprietary NVIDIA drivers expose the i2c buses, but support varies by laptop.
- Monitor asleep or off: DDC is unreachable in deep standby. Wake the panel with its power button.
- Sliders feel slow: DDC/CI needs about 50 ms per command. Sliders commit roughly 250 ms after you stop moving or when you release the mouse. Worker threads keep the window responsive in the meantime.
- Fractional scaling: mutter on X11 only offers a global scale with a limited set of values. Fractional scaling works fully on Wayland.

## Architecture

```
src/
  main.rs        entry point and --probe diagnostics
  ddc.rs         i2c/DDC-CI enumeration, per-monitor worker threads, capabilities
  mutter.rs      org.gnome.Mutter.DisplayConfig (typed DBus reply structs) and gsd brightness
  layout.rs      Layout tab: tiles, mini-map, snapping, mirroring, apply/revert flow
  ui.rs          app shell, sidebar, Image tab, Profiles tab
  profiles.rs    profile store (JSON at ~/.config/display-widget/profiles.json)
```

Dependencies worth knowing: `ddc-i2c`, `ddc`, and `ddc-hi` for DDC/CI over i2c-dev (no libudev needed), `mccs` and `mccs-caps` for VCP capability parsing, `zbus` for DBus, and `eframe`/`egui` for the GUI.

## License

MIT. See [LICENSE](LICENSE).

---

Maintained by [sarvkk](https://github.com/sarvkk). Bug reports and patches are welcome through [issues](https://github.com/sarvkk/AcerDisplay/issues).
