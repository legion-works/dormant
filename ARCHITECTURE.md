# Architecture

Crate map, data flow, and where-to-find-it guide for the dormant codebase.

## Crate map

| Crate | Purpose | Has binaries? |
|---|---|---|
| `dormant-core` | Domain types, traits, config schema/validation, zone fusion engine, rules engine, state machine, IPC protocol, reload types, doctor wire types (`DoctorReport`/`Check`/`CheckStatus`), panel-wear ledger model (`wear.rs` — `WearLedger`/`WearIdentity`/`PanelType`/`WearHandle`/identity normalization; `DaemonEvent::WearSnapshot` + `CompensationAdvisory`), ownership gate (`ownership.rs` — `OwnershipGate` trait + `AlwaysOwned` impl, the seam a future multi-instance coordinator would replace without touching the state machine) — pure logic, no I/O | No |
| `dormant-sensors` | Sensor sources: MQTT (`mqtt.rs`), Home Assistant WebSocket (`ha_ws.rs`), USB-serial LD2410 radar (`usb_ld2410.rs`), plus a shared backoff helper and a static registry | No |
| `dormant-displays` | Display controllers: arbitrary shell command (`command.rs`), DDC/CI (`ddcci.rs`), abstract VCP operations (`vcp_ops.rs` — ddc-hi real backend + scripted fake, panic-recovery via `catch_unwind`), per-panel DDC/CI bus lock (`ddc_lock.rs` — `PanelLocks` registry, `VcpPriority::Command` vs `Sampler`, poison recovery), Home Assistant passthrough (`ha_passthrough.rs`), Samsung Tizen (`samsung_tizen.rs` — port 8002 WebSocket + Wake-on-LAN + pairing, with `pair()`'s connect step behind an injectable `PairConnect` trait — `RealPairConnect` for the real WSS handshake, `test_support::FakePairConnect` behind the `test-util` feature for deterministic pairing-route tests in `dormant-web`) plus its audio-safe IP Control G2 JSON-RPC transport (`samsung_ip.rs` — port 1516 `backlightControl` for `brightness_zero`), execution engine with fallback/retry (`executor.rs`), static registry | No |
| `dormant-doctor` | Hardware/connectivity health checks: probes for config, MQTT, HA WebSocket, USB LD2410, DDC/CI, Samsung Tizen (reachability + power state + token presence); live coalesced `DoctorService` for the daemon + web UI; control-path verification — `dormantctl doctor --exercise <display>` routes an `Exercise` IPC request through the daemon, which pauses the target's rules and steps blank→read→wake→read on the live controller chain to confirm each command actually moved the panel. Wire types live in `dormant_core::doctor` to avoid a cycle | No |
| `dormant-web` | Loopback-only web dashboard: axum HTTP/WS bridge that reads live engine state and serves a SPA (`crates/dormant-web/webui/`). Optional dependency of `dormantd`, gated behind the `web-ui` Cargo feature — when off, zero web code is compiled. `config_patch.rs` also gates entity create/delete (`CreateEntity`/`DeleteEntity` patch ops, a per-collection closed creatable-field allowlist, and a reserved-entity-id ban); `routes/pair.rs` is the Samsung pairing wizard (`POST`/`GET /api/pair/samsung[/<id>]`, non-blocking + single-flight over `PairConnect`) | No |
| `dormant-render` | Local Wayland layer-shell `RenderSink`: software-blank black overlay (final ladder fallback when every display controller failed) and libmpv-driven screensaver overlay (last-resort idle surface). Wayland I/O is `target_os = "linux"`-gated; non-Linux builds expose a no-op stub with the same `LayerShellRenderSink` surface so callers compile unconditionally | No |
| `dormantd` | Daemon binary: config loading, event loop, IPC server, inhibit-activity watcher, single-instance flock, reload handling, panel-wear tracker (`wear_tracker.rs` — periodic panel-state sampling through `CommandSink::read_state_sampled`, ledger persistence to `wear_state_dir()`, advisory event emission), desktop failure notifier (`notifier.rs` — pure `decide`/`reconcile` policy over a daemon-lifetime `NotifyState`, `ZbusSink` session-bus I/O boundary), dispatch-relevant reload voiding (`reload.rs::zero_changed_displays`/`dispatch_relevant_eq`), optional web UI spawn, logging | **Yes** — `dormantd` |
| `dormantctl` | CLI binary: `status`, `pause`, `resume`, `blank`, `wake`, `reload`, `validate`, `watch`, `emergency-wake` (IPC fast path with a direct-hardware fallback when the daemon is unresponsive), `doctor` (per-target subcommands including `samsung`, plus `--exercise <display>` for control-path verification), `pair samsung <host>` subcommands. Also re-exports its IPC `client` module as a library entry (`crates/dormantctl/src/lib.rs`) so `dormant-tray` can drive the same protocol without forking the socket glue | **Yes** — `dormantctl` |
| `dormant-tray` | KDE `StatusNotifierItem` tray applet (`ksni`): live icon (Normal / Attention / Paused / Unreachable), pause/resume/blank/wake menu items, tooltip, reconnecting IPC event-stream reader. Linux-only — non-Linux bin prints a notice and exits 1 so cross-platform `cargo check` stays green | **Yes** — `dormant-tray` |

Each crate follows the convention: one module per concept, one file per sensor/controller, explicit static registry with no proc-macro magic.

## Data flow

```
                  ┌──────────────┐
  MQTT ──────────▶│              │
  HA WebSocket ──▶│  Sensors     │──▶ PresenceEvent ──▶
  USB LD2410 ────▶│  (registry)  │                     │
                  └──────────────┘                     │
                                                       ▼
                  ┌──────────────┐           ┌──────────────────┐
  Config ────────▶│  Zone Engine │──▶ Zone   │  Rules Engine /  │
  (schema.rs)     │  (fusion)    │    State  │  State Machine    │
                  └──────────────┘           └──────┬───────────┘
                                                    │
                                          Blank / Wake commands
                                                    │
                                                    ▼
                  ┌──────────────┐
                  │  Executor    │──▶ Controller chain (fallback)
                  │  (retry,     │      ├── kwin-dpms (fallback, audio-unsafe)
                  │   escalation)│      ├── ddcci
                  └──────────────┘      ├── command
                                        ├── ha-passthrough
                                        └── samsung-tizen
```

1. **Sensors** produce `PresenceEvent` values (occupied / vacant) and push them to the zone engine.
2. The **zone engine** fuses events from multiple sensors per zone using the configured mode (`any`, `all`, `quorum`, `weighted`). Unavailable sensors are treated as *present* (fail-safe — never blank a room you can't see).
3. The **rules engine** maps zone state to display commands, applying grace periods, min-blank/min-wake floors, and inhibitor checks (user activity, manual pause).
4. The **display executor** walks an ordered controller chain per display: tries the first controller, falls back on failure, retries wakes with bounded backoff, and escalates to the next controller if all retries are exhausted.

## Where do I look for X?

| Task | Where |
|---|---|
| Add a sensor | `dormant-sensors/src/<name>.rs` (new module) + `dormant-sensors/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config variant) |
| Add a display controller | `dormant-displays/src/<name>.rs` (new module) + `dormant-displays/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config fields if needed) |
| Add a config key | `dormant-core/src/config/schema.rs` (struct field + serde) + `dormant-core/src/config/defaults.rs` (default value) + `dormant-core/src/config/validate.rs` (validation rule AND the `KNOWN_KEYS` known-key-tree entry for unknown-key detection — `mod.rs` only calls `validate::collect_unknown_keys`, it does not hold the tree) |
| Add an error code | `dormant-core/src/error.rs` (`pub const E_*` + variant in `DormantError`) |
| Change timing defaults | `dormant-core/src/config/defaults.rs` (single source of truth) |
| Add a doctor probe | `dormant-doctor/src/probes/<name>.rs` (new probe) + `dormant-doctor/src/probes/mod.rs` + `dormant-doctor/src/lib.rs` (re-export) + `dormantctl/src/cmd_doctor.rs` (CLI dispatch + subcommand) |
| Wire doctor into the daemon | `dormantd/src/app.rs` (construct one `DoctorService` shared by IPC + web UI) + `dormant-doctor/src/service.rs` (coalesced singleflight logic) |
| Add a web route | `dormant-web/src/routes/<name>.rs` (new module) + `dormant-web/src/routes/mod.rs` (mount) + `dormant-web/src/server.rs` (router) |
| Add a CLI subcommand | `dormantctl/src/cmd_<name>.rs` (new file) + register variant + dispatch in `dormantctl/src/main.rs`. Short commands that share a handler (e.g. `pause`+`resume`, `blank`+`wake`) co-locate in one `cmd_*.rs` |
| Add a render ladder stage | `dormant-render/src/<module>.rs` (shared pure logic — `command`, `latch`, `settings`, `playlist`) + `dormant-render/src/linux/<impl>.rs` for the Wayland implementation (Linux-only). The cross-platform `LayerShellRenderSink` re-export is in `dormant-render/src/lib.rs` |
| Add a tray menu/state piece | `dormant-tray/src/<module>.rs` (pure logic — `state`, `tooltip`, `menu`, `icon`; unit-tested without D-Bus); Linux-only glue lives in `dormant-tray/src/tray.rs` and `dormant-tray/src/ipc_loop.rs` |
| Drive the daemon from another binary | Reuse `dormantctl::client` (re-exported via `crates/dormantctl/src/lib.rs`) — `dormant-tray` is the canonical example |
| Adjust panel-wear tracking behavior | `dormant-core/src/wear.rs` (pure model — `WearLedger`/`PanelType`/`sanitize_identity_key`/`brightness_norm`) + `dormant-core/src/config/schema.rs` (the `[wear]` and `[displays.<id>].panel_type` TOML keys) + `dormantd/src/wear_tracker.rs` (pure `tick` + async shell — sampling cadence, ledger I/O, advisory latch) + `dormant-web/src/routes/wear.rs` (HTTP exposure — `GET /api/wear`, `GET /api/wear/<display>`) |
| Add a panel-bus readback / usage-hours seed | Add the per-controller contract to `dormant-core/src/traits.rs` (`DisplayController::read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`), implement on the controller in `dormant-displays/src/<name>.rs`, surface the chain-walk on `dormant-displays/src/executor.rs`; serialized per-panel through `dormant-displays/src/vcp_ops.rs` + `dormant-displays/src/ddc_lock.rs` (command-priority discipline) |
| Adjust failure-notification behavior | `dormantd/src/notifier.rs` (pure `decide`/`reconcile` policy + `NotifyState`/`ZbusSink`) + `dormant-core/src/config/schema.rs` (the `[notifications]` section) + `dormantd/src/reload.rs` (`zero_changed_displays`/`dispatch_relevant_eq` — the reload-voiding gate) + `dormant-tray/src/state.rs` (Failure icon predicate) + `dormant-web/webui/src/app/components/FailureBanner.tsx` (dashboard banner) |
| Adjust entity CRUD (create/delete sensors/zones/displays/rules) | `dormant-web/src/config_patch.rs` (`CreateEntity`/`DeleteEntity` gates, `CREATABLE_FIELDS` per-collection allowlist, `RESERVED_ENTITY_IDS`, `validate_entity_id`) + `dormant-web/src/routes/config_apply.rs` (the `entity_crud_enabled` server-side gate, ahead of the shared apply pipeline) + `dormant-web/webui/src/app/config/{CreateEntityForm,entityCrud}.ts(x)` (client mirror + create form) + `{Sensors,Zones,Displays,Rules}Section.tsx` (Add/Delete affordances, cross-ref dropdowns) |
| Adjust the Samsung pairing wizard | `dormant-web/src/routes/pair.rs` (`POST`/`GET /api/pair/samsung[/<id>]`, `PairStatus`/`PairId`/`Token` redaction, `pair_lock` single-flight, `sweep_expired`) + `dormant-displays/src/samsung_tizen.rs` (`PairConnect` trait, `RealPairConnect`, `pair`/`pair_with_connect`) + `dormant-displays/src/test_support.rs` (`FakePairConnect`, `test-util` feature) + `dormant-core/src/config/mod.rs` (`upsert_samsung_token`, 0600 atomic write) + `dormant-web/webui/src/app/config/PairingWizard.tsx` |

## Event and error-code grep anchors

Every log event name and error code is a literal string at the definition site — never `format!`-constructed, never macro-generated. This makes them reliably greppable:

- **Error codes:** `E_CONFIG_INVALID`, `E_CONFIG_UNKNOWN_KEY`, `E_ZONE_CYCLE`, `E_ZONE_UNKNOWN_MEMBER`, `E_CREDS_PERMS`, `E_CREDS_MISSING`, `E_MODE_UNSUPPORTED`, `E_BLANK_FAILED`, `E_WAKE_FAILED`, `E_RELOAD_WAKE_FAILED`, `E_HA_AUTH`, `E_SENSOR_IO`, `E_DISPLAY_IO`, `E_RENDER_UNAVAILABLE`, `E_SCREENSAVER_SOURCE`, `E_IPC` — all defined in `dormant-core/src/error.rs`.
- **Log events:** grep for `event = "..."` in the source. Key events include `sensor_event`, `zone_transition`, `rule_blank`, `rule_wake`, `wake_failed`, `reload_complete`, `reload_defensive_wake`, `wear_tracker_started`/`_resumed`/`_parked`, `wear_advisory`, `wear_ledger_corrupt`, `wear_ledger_future_version`, `wear_ledger_seeded`, `wear_persist_failed`, `wear_sample_fallback`, `notifier_started`, `notify_sent`, `notify_failed`, `notify_unreachable`, `notify_suppressed`, `notify_events_lagged`, `notify_close_failed` (all defined in `dormantd/src/notifier.rs`).
- **Web security/pairing events:** `web_reject_host`, `web_reject_origin` (`dormant-web/src/security.rs`); `pair_started`, `pair_succeeded`, `pair_failed` (`dormant-web/src/routes/pair.rs`, never carrying the token as a field). The `feature_disabled`/`pairing_in_progress`/`pair_not_found`/`entity_exists` strings are HTTP JSON `error` bodies, not `tracing` events — grep `error.rs` in `dormant-web/src`, not `event = "..."`, to find them.
- **Wire events** (`DaemonEvent` in `dormant-core/src/rules.rs`): tag is `event` — `sensor_changed`, `zone_changed`, `display_phase`, `config_reloaded`, `wake_retry`, `wake_recovered`, `blank_failure`, `blank_recovered`, `wear_snapshot`, `compensation_advisory`. A `#[serde(other)]` `Unknown` variant keeps older CLIs/WebUI builds streaming past foreign tags rather than failing the iterator (`crates/dormantctl/src/client.rs::EventStream` round-trips them through `DaemonEvent::Unknown`).
- **Snapshot keys** (`DisplaySnapshot` in `dormant-core/src/rules.rs`): `wake_attempts` (consecutive failed wake attempts, `0` when healthy) and `last_blank_failed` (whether the most recent blank command exhausted its controller chain) — read by the notifier's `reconcile`, the tray's `Failure` icon predicate (`dormant-tray/src/state.rs`), and the web dashboard's failure banner (`dormant-web/webui/src/app/components/FailureBanner.tsx`), independently of each other.

Config keys follow the TOML path: `daemon.log_level`, `sensors.<id>.type`, `zones.<id>.mode`, `displays.<id>.controllers`, `displays.<id>.panel_type`, `rules.<id>.zone`, `wear.<key>`, `notifications.<key>` (`enabled`, `wake_attempt_threshold`, `cooldown`, `notify_recovery`), etc. — all resolved in `dormant-core/src/config/mod.rs`.

## Panel-wear tracking

`dormant` attributes per-display brightness-weighted on-time to a coarse
grid overlaid on each panel — the operator sees a heat map (and a
"compensation advisory" nudge) and can reason about uneven burn-in risk.
Three concerns, kept cleanly separated:

- **Pure model** — `crates/dormant-core/src/wear.rs` (`WearLedger`,
  `WearIdentity`, `PanelType`, `sanitize_identity_key`, `brightness_norm`,
  `WearHandle = Arc<RwLock<HashMap<String, WearLedger>>>`). No I/O; the
  tracker in `dormantd` owns reading/writing/scheduling. `brightness_norm`
  scales per-controller readbacks to `0.0..=1.0` using the controller's
  `native_max` (DDC/CI `100`, Samsung port-1516 `50`).
- **Sample + persist loop** — `crates/dormantd/src/wear_tracker.rs`. A
  pure `tick(snapshot, samples, config, now)` advances the in-memory
  ledgers (attribution, dwell tracking, advisory latch, persist-due
  bookkeeping) and returns `TrackerAction`s; the async shell owns file
  I/O, event publication, and per-display ledger creation
  (`load_or_create_ledger` — corrupt-file recovery, future-schema
  read-only mode). The shell samples displays in the `active` OR `grace`
  phase (spec §4.2 pins both to the same attribution row — a display in
  its grace period still gets a real brightness read, not the fallback)
  through `CommandSink::read_state_sampled` (the sampler-priority
  variant of `read_state`); every other phase attributes a fixed factor
  and needs no hardware read. Controllers with a single physical bus
  (DDC/CI) implement the variant under the
  `crates/dormant-displays/src/vcp_ops.rs` + `crates/dormant-displays/src/ddc_lock.rs`
  panel-lock discipline — `VcpPriority::Command` blocks until the panel
  is free; `VcpPriority::Sampler` does a double-checked `try_lock` and
  yields instantly to any command-path caller that announced itself.
- **Wire events** — `DaemonEvent::WearSnapshot` and
  `DaemonEvent::CompensationAdvisory` (additive variants in
  `crates/dormant-core/src/rules.rs`; the `#[serde(other)] Unknown`
  catch-all keeps older CLIs/WebUI builds streaming past foreign tags).

Identity uses the panel-derived key from
`DisplayController::panel_identity()` when one is exposed (DDC/CI's
canonical panel-lock key, Samsung's `"samsung:<host>"`) so a
`[displays.*]` config rename doesn't orphan or collide with an existing
ledger; controllers with no panel-derived identity (`command`,
`kwin-dpms`, `ha-passthrough`) fall back to the sanitized config
display key. `seeded_usage_hours` (from `read_usage_hours`, DDC/CI VCP
`0xC0`) seeds the ledger's prior-on-hours if the panel was not new
when tracking started.

Persistence path: `dormant_core::paths::wear_state_dir()` returns
`$XDG_STATE_HOME/dormant/wear` (or `$HOME/.local/state/dormant/wear` as
fallback) — one ledger file per tracked display, baselined at
`advisory_baseline_epoch_s` (the "assume-healthy" anchor for the
advisory formula).

Configuration: the top-level `[wear]` section (`WearConfig` in
`crates/dormant-core/src/config/schema.rs`, defaults in
`crates/dormant-core/src/config/defaults.rs::WEAR_*`) — `enabled`,
`sample_interval`, `persist_interval`, `read_timeout`, `grid_rows`,
`grid_cols`, `fallback_brightness`, `screensaver_factor`,
`short_cycle_dwell`, `advisory_after`. Panel technology
(`[displays.<id>].panel_type`, `woled`/`qd-oled`/`unknown`) is
config-declared (never auto-detected — see the doctor/wear spec) and
recorded on the ledger, but v1's attribution math does not yet branch on
it — it is stored now as the bridge for a later per-channel weighting
without a schema break, not a live heuristic today.

Exposure: `GET /api/wear` (per-display summary; `advisory` is
server-derived from `wear.advisory_after` and
`max(last_long_dwell_epoch_s, advisory_baseline_epoch_s)`, independent
of any WS nudge the client may have missed) and
`GET /api/wear/<display>` (summary + per-cell `cells` + min-max
normalized `heat`) at `crates/dormant-web/src/routes/wear.rs`.

## Failure notifications

`dormantd/src/notifier.rs` surfaces repeated wake-command failures and
one-shot blank-command failures as desktop notifications, split the same
way as `wear_tracker.rs`: pure `decide`/`reconcile` policy functions (no
I/O) mutate a daemon-lifetime `NotifyState` (one open episode per
`(display, kind)`) and return actions; an async shell drives the
`NotifySink` trait, whose production impl (`ZbusSink`) calls
`org.freedesktop.Notifications` over the session D-Bus with a 2s
per-call timeout and a 60s reconnect backoff. `NotifyState` and the
`ZbusSink`'s cached connection are constructed once in `App::start` and
threaded unchanged through every reload generation, so open episodes (and
their D-Bus notification ids) survive a reload.

Reload can void carried-forward failure evidence: `dormantd/src/reload.rs`'s
`zero_changed_displays` zeroes a display's `wake_attempts`/
`last_blank_failed` before they are seeded into the new generation if the
display's dispatch-relevant config changed (per `dispatch_relevant_eq` —
controllers, blank/degraded mode, ladder, output/DDC target, host/WoL MAC,
blank/wake command or service+data, `modes`, command timeout, or the
unreachable-as-blanked flag) or if the display was added/removed. The
notifier's post-reload `reconcile` then closes any now-stale open
notification for that display **without** a recovery notice — reconcile
never emits one, unlike a genuine `WakeRecovered`/`BlankRecovered` event.

Both `wake_attempts` and `last_blank_failed` are plain in-memory
`DisplaySnapshot` fields with no on-disk persistence (unlike the wear
ledger above) — a full daemon restart, as opposed to a config reload,
loses all open episodes and failure counters. See
[`docs/src/failure-notifications.md`](docs/src/failure-notifications.md)
for the full trigger/threshold/cooldown semantics and the config keys.

## Audio-safe blanking

DPMS-based blanking (including `kwin-dpms`) disables the DRM/KMS output,
which tears down the associated ALSA audio sink — audio dies along with the
picture. This is architectural, not a config setting.

Three display-controller modes blank without tearing down the output,
preserving audio:

- **`ddcci`** — VCP `0xD6` sends a "display power off" command over I2C.
  The monitor blanks its panel internally; the OS output and ALSA device
  remain active. Only works on DDC/CI-capable monitors that support D6.
- **`samsung-tizen`** — `KEY_PICTURE_OFF` blanks the TV panel over WebSocket.
  The TV continues rendering audio; the HDMI output remains active.
- **`samsung-tizen`** (`brightness_zero`) — Samsung IP Control G2 JSON-RPC on
  port 1516 calls `backlightControl` to dim the panel to 0 via `samsung_ip`.
  Source and audio keep running; the HDMI output stays active. Used when
  `KEY_PICTURE_OFF` would cut the source or pause media.

Per-display strategy:
- DDC/CI monitor → `ddcci` power_off (audio-safe, verified on AOC AG326UZD)
- Samsung Tizen TV → `samsung-tizen` picture-off (audio-safe, verified on S90D)
- Samsung Tizen TV where the source must keep running → `samsung-tizen`
  `brightness_zero` via port 1516 IP Control G2 (audio-safe, softer panel
  change — does not pause media)
- Outputs with no DDC/CI and no audio → `kwin-dpms` is acceptable (no audio to kill)
- Outputs with audio but no DDC/CI → Tizen passthrough or `command` with an
  audio-safe external command; otherwise live with the audio loss

See `docs/research/2026-07-05-kwin-dpms-verification.md` and
`docs/research/2026-07-05-s90d-verification.md` for the hardware spike data.
