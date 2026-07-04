# Displays

dormant controls displays through an ordered controller chain. The first controller in the chain is tried first; if it fails, the next one is tried. Wake commands retry with exponential backoff before escalating to the next controller.

## Controllers

### `command` — shell commands

Executes arbitrary shell commands to blank and wake a display. The most flexible controller — works with any display that can be controlled from the command line.

```toml
[displays.escape]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "/usr/bin/xset dpms force off"
wake_command = "/usr/bin/xset dpms force on"
modes = ["power_off"]
```

Set `modes` to declare which blank modes your commands support. dormant cannot auto-detect this for shell commands, so you must be honest — declaring a mode your commands don't actually deliver leaves the screen on.

### `ddcci` — DDC/CI (monitor control)

Controls PC monitors via DDC/CI over I2C (`/dev/i2c-*`). Always supports brightness-zero; supports power-off when the monitor exposes VCP `0xD6`.

```toml
[displays.main]
controllers = ["ddcci"]
blank_mode = "power_off"
restore_brightness = 80
```

`restore_brightness` sets the brightness level to restore on wake (0–100, default 80).

#### I2C permissions

Your user needs read/write access to `/dev/i2c-*` devices. On Debian/Ubuntu, add your user to the `i2c` group:

```bash
sudo usermod -a -G i2c $USER
```

Some distributions use `plugdev` instead. If no `i2c` group exists, create a udev rule:

```bash
# /etc/udev/rules.d/99-i2c.rules
SUBSYSTEM=="i2c-dev", GROUP="i2c", MODE="0660"
```

Then:

```bash
sudo groupadd i2c
sudo usermod -a -G i2c $USER
sudo udevadm control --reload-rules && sudo udevadm trigger
```

#### Monitor compatibility

Not all monitors support DDC/CI power-off (`D6 01`). Run `dormantctl doctor` to probe your monitor's VCP capabilities:

```bash
dormantctl doctor ddcci
```

If `power_off` is unsupported, `brightness_zero` is always available as a fallback (DDC/CI unconditionally supports brightness control). `screen_off_audio_on` is not a DDC/CI mode — use a different controller for that.

### `ha-passthrough` — Home Assistant passthrough

Calls arbitrary HA services for blanking and waking. Use this when your display is controlled through an HA integration (smart plug, IR blaster, media player).

```toml
[displays.tv_plug]
controllers = ["ha-passthrough"]
blank_mode = "power_off"
ha_url = "http://ha.local:8123"
blank_service = "switch.turn_off"
blank_data = { entity_id = "switch.tv_power" }
wake_service = "switch.turn_on"
wake_data = { entity_id = "switch.tv_power" }
modes = ["power_off"]
```

The `ha_token` goes in the credentials file, not in the main config.

### `kwin-dpms` — KWin DPMS (planned, M1 spike)

Controls KDE KWin outputs via DBus DPMS. Awaiting hardware verification.

```toml
[displays.desk]
controllers = ["kwin-dpms", "ddcci"]
blank_mode = "power_off"
output = "DP-1"
```

### `samsung-tizen` — Samsung Tizen TV (planned, M1 spike)

Controls Samsung Tizen (OLED) TVs via the `KEY_PICTURE_OFF` remote key over WebSocket. The token goes in the credentials file:

```toml
[credentials]
[samsung]
"192.168.1.50" = "eyJ..."
```

Awaiting hardware verification on an S90C.

## Fail-safe wake contract

Every controller must satisfy three invariants for `wake()`:

1. **Idempotent** — safe to call on an already-awake display.
2. **Retries or escalates** — must not silently give up. Internally retry, or let the executor's chain handle it.
3. **No permanent failure state** — a screen that won't wake is the worst outcome. Controllers must report failures clearly so the user can intervene.

## Doctor check

```bash
dormantctl doctor ddcci
```

Verifies: controller reachability, supported modes vs configured mode, last known state, and performs a dry-run capability probe (does not blank the display).
