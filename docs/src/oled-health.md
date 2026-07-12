# Panel-wear tracking

dormant records brightness-weighted panel on-time and shows it in the web
dashboard. v1 measures and advises; it does not alter blank/wake timing.

## What it records

For displays with readable brightness (`ddcci` and `samsung-tizen`), the tracker
samples panel state every `wear.sample_interval` (default `60s`) and records:

- `total_on_hours` — brightness-weighted on-time. One hour at 50% brightness
  adds 0.5 hours. Blanked, blanking, and waking time adds zero.
- `seeded_usage_hours` — a DDC/CI panel's lifetime VCP `0xC0` counter, read once
  when a new ledger is created and available.
- `last_long_dwell_epoch_s` — the last blanked dwell lasting at least
  `wear.short_cycle_dwell` (default `10m`).

`render_screensaver` uses the fixed `wear.screensaver_factor` (default `0.35`)
instead of brightness readback.

The ledger has a `wear.grid_rows` × `wear.grid_cols` grid for future spatial
attribution. v1 writes the same value to every cell. It does not know which
region or content was shown.

`displays.<id>.panel_type` accepts `"woled"`, `"qd-oled"`, or `"unknown"` and is
stored in the ledger. v1 does not change its wear formula by panel type and does
not auto-detect the value.

## Advisory

The panel-exposure card reports how long the display has gone without a blanked
dwell of at least `wear.short_cycle_dwell`. After `wear.advisory_after` (default
`96h`), it shows an advisory such as:

> no long standby window in 5 days

The day count is also returned by `GET /api/wear`. The advisory never forces a
rest window, blank, or state-machine transition.

## Ledger files

Each display gets one JSON ledger under:

```text
$XDG_STATE_HOME/dormant/wear/          # when XDG_STATE_HOME is set
~/.local/state/dormant/wear/           # fallback
```

The filename uses the stable display identity where available: DDC/CI EDID
manufacturer/model/serial, `samsung:<host>`, or the config display id. Writes are
atomic and mode `0644`; the ledger contains no credentials.

dormant persists every `wear.persist_interval` (default `5m`), on shutdown, and
when tracking is disabled at runtime. A crash can lose at most one persistence
interval. Orphaned ledgers are not pruned automatically.

Delete the `wear/` directory to erase the history. New ledgers are created on
the next sample.

## Configuration

Tracking is enabled by default. While `wear.enabled = false`, the tracker takes
no samples and performs no ongoing ledger I/O.

```toml
[wear]
enabled = true
sample_interval = "60s"
persist_interval = "5m"
read_timeout = "2s"
grid_rows = 9
grid_cols = 16
fallback_brightness = 0.5
screensaver_factor = 0.35
short_cycle_dwell = "10m"
advisory_after = "96h"

[displays.monitor]
panel_type = "qd-oled"
```

| Key | Default | Description |
|---|---|---|
| `wear.enabled` | `true` | Enable tracking and ledger I/O |
| `wear.sample_interval` | `"60s"` | Panel-state sample interval |
| `wear.persist_interval` | `"5m"` | Ledger write interval |
| `wear.read_timeout` | `"2s"` | Read budget for one panel sample |
| `wear.grid_rows` | `9` | Logical grid rows; uniform in v1 |
| `wear.grid_cols` | `16` | Logical grid columns; uniform in v1 |
| `wear.fallback_brightness` | `0.5` | Brightness fraction when readback fails |
| `wear.screensaver_factor` | `0.35` | Fixed factor during `render_screensaver` |
| `wear.short_cycle_dwell` | `"10m"` | Blanked dwell counted as a long rest window |
| `wear.advisory_after` | `"96h"` | Time without a long rest window before advising |

The tracker is local-only. Its sole network surface is the loopback web API;
there is no telemetry, analytics, cloud sync, or phone-home path.
