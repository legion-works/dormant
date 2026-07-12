<p align="center">
  <img src="design/web-ui/assets/logo.svg" alt="dormant" width="280">
</p>

<p align="center">
  <strong>Blank OLED screens when the room empties. Wake them the moment you return.</strong>
</p>

<p align="center">
  <a href="https://github.com/legion-works/dormant/actions/workflows/ci.yml"><img src="https://github.com/legion-works/dormant/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="MSRV 1.88">
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0">
  <img src="https://img.shields.io/badge/platform-Linux-informational" alt="Linux">
</p>

---

OLED panels burn in when they hold a static image. OS idle timers are a blunt fix — they blank while you're reading and linger after you leave. Presence sensors know the difference: they can tell an empty room from a still one.

`dormant` is a Rust daemon that reads those sensors and blanks your displays only when the room is actually empty, then wakes them the instant someone walks back in. It runs your PC monitors and your TVs, over local buses and over the network, on rules you write per display.

## Why it exists

The whole point is protecting OLEDs *without* the usual trade-offs — no black bars burned into the panel, no audio cutting out when the TV screen goes dark, no three-second wait to see your desktop again. Every blank mode makes a different bargain:

| Mode | OLED protection | Audio survives | Wake |
|---|---|---|---|
| `screen_off_audio_on` | Full | Yes | Fast |
| `power_off` | Full | No | ~1s standby |
| `brightness_zero` | Partial — pixels stay lit | Yes | Instant |

Three display paths leave the OS output alone, so audio keeps playing:

- **`ddcci` `power_off`** writes VCP `0xD6` straight to the monitor's internal controller. The panel blanks; the OS output — and its audio sink — stay up.
- **`samsung-tizen` `screen_off_audio_on`** sends `KEY_PICTURE_OFF` over WebSocket. The TV cuts its backlight; the HDMI source keeps running and the audio plays on.
- **`samsung-tizen` `brightness_zero`** drives the TV's backlight to zero over Samsung IP Control G2 on port 1516 — a near-black dim rather than a true off, but the only mode that leaves the source's audio uninterrupted on a 2024 Samsung OLED.

Every DPMS path (`kwin-dpms` included) tears the output down and takes the audio with it, which is why it's a documented fallback, not a default.

## What's in the box

Three binaries: **`dormantd`** (the daemon), **`dormantctl`** (the CLI), and **`dormant-tray`** (a KDE tray applet).

### Sensors

| Source | |
|---|---|
| MQTT — Zigbee2MQTT, ESPHome, any broker (auth supported) | Ready |
| Home Assistant WebSocket | Ready |
| USB-serial LD2410 mmWave radar | Ready |

Zones fuse multiple sensors with `any` / `all` / `quorum` / `weighted` logic. A sensor that goes quiet — broker down, USB unplugged, a stale reading — resolves as *present*, never absent. dormant will not blank a room it can't see.

### Display controllers

| Controller | |
|---|---|
| `ddcci` — DDC/CI power-off and brightness (audio-safe) | Ready |
| `samsung-tizen` — Samsung Tizen TVs over WebSocket | Ready |
| `kwin-dpms` — KDE Wayland DPMS (audio-unsafe fallback) | Ready |
| `ha-passthrough` — any Home Assistant service call | Ready |
| `command` — arbitrary blank/wake shell commands | Ready |

Each display gets an ordered controller chain with automatic fallback and bounded wake retry. A wake that fails on one controller escalates to the next. Repeated failures surface through desktop notifications, the tray, and the web dashboard.

A display referenced by no rule is **manual-only**: the daemon builds it, `dormantctl status` / the web UI / the tray show it, and it responds to hand-issued `blank` / `wake` commands — but no zone or rule ever drives it. This is the way a TV joins dormant without a keep-awake dummy zone.

### Panel-wear tracking

dormant tracks brightness-weighted on-hours per display and shows them in the web dashboard as a panel-exposure card. An advisory appears after a long stretch without a rest window. Tracking does not change blank/wake timing. Ledgers stay under the daemon's local state directory; there is no telemetry. See [Panel-wear tracking](./docs/src/oled-health.md) for the limits and the `wear.*` keys.

### Web dashboard and tray

Build with `--features web-ui` for a loopback web dashboard: a live view of the sensor → zone → rule → display pipeline, force blank/wake, pause/resume, panel-wear tracking, failure state, and a config editor. The editor can create and delete sensors, zones, displays, and rules; its Samsung pairing wizard stores the granted token in `credentials.toml`. Config writes preserve comments and pass daemon-identical validation before they reach disk.

Build with `--features render` for blanking that never touches DPMS: a fullscreen black Wayland overlay, an escalation ladder (screensaver → black → power-off on your own dwell timers), and a muted streaming screensaver driven by mpv. The screensaver applies a 2 px pixel shift every 2 minutes by default; the uniform black overlay never shifts.

The `dormant-tray` applet puts per-display status and blank/wake/pause controls in the KDE system tray.

## Quickstart

### Install from release (Linux x86_64 / aarch64)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/legion-works/dormant/releases/download/v0.1.0/dormantd-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/legion-works/dormant/releases/download/v0.1.0/dormantctl-installer.sh | sh
```

`dormant-tray-installer.sh` is also available in the same directory. Checksums are published alongside every artifact; verify with `sha256sum -c <artifact>.sha256`.

### Build from source (Linux, Rust 1.88+)

```bash
git clone https://github.com/legion-works/dormant.git
cd dormant
sudo apt install libudev-dev libwayland-dev libmpv-dev pkg-config
cargo build --release --features web-ui,render
install -Dm755 target/release/dormantd  ~/.local/bin/dormantd
install -Dm755 target/release/dormantctl ~/.local/bin/dormantctl
install -Dm755 target/release/dormant-tray ~/.local/bin/dormant-tray
```

Write `~/.config/dormant/config.toml`:

```toml
config_version = 1

[sensors.desk]
type = "mqtt"
broker_url = "mqtt://localhost:1883"
topic = "zigbee2mqtt/desk-presence"

[zones.office]
mode = "any"
members = ["desk"]

[displays.monitor]
controllers = ["ddcci"]
blank_mode = "power_off"

[rules.office]
zone = "office"
displays = ["monitor"]
grace_period = "60s"
```

Then:

```bash
dormantctl validate     # check the config
dormantd                # start the daemon
dormantctl status       # watch the pipeline
dormantctl doctor       # diagnose sensors and displays against your hardware
dormantctl doctor exercise monitor  # verify a real blank → wake control path
```

Run it as a user service:

```bash
mkdir -p ~/.config/systemd/user
cp crates/dormantd/systemd/dormant.service ~/.config/systemd/user/
systemctl --user enable --now dormant
```

## Documentation

Configuration reference, sensor and controller guides, and the `doctor` command are in [docs/](./docs/src/introduction.md). Hardware-specific findings — which DDC codes work, how Samsung standby behaves — live in [docs/research/](./docs/research/).

## Status

Running in production on the author's hardware — an AOC AGON OLED monitor and a Samsung S90D — driven by real Zigbee and mmWave presence sensors. CI covers the full workspace on Linux, macOS, and Windows. The daemon caps Tokio at two workers, calls `malloc_trim` after screensaver teardown, and sets `MALLOC_ARENA_MAX=2` in the systemd unit. The shipped watchdog restarts a wedged engine; last-known-good rollback can recover a bad boot config.

It's a young project with one maintainer, aimed at homelabs and single-operator setups; interfaces can still shift before 1.0, and the web dashboard binds to loopback with no authentication by design.

## License

MIT OR Apache-2.0, at your option.

---

<p align="center">
  <img src="design/web-ui/assets/legion-mark.svg" alt="Legion Works" width="16" valign="middle">
  &nbsp;A <strong>Legion Works</strong> fleet daemon. Many programs. One consensus.
</p>
