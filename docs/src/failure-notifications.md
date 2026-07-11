# Failure notifications

A silent control failure on a display you're not looking at is invisible —
the daemon logs it and keeps retrying, but nothing tells the operator. When a
wake command keeps failing or a blank command exhausts its whole controller
chain, `dormant` surfaces it on three independent surfaces:

- a **desktop notification** over the session D-Bus (`org.freedesktop.Notifications`),
- a **tray icon state** (`Failure`, outranking `Paused`) with a badge and
  tooltip detail, and
- a **web-dashboard failure banner** on the Dashboard view.

The tray and web-dashboard surfaces derive directly from the per-display
snapshot (`wake_attempts > 0 || last_blank_failed`) and always reflect the
truth regardless of the `[notifications]` config — only the desktop
notification is gated by `[notifications] enabled`. Disabling notifications
silences the desktop popups only; the tray icon and dashboard banner keep
showing a failing display.

## What fires and when

| Trigger | Gate | Urgency | Recovery notice |
|---|---|---|---|
| Wake failure | consecutive wake attempts reach `wake_attempt_threshold` (default `3`) | Critical | Yes, if `notify_recovery` |
| Blank failure | a blank command exhausts its controller chain — **one-shot, no threshold** | Critical | Yes, if `notify_recovery` |
| Recovery (wake or blank) | the display succeeds its wake/blank command after a prior failure notice | Normal | gated by `notify_recovery` itself |

Two details worth being precise about:

- **The threshold applies to wake failures only.** Each failed wake attempt
  increments a per-display counter; a notification fires only once that
  counter reaches `wake_attempt_threshold`. Attempts below the threshold are
  logged (`notify_suppressed`, reason `BelowThreshold`) but produce no
  desktop popup. Blank failures have no threshold at all — the first blank
  command that exhausts its whole controller chain notifies immediately.
  (This means the tray/web failure state, which fires on `wake_attempts > 0`,
  can go "red" before the desktop notification threshold is reached — the
  tray and dashboard are more sensitive by design.)
- **The cooldown applies to both.** Once a failure notification for a
  display has fired, a repeat of the *same* kind of failure on the *same*
  display within `cooldown` (default `15m`, floor `1m`) is logged
  (`notify_suppressed`, reason `Cooldown`) instead of re-notifying — the
  existing notification is left in place. After the cooldown window passes,
  the next failure replaces the prior notification (same D-Bus notification
  id) rather than stacking a new one.

## Silencing it

Desktop notifications alone can be turned off without touching anything
else:

```toml
[notifications]
enabled = false
```

With `enabled = false`, the notifier task is never spawned — no D-Bus
connection, no session-bus traffic, no notification of any kind. The tray
icon and the web dashboard's failure banner are unaffected: they read the
same per-display failure state directly from the engine snapshot, not from
the notifier.

## Why wake failures are critical urgency

Failure notifications (wake and blank) use the freedesktop `critical`
urgency hint; most desktop notification daemons persist a critical
notification until the user dismisses it, rather than letting it time out
and vanish. Recovery notices use `normal` urgency, since they are
informational rather than something the operator must act on.

This asymmetry is deliberate: a display that won't wake is the worst failure
mode a presence daemon can have — the whole point of `dormant` is that the
screen comes back the instant you're back in the room, and a wake failure
means it silently didn't. A notification that quietly expires while the
operator is away from their desk defeats the point, so wake and blank
failures are pushed as loud, sticky notices; a recovery is a "for your
information," not an emergency.

## Reload carry-over semantics

Failure state and open notification episodes are daemon-lifetime, not
generation-lifetime — a config reload does not reset them:

- The notifier's episode bookkeeping (`NotifyState` — one open episode per
  `(display, kind)`, the D-Bus notification id it maps to, and the cooldown
  clock) is constructed once in `App::start` and threaded unchanged through
  every reload generation, exactly like the reload-surviving `ZbusSink`
  connection. A reload does not close or re-open a still-failing display's
  notification.
- The rules engine's own per-display `wake_attempts` / `last_blank_failed`
  counters are seeded into the freshly-built generation from the old
  generation's snapshot, so an in-flight failure survives a reload as far as
  the engine is concerned too.

**The dispatch-relevant voiding rule.** A reload can change *how* a display
is driven — its controller chain, blank/wake commands, DDC/CI target,
Home Assistant service calls, and so on. If that happens, the failure
evidence accumulated under the *old* dispatch logic is no longer a
trustworthy signal about the *new* one, so it is voided rather than carried
forward: before a display's failure counters are seeded into the new
generation, they are zeroed if the display's dispatch-relevant config
changed (controllers, blank/degraded mode, ladder, output/DDC target,
host/WoL MAC, blank/wake command or service+data, controller `modes`,
command timeout, or the unreachable-treated-as-blanked flag) — or if the
display was added or removed outright (no baseline to compare against).
Fields that don't affect how a command is dispatched (`screensaver`,
`restore_brightness`, `samsung_restore_backlight`, `panel_type`) never
trigger voiding.

When a display's evidence is voided this way, the notifier's post-reload
reconciliation sees it reporting healthy and closes any notification that
was open for it — but **without** a recovery notice. Reconciliation never
emits a recovery notice under any circumstance (unlike a genuine wake/blank
recovery event caught live); this is intentional, because voided evidence
isn't a real recovery — the config changed, it wasn't fixed. The same
no-recovery-notice rule applies to a display removed from config entirely:
its open notification is closed silently.

## The daemon-restart limitation

Everything above only holds within one running daemon process — a config
*reload* swaps generations in place, but a full **daemon restart** (killing
and restarting `dormantd`) starts over with nothing:

- `NotifyState` — the notifier's open-episode bookkeeping and D-Bus
  notification ids — lives only in daemon process memory. It is never
  persisted to disk (there is no on-disk equivalent of the wear ledger's
  `$XDG_STATE_HOME/dormant/wear` for notification state).
- The rules engine's `wake_attempts` / `last_blank_failed` counters are
  likewise pure in-memory bookkeeping, with no persisted state file. A fresh
  process starts every display at `wake_attempts = 0`,
  `last_blank_failed = false`.

So a failure that was in flight when the daemon was killed does not
re-surface after it restarts, even if the underlying hardware problem is
still there — the notifier's startup reconciliation only ever sees the
fresh (healthy-looking) snapshot from the new process. The failure has to
recur and re-accumulate past the threshold before it notifies again. This is
a known gap, not a design choice to hide restarts — flag it if it becomes a
real operational pain point.

## Privacy: session bus only

Desktop notifications talk to exactly one thing: the local
`org.freedesktop.Notifications` service on the user's own D-Bus **session**
bus (never the system bus, never a network socket). There is no telemetry,
no external process, no data leaving the machine — consistent with the
project's no-telemetry, no-phone-home stance.

Every D-Bus call (connect, `Notify`, `CloseNotification`) is bounded by a
2-second timeout. If the session bus is unreachable, the notifier logs
`notify_unreachable` once and backs off for 60 seconds before trying again,
rather than retrying in a tight loop or blocking anything else the daemon is
doing. The notification's application identity (both the app name and the
app icon hint passed to `Notify`) is `"dormant"`.

## Configuration reference

```toml
[notifications]
enabled = true                  # kill-switch; false = no notifier task, no D-Bus I/O
wake_attempt_threshold = 3      # consecutive wake failures before notifying (>= 1)
cooldown = "15m"                # minimum time between repeat notices per display (>= 1m)
notify_recovery = true          # send a Normal-urgency notice when a failing display recovers
```

See the commented `[notifications]` block in `examples/config.toml` for the
same keys with inline explanations.
