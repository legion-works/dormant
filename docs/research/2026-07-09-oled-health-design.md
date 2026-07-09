# OLED Health — wear tracking, heat map & compensation coordination (design note)

Status: **design, not built.** This is the fleshed-out design for the "OLED
health" roadmap direction (wear ledger + heat map + wear-evening screensaver +
compensation-cycle coordination). It is grounded in the prior-art survey at
[`2026-07-09-oled-health-prior-art.md`](./2026-07-09-oled-health-prior-art.md)
— read that first for the wear model, TV-firmware mechanisms, and the DDC/CI
telemetry landscape it draws on. Produced by a cross-model design pass; captured
here so the design survives to a proper spec → council → build later.

Hard constraints inherited from the project: no telemetry, no cloud, no
phone-home; the wake path is sacred (health optimization never blocks or delays a
wake); fail-safe presence is never weakened. The whole feature is a **passive
observer + light-touch active steerer** layered on the existing state machine — it
adds no sensor, sends no command to the engine on the hot path, and is gated
behind a default-on `wear` Cargo feature and a `wear.enabled` config kill-switch.

## The four parts

### 1. Wear ledger (data model + persistence)

A per-display `WearLedger` is the single source of truth: a brightness-weighted
on-hours accumulator indexed by a **low-resolution logical grid** mapped onto the
panel's active area (logical, not pixel-bound, so a resolution change never
invalidates it).

- **Structure**: stable hardware key (see Open defaults), `panel_type` (`WOLED` |
  `QDOLED` | `Unknown` — from DDC/CI VCP `0xB6`, EDID heuristics, or config
  override), `grid_rows × grid_cols`, `cells: Vec<WearCell>` (row-major),
  `total_on_hours`, `sample_count`, `last_sample_at`, `schema_version`. Each
  `WearCell` holds `wear_hours: f64`, `blue_wear_factor: f64` (WOLED ≈ 1.2–1.5×
  for pure-white cells since blue ages fastest; QDOLED = 1.0), and
  `peak_brightness` (diagnostic).
- **Storage**: one JSON file per display under `$XDG_STATE_HOME/dormant/wear/`
  (fallback `$HOME/.local/state/dormant/wear/`), atomic write-then-rename. ~7 KB
  per display at 16×9 — negligible, never pruned, `cat`-auditable.
- **Loading**: `load_or_default` returns a zero grid when the file is missing,
  corrupt, or a `schema_version` mismatch (corrupt files renamed `*.corrupt`).
- **Heat map** is a *derived view* (`wear_hours` min-max normalized to `[0,1]`),
  computed lazily on read — not a second persisted object.

### 2. Wear sampler (pure consumer of the snapshot stream)

`WearTracker` is a self-contained tokio task spawned by `App::start` when
`wear.enabled`. It **only reads** — never sends commands to the engine.

- **Loop**: sleep `sample_interval_secs`, read the latest `StateSnapshot` (via a
  lagging `watch`/`broadcast` consumer — wear is cumulative, a missed tick is
  harmless), and for each `active` display attribute `active_duration ×
  brightness_factor` to cells, then atomically persist.
- **Brightness sampling, three tiers**: DDC/CI VCP `0x10` via a *short-lived
  secondary* I2C handle (never blocks the primary blank/wake connection); Samsung
  IP `backlightControl` read; and `fallback_brightness` (default 0.5 — an honest
  medium assumption) for `command`/`kwin-dpms`/`ha-passthrough` which can't read.
  Trait extension: an additive `read_state()`/`read_brightness()` default-`None`
  method (DDC/CI + Samsung override) — the same additive-trait shape used
  elsewhere in the codebase.
- **Fail-safe**: each sample runs under a 2 s `tokio::time::timeout`; a hang / I/O
  error / busy bus logs a `WARN` (`wear_sample_timeout` / `wear_persist_failed`)
  and the next sample continues. The in-memory ledger is authoritative, so a
  dropped write is just a delayed write. The wake path never blocks on telemetry.
- **Attribution strategies** (per-display `region_strategy`): `uniform` (v1 — whole
  active duration applied to all cells; the Wayland reality is that dormant cannot
  see desktop content, so desktop-time wear is uniform); `static-bias` (v2 —
  operator-declared screen-relative regions like `taskbar-bottom` with bias
  factors); `content-aware` (v3 — only for the screensaver, where libmpv can
  report tile luma cheaply because dormant owns the frame). The key insight both
  design passes reached independently: **uniform during `Active`, exact per-region
  during `Blanked`/`Staged`** — dormant only knows precisely what it renders itself.
- **Crash safety**: on restart, attribute `min(elapsed_since_last_persist,
  sample_interval)` for the interrupted span — undercount is harmless, overcount
  would misdirect wear-evening.

### 3. Wear-evening render steering (dormant's own surfaces only)

Never touches desktop compositor content — only the black overlay + libmpv
screensaver surfaces.

- **Micro-shift** (invisible): full-surface pixel shift on the layer-shell surface,
  default `step = 2 px`, `interval = 120 s`, raster-scan, `max_radius = 4 px`
  (the AOSP `BurnInProtectionHelper` parameters, proven at scale). On a black
  surface it's invisible; on a 3840 px panel a 2 px move is 0.05 %. Runs only
  during `Staged`; a no-op when the surface is torn down (`Active`).
- **Macro-pan / content placement** (v2+): playlist `order: "wear-even"` scores
  remaining tracks against the current heat map (per-image luminance pre-scan
  cached in memory; per-video operator `wear_tag: dark|medium|bright`). An
  alternative expression is libmpv `video-pan-x`/`video-pan-y` panning the viewport
  toward the coldest cells — gated on a compositor-stutter probe. Real-time
  per-frame video analysis is deferred.

### 4. Compensation-cycle coordination

Firmware/T-CON compensation cycles are **opaque** — no DDC/CI, CEC, or Samsung IP
command triggers or queries them. dormant can only make the powered-standby window
long enough when one occurs.

- **Dwell enforcement**: `Blanked` enforces a minimum dwell before any downstream
  hard-power-cut (reuse the reserved `min_blank_time`). Wake-on-presence always
  wins — a cycle may be interrupted if someone walks in; that is the correct
  tradeoff.
- **Prefer `picture_off` / DPMS-off over mains power-off near the boundary**: when
  `total_on_hours` is within ~30 min of the configured `short_cycle_hours` (default
  4 h), prefer the panel-off-but-powered path (Samsung `KEY_PICTURE_OFF` /
  `backlightControl 0`; DDC/CI `0xD6` is already this) so the panel can run its
  cycle.
- **Advisory only**: emit `CompensationAdvisory { display_id, total_hours }` on a
  boundary crossing; the web UI / tray show a non-intrusive banner and the last
  blanked-window dwell ("12 min ✓ likely completed" vs "2 min ✗ may have been
  interrupted"). **Never** a "run cycle now" button — the research shows manual
  triggering accelerates degradation (LG caps it at ~once/month).

### 5. Web UI surface

Loopback-only, same security boundary as the rest of the dashboard.
`GET /api/wear` (summary) + `GET /api/wear/{display_id}` (full heat map JSON);
`DaemonEvent::WearSnapshot` pushed per tick over the existing `/api/events` WS;
an HTML `<canvas>` heat-map overlay on each display card (viridis ramp — a
diagnostic, not an alarm; no red "WARNING").

## Open defaults (UX calls to settle at spec time — the two design passes converged on architecture, differed here)

- **Grid resolution**: 16×9 (144 cells) vs 32×18 (576). Support a 4–64/axis range; pick a default.
- **Stable storage key**: EDID hash (local DP/DDC) + MAC/entity-id (network) so the ledger follows the *panel*, not the `DP-1` connector across a GPU-port swap. This is the substantive requirement; the config `DisplayId` is the operator-facing name.
- **Short-cycle dwell**: 10 min (near Samsung's published 7–10) vs 15 min (more conservative).
- **Sampler interval**: 30 s (catches short spans) vs 60 s (lighter on the I2C bus).

## Unknowns — VERIFY ON HARDWARE before/early in build

### Resolved 2026-07-09 — DDC/CI probe on the AOC AG326UZD (DP-1, `/dev/i2c-4`, VCP 2.2, daemon live during probe)

- **VCP `0x10` (Brightness) → abstract 0–100, NOT nits.** `current = 100, max = 100`. The attribution scale is a 0–100 fraction, not absolute luminance. ✓ resolved.
- **VCP `0xC0` (Display Usage Time) → SUPPORTED, reads `966 hours`.** The ledger CAN seed from the panel's own power-on-hours counter on first startup instead of zero. ✓ resolved — a real win.
- **VCP `0xB6` (Display Type) → reports `LCD (active matrix)` (sl=0x03), NOT OLED.** Do NOT rely on `0xB6` to identify panel type on this unit — it either mis-reports (common firmware laziness) or the AG326UZD is genuinely not an OLED panel (see the note below; this needs an independent confirm). Panel type must come from EDID heuristics / model lookup / config override, not `0xB6`. Adjacent codes present that may help: `0xB2` (flat-panel sub-pixel layout), `0xC8` (display controller type). ✓ resolved (as "don't trust 0xB6").
- **I2C bus contention → NEEDS a shared mutex / single DDC multiplexer, NOT an independent secondary handle.** Baseline `getvcp 0x10` ×15 (daemon idle): min 243 / median 245 / max 251 ms, 0 errors. During ONE concurrent `setvcp 0xD6 0x05`→wait→`0x01` write: median 267 ms but spikes to **2370 ms** (plus 1100/651/626 ms), 0 errors. No read errors, but write-window latency blows far past the 50 ms threshold — a wear-sampler reading brightness must share the daemon's DDC connection/mutex so its reads don't collide with power-mode writes. ✓ resolved.
- **Note — baseline DDC read latency is ~245 ms** even idle on this monitor. Cheap enough for a 30–60 s sampler, but the exercise sequence's ~6 reads cost ~1.5 s of the read budget; factor it into any per-read timeout.

### Still open

- **Is the AG326UZD actually an OLED?** `0xB6` says LCD, and the operator previously observed that brightness-0 only *dims* (emission floor) rather than going near-black — both signals are LCD-like. If the desk monitor is genuinely LCD/MiniLED, wear-tracking IT is moot (LCDs don't burn in) and OLED-health's real target on this setup is the Samsung S90D. Does not invalidate the feature (the S90D and other users' OLED monitors are valid targets) — it's a targeting nuance. NEEDS operator confirmation of the actual panel technology.
- **Samsung S90D IP Control `0x08` Maintenance Control** — does it return burn-protection timer data? `0x25` brightness read via `samsung_ip.rs`. (Not probed yet — TV may be off.)
- **Real brightness-to-wear curve** — the linear `brightness × hours` model is the Steam-Deck back-of-envelope; leave a pluggable `WearModel` trait for a better curve later (published T95-at-nits curves are typically under NDA).
- **libmpv panning smoothness** and **4K image pre-scan cost** (>100 ms → background task) — gate the v2 macro-steering paths.

## Suggested phasing (not a committed plan)

- **v1 — minimal shippable**: ledger with `uniform` attribution + JSON persistence, DDC/CI + Samsung IP brightness sampling (+ `fallback_brightness`), heat map in the web UI, full-surface micro-shift on the render overlay, `CompensationAdvisory`, a `[wear]` config section. Self-contained, zero wake-path impact; the heat-map UI alone justifies shipping.
- **v2**: `static-bias` regions, `wear-even` playlist order, `panel_type` detection, libmpv macro-pan prototype (gated).
- **v3**: content-aware screensaver attribution, long-cycle dwell tracking, tray integration, a `wlr-screencopy` 1-frame-per-10-min desktop sampler spike.
- **Skip forever**: per-subpixel tracking, host-triggered compensation cycles, telemetry/cloud, any "run cycle now" control.

## Build dependency order (when it's specced)

1. Additive `read_state()`/`read_brightness()` on the `DisplayController` trait (default `None`).
2. DDC/CI (secondary I2C handle) + Samsung IP brightness-read impls.
3. `crates/dormant-core/src/wear.rs` — pure data types (`WearLedger`/`WearCell`/`WearHeatMap`/`WearConfig`/`PanelType`), no I/O, behind the `wear` feature.
4. `paths::wear_state_dir()`.
5. `defaults.rs` knobs + `WearConfig` on `DaemonConfig` + `validate_wear`.
6. `crates/dormantd/src/wear_tracker.rs` sampler task, spawned in `App::start`.
7. `DaemonEvent::WearSnapshot` + `CompensationAdvisory`.
8. Web routes + heat-map `<canvas>` component.
9. `dormant-render` micro-shift timer in the surface lifecycle.
10. `dormant-render` libmpv macro-pan (v2+, gated on the stutter probe).
