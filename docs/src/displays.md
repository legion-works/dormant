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

### `kwin-dpms` — KWin DPMS

Controls KDE KWin outputs via `kscreen-doctor --dpms`. Per-output DPMS works on
Plasma 6.7.2+ (Wayland). **Audio-unsafe:** DPMS disables the DRM/KMS output,
which destroys the ALSA audio device for that output. Use only for displays
with no audio sink and no DDC/CI.

```toml
[displays.desk]
controllers = ["kwin-dpms", "ddcci"]
blank_mode = "power_off"
output = "DP-1"
```

See `docs/research/2026-07-05-kwin-dpms-verification.md` for the spike data.

### `samsung-tizen` — Samsung Tizen TV

Controls Samsung Tizen (OLED) TVs via `KEY_PICTURE_OFF` remote key over
WebSocket (port 8002) and via Samsung IP Control G2 JSON-RPC (HTTPS port
1516, `backlightControl`) for the audio-safe `brightness_zero` mode.
Verified on S90D (QA65S90DAKXXA). Requires a persistent socket with
keepalive — the TV silently drops idle connections. Use REST `/api/v2/`
PowerState for real panel state, not socket liveness. Two standby depths
exist (warm network-standby / deep standby); see the spike doc for the
wake matrix.

Two blank modes are audio-safe:

- `screen_off_audio_on` (default): `KEY_PICTURE_OFF` — true picture-off.
  Audio continues on the TV speakers but the HDMI source is paused and the
  panel is dark. Verified end-to-end on S90D.
- `brightness_zero`: Samsung IP Control G2 `backlightControl` → 0.
  Near-black **dim**, not true-off — the HDMI source keeps running and
  audio plays uninterrupted. Use this when the operator wants audio
  playing while the panel is unreadable. The TV's backlight range is
  0–50; dormant saves the current value on the first blank and restores
  it on wake (first-blank-wins: a re-blank while already dimmed does not
  clobber the saved value).

`brightness_zero` is a softer panel-state change than `screen_off_audio_on`
and may be preferable for OLED longevity in the long run (no panel power
cycling), but it does not produce a true pixel-off — it only dims.

The token goes in the credentials file:

```toml
[credentials]
[samsung]
"10.1.1.7" = "eyJ..."
```

See `docs/research/2026-07-05-s90d-verification.md` for the full spike data
including wake matrix, latency measurements, and socket survival findings.

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

## Audio-safe blanking

DPMS-based controllers (`kwin-dpms`, `command` with `xset dpms`) disable the
DRM/KMS output, which destroys the ALSA audio device for that output. Audio
stops when the display blanks. This is how the kernel DRM pipeline works — it
is not configurable.

Two controllers blank without touching the output, preserving audio:

| Controller | Mechanism | Audio-safe because |
|---|---|---|
| `ddcci` | VCP `0xD6` (monitor-internal command over I2C) | Panel blanks internally; OS output stays active |
| `samsung-tizen` | `KEY_PICTURE_OFF` (TV-internal command over WebSocket) or IP-Control `backlightControl` (dim, near-black) | TV blanks/dims panel; HDMI output stays active |

**Per-display strategy:**

1. If the display has DDC/CI and supports VCP D6 → use `ddcci` power_off.
2. If the display is a Samsung Tizen TV → use `samsung-tizen` picture-off.
3. If the display has no DDC/CI and no audio → `kwin-dpms` is fine.
4. If the display has audio but neither DDC/CI nor Tizen → use a `command`
   controller with an audio-safe external command (e.g. a TV-specific IR
   blaster or HA automation), or accept the audio loss.

Run `dormantctl doctor` to probe DDC/CI VCP D6 support. For Tizen TVs, the
doctor check verifies WebSocket reachability, token validity, and REST
PowerState.

## Escalation ladder & audio-safe black

When a hardware blank mode fails (controller unreachable, capability missing),
dormant can fall back to a **software render** — a fullscreen overlay drawn
directly on the output via the Wayland layer-shell protocol.

### `render_black` — audio-safe black overlay

A fullscreen black layer-shell overlay that covers the entire output.
The panel stays on (no DPMS, no VCP power-off), so audio continues playing
through any sink attached to that output. Input (mouse/keyboard) or a
presence event tears it down instantly; the cursor is hidden while the
overlay is up.

This is the preferred first stage in an OLED escalation ladder when the
display doubles as an audio sink:

```toml
[displays.oled]
controllers = ["ddcci"]
modes = ["power_off"]
ladder = [
  { kind = "render_black", dwell = "30s" },
  { kind = "power_off" },
]
output = "DP-1"
```

Here `render_black` buys 30 seconds of audio-safe, cursorless black before
the panel actually powers off. If the sensor reports presence during those
30 seconds, the overlay vanishes — no wake latency, no re-handshake.

### When to use it

- **OLED + audio over monitor:** the panel powers off via DDC/CI or Tizen,
  but you want audio to keep playing during a short absence.
- **DDC/CI fallback:** the primary `power_off` controller is unreachable;
  the ladder falls through to a render stage instead of leaving the screen on.
- **KWin DPMS replacement:** DPMS destroys the DRM/KMS output and its audio
  sink; `render_black` preserves both.

### Build requirement

The render backend is **off by default**. Build with:

```bash
cargo build --release --features render
```

Without the `render` feature, configs containing a render stage are rejected
at startup with error `E_RENDER_UNAVAILABLE`. On Linux, the render backend
also requires `libwayland-dev` at build time.

## Manual-only displays

A display listed in `[displays]` that no `[rules]` entry references is
**manual-only**: the daemon builds a full executor and controller chain for it,
and it appears in `dormantctl status` / the web UI / the tray app, but no zone
or rule drives it. It responds exclusively to manual control commands
(`dormantctl blank <id>`, `dormantctl wake <id>`) and the web/tray interfaces.

A `ladder` requires a rule.  Validation rejects a `ladder` on a rule-less
display with `E_CONFIG_INVALID`:

```
display 'tv' has a ladder but is in no rule; a ladder is an auto-escalation
that needs a rule to drive it — use blank_mode for manual-only control,
or add a rule
```

Use `blank_mode` (or `blank_mode` + `degraded_mode`) for manual-only displays.

Manual-only phase is preserved across config reloads (SIGHUP /
`dormantctl reload`).  A display you blanked stays blanked.  However, a full
daemon **restart** loses state — the display starts `active` (phase
persistence to disk is not implemented in v1).

**Known limitation:** a manual blank or wake command issued in the brief
window while a config reload is in progress may be lost (the new generation
restores the pre-command state).  Re-issue the command after the reload
settles.  Tracked in [issue #9](https://github.com/legion-works/dormant/issues/9).

```toml
# A Samsung Tizen TV controlled entirely by hand.
[displays.tv]
controllers = ["samsung-tizen"]
blank_mode = "screen_off_audio_on"
host = "192.168.1.50"
```
