# OLED health

dormant can track how much on-time each display has accumulated, weighted by
brightness, and surface it in the web dashboard as a "panel exposure" card.
This is a measurement and advisory feature — **v1 tracks and reports; it does
not compensate.** Nothing here changes when or how a display blanks or wakes.

## What is tracked

For every display with a controller that can report brightness (`ddcci`,
`samsung-tizen`), the wear tracker samples panel state on a schedule
(`wear.sample_interval`, default 60 s) and accumulates a running total:

- **`total_on_hours`** — cumulative brightness-weighted on-time. An hour at
  full brightness counts as a full hour; an hour at half brightness counts as
  half an hour. Time spent blanked, blanking, or waking counts as zero. Time
  spent on the `render_screensaver` ladder stage counts at a fixed discount
  (`wear.screensaver_factor`, default `0.35`) rather than a real brightness
  read.
- **`seeded_usage_hours`** — for `ddcci` displays, the panel's own lifetime
  usage counter (VCP `0xC0`) is read once, when the ledger is first created,
  and stored alongside dormant's own total. This lets a panel that already
  had hours on it before dormant started tracking show a truthful "prior
  usage" figure instead of implying it started brand new.
- **`last_long_dwell_epoch_s`** — the last time the display completed a
  blanked dwell of at least `wear.short_cycle_dwell` (default 10 minutes).
  This is the basis for the compensation advisory below.

Attribution runs against a low-resolution logical grid overlaid on the panel
(`wear.grid_rows` × `wear.grid_cols`, default 9×16), because the ledger format
is designed to support *spatial* wear attribution later. **In v1, every cell
in the grid always receives the exact same value** — there is no per-region
tracking yet, so the "heat map" the ledger format supports is, honestly, flat.
See [the v1 uniform-exposure note](#v1-is-a-uniform-exposure-ledger-not-a-heat-map)
below.

## What is NOT tracked

- **No spatial/regional wear.** Nothing about *which part* of the screen was
  lit is tracked in v1 — see above.
- **No content awareness.** dormant does not know what was on screen (a
  static UI vs. a moving video vs. a screensaver slideshow), only the ladder
  stage and the brightness level. The `render_screensaver` discount is a
  fixed factor, not a measurement.
- **No panel-type-specific wear math.** A display's `panel_type`
  (`woled` / `qd-oled` / `unknown`) is config-declared and stored in the
  ledger, but v1's attribution formula does not yet branch on it — it is
  recorded now so a later version can weight WOLED and QD-OLED aging
  differently without a schema break.
- **No compensation enforcement.** dormant does not hold a display blanked
  longer, force a "rest cycle", or change any blank/wake timing based on wear
  data. The advisory (below) is a UI nudge, nothing more.
- **No panel-type auto-detection.** `panel_type` is set by the operator in
  config; dormant never probes the panel to guess it (some monitors report
  misleading VCP capability strings, so a guess would be worse than no
  answer).

## Where the data lives

Each tracked display gets its own JSON file under the daemon's state
directory:

```
$XDG_STATE_HOME/dormant/wear/          # if XDG_STATE_HOME is set
~/.local/state/dormant/wear/           # fallback otherwise
```

The filename is derived from the display's stable identity (the DDC/CI
panel's EDID-derived manufacturer/model/serial where available, or
`samsung:<host>` for a Tizen TV, or the config display name as a last
resort), sanitized to lowercase ASCII — not a hash — so the file is directly
readable from its name:

```
$ ls ~/.local/state/dormant/wear/
wear-ddc-aoc-ag326uzd-xk2r9ja000013.json

$ cat ~/.local/state/dormant/wear/wear-ddc-aoc-ag326uzd-xk2r9ja000013.json
{
  "schema_version": 1,
  "identity": {
    "key": "ddc-aoc-ag326uzd-xk2r9ja000013",
    "display_name": "main_monitor"
  },
  "panel_type": "qd-oled",
  "grid_rows": 9,
  "grid_cols": 16,
  "cells": [ { "wear_hours": 42.7 }, "... 143 more, all equal in v1 ..." ],
  "total_on_hours": 42.7,
  "seeded_usage_hours": 966,
  "sample_count": 2563,
  "last_sample_at_epoch_s": 1783720800,
  "last_long_dwell_epoch_s": 1783634400,
  "advisory_baseline_epoch_s": 1780000000
}
```

The file is written atomically (temp file + rename) with mode `0644` — there
are no secrets in it. It is never pruned: an orphaned ledger (e.g. after a
display is renamed or a panel is disconnected) stays on disk, inert and
auditable, rather than being silently deleted.

Persistence happens every `wear.persist_interval` (default 5 minutes), on
daemon shutdown, and whenever tracking is disabled at runtime — the
in-memory ledger is authoritative between writes, so a crash loses at most
one persist interval's worth of accumulation.

## The compensation advisory

The web dashboard's panel-exposure card shows a line like:

> no long standby window in 5 days

This fires when a display has gone longer than `wear.advisory_after`
(default 96 hours) since its last qualifying blanked dwell — the same
`hours_since_long_dwell` value the advisory is derived from is included in
every `GET /api/wear` response, not just pushed over the WebSocket, so a
freshly-opened dashboard always shows the truth even if it missed the
original notification.

**What the advisory means:** it is a hint that this display has been
continuously lit (at some brightness) for a while, in case the operator
wants to manually give it a rest — turning it off overnight, for instance.
**What it does not mean:** dormant is not about to do anything about it.
There is no enforcement, no automatic blank, no state-machine change. A
display that has *never* had an observed long dwell (the common case right
after dormant starts tracking it) still gets a real day count, computed from
the ledger's creation time rather than from a missing observation — it is
never rendered as an unhelpful "no long standby window in `?` days".

## v1 is a uniform-exposure ledger, not a heat map

The wear ledger's grid shape (`grid_rows` × `grid_cols` cells, a
`heat_map()` accessor, a viridis-style rendering component in the web UI)
exists to support genuine per-region wear tracking. **v1 does not do that
yet.** Every sample attributes the exact same value to every cell — the
grid is a placeholder for a real spatial model, not a live one. The web UI
is deliberately honest about this: the panel-exposure card leads with the
panel-wide `total_on_hours` number, not the grid, and the grid itself
renders with the caption "no spatial attribution yet — arrives with
content-aware tracking (v2)". If a future version tracks which region of
the screen was actually lit, the same file format and the same UI component
carry it — no schema break, no re-migration of existing ledgers.

## Privacy

Wear tracking is entirely local:

- Ledger files live only on the machine running `dormantd`, under the
  daemon's own state directory.
- The only network-facing surface is the existing loopback-only web
  dashboard (`GET /api/wear`, `GET /api/wear/<display>`) — see
  [Web UI](./web-ui.md) for its security posture. Nothing is pushed
  anywhere else.
- There is no telemetry, no analytics, no cloud sync, and no export path.
  Deleting the `wear/` directory removes all wear history; dormant will
  simply start a fresh ledger for each display the next time it samples one.

## Configuration

Tracking is enabled by default; set `wear.enabled = false` to turn it off —
no ledger files are opened or created, and no panel reads happen, while it
is off.

```toml
[wear]
enabled = true                 # kill-switch; false = tracker parks, no I/O
sample_interval = "60s"        # how often to sample panel brightness
persist_interval = "5m"        # how often to write ledgers to disk
read_timeout = "2s"            # per-sample read budget (never blocks a wake)
grid_rows = 9                  # logical grid rows (v1: uniform, see above)
grid_cols = 16                 # logical grid columns
fallback_brightness = 0.5      # assumed brightness when a real read fails
screensaver_factor = 0.35      # attribution discount while on the screensaver stage
short_cycle_dwell = "10m"      # blanked dwell long enough to count as a "rest"
advisory_after = "96h"         # how long without a qualifying dwell before advising

[displays.monitor]
panel_type = "qd-oled"         # "woled" | "qd-oled" | "unknown" (default) — recorded only, see above
```

See the commented `[wear]` block in `examples/config.toml` for the same keys
with inline explanations.
