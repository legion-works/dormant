# dormant

Blanks OLED PC monitors and TVs when presence sensors detect an empty room, and wakes them on return. Rust daemon + CLI.

Multi-sensor fusion (any/all/quorum/weighted zones), MQTT, Home Assistant WebSocket, and USB mmWave radar input. Controls displays via DDC/CI, shell commands, Home Assistant service calls, and (planned) KWin DPMS and Samsung Tizen.

Status: **pre-alpha** — design phase complete, core daemon functional, hardware integration spikes in progress (KWin DPMS, Samsung Tizen). Not yet suitable for unattended production use.

## What dormant protects against

OLED panels degrade with static content. The effectiveness of each blank mode varies:

| Mode | OLED protection | Audio | Wake speed |
|---|---|---|---|
| `screen_off_audio_on` | Full | Yes | Fast |
| `power_off` | Full | No | Slower |
| `brightness_zero` | Partial (pixels still powered) | Yes | Instant |

`screen_off_audio_on` is the recommended mode for TVs where you want music to keep playing while the screen is off. `power_off` gives the same protection with cold-boot wake latency. `brightness_zero` is a fallback for controllers that support neither — better than nothing, but the panel is still energized.

## Feature matrix

### Sensors

| Source | Status |
|---|---|
| MQTT (Zigbee2MQTT, ESPHome, any broker) | Ready |
| Home Assistant WebSocket | Ready |
| USB-serial LD2410 mmWave radar | Ready |
| Input activity (keyboard/mouse idle) | Ready (inhibitor) |

### Display controllers

| Controller | Status |
|---|---|
| Shell command (arbitrary blank/wake scripts) | Ready |
| DDC/CI (VCP power/input/screen-off) | Ready |
| Home Assistant passthrough (service calls) | Ready |
| KWin DPMS (KDE Wayland) | M1 spike in progress |
| Samsung Tizen (KEY_PICTURE_OFF) | M1 spike in progress |

## Quickstart

### Install (from source, pre-release)

```bash
git clone https://github.com/icetea/dormant.git
cd dormant
cargo build --release
install -Dm755 target/release/dormantd ~/.local/bin/dormantd
install -Dm755 target/release/dormantctl ~/.local/bin/dormantctl
```

### Minimal config

Save as `~/.config/dormant/config.toml`:

```toml
config_version = 1

[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk-sensor"

[zones.office]
mode = "any"
members = ["desk"]

[displays.main]
controllers = ["ddcci"]
blank_mode = "power_off"

[rules.office_blank]
zone = "office"
displays = ["main"]
```

### Run

```bash
# Validate config
dormantctl validate

# Start the daemon
dormantd

# Check status
dormantctl status
```

## Systemd user unit

Install the daemon as a user service:

```bash
mkdir -p ~/.config/systemd/user
cp crates/dormantd/systemd/dormant.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dormant
```

## Documentation

Full docs at [docs/](./docs/src/introduction.md) — configuration reference, sensor setup guides, troubleshooting, and the doctor command reference.

## License

MIT OR Apache-2.0, at your option.
