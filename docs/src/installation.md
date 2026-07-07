# Installation

## Prerequisites

- Linux (x86_64 or aarch64) with a desktop environment (X11 or Wayland)
- Rust 1.88+ (MSRV) if installing from source
- `libudev` for USB-serial sensor support: `sudo apt install libudev-dev pkg-config`
- If `pkg-config` cannot find `libudev`, set `PKG_CONFIG_PATH=/usr/lib/pkgconfig`

### Optional: render backend

The software render backend (`render_black`, `render_screensaver` ladder stages)
is off by default. To enable it:

- **Build:** add `--features render` — `cargo build --release --features render`
- **Deps (Linux):** `sudo apt install libwayland-dev` (Wayland client protocol
  headers)
- Without the feature, configs using render stages are rejected at startup with
  `E_RENDER_UNAVAILABLE`; the daemon, CLI, and non-render sensors/displays still
  build and run normally.

## From source (current, pre-release)

```bash
git clone https://github.com/icetea/dormant.git
cd dormant
cargo build --release
install -Dm755 target/release/dormantd ~/.local/bin/dormantd
install -Dm755 target/release/dormantctl ~/.local/bin/dormantctl
```

Binaries land in `~/.local/bin/` — make sure this is on your `PATH`.

### Tray applet (M3, Linux only)

`dormant-tray` is a KDE `StatusNotifierItem` applet: status glance +
pause/resume + blank/wake controls, riding the daemon's Unix socket in
the background.

```bash
install -Dm755 target/release/dormant-tray ~/.local/bin/dormant-tray
```

The `Exec=` line in `dormant-tray.desktop` resolves `dormant-tray` via
`$PATH` (no `%h` expansion in `.desktop` Exec keys) — make sure
`~/.local/bin/` is on `PATH` for the user session, or copy the binary
into a directory that already is.

## From release (planned)

Once M1 ships, releases will include a shell installer:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/icetea/dormant/releases/download/v0.1.0/dormantd-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/icetea/dormant/releases/download/v0.1.0/dormantctl-installer.sh | sh
```

Checksums are provided for every release artifact. Verify before running:

```bash
sha256sum -c dormantd-x86_64-unknown-linux-gnu.tar.xz.sha256
sha256sum -c dormantctl-x86_64-unknown-linux-gnu.tar.xz.sha256
```

## Systemd user unit

dormant runs as a user service — it does not need root. Install the provided unit:

```bash
mkdir -p ~/.config/systemd/user
cp crates/dormantd/systemd/dormant.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dormant
```

Check status:

```bash
systemctl --user status dormant
journalctl --user -u dormant -f
```

The unit is configured to restart on failure and reload (SIGHUP) on `systemctl --user reload dormant`. To stop:

```bash
systemctl --user stop dormant
```

## Configuration file location

dormant reads config from these paths, first match wins:

1. `--config` CLI flag
2. `$DORMANT_CONFIG` environment variable
3. `$XDG_CONFIG_HOME/dormant/config.toml` (usually `~/.config/dormant/config.toml`)

Credentials (HA tokens, Samsung TV tokens) go in a separate file with restricted permissions:

```
$XDG_CONFIG_HOME/dormant/credentials.toml
```

Set permissions to `600` — dormant will refuse to load a credentials file readable by others:

```bash
chmod 600 ~/.config/dormant/credentials.toml
```

## Tray autostart

To launch `dormant-tray` automatically on every graphical session, drop
the bundled `.desktop` file into `~/.config/autostart/`:

```bash
cp crates/dormant-tray/assets/dormant-tray.desktop ~/.config/autostart/
```

It runs as a normal desktop application (not a D-Bus service) — the
session manager picks it up after login. The `.desktop` file's `Exec=`
resolves `dormant-tray` from `PATH`, so `~/.local/bin/` must be on
`PATH` for the user session.
