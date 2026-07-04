# Installation

## Prerequisites

- Linux (x86_64 or aarch64) with a desktop environment (X11 or Wayland)
- Rust 1.88+ (MSRV) if installing from source
- `libudev` for USB-serial sensor support: `sudo apt install libudev-dev pkg-config`
- If `pkg-config` cannot find `libudev`, set `PKG_CONFIG_PATH=/usr/lib/pkgconfig`

## From source (current, pre-release)

```bash
git clone https://github.com/icetea/dormant.git
cd dormant
cargo build --release
install -Dm755 target/release/dormantd ~/.local/bin/dormantd
install -Dm755 target/release/dormantctl ~/.local/bin/dormantctl
```

Binaries land in `~/.local/bin/` — make sure this is on your `PATH`.

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
