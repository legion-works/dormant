# Troubleshooting

## Doctor command

`dormantctl doctor` is the first diagnostic tool. It runs a series of checks against your config and live state.

```bash
# Full system check
dormantctl doctor

# Per-component checks
dormantctl doctor mqtt      # probe MQTT sensors
dormantctl doctor ha        # probe HA WebSocket sensors
dormantctl doctor usb /dev/ttyUSB0  # probe USB LD2410
dormantctl doctor ddcci     # probe DDC/CI displays
dormantctl doctor config    # validate configuration
```

Each check reports status: OK, WARN, or FAIL. Warnings are non-fatal (e.g., a controller reports its last known state is stale). Failures indicate something needs fixing.

## Common errors

| Code | Meaning | Fix |
|---|---|---|
| `E_CONFIG_INVALID` | Config file syntax or structure error | Check TOML syntax (`dormantctl validate` shows exactly where). Missing `config_version` key, wrong type for a value, invalid duration format. |
| `E_CONFIG_UNKNOWN_KEY` | A key in the config is not recognized | Remove the key, or check for typos. Use `--strictness=warn` to see all unknown keys as warnings instead of errors. |
| `E_ZONE_CYCLE` | Zone references create a dependency cycle | Zone A cannot reference Zone B if B references A. Flatten nested zone chains. |
| `E_ZONE_UNKNOWN_MEMBER` | Zone references a sensor or zone that does not exist | Check the member name. Sensor IDs must match `[sensors.<id>]`. Zone references must use `"zone:<id>"` syntax. |
| `E_CREDS_PERMS` | Credentials file has incorrect permissions | `chmod 600 ~/.config/dormant/credentials.toml` |
| `E_CREDS_MISSING` | Required credential is missing | The config references a credential (e.g., HA token) but the credentials file does not contain it. |
| `E_MODE_UNSUPPORTED` | Display controller does not support the configured blank mode | Run `dormantctl doctor ddcci` to see supported modes. Choose a different mode or add a fallback chain. |
| `E_BLANK_FAILED` | Blank command failed | Check the controller's logs. For DDC/CI: is `/dev/i2c-*` accessible? For command: does the shell command work when run manually? |
| `E_WAKE_FAILED` | Wake command failed | Same checks as blank. Verify the wake command works standalone. Check the `wake_retries` count. |
| `E_RELOAD_WAKE_FAILED` | Defensive wake on config reload failed | A display was physically blanked before reload; the wake attempt to restore it failed. Check the controller and increase `wake_retries`. |
| `E_HA_AUTH` | Home Assistant authentication failed | Verify the `ha_token` in credentials file. Check that the token is still valid in HA. |
| `E_SENSOR_IO` | Sensor I/O error | MQTT: broker reachable? USB: port exists and has correct permissions? HA: WebSocket URL reachable? |
| `E_DISPLAY_IO` | Display controller I/O error | Network controller: host reachable? DDC/CI: I2C bus accessible? Command: binary exists? |
| `E_IPC` | Inter-process communication error | Check that `dormantd` is running. Verify the socket path matches between daemon and CLI. |

## Fail-safe: why my display won't blank

This is by design. The most common "my display won't blank" scenarios and why they happen:

### Sensor is stale

If a sensor stops reporting data (MQTT broker down, USB cable unplugged, HA WebSocket disconnect), the zone treats it as **present** — not absent. This prevents dormant from blanking a room it cannot see.

Check with:

```bash
dormantctl status
```

Look for `unavailable` sensor states. Fix the sensor, and blanking will resume.

### Grace period has not elapsed

The rule's `grace_period` prevents rapid toggling. If you just left the room, it may still be counting down. The default is 60 seconds. Reduce it if your room layout allows faster detection:

```toml
[rules.office_blank]
grace_period = "30s"
```

### Inhibitor is active

If you set `inhibitors = ["user-activity"]`, dormant will not blank displays while it detects keyboard/mouse activity. This prevents blanking while you are at the desk but still (e.g., reading).

```bash
dormantctl pause
```

Shows active inhibitors. Use `dormantctl pause off` to force-resume blanking.

### Min-wake-time floor

After a wake, the display cannot be blanked again for `min_wake_time` (default 10 s). This prevents rapid blank/wake thrashing if someone briefly leaves and returns.

## Emergency wake

When a display stays blank and pressing keys on a presence-mapped keyboard shortcut doesn't help (no sensors in the room, or someone manually blanked the panel and left), `dormantctl emergency-wake` is the panic-recovery command: it force-wakes every configured display regardless of the rules engine's state.

```bash
dormantctl emergency-wake
```

The command tries the daemon's IPC first (2-second timeout). If the daemon is healthy, it pauses every rule indefinitely alongside the wake so nothing re-blanks until you run `dormantctl resume`. If the daemon is wedged or unreachable (the very failure mode this command exists for), `dormantctl` falls back to constructing display controllers directly from your `config.toml` + `credentials.toml` and sending the wake command itself — best-effort, one attempt per display.

For a one-keystroke recovery, bind the command to a global shortcut:

- **KDE Plasma**: System Settings → Shortcuts → Custom Shortcuts → Edit → New → Global Shortcut → Command. Command: `dormantctl emergency-wake`. Trigger: your key.
- **GNOME**: Settings → Keyboard → View and Customise Shortcuts → Custom Shortcuts. Command: `dormantctl emergency-wake`.
- **Sway / wlroots**: bind in `~/.config/sway/config`: `bindsym $mod+grave exec dormantctl emergency-wake`.

First-class daemon-registered shortcuts (no compositor setup) are a separate roadmap item.

## Control-path verification — `dormantctl doctor --exercise <display>`

Confirms that a blank/wake **actually moved the panel** — the systemic guard against the failure mode where a controller logs `blank_succeeded` while the panel never changed (the samsung stale-socket + port-1516 400s both did exactly this).

```bash
dormantctl doctor --exercise mon
```

The command routes through the daemon over IPC: the daemon pauses the target's rule(s) for the exercise window, runs `blank → read → wake → read → restore` on its live controllers, and replies with a per-step report. Exit code is non-zero only when at least one step verdict is `Failed` — a confirmable panel that did not move despite the controller returning `Ok`. The wake path is sacred: the restore step guarantees a final wake regardless of any earlier failure, so an exercise can never leave a display dark.

### Verdicts

- `Confirmed` (✓, green) — the panel state moved as expected (blank step: state changed from baseline; wake step: state returned to baseline).
- `Unconfirmable` (~, yellow) — the controller has no readback for the panel. The command was issued but the panel could not be observed. Exit 0 (honest, not a fake pass).
- `Failed` (✗, red) — the controller can read the panel but the state did NOT move as expected. Exit non-zero.

### Confirmability by controller

| Controller | Confirms via |
|---|---|
| `ddcci` | VCP `0x10` (brightness, 0–100) + VCP `0xD6` (power: `0x01` → On, otherwise → Standby) |
| `samsung-tizen` | REST `PowerState` (`"on"` / `"standby"`); port-1516 backlight when configured for `BrightnessZero` |
| `command` / `kwin-dpms` / `ha-passthrough` | none — every step is `Unconfirmable` |

### Operational notes

- The exercise causes a brief blank → wake blink on a display currently in use (the wake step + the defensive restore wake).
- The command is per-display only (`--exercise <name>`); there is no `--all` or default target.
- `command` / `kwin-dpms` / `ha-passthrough` controllers report `Unconfirmable` for every step because they cannot observe the panel — exit 0, but no verification was possible.

## Logging

Set `log_level = "debug"` in the daemon config for detailed logs:

```toml
[daemon]
log_level = "debug"
```

Key log events to search for:

- `sensor_event` — each presence update with source, value, and timestamp
- `zone_transition` — when a zone changes between occupied and vacant
- `rule_blank` / `rule_wake` — when a rule fires a blank or wake command
- `wake_failed` — wake attempt failures with controller and retry count
- `reload_complete` — config reload finished
- `reload_defensive_wake` — display woken because it was blanked before the reload

Logs are written to stderr by default. When running under systemd, view them with:

```bash
journalctl --user -u dormant -f
```
