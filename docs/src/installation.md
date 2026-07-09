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

## From source

```bash
git clone https://github.com/legion-works/dormant.git
cd dormant
cargo build --release
install -Dm755 target/release/dormantd ~/.local/bin/dormantd
install -Dm755 target/release/dormantctl ~/.local/bin/dormantctl
```

Binaries land in `~/.local/bin/` — make sure this is on your `PATH`.

### Tray applet (Linux only)

`dormant-tray` is a KDE `StatusNotifierItem` applet: status glance +
pause/resume + blank/wake controls, riding the daemon's Unix socket in
the background.

```bash
install -Dm755 target/release/dormant-tray ~/.local/bin/dormant-tray
```

See [Tray autostart](#tray-autostart) below to run it on every login.

## From release

The cargo-dist pipeline publishes shell installers and tarballs for each binary on every release. Install the latest release (Linux x86_64 / aarch64):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/legion-works/dormant/releases/download/v0.1.0/dormantd-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/legion-works/dormant/releases/download/v0.1.0/dormantctl-installer.sh | sh
```

`dormant-tray-installer.sh` is also available in the same directory. Checksums are published alongside every artifact; verify with:

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

Run `dormant-tray` on every graphical session with the provided user
unit — the same mechanism as the daemon:

```bash
mkdir -p ~/.config/systemd/user
cp crates/dormant-tray/systemd/dormant-tray.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dormant-tray
```

The unit uses `ExecStart=%h/.local/bin/dormant-tray`, so systemd expands
the path from your home directory at launch — no reliance on `PATH`. It
starts after `dormant.service` and restarts on failure. A plain XDG
`.desktop` autostart does not work here: the systemd autostart generator
resolves a relative `Exec=` against a minimal boot `PATH` that excludes
`~/.local/bin`, so no unit gets generated.
