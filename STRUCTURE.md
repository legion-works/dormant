# Codebase Structure

## Directory Layout

```
oled-proximity/
├── crates/
│   ├── dormant-core/         # Pure domain logic: types, traits, config, rules, state machine, IPC, reload, doctor wire types
│   ├── dormant-sensors/      # Sensor sources: MQTT, HA WebSocket, USB LD2410 + registry
│   ├── dormant-displays/     # Display controllers: command, ddcci, kwin-dpms, macOS gamma/sleep, samsung-tizen (+ samsung_ip IP-Control-G2 transport), ha-passthrough + executor/registry
│   ├── dormant-doctor/       # Offline + live coalesced hardware/connectivity probes (config, mqtt, ha, usb, ddcci, samsung, macOS)
│   ├── dormant-web/          # Loopback-only axum HTTP/WS bridge + SPA (webui/)
│   ├── dormant-render/       # Local Wayland layer-shell render sink (black overlay + libmpv screensaver); Linux-only I/O
│   ├── dormantd/             # Daemon binary: App, event loop, IPC server, single-instance flock, inhibit-activity, reload watcher, logging
│   ├── dormantctl/           # CLI binary + library re-exporting the IPC client (status/pause/resume/blank/wake/reload/validate/watch/doctor)
│   └── dormant-tray/         # KDE StatusNotifierItem tray applet (ksni): icon, menu, tooltip, reconnecting event stream; Linux-only
├── docs/                     # mdBook source (src/) + built site (book/) + research notes (research/)
├── design/                   # Design-system assets used by the web UI
├── examples/                 # Reference config.toml, credentials.toml, and ESPHome sensor configs
├── fixtures/                 # Raw byte fixtures used by sensor parsing tests
├── .github/                  # Workflows and issue templates
└── Cargo.toml                # Workspace root + member list + shared dependencies
```

## Crate Purposes

**`crates/dormant-core/`**
- Purpose: Pure-logic domain — no I/O. Every other crate depends on this.
- Contains: `types`, `traits`, `error`, `ipc_proto`, `paths` (XDG helpers + `state_dir`/`wear_state_dir` test seams), `reload`, `rules`, `state_machine`, `zone`, `config` (`schema`/`defaults`/`validate`/`mod`), `doctor` (wire types only), `wear` (per-panel ledger model — `WearLedger`/`WearIdentity`/`PanelType`/`sanitize_identity_key`/`brightness_norm`/`WearHandle`), `ownership` (`OwnershipGate` trait + `AlwaysOwned` impl — multi-instance coordination seam the engine consults before driving a display), `fakes` (gated by `test-fakes` feature).
- Key files: `crates/dormant-core/src/lib.rs`, `crates/dormant-core/src/config/schema.rs`, `crates/dormant-core/src/config/defaults.rs` (single source of truth for every timing knob, including the `WEAR_*` constants), `crates/dormant-core/src/wear.rs`, `crates/dormant-core/src/rules.rs`, `crates/dormant-core/src/zone.rs`, `crates/dormant-core/src/error.rs`.

**`crates/dormant-sensors/`**
- Purpose: Sensor sources that emit `PresenceEvent`s. One module per sensor.
- Contains: `mqtt.rs`, `ha_ws.rs`, `usb_ld2410.rs`, `backoff.rs`, `registry.rs`.
- Key files: `crates/dormant-sensors/src/registry.rs` (explicit static registry — add new sources here).

**`crates/dormant-displays/`**
- Purpose: Display controllers that turn rules-engine `CommandSink` calls into real blank/wake operations, plus per-panel DDC locks, reload-safe blank-owner state, and serialized gamma holds.
- Contains: `command.rs`, `ddcci.rs` (Linux-only), `kwin_dpms.rs` (Linux-only), `samsung_tizen.rs` (port 8002 WebSocket remote control + Wake-on-LAN + network pairing — `pair`/`pair_with_connect` behind an injectable `PairConnect` trait, `RealPairConnect` the production impl), `samsung_ip.rs` (port 1516 IP Control G2 JSON-RPC — `backlightControl` for the audio-safe `brightness_zero` blank path; uses `dormant_core::paths::state_dir_from_env` for token-store path derivation), `ha_passthrough.rs`, `vcp_ops.rs` (abstract `VcpOps` trait + ddc-hi real backend + scripted fake; every physical transaction runs under `spawn_blocking` with `catch_unwind` for panic recovery), `ddc_lock.rs` (`PanelLocks` registry, `PanelLock` with command-priority + poison recovery), `executor.rs` (per-display fallback chain + retry; chain-walks `read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`), `registry.rs`, `test_support.rs` (`FakePairConnect` — a scripted `PairConnect` fake for the pairing-route tests in `dormant-web`; gated behind the `test-util` Cargo feature so it's reachable from another crate's dev-dependencies, not `#[cfg(test)]`-only).
- Key files: `crates/dormant-displays/src/registry.rs` (`CONTROLLER_TYPES`, `capabilities()`, `build_controllers()` — the new controllers accept an `Arc<PanelLocks>` argument for the same bus lock to be shared across an `App` generation).

**`crates/dormant-doctor/`**
- Purpose: Probe logic + live coalesced `DoctorService`. Re-exports the wire types from `dormant_core::doctor`.
- Contains: `probes/` (config, ddcci, ha, mqtt, samsung, usb — one file per probe; `samsung.rs` covers reachability on ports 8001/8002, REST power-state, and `credentials.samsung.<host>` token presence), `service.rs` (singleflight `DoctorService`), `types.rs` (`ProbeResult`/`ProbeStatus`).
- Key files: `crates/dormant-doctor/src/lib.rs` (`probe_all_offline` is the bare-doctor entry point used by `dormantctl doctor`; `probe_samsung` is the per-target dispatch).

**`crates/dormant-web/`**
- Purpose: Optional loopback-only web dashboard. Gated behind the `web-ui` Cargo feature of `dormantd`; when off, zero web code compiles.
- Contains: `server.rs` (axum router — `route_post!` derives the live `POST`-route set for the inverted origin-classification meta-test), `routes/` (command, config, `config_apply`, doctor, events, **wear** — `GET /api/wear` + `GET /api/wear/<display>` reads the shared `WearHandle` directly so a fresh GET is always the truth even if the browser missed a WS nudge; **pair** — `POST`/`GET /api/pair/samsung[/<id>]`, the Samsung pairing wizard: non-blocking + single-flight via `pair_lock`, `PairId`/`PairStatus`/`Token` — the latter's `Debug`/`Display` redact to `***` — and a lazy `sweep_expired` on terminal entries older than 5 minutes), `state.rs` (`WebState`/`WebStateInner`; carries the per-generation `WearHandle` from `dormantd` plus the pairing map/lock and the injected `PairConnect`/`upsert_token` seams), `config_patch.rs` (pure patch hygiene + `toml_edit` application — only `config_apply.rs` consumes it; also the entity CRUD gate: `CreateEntity`/`DeleteEntity` patch variants, the per-collection `CREATABLE_FIELDS` closed allowlist, `RESERVED_ENTITY_IDS`/`validate_entity_id` id hygiene), `assets.rs` (embedded SPA), `error.rs`, `security.rs` (`STRICT_ORIGIN_PATHS`/`ACKNOWLEDGED_WEAK_ROUTES` — the generalized strict-Origin classification set covering both `/api/config/apply` and `/api/pair/samsung`), `test_support.rs` (the seam exports `dormant-web` uses in dev-dependencies to substitute `PairConnect`/`upsert_token` for in-process pairing-route tests — gated behind the `test-util` feature, mirroring `dormant-displays/src/test_support.rs`).
- Key files: `crates/dormant-web/src/lib.rs` (`spawn` is the entry point), `crates/dormant-web/src/routes/mod.rs` (mount the routes).
- Web UI assets: `crates/dormant-web/webui/src/` (SPA — React/Vite — `App.tsx`, `main.tsx`, `app/views/*` (`Dashboard`, `Displays`, `Doctor`, `Config`, `Events`), `app/components/*` (`WearCard` for the panel-exposure summary + advisory banner, `HealthChip`, `StatusChip`, `Card`, …), `app/config/*` (`WearSection` — the `[wear]` TOML editor with the ten known keys, `ScreensaverEditor` now includes `shift_px` + `shift_interval` for static-image burn-in mitigation, `DisplaysSection` adds the `panel_type` select, `CreateEntityForm` — the shared per-collection entity-create form driven by `entityCrud.ts`'s `CREATABLE_FIELDS`/`validateEntityId` mirrors, `PairingWizard` — the Samsung pairing host-input/poll-loop/post-pair hand-off card, `AudioSection` — the `[audio]` TOML editor for the PipeWire-aware blanking gate (`playback`/`call`/role filter signals), `WatchdogSection` — the `[watchdog]` TOML editor (`lkg_enabled`/`lkg_rollback_enabled`/`stability_window`), `SettingsForm` — the patch-application orchestrator wired to `ApplyBar`, `SensorsSection`/`ZonesSection`/`DisplaysSection`/`RulesSection` add/delete affordances + unlocked cross-reference dropdowns, `fields.tsx` + `constants.ts` + `entityCrud.ts` + `patch.ts` — the shared field widgets, creatable-fields metadata, and TOML-edit helpers used by every section), `app/hooks/*`, `__tests__/`.

**`crates/dormantd/`**
- Purpose: The daemon binary — wires config → sensors → zones → rules → displays, with hot reload, inhibit-activity, the panel-wear tracker, IPC, optional web UI.
- Contains: `app.rs` (`App`, `AppHandle`, generation assembly + reload + teardown — also constructs one `Arc<PanelLocks>` above generation swaps and threads it into controllers, plus the per-generation `WearHandle` shared with the web UI), `inhibit_activity.rs`, `idle_source.rs`, `ipc.rs`, `single_instance.rs` (per-user-session `flock` guard — non-config-overridable), `reload.rs`, `wear_tracker.rs` (daemon-lifetime task — pure `tick(snapshot, samples, config, now)` plus async shell; samples displays in the `active` OR `grace` phase — both attribute a real brightness read per spec §4.2 — through `CommandSink::read_state_sampled`, persists ledgers to `wear_state_dir()`, publishes `DaemonEvent::WearSnapshot` + `CompensationAdvisory`), `notifier.rs` (desktop failure-notification daemon-lifetime task — pure `decide`/`reconcile` policy over `NotifyState` + `ZbusSink` session-bus I/O; open episodes + D-Bus notification ids survive reload), `boot.rs` (one `boot(plan, inputs)` — the ONLY function that calls `App::start` on a production boot path and the ONLY one that acquires the single-instance flock; the immediate-rollback build-failure write lives here because `decide` never validates the chosen config), `boot_guard.rs` (pure `decide`/`should_promote` crash-loop + LKG-promotion verdict logic with zero I/O, plus the sync `prepare` I/O shell that owns every verdict-driven `crash-loop.json` write before logging is initialised — `CRASH_LOOP_THRESHOLD`/`CRASH_LOOP_WINDOW`/`LKG_HEALTH_DEFER_CAP` are the documented consts here), `sd_notify.rs` (`SdNotify`, `watchdog_interval_from_env` — the `WATCHDOG=1` ping boundary; `WatchdogSec`/ping cadence come from systemd's `NOTIFY_SOCKET`/`WATCHDOG_USEC` and are deliberately not config), `logging.rs`, `lib.rs`, `main.rs`. Also: `systemd/dormant.service` (unit file, `Type=notify`, `WatchdogSec`), `tests/` (`daemon_smoke.rs`, `ipc_roundtrip.rs`, `web_config_apply.rs`, `boot_rollback.rs` — the watchdog/rollback integration tests).
- Key files: `crates/dormantd/src/main.rs` (binary entry), `crates/dormantd/src/app.rs` (runtime assembly), `crates/dormantd/src/wear_tracker.rs` (panel-wear tracker — pure tick + async shell split).

**`crates/dormantctl/`**
- Purpose: CLI companion. Talks to a running daemon over the Unix socket; some subcommands are offline (validate, doctor). Also exposes an IPC `client` library entry for out-of-binary consumers (e.g. `dormant-tray`). Emergency-wake falls back to direct-hardware probing + waking through the registry when the daemon is wedged or unreachable.
- Contains: `main.rs` (clap dispatch), `lib.rs` (`pub mod client` re-export for library users), `client.rs` (IPC client), `cmd_blank.rs` (blank + wake), `cmd_doctor.rs` (per-target subcommands + `exercise` for control-path verification), `cmd_emergency_wake.rs` (`dormantctl emergency-wake` — IPC fast path with a direct-hardware fallback when the daemon is unresponsive; probes each freshly-built executor before waking), `cmd_pair.rs` (`dormantctl pair samsung <host>` — connects to the TV, prompts the on-screen allow, and stores the returned token via `dormant_core::config::upsert_samsung_token`), `cmd_pause.rs` (pause + resume), `cmd_status.rs`, `cmd_validate.rs`, `cmd_watch.rs`.
- Key files: `crates/dormantctl/src/main.rs` (register new subcommands here).

**`crates/dormant-render/`**
- Purpose: Local Wayland layer-shell `RenderSink` — software-blank black overlay (final ladder fallback) and libmpv-driven screensaver overlay. Linux-only Wayland I/O; non-Linux stub exposes the same surface so `use dormant_render::LayerShellRenderSink` compiles everywhere.
- Contains: `lib.rs` (cross-platform re-export + `ShiftSettings` re-export), `command.rs` (async→sync bridge encode), `latch.rs` (T7 first-input-event latch), `shift.rs` (pure pixel-shift raster-walk math — `margin`, `raster_offsets`, `ShiftState`; the `wp_viewport` source-rect cycle that walks the screensaver buffer over `shift_px`/`shift_interval` for OLED static-image burn-in mitigation — U5: the black overlay never shifts), `playlist.rs` (screensaver item list), `settings.rs` (`ScreensaverSettings`, `ScaleMode`, `TransitionMode`, `ShiftSettings` — re-exported), `screensaver.rs` (Linux-only libmpv backend), `linux/` (real Wayland layer-shell impl: `blend.rs`, `connection.rs`, `mod.rs`, `state.rs`, `surface.rs`; the screensaver install path reads `ShiftSettings` from `WaylandState` at attach time and runs the `set_source` + `damage_buffer` + `commit` cycle on the calloop timer), `stub.rs` (non-Linux no-op sink), `examples/`.
- Key files: `crates/dormant-render/src/lib.rs` (entry points `LayerShellRenderSink` + `ScreensaverSettings` + `ShiftSettings`), `crates/dormant-render/src/settings.rs` (config-mirror types), `crates/dormant-render/src/shift.rs` (pixel-shift math).

**`crates/dormant-tray/`**
- Purpose: KDE `StatusNotifierItem` tray applet (`ksni`). Linux-only; non-Linux bins print a notice and exit 1.
- Contains (cross-platform): `lib.rs` (re-exports + `DEFAULT_WEB_PORT`), `state.rs` (pure icon-state derivation), `tooltip.rs` (pure tooltip build), `menu.rs` (pure menu model — testable without D-Bus), `icon.rs` (pixmap construction + runtime overlays). Linux-only: `tray.rs` (ksni `Tray` impl), `ipc_loop.rs` (reconnecting event-stream reader). `assets/` (SVG glyphs), `systemd/` (`dormant-tray.service` user unit), `build.rs` (compile-time SVG → PNG embedding), `tests/` (`event_pump_shutdown.rs`, `ipc_roundtrip.rs`).
- Key files: `crates/dormant-tray/src/main.rs` (binary entry — gates on `target_os = "linux"`), `crates/dormant-tray/src/state.rs` (testable pure logic), `crates/dormant-tray/src/lib.rs` (module surface + `DEFAULT_WEB_PORT`).

## Key File Locations

**Workspace manifest:** `Cargo.toml` — member list + shared `[workspace.dependencies]`.
**Entry points:**
- `crates/dormantd/src/main.rs` — daemon binary entry.
- `crates/dormantctl/src/main.rs` — CLI binary entry.
- `crates/dormantctl/src/lib.rs` — library entry; re-exports the IPC `client` module so `dormant-tray` (or any out-of-process consumer) drives the same protocol.
- `crates/dormant-web/src/lib.rs` — `spawn(bind, state) → JoinHandle` — daemon calls this when `web-ui` is enabled.
- `crates/dormant-tray/src/main.rs` — tray binary entry (Linux-only; non-Linux prints a notice and exits 1).

**Configuration:**
- `crates/dormant-core/src/config/schema.rs` — TOML-mirroring structs (`Config`, `DaemonConfig`, `SensorConfig`, `DisplayConfig`, `RuleConfig`, `Credentials`, …).
- `crates/dormant-core/src/config/defaults.rs` — single source of truth for every timing knob.
- `crates/dormant-core/src/config/validate.rs` — cross-reference validation rules AND the known-key tree for unknown-key detection (`KNOWN_KEYS`, `validate.rs:42`) — **not** `config/mod.rs` (F5 correction: an earlier revision of this file misattributed the tree to `mod.rs`).
- `crates/dormant-core/src/config/mod.rs` — loader; calls `validate::collect_unknown_keys` but does not itself hold the known-key tree.
- `crates/dormant-core/src/paths.rs` — XDG path resolution plus macOS Application Support state paths (`config_path`, `socket_path`, `sibling_credentials`, `state_dir()` / `wear_state_dir()`, and `state_dir_from_env`).
- `examples/config.toml`, `examples/credentials.toml` — working reference configs.

**Domain logic:** `crates/dormant-core/src/{types,traits,rules,state_machine,zone,reload,ipc_proto,error,doctor,wear}.rs`. `wear.rs` owns the per-panel `WearLedger` model (no I/O) — the tracker in `dormantd` owns sampling/persistence.
**Display executor (fallback + retry):** `crates/dormant-displays/src/executor.rs` (chain-walks `read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`).
**DDC/CI VCP operations:** `crates/dormant-displays/src/vcp_ops.rs` (abstract `VcpOps` trait + ddc-hi real backend + scripted fake; every transaction runs inside `spawn_blocking` with `catch_unwind` for panic recovery).
**Per-panel DDC/CI bus lock:** `crates/dormant-displays/src/ddc_lock.rs` (`PanelLocks` registry + `PanelLock` with `VcpPriority::Command` vs `Sampler` discipline + poison recovery).
**Sensor source registry:** `crates/dormant-sensors/src/registry.rs`.
**Display controller registry:** `crates/dormant-displays/src/registry.rs`.
**Doctor probes:** `crates/dormant-doctor/src/probes/{config,ddcci,ha,mqtt,samsung,usb,macos_idle,macos_power,macos_display_sleep}.rs`.
**Web routes:** `crates/dormant-web/src/routes/{command,config,config_apply,doctor,events,wear,pair}.rs`. `wear.rs` reads the shared `WearHandle` directly (no engine round-trip; mirrors the `doctor` route's read-only-diagnostics ethos). `pair.rs` is the Samsung pairing wizard — non-blocking (`202` + poll) and single-flight, calling `dormant_displays::samsung_tizen::pair_with_connect` + `dormant_core::config::upsert_samsung_token` in-process (no daemon IPC round-trip).
**Web config-patch module:** `crates/dormant-web/src/config_patch.rs` — pure patch hygiene / allowlist / `toml_edit` application; the `config_apply.rs` route is the only consumer. Also owns the entity-CRUD gate (`CreateEntity`/`DeleteEntity`, `CREATABLE_FIELDS`, `RESERVED_ENTITY_IDS`) — `entity_crud_enabled` itself is enforced one level up, in `config_apply.rs`, ahead of the shared 5-stage `Set`/`Remove` pipeline.
**Wear tracker:** `crates/dormantd/src/wear_tracker.rs` — pure `tick(snapshot, samples, config, now)` advances ledgers; async shell owns file I/O, `load_or_create_ledger`, and `DaemonEvent::WearSnapshot`/`CompensationAdvisory` publication.
**Render entry:** `crates/dormant-render/src/lib.rs` (re-exports `LayerShellRenderSink` + `ScreensaverSettings`).
**Tray entry:** `crates/dormant-tray/src/main.rs` (binary) and `crates/dormant-tray/src/lib.rs` (library surface).
**Tests:**
- Co-located `#[cfg(test)] mod tests` in every source file.
- Integration tests: `crates/dormant-core/tests/`, `crates/dormant-sensors/tests/`, `crates/dormantd/tests/` (`boot_rollback.rs` is the watchdog/rollback suite — `lkg_*` + `crash_loop_*` + `config_rollback_*` end-to-end scenarios), `crates/dormant-web/webui/src/__tests__/`.
- Property regressions: `crates/dormant-core/proptest-regressions/` (currently only `state_machine.txt` — no failure has shrunk a regression seed for `wear::resize_grid` yet; its total-conservation invariant is checked by a live `proptest!` block co-located in `wear.rs`, not (yet) by a pinned regression file here).
- HTTP/SPA fixtures: `crates/dormant-sensors/fixtures/`, `crates/dormant-web/webui/src/__tests__/fixtures/`, `crates/dormant-core/tests/fixtures/config/`, `crates/dormantd/tests/fixtures/pw_dump/` (real PipeWire 1.6.7 `pw-dump` captures for the audio-poller classifier — `idle`/`idle_dirty`/`movie`/`movie_paused`/`call`/`mic_only`; `include_str!`-embedded, `README.md` documents the probe and signal table).

## Naming Conventions

**Source files:** `<concept>.rs` at the crate root or one level under a sub-module. One concept per file; soft cap ~300 lines/file.
- Sensors: `crates/dormant-sensors/src/<backend>.rs` (e.g. `mqtt.rs`, `ha_ws.rs`, `usb_ld2410.rs`).
- Controllers: `crates/dormant-displays/src/<controller>.rs` (e.g. `kwin_dpms.rs`, `samsung_tizen.rs`).
- Doctor probes: `crates/dormant-doctor/src/probes/<target>.rs`.
- Web routes: `crates/dormant-web/src/routes/<resource>.rs`.
- CLI subcommands: `crates/dormantctl/src/cmd_<verb>.rs`. Short pairs share one file (`cmd_blank.rs` handles blank+wake, `cmd_pause.rs` handles pause+resume).

**Directories:** `<area>/` mirrors the crate split (`config/`, `probes/`, `routes/`, `webui/`).

**Types:** `<Name>Source` for sensors (e.g. `MqttSource`), `<Name>Controller` for displays (e.g. `DdcciController`). Config `type` strings literally match module names (`type = "usb-ld2410"` ↔ `usb_ld2410.rs`).

**Test files:** `tests/<feature>.rs` for integration; `__tests__/<Component>.test.tsx` for the SPA. Co-located `#[cfg(test)]` for unit tests.

## Where to Add New Code

**New sensor source:** Create `crates/dormant-sensors/src/<name>.rs` implementing `dormant_core::traits::SensorSource`, then add it to `crates/dormant-sensors/src/lib.rs` (`pub mod`) and register in `crates/dormant-sensors/src/registry.rs` (`SOURCE_TYPES` + `build` match arm). Add the config variant to `crates/dormant-core/src/config/schema.rs` (`SensorConfig` enum).

**New display controller:** Create `crates/dormant-displays/src/<name>.rs` implementing `dormant_core::traits::DisplayController`, then add to `crates/dormant-displays/src/lib.rs` and register in `crates/dormant-displays/src/registry.rs` (`CONTROLLER_TYPES` + `capabilities()` + `build_controllers` match arm). Gate with `#[cfg(target_os = "linux")]` if it needs platform I/O.

**New config key:** `crates/dormant-core/src/config/schema.rs` (struct field + serde rename) + `crates/dormant-core/src/config/defaults.rs` (default shim) + `crates/dormant-core/src/config/validate.rs` (cross-reference rule AND the `KNOWN_KEYS` known-key-tree entry — both live in this file, not `config/mod.rs`). Constants go in `crates/dormant-core/src/error.rs` (`pub const E_*`).

**New doctor probe:** Create `crates/dormant-doctor/src/probes/<target>.rs`, register in `crates/dormant-doctor/src/probes/mod.rs`, re-export in `crates/dormant-doctor/src/lib.rs`, add a CLI subcommand in `crates/dormantctl/src/cmd_doctor.rs` (`DoctorSubcommand` + dispatch).

**New web route:** Create `crates/dormant-web/src/routes/<name>.rs`, mount in `crates/dormant-web/src/routes/mod.rs`, register in the router at `crates/dormant-web/src/server.rs`. SPA view goes under `crates/dormant-web/webui/src/app/views/` with a matching route in `App.tsx`.

**New CLI subcommand:** Add `cmd_<name>.rs` (or co-locate with a sibling in an existing `cmd_*.rs`), declare the variant in `crates/dormantctl/src/main.rs` (`enum Command`), and dispatch it in the same file's `fn main`.

**New tray state/menu piece:** Add a pure-logic module under `crates/dormant-tray/src/<name>.rs` (derive icon state, build tooltip, model menu) so it can be unit-tested without a D-Bus session bus. Wire the Linux-only glue into `crates/dormant-tray/src/tray.rs` and `crates/dormant-tray/src/ipc_loop.rs`. Add new SVG glyphs to `crates/dormant-tray/assets/glyphs/` — `build.rs` embeds them at compile time.

**New render ladder stage:** Add the shared pure logic to `crates/dormant-render/src/<module>.rs` (so it compiles on every platform), implement the Linux-only Wayland glue under `crates/dormant-render/src/linux/`, and update the non-Linux stub at `crates/dormant-render/src/stub.rs` to keep the unconditional `use dormant_render::…` working.

**New web SPA component:** `crates/dormant-web/webui/src/app/components/<Name>.tsx` (pure) or `crates/dormant-web/webui/src/app/views/<View>.tsx` (routed). Hooks go in `crates/dormant-web/webui/src/app/hooks/`. Tests co-locate as `__tests__/<Name>.test.tsx`.

**New error code:** `crates/dormant-core/src/error.rs` — add `pub const E_<NAME>: &str`, add a variant to `DormantError` with `#[error("E_<NAME>: ...")]`, and map it in `DormantError::code()`.

**New wear sub-feature:** Pure model goes in `crates/dormant-core/src/wear.rs` (extend `WearLedger`/`PanelType`/identity helpers; bumping `WEAR_SCHEMA_VERSION` requires a migration branch in `load_or_create_ledger`). Sampling/persistence/advisory emission lives in `crates/dormantd/src/wear_tracker.rs` (the pure `tick` first, then a `TrackerAction` for the shell to execute). Wire events are additive `DaemonEvent` variants in `crates/dormant-core/src/rules.rs` — keep the `#[serde(other)] Unknown` catch-all so older consumers keep streaming. HTTP exposure goes in `crates/dormant-web/src/routes/wear.rs`, mounted in `routes/mod.rs` and the router at `crates/dormant-web/src/server.rs`; the SPA widget is `crates/dormant-web/webui/src/app/components/WearCard.tsx` (used by `app/views/Dashboard.tsx`) and the config editor is `crates/dormant-web/webui/src/app/config/WearSection.tsx`.

**New controller with a DDC/CI-shaped readback:** Implement the per-controller contracts in `crates/dormant-core/src/traits.rs` (`DisplayController::read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`); serialize physical transactions through the `PanelLock` (`crates/dormant-displays/src/ddc_lock.rs`) using `VcpPriority` (`crates/dormant-displays/src/vcp_ops.rs`). The chain-walk overrides on `dormant-displays/src/executor.rs` ensure the sampler priority is preserved across the `CommandSink` boundary.

**Shared utilities:** A new pure-logic helper belongs in `crates/dormant-core/` under a topical module. I/O helpers go in the relevant crate (`dormant-sensors`, `dormant-displays`, `dormant-doctor`, `dormant-web`). For daemon-owned persisted state (e.g. wear ledgers) reach for `crates/dormant-core/src/paths.rs::state_dir()` / `state_dir_from_env` rather than re-deriving the XDG-state-vs-`HOME` precedence at the call site.
