# Watchdog + last-known-good rollback

dormant combines three recovery mechanisms:

1. a health-gated last-known-good (LKG) config snapshot;
2. boot-time rollback for invalid configs and counted crash loops;
3. a systemd watchdog that restarts a process whose engine stops responding.

None of them changes the running rule engine in place. Rollback chooses the
config for a new process; the watchdog asks systemd to replace a wedged one.

## Last-known-good promotion

With `watchdog.lkg_enabled = true`, the daemon promotes the running config to
`$XDG_STATE_HOME/dormant/last-known-good.toml` after
`watchdog.stability_window` (default `5m`) when all of these remain true:

- every engine-liveness probe succeeds;
- no reload occurs during the window;
- the config bytes on disk still match the installed config;
- no configured display reports every controller unhealthy.

An uncommanded display has no health evidence and does not block promotion.
Repeated display-health deferrals are capped; after three, the candidate is
promoted with `lkg_promoted_with_unhealthy_display` rather than leaving the
daemon with no LKG.

The LKG is separate from `<config_dir>/backups/`. The web UI writes those
backups before each browser apply. The daemon writes the LKG only after a
healthy stability window, regardless of whether the config arrived through
the web UI, file watch, `SIGHUP`, IPC reload, or boot.

## Boot-time rollback

Rollback has three forms:

- **Immediate rollback:** the current config fails to build, its bytes differ
  from the LKG, and the LKG builds successfully. The daemon logs
  `config_rollback_boot` and starts from the LKG. If the LKG also fails, startup
  fails normally with `startup_failed`.
- **Counted rollback:** the same config fingerprint crashes or wedges at least
  three times in a 6-minute window. The next boot selects the LKG and records
  an active rollback.
- **Sticky continuation:** while the original config remains unchanged, later
  boots in that crash storm keep selecting the LKG and log
  `config_rollback_continued`. After the quiet window, the daemon gives the
  original config one retry and logs `config_rollback_retry`.

The original config file is never overwritten. `dormantctl status` and the web
dashboard show the pending rollback state.

### Recovery after a rollback boot

Fix the intended config, validate it, then restart the daemon:

```bash
dormantctl validate
systemctl --user restart dormant
```

Do not use `dormantctl reload` as the recovery step. In the current release,
the rollback process watches and reloads the LKG path until it is restarted,
even though the banner says "fix it and reload." This is tracked in
[issue #53](https://github.com/legion-works/dormant/issues/53). The restart is
the working recovery path.

### Counted-rollback gate

Set `watchdog.lkg_rollback_enabled = false` while intentionally restarting the
daemon rapidly during debugging. This disables the counted trigger for a new
storm. It does not disable immediate rollback for an invalid config or cancel
an already-active sticky rollback.

## Emergency wake

If displays are dark while the daemon is unavailable:

```bash
dormantctl emergency-wake
```

The command tries daemon IPC first with a bounded timeout. If IPC fails, it
loads the config and drives the display controllers directly.

## systemd watchdog

The shipped unit uses `Type=notify` and `WatchdogSec=150`. `dormantd` sends
`READY=1` after startup and sends `WATCHDOG=1` only after the engine answers a
liveness probe through its control channel. A scheduled but wedged process
therefore does not keep itself alive with superficial watchdog pings.

Reload sends watchdog pings at internal step boundaries so a slow controller
chain is not killed during a healthy generation swap. If a probe fails, the
daemon logs `watchdog_probe_failed`, withholds the ping, and resets any pending
LKG stability window.

Without `NOTIFY_SOCKET` / `WATCHDOG_USEC`, the same engine probe runs every 30
seconds for LKG health accounting, but no systemd notification is sent.

### Upgrade order

Install the new `dormantd` binary before reloading a unit that changes from
`Type=simple` to `Type=notify`:

```bash
install -Dm755 target/release/dormantd ~/.local/bin/dormantd
systemctl --user daemon-reload
systemctl --user restart dormant
```

Reloading the new unit while an old binary is still installed makes systemd
wait for a `READY=1` message that binary cannot send.

## Configuration

```toml
[watchdog]
lkg_enabled = true
lkg_rollback_enabled = true
stability_window = "5m"
```

| Key | Default | Notes |
|---|---|---|
| `watchdog.lkg_enabled` | `true` | Enable LKG promotion |
| `watchdog.lkg_rollback_enabled` | `true` | Enable counted crash-loop rollback |
| `watchdog.stability_window` | `"5m"` | Healthy runtime required before promotion; minimum `30s` |

`WatchdogSec`, crash-count thresholds, and the quiet window are fixed recovery
mechanics rather than config policy.

## State and privacy

Recovery state lives under `$XDG_STATE_HOME/dormant` (normally
`~/.local/state/dormant`):

| File | Purpose |
|---|---|
| `last-known-good.toml` | Proven-stable config snapshot |
| `last-known-good.meta.json` | Advisory fingerprint and promotion timestamp |
| `crash-loop.json` | Bounded crash/restart history |
| `discount-<nonce>` | One-shot clean-exit marker used around startup races |

Directories are mode `0700`; files are mode `0600`. The files stay local and
contain no telemetry.
