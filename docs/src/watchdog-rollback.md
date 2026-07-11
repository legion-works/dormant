# Watchdog and rollback

dormant ships three small, independent mechanisms that work together to keep
an unattended install from getting stuck on a bad config or a wedged engine:

1. **Last-known-good (LKG) snapshotting** — the daemon keeps a copy of the
   most recent config that has proven itself by running stably (and
   healthily) for a while.
2. **Boot-time rollback** — if the daemon can't start, or gets restarted
   after wedging, boot substitutes the LKG config in place of a bad one.
3. **systemd watchdog pings** — the daemon tells `systemd` it's alive only
   when its own engine actually answers, so a wedged-but-still-running
   process gets restarted instead of sitting dark forever.

None of this touches the running engine's own logic. Rollback happens only
at boot, before any display I/O exists, and the watchdog only ever pings —
it never intervenes directly. The composition is: wedge → no pings →
systemd restarts the process → boot notices the config that led to the
wedge and substitutes the LKG copy on the way back up.

## Last-known-good snapshotting

The daemon tracks a "candidate" config generation every time it starts or
successfully reloads (via the web UI, `SIGHUP`, direct file edit, or the
`dormantctl reload` IPC call — all four funnel through the same reload
path, so all four arm the same candidate). A candidate is promoted to LKG
only after it has run for `watchdog.stability_window` (default 5 minutes)
**continuously and healthily**:

- every watchdog engine-liveness probe during the window must have
  succeeded — a single failed probe resets the window's clock, so the
  daemon can't accidentally credit a config that wedged for a while and
  then recovered;
- no reload may have happened during the window (a reload starts a new
  window for the new config);
- the config bytes on disk still have to match what the candidate is
  running — an un-applied direct edit sitting on top of a still-installed
  config skips promotion for that tick (logged `lkg_skipped_dirty`) rather
  than snapshotting bytes that were never actually run;
- at least one display must not be in the "every controller unhealthy"
  state — see the display-health gate below.

Reaching "installed and running" is deliberately *not* the bar. A config
that assembles cleanly and then wedges five minutes later would clear that
bar too — the stability window is what "known **good**" actually means.

### Display-health gate

A config whose controllers can't reach any display (wrong IP, bad token, a
disconnected panel) shouldn't become the safety net everyone rolls back to.
On each promotion check, if any display's `ControllerHealth` rows are *all*
unhealthy, promotion is deferred (`lkg_deferred_display_health`, warned once
per candidate) rather than written.

This can't defer forever: a display that's genuinely, deliberately kept
offline gets re-commanded on its own retry schedule, so its health rows stay
all-unhealthy indefinitely. After `LKG_HEALTH_DEFER_CAP` (3) consecutive
deferred candidates — counted regardless of which display's health set
triggered each deferral, so a *fluctuating* set of unhealthy displays still
accumulates toward the cap — promotion proceeds anyway, loudly
(`lkg_promoted_with_unhealthy_display`). An imperfect LKG beats no safety
net at all. A display that was simply never commanded during the window
(empty health, not failing health) is treated as unproven, not failing, and
never blocks promotion or counts toward the cap — dormant won't flash real
hardware just to prove it's there.

### LKG vs. the web UI's config backups

dormant already keeps a rolling history of configs under
`<config_dir>/backups/` — every time you apply a config through the web UI,
the previous file is copied there before the new one is written, pruned to
the 5 most recent (`MAX_BACKUPS`). That mechanism and LKG are deliberately
separate things, not overlapping duplicates:

- `backups/` is a **web-apply undo history**: unconditional, written on
  every apply regardless of whether the new config turns out to work, and
  blind to any reload trigger other than the web UI (SIGHUP, direct file
  edit, and the IPC reload never touch it).
- The LKG snapshot is a **proven-stable** daemon-side copy: written only
  after the health-gated stability window above, and fed by *every* reload
  path plus boot — because it lives in the daemon, not the web server,
  which is also an optional Cargo feature (`web-ui`) that may not even be
  compiled in.

If you want to hand-revert to a specific prior config, `backups/` is the
audit trail to look at. If you want the daemon to protect itself
automatically against its own most recent bad edit, that's the LKG's job.

## Boot-time rollback

Every daemon start now runs a boot-time decision before it ever gets to
`App::build`. The rule of thumb: **a rollback boot is just a normal boot of
a config that once ran stably** — no new machinery runs in the engine
itself, and the reload path's wake-safety invariants (verified wake,
defensive wake, quiesce) apply exactly as they always have.

There are three shapes this can take:

- **Immediate rollback.** The current config fails to build (parse error,
  cross-reference validation failure) *and* an LKG exists whose bytes
  differ from the current config's. No counting needed — this failure is
  deterministic, so there's no risk of a loop. The daemon logs
  `config_rollback_boot` at error level with both configs' fingerprints and
  the failure detail, additionally mirrors a concise line to stderr (so a
  failed config's own `log_level = "off"` can't hide the rollback from
  view), and starts from the LKG instead. If the LKG *also* fails to build,
  the daemon exits exactly as it always did on a bad config
  (`startup_failed`).
- **Counted rollback.** The daemon starts, runs, and crashes or wedges
  (restarted by `systemd`, see below) three or more times within a 6-minute
  window, all with the *same* config fingerprint, and that config's bytes
  differ from the LKG's. On the third such start, boot rolls back before
  even attempting to build the current config — same LKG substitution, same
  loud logging, but `rollback_active` is recorded so subsequent boots know
  a rollback is in effect.
- **Sticky continuation.** Once `rollback_active` is set, if the operator's
  config file hasn't changed yet, every subsequent boot during the same
  storm keeps running the LKG rather than retrying the bad config again
  (`config_rollback_continued`, info level, no new counting). This is what
  keeps a crash-loop from burning through "one free rollback per restart"
  for the rest of the storm.

Whenever a boot substitutes the LKG, the operator's actual intent (the
config on disk) isn't discarded — it's left untouched on disk, and a
pending-reload banner is parked through the same UI plumbing an ordinary
rejected reload already uses ("your latest config failed and was rolled
back to last-known-good — fix it and reload: ..."), visible in both the web
dashboard and `dormantctl status`.

### The bounded storm/quiet oscillation

A config that's never actually fixed produces a predictable, bounded
pattern rather than either extreme (retrying forever vs. never trying
again): storm → sticky LKG → a quiet period once the previous start ages
past the 6-minute crash-loop window → one loud retry of the original config
(`config_rollback_retry`, re-parking the pending-reload banner) → storm
again if it's still broken. Every transition is logged
(`config_rollback_boot` / `config_rollback_continued` / `config_rollback_retry`),
so this cycles at roughly one storm per crash-loop window of quiet — never
silent, and never permanently stuck retrying the unedited config either.

### `watchdog.lkg_rollback_enabled` — the rapid-restart debugging escape

Counted rollback (not immediate rollback, not sticky continuation) is gated
by `watchdog.lkg_rollback_enabled` (default `true`). If you're deliberately
restarting the daemon rapidly while debugging — poking at something that
looks like a crash loop but isn't actually caused by the config — set this
to `false` so counted rollback doesn't kick in and substitute the LKG out
from under you. Immediate rollback (a config that flatly fails to validate)
and an already-active sticky rollback are unaffected by this gate; it only
suppresses the *new-storm* counted trigger.

### `dormantctl emergency-wake` — the standing escape hatch

None of the above helps if a display is stuck dark *right now* and you need
it back immediately. `dormantctl emergency-wake` remains the
daemon-independent panic button: it tries the daemon's IPC first, and when
the daemon is unresponsive or dead it builds the display controllers
directly and force-wakes everything in parallel. It doesn't care whether
the daemon is mid-rollback, mid-crash-loop, or not running at all.

## systemd watchdog

The shipped unit (`crates/dormantd/systemd/dormant.service`) is
`Type=notify` with `WatchdogSec=150`. The daemon sends `READY=1` once
startup is fully complete (after the IPC and web listeners are both up),
and sends `WATCHDOG=1` periodically — but **only when its own engine
answers a liveness probe**, not merely because the outer process is still
scheduled. Each probe rides the same control channel every other engine
command uses, so it proves the run loop is actually draining its mailbox,
not just that the process hasn't been killed.

Reload itself pings at several step boundaries internally (after
validating the new config, after quiescing displays, after teardown, once
per removed display during verified wake, and before any rollback-recovery
rebuild) so that a slow-but-healthy reload — which can legitimately run for
tens of seconds against a real controller chain — never gets mistaken for a
wedge and killed mid-swap.

If a probe fails, the daemon withholds the ping (`watchdog_probe_failed`,
warned once), and any in-flight LKG candidate loses its unbroken-healthy
claim, resetting its stability window. If `systemd` doesn't provide a
watchdog at all (no `NOTIFY_SOCKET`/`WATCHDOG_USEC` — non-systemd setups,
or the unit isn't `Type=notify`), the same probe arm still runs on a 30s
fallback cadence purely to keep LKG promotion working; the pings themselves
simply become no-ops.

### `WatchdogSec=150` — where the number comes from

The unit file documents the derivation directly (`dormant.service`); the
chapter here mirrors it. The bound is sized against the worst-case *single*
backoff burst against one stuck controller chain — not the sum of unrelated
retries — by chain length:

| Controller class | 1 chained slot | 2 slots | 3 slots |
|---|---:|---:|---:|
| generic 10s-slot (e.g. `command`/`kwin-dpms`) | 54s | 94s | 134s |
| Samsung-IP / Home Assistant 5s-slot | 34s | 54s | 74s |

`WatchdogSec=150` covers the 3-generic-slot bounded worst case (134s) plus
margin. Chains longer than three generic slots, or rule configs with raised
`wake_retries`/`wake_retry_backoff` values, exceed this budget and need
`WatchdogSec` (and the table above) re-derived and tuned upward alongside
them.

### End-to-end rollback bound

Putting the watchdog cadence and the crash-loop threshold together gives an
honest bound on how long a wedge-after-reload config can leave a screen
stuck before boot rolls it back:

one restart cycle = `RestartSec` (2s) + boot/probe assembly (5–20s) +
wedge-detect (≤150s) ≈ **157–172s**

The third same-fingerprint start (the one that trips counted rollback)
lands roughly two cycles in — **≈314–344s**, comfortably inside the
6-minute (360s) crash-loop window. Worst case, that's **≈5.2–5.7 minutes**
(call it 6 minutes with scheduling slop) of a config staying wedged before
the daemon rolls itself back to the LKG. During that window a blanked
screen stays blanked (a wedged engine can't wake it) — bounded and
documented, and strictly better than the pre-feature behavior of retrying
the same bad config every 2 seconds forever. `dormantctl emergency-wake`
(above) is the immediate escape at any point during that window.

### Slow hardware

The bound above assumes the default controller-retry timings. On slower
hardware — or with per-rule `wake_retries`/`wake_retry_backoff` overrides,
or longer controller chains than the table above covers — a single measured
retry cycle can run past the assumed 5–20s boot/probe figure. If that
happens, the third same-fingerprint start can miss landing inside one
restart-doubling and instead take one extra restart to trip the threshold —
the crash-loop detector still fires, just one cycle later than the ideal
bound above.

## Why the crash-loop thresholds aren't config

`CRASH_LOOP_THRESHOLD` (3 starts), `CRASH_LOOP_WINDOW` (6 minutes), and
`LKG_HEALTH_DEFER_CAP` (3 consecutive deferrals) are documented `pub const`
values in `boot_guard.rs`, not `[watchdog]` config keys. This is a
deliberate exception to "everything is configurable": this machinery has to
work correctly at the exact moment the config might be unloadable or
unparsable — reading a threshold out of the very file that might be the
problem isn't a safe design. `systemd`'s own `StartLimitBurst` exists at
the unit layer and can stop the restart loop outright, but it can't
substitute a known-good config; the unit file's comments note the
coupling.

## State files

Watchdog and rollback state lives directly under the daemon's state
directory root (`$XDG_STATE_HOME/dormant`, falling back to
`~/.local/state/dormant`) — a sibling of, not nested inside, the
`wear/` subdirectory panel-wear tracking uses:

- `last-known-good.toml` — a byte-for-byte copy of the config that most
  recently earned promotion. Directly loadable by the normal config loader
  — no wrapper format.
- `last-known-good.meta.json` — an advisory sidecar (schema version,
  fingerprint, save timestamp, and whether it was saved from `"boot"` or
  `"reload"`). Every load-bearing "is this the LKG?" comparison in the code
  uses a direct byte comparison of the two files, never this sidecar — it
  exists for log attribution only.
- `crash-loop.json` — the rolling record of recent daemon starts
  (fingerprint + timestamp + a per-start nonce), plus whether a rollback is
  currently active and which config fingerprint it was rolled back from.
  Capped at the 10 most recent entries. A corrupt or missing file is
  treated as empty rather than blocking boot.
- `discount-<nonce>` — a small marker file created when two daemon
  instances race for the single-instance lock; the loser's start record is
  disowned (never counted toward a crash loop) via this file rather than by
  rewriting `crash-loop.json` in place. Swept once stale.

All of these directories are `0700` and files `0600` — no privilege
boundary is crossed reading them back (the LKG file is loaded through the
same config loader, at the same trust level, as any other config file you
point the daemon at).

## Log events

Every event name below is a literal string at its definition site — grep
for `event = "<name>"`:

- `crash_loop_detected` — a real same-config crash pattern was seen but
  didn't trigger rollback for some other reason (no LKG, bytes already
  match the LKG, a rollback is already active, or the gate is off).
- `config_rollback_boot` — a rollback boot fired (immediate or counted).
- `config_rollback_continued` — a sticky-rollback boot during an ongoing
  storm.
- `config_rollback_retry` — the quiet-period retry of the original config
  after `CRASH_LOOP_WINDOW` has passed with no new start.
- `lkg_missing_rollback_disarmed` — `rollback_active` was set but the LKG
  is now missing or fails to validate; the flag is cleared loudly rather
  than looping silently.
- `lkg_saved` — a candidate was promoted to LKG.
- `lkg_save_failed` — writing the LKG file failed; retried next tick.
- `lkg_skipped_dirty` — promotion skipped because the on-disk config no
  longer matches the running candidate.
- `lkg_deferred_display_health` — promotion deferred because at least one
  display's controllers are all unhealthy.
- `lkg_promoted_with_unhealthy_display` — the display-health defer cap was
  reached; promotion proceeded anyway.
- `watchdog_probe_failed` — the engine didn't answer a liveness probe; the
  watchdog ping was withheld for that tick.
- `sd_notify_unavailable` — `sd_notify` couldn't reach `NOTIFY_SOCKET`;
  further attempts are disabled (this is expected and harmless on a
  non-systemd setup).

## Config

The `[watchdog]` section (all three keys optional — shown values are
defaults):

```toml
[watchdog]
lkg_enabled = true          # last-known-good snapshotting + boot rollback
lkg_rollback_enabled = true # counted crash-loop rollback; disable during rapid restart debugging
stability_window = "5m"     # healthy runtime before a config is promoted to LKG; >= 30s
```

`WatchdogSec` and the probe/ping cadence are **not** config — they come
from `systemd`'s own environment (`NOTIFY_SOCKET`/`WATCHDOG_USEC`) by
design, so a non-systemd install pays nothing for this feature. The
crash-loop constants above aren't config either, for the reasons given
above.

> **Warning — install the new binary before the updated unit, not after.**
> If you're upgrading to a dormant release that includes this feature, the
> order matters:
>
> 1. Install the new `dormantd` binary first.
> 2. Run `systemctl --user daemon-reload` to pick up the updated unit file
>    (`Type=notify` + `WatchdogSec=150`).
> 3. Restart: `systemctl --user restart dormant`.
>
> If you do this backwards — reloading the unit while the **old**
> `Type=simple` binary is still installed — `systemd` will wait for a
> `READY=1` notification that the old binary never sends (it doesn't know
> about `sd_notify` at all), time out, and kill the daemon. Binary first,
> then unit, then restart.
