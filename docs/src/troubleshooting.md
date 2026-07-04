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
