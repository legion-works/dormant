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
dormantctl doctor ddcci     # probe DDC/CI displays (Linux and macOS)
dormantctl doctor config    # validate configuration

# macOS-only, read-only checks
dormantctl doctor macos-idle            # two bounded raw idle-clock readings
dormantctl doctor macos-display-sleep   # display-sleep API availability + per-display state
dormantctl doctor macos-power           # active power assertions blocking display sleep
```

Each check reports status: OK, WARN, or FAIL. Warnings are non-fatal (e.g., a controller reports its last known state is stale). Failures indicate something needs fixing. The three `macos-*` checks exit 3 ("not yet supported") on Linux — they are read-only probes, never blanking or waking a display.

### Doctor-assisted issue drafting

When something fails and you want to file it later without hand-reconstructing the context, `--report-issue` and `--draft-feature` run the full offline probe set (same as bare `dormantctl doctor`) and write a ready-to-paste draft to a file. The probe table and the final `draft written to ...` message still go to stdout:

```bash
# Bug report — pre-filled with version, environment, display inventory, and
# the probe table. Default path: ./dormant-issue-<YYYY-MM-DD>.md
dormantctl doctor --report-issue

# Feature request — same environment capture, no probe-failure framing.
# Default path: ./dormant-feature-<YYYY-MM-DD>.md
dormantctl doctor --draft-feature

# Explicit path
dormantctl doctor --report-issue /tmp/my-bug.md
```

Both flags mirror the field order and headings of `.github/ISSUE_TEMPLATE/{bug,feature}.yml`, so pasting the draft into a new GitHub issue lines up with the template. Machine-collectable fields (dormant version, OS/kernel, session type, compositor, the display inventory, config load status, and the probe table) are pre-filled; everything else is left as an `<!-- fill in -->` placeholder. Never overwrites an existing file — a name collision gets a `-2`, `-3`, … suffix.

Redaction is layered, not just allowlisting: display entries only ever carry `id`, panel type, controller list, and blank mode (never a host, token, or MAC address). On top of that, every literal secret value the loaded config and credentials actually hold — broker URLs, hosts, MAC addresses, HA/Samsung/MQTT credentials — is substituted with `[redacted]` across the **entire draft, including probe result text**, since a probe's own diagnostic detail can otherwise echo a host or broker URL verbatim (e.g. an mqtt auth-failure message). A second, cheaper pass scrubs any leftover bare IPv4 literal as defense-in-depth. Still, glance at the file before posting it publicly — redaction only catches values dormant itself knows about, not anything you type into a `<!-- fill in -->` section by hand.

The two flags are mutually exclusive with each other and with any doctor subcommand (`ddcci`, `mqtt`, …) — the draft always runs the full offline set. Pass the path with `=` (`--report-issue=./ddcci`) if you want a file genuinely named after a subcommand; the space form (`--report-issue ddcci`) is rejected because clap can't tell "PATH" from "subcommand" there.

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

## Display will not blank

### Sensor is stale

**Symptom:** the rule stays active after the room empties.

**Cause:** a sensor stopped reporting. The zone treats `unavailable` as present by default, so dormant will not blank a room it cannot see.

Check with:

```bash
dormantctl status
```

Look for `unavailable` sensor states, then repair the broker, connection, or device.

### Grace period has not elapsed

**Symptom:** the zone is vacant, but blanking is delayed.

**Cause:** `rules.<id>.grace_period` is still counting down. The default is 60 seconds.

```toml
[rules.office_blank]
grace_period = "30s"
```

### Inhibitor is active

**Symptom:** the sensor and zone are vacant, but the rule remains inhibited.

**Cause:** `"user-activity"` is active, or the rule was paused manually.

```bash
dormantctl status
```

Resume manual pauses with:

```bash
dormantctl resume
```

`"audio-playback"` and `"call"` may appear in config, but audio detection is
not shipped and cannot currently inhibit a rule.

### Min-wake-time floor

**Symptom:** a just-woken display stays on despite vacancy.

**Cause:** `rules.<id>.min_wake_time` has not elapsed. The default is 10 seconds.

```bash
dormantctl status
```

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

### macOS: gamma emergency restore

`macos-gamma-black` (see [Displays](./displays.md#macos-gamma-black--macos-gamma-black-quartz)) blanks
by writing an all-black Quartz gamma table — there is no periodic
reassertion and no restore-on-drop, so a dead daemon does not strand the
panel any *more* black than it already is, but it also does nothing to fix
it. `dormantctl emergency-wake` restores gamma through a dedicated,
daemon-independent path:

- Before writing a black table, dormant records a breadcrumb file
  (`$XDG_STATE_HOME/dormant/gamma-blank.json` or the `~/.local/state`
  fallback) listing every display currently gamma-blanked, cleared again
  after a confirmed wake.
- `emergency-wake` restores from that breadcrumb via
  `CGDisplayRestoreColorSyncSettings()` — a **system-wide** restore, not a
  per-panel replay of the exact pre-blank table — and does this
  independently of whether the daemon's IPC call succeeds, fails, or times
  out. It also runs on the *next* dormantd startup if a stale breadcrumb is
  found, so a crash never leaves gamma permanently black either.
- **Wedged-daemon caveat**: if `emergency-wake`'s IPC call times out
  (daemon alive but hung, not merely unreachable) rather than being
  refused, the command prints a warning that the daemon may still complete
  a queued blank write *after* the fallback restore already fixed the
  display — re-blanking it. If you see the display go black again shortly
  after an emergency wake, stop `dormantd` (or `launchctl kickstart -k` /
  `systemctl --user restart dormant` it) before rerunning
  `emergency-wake`.

## Control-path verification — `dormantctl doctor exercise <display>`

Confirms that a blank/wake **actually moved the panel** — the systemic guard against the failure mode where a controller logs `blank_succeeded` while the panel never changed (the samsung stale-socket + port-1516 400s both did exactly this).

```bash
dormantctl doctor exercise mon
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

## Failure notification does not appear

**Symptom:** the tray or web dashboard marks a display failed, but no desktop
notification appears.

**Cause:** wake failures notify only after
`notifications.wake_attempt_threshold` consecutive attempts; repeats are
limited by `notifications.cooldown`. Desktop notices may also be disabled.

```bash
journalctl --user -u dormant -f
```

Check `notifications.enabled`, the session D-Bus, and `notify_suppressed` /
`notify_unreachable` events. Blank failures notify on the first exhausted
controller chain. See [Failure notifications](./failure-notifications.md).

## Daemon booted from last-known-good config

**Symptom:** `dormantctl status` or the web UI says the current config was
rolled back, and edits to the original config do not take effect after reload.

**Cause:** the daemon booted from `last-known-good.toml` because the operator
config failed validation during a crash loop.

Fix the intended config, then reload — the daemon watches the operator config
even after a rollback boot, so the watcher picks the fix up on save (or run
`dormantctl reload` explicitly):

```bash
dormantctl validate
dormantctl reload
```

A successful reload of the fixed config clears the rollback state and the
banner (`config_rollback_recovered` in the journal). A service restart also
works, as the fallback path. See
[Watchdog + last-known-good rollback](./watchdog-rollback.md).

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
