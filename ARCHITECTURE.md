# Architecture

Crate map, data flow, and where-to-find-it guide for the dormant codebase.

## Crate map

| Crate | Purpose | Has binaries? |
|---|---|---|
| `dormant-core` | Domain types and traits, config, zone fusion, rules, state machine, IPC, reload, and doctor wire types. `wear.rs` and `spatial_grid.rs` define the ledger and spatial attribution. `ownership.rs`, `coordination.rs`, and `observation.rs` hold ownership and diagnostic state. `output_panel.rs` classifies connector identity evidence. The crate defines `DaemonEvent`, including the `Ownership` event that daemon services publish. It does not perform device or network I/O. | No |
| `dormant-sensors` | Sensor sources: MQTT (`mqtt.rs`), Home Assistant WebSocket (`ha_ws.rs`), USB-serial LD2410 radar (`usb_ld2410.rs`), plus a shared backoff helper and a static registry | No |
| `dormant-displays` | Controllers: `command.rs`, `ddcci.rs` (Linux, macOS, and Windows), `kwin_dpms.rs` (Linux), `ha_passthrough.rs`, and `samsung_tizen.rs` (WebSocket, Wake-on-LAN, pairing). `samsung_ip.rs` provides port 1516 backlight control and input readback. `vcp_ops.rs` owns DDC/CI transport, panic recovery, the physical bus gate, and the five-minute handle cache. `ddc_lock.rs` gives commands priority over sampling. `connector_panel.rs` reads Linux DRM EDID independently of DDC/CI. `executor.rs` handles fallback and wake retries, and `blank_owner.rs` preserves the successful blank controller across reloads. macOS modules cover Quartz gamma black, display catalog, gamma recovery breadcrumbs, display sleep, and power assertions. | No |
| `dormant-doctor` | Probes config, MQTT, Home Assistant, USB LD2410, DDC/CI, Samsung Tizen, and the Linux input filter. `DoctorService` combines concurrent live requests and includes the active wear sampling probe. `draft.rs` formats issue and feature drafts. `dormantctl doctor exercise <display>` asks the daemon to pause the target's rules and run blank, read, wake, and read. Wire types live in `dormant_core::doctor`. Read-only platform probes cover macOS idle, display sleep, display catalog, power assertions, and shared DDC/CI power-off risk, plus Windows idle (`windows_idle.rs`). | No |
| `dormant-web` | Axum HTTP and WebSocket bridge. It serves engine state and the dashboard in `crates/dormant-web/webui/`. It binds to loopback by default and requires opt-in for a non-loopback bind. The `web-ui` feature enables it in `dormantd`. `config_patch.rs` restricts entity creation and deletion. `routes/pair.rs` handles Samsung pairing. `routes/wear_sampling.rs` supplies sampling status and disable controls. | No |
| `dormant-render` | Implements the black overlay and libmpv screensaver stages for Linux Wayland. `shift.rs` applies `shift_px` and `shift_interval` to the screensaver, not to the black overlay. `luma.rs` caches luma grids. `wear_order.rs` sorts the playlist for wear distribution. On non-Linux targets, `LayerShellRenderSink::show` returns `E_RENDER_UNAVAILABLE`. | No |
| `dormantd` | Runs the event loop, IPC server, single-instance lock, reload, and optional web UI. Activity sources include `windows_idle.rs` (`GetLastInputInfo`) and `macos_idle.rs`. `audio_source.rs` and `inhibit_audio.rs` poll `pw-dump` on Linux. `direct_switch.rs`, `coordination_poll.rs`, `activity_follow.rs`, `hooks.rs`, and `wake_hooks.rs` handle local switching, observations, and hooks. `state_publisher.rs` maps events and snapshots to MQTT state and Home Assistant discovery. The daemon owns wear tracking, active sampling and portal consent, failure notifications, boot rollback, and render sink initialization and retry. | **Yes** (`dormantd`) |
| `dormantctl` | CLI binary: `status`, `pause`, `resume`, `blank`, `wake`, `switch`, `reload`, `validate`, `watch`, `emergency-wake`, `doctor`, `wear` (`wear enable-sampling` / `wear disable-sampling`, requiring `--display` when multiple displays configure sampling), and `pair samsung <host>`. It also provides macOS `launchd install`/`launchd uninstall` and re-exports its IPC `client` module as a library entry (`crates/dormantctl/src/lib.rs`) so `dormant-tray` can drive the same protocol without forking the socket glue | **Yes** — `dormantctl` |
| `dormant-tray` | Tray applet: KDE `StatusNotifierItem` (`ksni`) on Linux and a native `AppKit` `NSStatusItem` backend on macOS (`tray_macos.rs`). Cross-platform pure logic (`state`, `tooltip`, `menu`, `icon`, `dispatch`, `action_table`, `tray_state`, `template_icon`) feeds the live icon (Normal / Attention / Paused / Failure / Unreachable), pause/resume/blank/wake menu items, tooltip, and the reconnecting IPC event-stream reader; `menu_refresh.rs` consumes its refresh channel so Linux calls `ksni::Handle::update()` when daemon state changes. Linux and macOS share the `TrayState`/`ipc_loop` machinery; only the platform-glue modules (`tray.rs` vs `tray_macos.rs`) are `cfg`-gated. Non-Linux/macOS bins print a notice and exit 1 so the portability `cargo check` stays green | **Yes** — `dormant-tray` |

Each crate follows the convention: one module per concept, one file per sensor/controller, explicit static registry with no proc-macro magic.

## Data flow

Wear-even screensaver state crosses the daemon boundary through the wear
tracker's item journal. `dormant-render/src/luma.rs` owns cached host-side luma
grids, while `wear_order.rs` applies deterministic ordering at session install;
missing data fails open without blocking playback.

```
                  ┌──────────────┐
  MQTT ──────────▶│              │
  HA WebSocket ──▶│  Sensors     │──▶ PresenceEvent ──▶
  USB LD2410 ────▶│  (registry)  │                     │
                  └──────────────┘                     │
                                                       ▼
                  ┌──────────────┐           ┌──────────────────┐
  Config ────────▶│  Zone Engine │──▶ Zone   │  Rules Engine /  │◀── User activity (idle_source.rs)
  (schema.rs)     │  (fusion)    │    State  │  State Machine    │◀── Audio / call (audio_source.rs, pw-dump poll)
                  └──────────────┘           └──────┬───────────┘
                                                    │
                                          Blank / Wake commands
                                                    │
                                                    ▼
                  ┌──────────────┐
                  │  Executor    │──▶ Controller chain (fallback)
                  │  (retry,     │      ├── kwin-dpms (Linux fallback, audio-unsafe)
                  │   escalation)│      ├── ddcci (Linux, macOS, Windows; VCP 0xD6)
                  └──────────────┘      ├── command
                                        ├── ha-passthrough
                                        ├── samsung-tizen (audio-safe)
                                        ├── macos-gamma-black (macOS, Quartz LUT, audio-safe)
                                        └── macos-display-sleep (macOS last resort, audio-unsafe)
```

1. **Sensors** send `PresenceEvent` values to the zone engine. Sensor states are present, absent, or unavailable.
2. The **zone engine** combines states with `any`, `all`, `quorum`, or `weighted` logic. The default `unavailable_policy` treats an unavailable sensor as present.
3. The **rules engine** maps zone state to display commands, applying grace periods, min-blank/min-wake floors, and per-kind inhibitor checks: user activity (`idle_source.rs`) and an opt-in PipeWire audio/call poller (`audio_source.rs` — audio playback and call detection via `pw-dump` polling; a rule reacts only to the kinds it declares in `inhibitors`). Kinds OR together per rule (any declared kind active holds the blank). Manual pause (`Overlays.paused`, driven by `dormantctl pause`/`resume` or the web UI) is a separate mechanism, not one of these inhibitor kinds.
4. The **display executor** walks an ordered controller chain per display: tries the first controller, falls back on failure, retries wakes with bounded backoff, and escalates to the next controller if all retries are exhausted.
5. For `scope = "shared"`, the coordination poller reads local VCP `0x60` at `coordination.poll_interval`. It refreshes brightness and power at `coordination.state_poll_interval` (default `max(30s, poll_interval)`). A positive `output_panel` mismatch skips the reads and counts toward `coordination.loss_confirmations`. Repeated ownership transitions can mark ownership contested until readback settles. The poller uses no network pairing or claim broadcasts.

## Shared-panel direct switch flow

Local direct-switch traffic — hotkey, CLI switch (`dormantctl switch`), tray, web UI, and local activity edges —
flows through the direct-switch service in `dormantd/src/direct_switch.rs`.

```
  Hotkey / CLI / Activity edge / Tray / Web
         │
         ▼
  ┌─────────────────┐
  │ dormantctl /    │
  │ tray / web IPC  │──── ▶ DirectSwitchHandle.pull / push
  │ activity_follow │
  └────────┬────────┘
           │
           ▼
  ┌─────────────────┐
   │ DirectSwitch    │   before_acquire (pull) or before_release (push)
   │ Service         │   ├─ Power on if in standby, then write VCP 0x60
   │                 │   └─ after_acquire (pull) or after_release (push)
  └────────┬────────┘
           │
           ▼
  ┌─────────────────┐
  │coordination_poll │   VCP 0x60 readback updates debounced input state
  │(every poll_interval) │
  └─────────────────┘
```

The `DirectSwitchHandle` in `dormantd/src/direct_switch.rs` performs local input switches over the machine's own DDC bus with no network protocol, peer transport, or frame signing. It handles hook execution (in-order slot, per-entry timeout, and aggregate `displays.<id>.hooks.timeout` ceiling) and DDC input-source writes (`VcpPriority::Command`), while `coordination_poll` periodically reads back VCP `0x60` to maintain debounced local vs peer observation state.

The poller dispatches `on_observed_loss` after a confirmed loss. It dispatches `on_observed_gain` after a confirmed gain or a return from a brief peer reading. By default, `observed_gain_requires_presence` defers the gain hook until a driving zone confirms presence. `wake_hooks.rs` dispatches `on_wake` for input wake, force wake, and wake with confirmed presence. These hooks do not write the input source. On Windows, `hooks.rs` uses a Job Object to terminate the command and its descendants on timeout.

## Filtered-activity fan-out

One physical input reader drives the consumers below through a single watch
channel:

```
  ┌──────────────────┐
  │ InputAuthority    │   evdev nodes (Linux) or CGEventTap (macOS)
  │ Supervisor        │   serialized stock→filtered transitions
  └────────┬─────────┘
           │ FilteredActivityTx (watch channel)
           ▼
   ┌───────┼───────────┬──────────────────┐
   │       │           │                  │
   ▼       ▼           ▼                  ▼
┌──────┐ ┌────────┐ ┌──────────────┐ ┌──────────────┐
│idle  │ │activity│ │claim-edge    │ │InputWake     │
│src   │ │inhibit │ │(edge/armed   │ │validator     │
│(stock│ │or      │ │ policy)      │ │(F8 ≤500ms   │
│ cmpst│ │idle obs│ │              │ │ proof)       │
└──────┘ └────────┘ └──────────────┘ └──────────────┘
```

The `InputAuthoritySupervisor` in `dormantd/src/filtered_activity.rs` is the
sole producer-authority arbiter: it serializes every stock↔filtered
transition, never grants two concurrent producer tokens, and publishes a
single `FilteredActivity` (edge sequence + last-activity timestamp) that feeds
all downstream consumers. The supervisor's stock fallback runs a compositor
idle-source while the filtered source is being probed, recovered, or has
failed — activity reporting never stops, it degrades from filtered (per-device)
to stock (per-seat) and back.

## Where do I look for X?

| Task | Where |
|---|---|
| Add a sensor | `dormant-sensors/src/<name>.rs` (new module) + `dormant-sensors/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config variant) |
| Add a display controller | `dormant-displays/src/<name>.rs` (new module) + `dormant-displays/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config fields if needed) |
| Add a config key | `dormant-core/src/config/schema.rs` (struct field + serde) + `dormant-core/src/config/defaults.rs` (default value) + `dormant-core/src/config/validate.rs` (validation rule AND the `KNOWN_KEYS` known-key-tree entry for unknown-key detection — `mod.rs` only calls `validate::collect_unknown_keys`, it does not hold the tree) + the relevant public page in `docs/src/` + a `changelog.d/` fragment (see `changelog.d/README.md`) |
| Add an error code | `dormant-core/src/error.rs` (`pub const E_*` + variant in `DormantError`) |
| Change configurable timing defaults | `dormant-core/src/config/defaults.rs` |
| Add a doctor probe | `dormant-doctor/src/probes/<name>.rs` (new probe) + `dormant-doctor/src/probes/mod.rs` + `dormant-doctor/src/lib.rs` (re-export) + `dormantctl/src/cmd_doctor.rs` (CLI dispatch + subcommand) |
| Wire doctor into the daemon | `dormantd/src/app.rs` (construct one `DoctorService` shared by IPC + web UI) + `dormant-doctor/src/service.rs` (coalesced singleflight logic) |
| Change the IPC transport | `dormantd/src/ipc.rs` and `dormantctl/src/client.rs` share protocol types from `dormant-core/src/ipc_proto.rs`. Unix uses a domain socket. Windows uses a local named pipe. `dormantd/src/ipc_pipe_security.rs` sets a protected DACL for the current user. `dormantctl/src/ipc_pipe_security.rs` checks that the current user owns the pipe. `dormantd/src/single_instance.rs` uses `flock` on Unix and `LockFileEx` on Windows. |
| Add a web route | `dormant-web/src/routes/<name>.rs` (new module) + `dormant-web/src/routes/mod.rs` (mount) + `dormant-web/src/server.rs` (router) |
| Add a CLI subcommand | `dormantctl/src/cmd_<name>.rs` (new file) + register variant + dispatch in `dormantctl/src/main.rs`. Short commands that share a handler (e.g. `pause`+`resume`, `blank`+`wake`, `wear enable-sampling`+`disable-sampling`) co-locate in one `cmd_*.rs` |
| Add a render ladder stage | `dormant-render/src/<module>.rs` (shared pure logic — `command`, `latch`, `settings`, `playlist`, `luma` for luma grid caching, `wear_order` for wear-even playlist ordering, `shift` for pixel-shift raster-walk math) + `dormant-render/src/linux/<impl>.rs` for the Wayland implementation (Linux-only). The cross-platform `LayerShellRenderSink` re-export is in `dormant-render/src/lib.rs` |
| Adjust screensaver wear-even playlist ordering or luma scanning | `dormant-render/src/{luma,wear_order}.rs` + `dormant-core/src/spatial_grid.rs` + `dormantd/src/wear_tracker.rs` |
| Adjust spatial grid / luma grid resampling math | `dormant-core/src/spatial_grid.rs` |
| Add a tray menu/state piece | `dormant-tray/src/<module>.rs` for cross-platform pure logic — `state` (icon-state derivation), `tooltip` (tooltip build), `menu` (menu model), `icon` (pixmap construction + runtime overlays), `dispatch` (`DispatchPlan` + `DispatchCapabilities` trait + `SystemCapabilities` impl, decouples action planning from platform I/O), `action_table` (tagged projection of menu actions for platform target/action callbacks — the bridge from the platform-neutral menu to `ksni::StandardItem` callbacks or AppKit `NSMenuItem` `setTarget:`/`setAction:`/`setTag:`), `tray_state` (`TrayState` shared by the IPC loop and the platform frontends), `template_icon` (pure monochrome renderer for the macOS template icon — black + source alpha, `setTemplate(true)` lets AppKit tint per appearance). Linux-specific glue lives in `dormant-tray/src/tray.rs`; macOS-specific glue lives in `dormant-tray/src/tray_macos.rs` (`AppKit` `NSStatusItem` + `NSMenu` + `NSMenuItem` with `MenuTarget` `objc2` class that forwards `performAction:` to a tokio mpsc `Action` channel). `dormant-tray/src/ipc_loop.rs` is shared (Linux + macOS) |
| Drive the daemon from another binary | Reuse `dormantctl::client` (re-exported via `crates/dormantctl/src/lib.rs`) — `dormant-tray` is the canonical example |
| Adjust panel-wear tracking behavior | `dormant-core/src/wear.rs` (pure model — `WearLedger`/`PanelType`/`sanitize_identity_key`/`brightness_norm`) + `dormant-core/src/config/schema.rs` (the `[wear]` and `[displays.<id>].panel_type` TOML keys) + `dormantd/src/wear_tracker.rs` (pure `tick` + async shell — sampling cadence, ledger I/O, advisory latch) + `dormant-web/src/routes/wear.rs` (HTTP exposure — `GET /api/wear`, `GET /api/wear/<display>`) |
| Adjust active wear sampling | `dormantd/src/active_sampler.rs` + `active_sampler/linux.rs` + `active_sampler/source_gate.rs` (source-gated TV sampling, portal/PipeWire capture, consent binding to output position, portal stream position retention, generation-tagged warm worker retention across capture timeouts, and latest-grid flow) + `dormantd/src/screencast_consent.rs` (token rotation and daemon-owned consent) + `dormant-core/src/config/schema.rs`/`defaults.rs` (`[wear.active_sampling]`, `[displays.<id>].sampling`, `compositor_output`) + `dormant-doctor/src/probes/wear_sampling.rs` + `dormant-web/src/routes/wear_sampling.rs` + `dormantctl/src/cmd_wear.rs` |
| Check the panel on a compositor connector | `dormant-core/src/output_panel.rs` classifies the `displays.<id>.output_panel` match. `dormant-displays/src/connector_panel.rs` reads EDID from Linux DRM. A confirmed mismatch feeds the ownership-loss debounce and stops active sampling. Missing EDID does not close the gate. See `dormantd/src/coordination_poll.rs` and `dormantd/src/active_sampler.rs`. |
| Retry render sink initialization | `dormantd/src/retry_render_sink.rs` saves construction failures and retries on a later `show` request. `dormantd/src/app.rs` supplies the per-display builder. |
| Add a panel-bus readback / usage-hours seed | Add the per-controller contract to `dormant-core/src/traits.rs` (`DisplayController::read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`/`claim_identity`), implement on the controller in `dormant-displays/src/<name>.rs`, surface the chain-walk on `dormant-displays/src/executor.rs`; serialized under the per-panel lock plus the process-wide physical-DDC gate; `dormant-displays/src/vcp_ops.rs` owns the cached-handle lifetime and transport-error invalidation, while `dormant-displays/src/ddc_lock.rs` enforces command priority |
| Adjust failure-notification behavior | `dormantd/src/notifier.rs` (pure `decide`/`reconcile` policy + `NotifyState`/`ZbusSink`) + `dormant-core/src/config/schema.rs` (the `[notifications]` section) + `dormantd/src/reload.rs` (`zero_changed_displays`/`dispatch_relevant_eq` — the reload-voiding gate) + `dormant-tray/src/state.rs` (Failure icon predicate) + `dormant-web/webui/src/app/components/FailureBanner.tsx` (dashboard banner) |
| Adjust watchdog / boot-rollback behavior | `dormantd/src/boot_guard.rs` (pure `decide` crash-loop verdict + `should_promote` LKG-promotion gate + the sync `prepare` I/O shell) + `dormantd/src/boot.rs` (`boot()` — build/substitute/lock/start, the one boot()-owned immediate-rollback write) + `dormantd/src/sd_notify.rs` (`SdNotify`, `watchdog_interval_from_env`) + `dormantd/src/app.rs` (`watchdog_tick`/`lkg_tick` — the probe arm, LKG candidate bookkeeping, reload step-boundary pings) + `dormant-core/src/config/schema.rs` (`WatchdogConfig`, the `[watchdog]` section) + `crates/dormantd/systemd/app-dormant.service` (`Type=notify`, `WatchdogSec`) + `dormant-web/webui/src/app/config/WatchdogSection.tsx` (web UI section) |
| Adjust multi-machine coordination | `dormant-core/src/config/schema.rs` (`CoordinationConfig`, `[coordination]`) + `dormant-core/src/{coordination,ownership,observation}.rs` (pure ownership observation state) + `dormantd/src/{coordination_poll,direct_switch,activity_follow}.rs` (local VCP `0x60` input polling at `poll_interval`, slower panel-state refresh at `state_poll_interval`, direct DDC input switches, local activity follow) + `dormantctl/src/cmd_switch.rs` (`dormantctl switch`) |
| Adjust entity CRUD (create/delete sensors/zones/displays/rules) | `dormant-web/src/config_patch.rs` (`CreateEntity`/`DeleteEntity` gates, `CREATABLE_FIELDS` per-collection allowlist, `RESERVED_ENTITY_IDS`, `validate_entity_id`) + `dormant-web/src/routes/config_apply.rs` (the `entity_crud_enabled` server-side gate, ahead of the shared apply pipeline) + `dormant-web/webui/src/app/config/{CreateEntityForm,entityCrud}.ts(x)` (client mirror + create form) + `{Sensors,Zones,Displays,Rules}Section.tsx` (Add/Delete affordances, cross-ref dropdowns) |
| Adjust the Samsung pairing wizard | `dormant-web/src/routes/pair.rs` (`POST`/`GET /api/pair/samsung[/<id>]`, `PairStatus`/`PairId`/`Token` redaction, `pair_lock` single-flight, `sweep_expired`) + `dormant-displays/src/samsung_tizen.rs` (`PairConnect` trait, `RealPairConnect`, `pair`/`pair_with_connect`) + `dormant-displays/src/test_support.rs` (`FakePairConnect`, `test-util` feature) + `dormant-core/src/config/mod.rs` (`upsert_samsung_token`, 0600 atomic write) + `dormant-web/webui/src/app/config/PairingWizard.tsx` |
| Adjust MQTT state publishing | `dormant-core/src/config/schema.rs` (`PublishConfig`, `[publish]`) + `dormant-core/src/config/defaults.rs` + `dormant-core/src/config/validate.rs` + `dormantd/src/state_publisher.rs` (pure state-to-record mapping for HA discovery and state/availability publishing) |

## Event and error-code grep anchors

Search for log event names and error codes as literals. Some log calls pass a selected literal as a field. Wire names come from the `DaemonEvent` variant names, not the log calls.

- **Error codes:** `E_CONFIG_INVALID`, `E_CONFIG_UNKNOWN_KEY`, `E_ZONE_CYCLE`, `E_ZONE_UNKNOWN_MEMBER`, `E_CREDS_PERMS`, `E_CREDS_MISSING`, `E_MODE_UNSUPPORTED`, `E_BLANK_FAILED`, `E_WAKE_FAILED`, `E_RELOAD_WAKE_FAILED`, `E_HA_AUTH`, `E_SENSOR_IO`, `E_DISPLAY_IO`, `E_RENDER_UNAVAILABLE`, `E_SCREENSAVER_SOURCE`, `E_IPC`, `E_HOOK_FAILED`, `E_HOOK_TIMEOUT` — all defined in `dormant-core/src/error.rs`.
- **Log events:** Search for `event = "..."`. `display_phase` is in `rules.rs`. `wake_failed` is in `executor.rs`. `sensor_source_exited`, `config_reloaded`, and `reload_defensive_wake` are in `app.rs`.
  - Coordination (`coordination_poll.rs`): `coord_ownership_contested`, `coord_ownership_settled`, `coord_contested_readback_changed`, `output_panel_mismatch`, `output_panel_restored`.
  - Wear (`wear_tracker.rs`): `wear_tracker_started`, `wear_tracker_resumed`, `wear_tracker_parked`, `wear_advisory`, `wear_ledger_corrupt`, `wear_ledger_unreadable`, `wear_ledger_future_version`, `wear_ledger_seeded`, `wear_persist_failed`, `wear_sample_fallback`, `wear_identity_refreshed`. `wear_sampling_consent_target` is in `ipc.rs`.
  - Notifications (`notifier.rs`): `notifier_started`, `notify_sent`, `notify_failed`, `notify_unreachable`, `notify_suppressed`, `notify_events_lagged`, `notify_close_failed`.
  - Watchdog and rollback: `crash_loop_detected`, `config_rollback_boot`, `config_rollback_continued`, `config_rollback_retry`, `lkg_missing_rollback_disarmed` (`main.rs` and `boot.rs`). `lkg_saved`, `lkg_save_failed`, `lkg_skipped_dirty`, `lkg_deferred_display_health`, `lkg_promoted_with_unhealthy_display`, and `watchdog_probe_failed` are in `app.rs`. `sd_notify_unavailable` is in `sd_notify.rs`.
  - Gamma recovery (`gamma_recovery.rs`): `gamma_stale_breadcrumb_restored`, `gamma_breadcrumb_clear_failed`, `gamma_stale_breadcrumb_restore_failed`. The `caller` field identifies startup or shutdown.
  - Hooks (`hooks.rs`): `hook_started`, `hook_slot_timeout`, `hook_slot_timeout_aborted`. An action that aborts its slot selects `claim_release_aborted`, `claim_acquire_aborted`, `observed_loss_aborted`, `observed_gain_aborted`, or `wake_hook_aborted` as the `event` field.
- **Web security/pairing events:** `web_reject_host`, `web_reject_origin` (`dormant-web/src/security.rs`); `pair_started`, `pair_succeeded`, `pair_failed` (`dormant-web/src/routes/pair.rs`, never carrying the token as a field); `entity_created`, `entity_deleted` (`dormant-web/src/routes/config_apply.rs::post_apply`, fired only after check_patches, apply_patches, daemon-identical validate, backup, and the atomic rename have ALL succeeded — never on a rejected patch — and carrying `collection`+`id` only, never the entity's `value`). The `feature_disabled`/`pairing_in_progress`/`pair_not_found`/`entity_exists` strings are HTTP JSON `error` bodies, not `tracing` events — grep `error.rs` in `dormant-web/src`, not `event = "..."`, to find them.
- **Wire events** (`DaemonEvent` in `dormant-core/src/rules.rs`): Serde converts each variant name to `snake_case` in the `event` field. Tags include `sensor_changed`, `zone_changed`, `display_phase`, `pause_changed`, `config_reloaded`, `wake_retry`, `wake_recovered`, `blank_failure`, `blank_recovered`, `wear_snapshot`, `wear_sampling_started`, `wear_sampling_degraded`, `wear_sampling_source_gate`, `compensation_advisory`, `ownership`, and `operations_changed`. Pairing status uses request and response, not a daemon event. IPC sends `subscribed` as the first frame on a new event connection. It does not broadcast this frame.
- **Snapshot keys** (`DisplaySnapshot` in `dormant-core/src/rules.rs`): `scope`, `owned`, `observed_input_code`, and `panel_state` report ownership and local readback. `wake_attempts` and `last_blank_failed` report command failures. `rules` lists the rule IDs that drive the display. `Force blank shared panel — affects all connected machines` is the tray's label for shared displays.

Config keys follow the TOML path: `daemon.log_level` (bare `debug`/`trace` scopes to dormant crates via `dormantd/src/logging.rs` with an `info` global floor for external dependencies; explicit `EnvFilter` directives pass through unchanged), `sensors.<id>.type`, `zones.<id>.mode`, `displays.<id>.controllers`, `displays.<id>.panel_type`, `displays.<id>.hooks.timeout`, `rules.<id>.zone`, `coordination.<key>` (`poll_interval`, `state_poll_interval` defaults to `max(30s, poll_interval)`, `loss_confirmations`, `flap_threshold` defaults to `5`, `flap_window` defaults to `"120s"`, `flap_settle` defaults to `"60s"`, `activity_follow`, `arm_after`, `cooldown`; duration floors must be `>= poll_interval`), `wear.<key>`, `notifications.<key>`, `watchdog.<key>`, `publish.<key>` (`enabled`, `broker_url`, `base_topic`, `discovery_prefix`, `instance_id`), etc. — all resolved in `dormant-core/src/config/mod.rs`.

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
  (`load_or_create_ledger` — corrupt/unreadable file recovery, future-schema
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

The tracker prefers `CommandSink::wear_identity()`. DDC/CI uses the EDID manufacturer, model, and serial when available, or the bus identity otherwise. Samsung uses `samsung:<host>`. A controller without a panel identity falls back to the config display key. This keeps the ledger across config renames when a stable panel identity is available. `read_usage_hours` can seed prior hours from DDC/CI VCP `0xC0`.

The tracker stores ledgers in `wear/` under the daemon state directory. Linux and macOS use `$XDG_STATE_HOME/dormant` or `$HOME/.local/state/dormant`. Windows normally uses `%LOCALAPPDATA%\dormant\state`. Each ledger stores `advisory_baseline_epoch_s` for the advisory calculation.

Configuration: the top-level `[wear]` section (`WearConfig` in
`crates/dormant-core/src/config/schema.rs`, defaults in
`crates/dormant-core/src/config/defaults.rs::WEAR_*`) — `enabled`,
`sample_interval`, `persist_interval`, `read_timeout`, `grid_rows`,
`grid_cols`, `fallback_brightness`, `screensaver_factor`,
`short_cycle_dwell`, `advisory_after`, and the `[wear.active_sampling]` table. Panel technology
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
blank/wake command or service+data, `ha_url`, `modes`, command timeout, or the
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

## Watchdog and rollback

Three composed mechanisms, kept separate the same way wear-tracking and
failure-notifications are — pure decision logic with no I/O, driven by a
thin async/sync shell:

- **Pure verdict logic** — `crates/dormantd/src/boot_guard.rs`:
  [`decide`] (the §5.2 crash-loop verdict matrix: `Proceed` /
  `RollBack` / `ContinueRollback`, zero I/O) and [`should_promote`] (the
  §4 LKG-promotion gate: `Wait` / `Promote` / `SkipDirty` / `DeferHealth` /
  `PromoteDespiteHealth`). The documented consts `CRASH_LOOP_THRESHOLD`
  (3), `CRASH_LOOP_WINDOW` (6m), and `LKG_HEALTH_DEFER_CAP` (3) live here.
- **Sync I/O shell** — `boot_guard::prepare` records this boot's start
  entry, builds `LkgInfo`, calls `decide`, and performs every
  verdict-driven `crash-loop.json` write itself, all before logging is
  initialised (events are deferred into the returned `BootPlan` and
  emitted by `dormantd/src/main.rs::emit_deferred_events` once it is).
- **Structural ordering shell** — `crates/dormantd/src/main_sequence.rs`
  enforces startup's non-negotiable order through
  `run_startup_sequence` (the closure chain
  `restore_stale_gamma → peek_boot_options → boot_guard::prepare →
  boot`) so no early-return inside `boot::boot` can bypass the
  stale-gamma restore, the config peek, or `prepare`'s verdict-driven
  writes; its inverse `run_boot_with_shutdown_restore` wraps the boot
  future so every verdict path — lock-failed, build-failed, started,
  run-loop-failed, clean shutdown — converges on the same shutdown
  gamma-restore. `main.rs` owns the I/O closures (real gamma calls,
  real config load, real boot); the shell owns the order, kept
  testable on every platform.
- **The boot sequence** — `crates/dormantd/src/boot.rs::boot(plan, inputs)`
   owns the single-instance lock, the one `App::build`/`App::start` call
  site, the one boot()-owned crash-loop write (the immediate-rollback
  path, which `decide` can't predict since it never validates the chosen
  config), and parking the pending-reload banner through
  `AppHandle::control_sender()`.
- **The watchdog probe arm** — `crates/dormantd/src/app.rs`'s
  `Runner::watchdog_tick`/`lkg_tick`: a periodic `run_loop` arm that pings
  `WATCHDOG=1` (via `sd_notify.rs`'s `SdNotify`) only after the engine
  answers a snapshot round-trip on the same control channel `reload()`
  uses, and runs the LKG-promotion check on every healthy tick. `reload()`
  itself pings at internal step boundaries (post-validate, post-quiesce,
  post-teardown, per removed-display wake, pre-`rebuild_old`) so a slow
  but healthy reload is never mistaken for a wedge.

State files live at `paths::state_dir()` root (sibling of, not nested
inside, `wear_state_dir()`): `last-known-good.toml`,
`last-known-good.meta.json`, `crash-loop.json`, `discount-<nonce>`.
`[watchdog]` config (`WatchdogConfig` in `dormant-core/src/config/schema.rs`,
defaults in `defaults.rs::LKG_*`, the `stability_window >= 30s` floor in
`validate.rs::validate_watchdog`) covers `lkg_enabled`,
`lkg_rollback_enabled`, `stability_window` — `WatchdogSec` and ping cadence
are deliberately not config; they come from systemd's own
`NOTIFY_SOCKET`/`WATCHDOG_USEC` environment. See
[`docs/src/watchdog-rollback.md`](docs/src/watchdog-rollback.md) for the
full mechanism writeup, the `WatchdogSec=150` derivation, and the
binary-before-unit upgrade order warning.

**Systemd-only watchdog.** `sd_notify.rs` and the engine-liveness
`WATCHDOG=1` ping are systemd-only — launchd has no equivalent mechanism
to detect a *running but wedged* (hung, not exited) `dormantd`. The
crash-loop / LKG-rollback logic above is otherwise fully
supervisor-agnostic (it counts process starts from its own state files,
not from systemd or launchd), so launchd's `ThrottleInterval=10` paces
restarts the same way systemd's `RestartSec=2` does, and a
config-caused crash loop still rolls back to last-known-good on macOS —
only the *wedged-but-alive* detection is Linux/systemd-only. See
[Installation: LaunchAgent (macOS)](docs/src/installation.md#launchagent-macos).

## Audio-safe blanking

DPMS-based blanking (including `kwin-dpms`) disables the DRM/KMS output,
which tears down the associated ALSA audio sink — audio dies along with the
picture. This is architectural, not a config setting.

These display-controller modes blank without disabling the output:

- **`ddcci`** — VCP `0xD6` sends a "display power off" command over I2C.
  The monitor blanks its panel internally; the OS output and ALSA device
  remain active. Only works on DDC/CI-capable monitors that support D6.
  The same VCP semantics apply on Linux, macOS, and Windows.
- **`samsung-tizen`** — `KEY_PICTURE_OFF` blanks the TV panel over WebSocket.
  The TV continues rendering audio; the HDMI output remains active.
- **`samsung-tizen`** (`brightness_zero`) — Samsung IP Control G2 JSON-RPC on
  port 1516 calls `backlightControl` to dim the panel to 0 via `samsung_ip`.
  Source and audio keep running; the HDMI output stays active. Used when
  `KEY_PICTURE_OFF` would cut the source or pause media.
- **`macos-gamma-black`** (macOS only) — writes an all-zero Quartz gamma
  table via CoreGraphics. A color-LUT black, not a power-state or output
  change, so it never touches audio. Trades that safety for a restart
  caveat: no periodic reassertion, restore depends on an in-process
  captured table or the on-disk breadcrumb — see [Displays: macOS gamma
  black](docs/src/displays.md#macos-gamma-black--macos-gamma-black-quartz).

`macos-display-sleep` (macOS's last-resort fallback, `pmset
displaysleepnow`) is deliberately **not** in the audio-safe list above —
unlike the LUT-level and DDC/CI-internal modes, it is a real, whole-machine
hardware/firmware display sleep with no audio-safety guarantee.

Per-display strategy:
- DDC/CI monitor → `ddcci` power_off (audio-safe, verified on AOC AG326UZD;
  same controller on Linux, macOS, and Windows)
- Samsung Tizen TV → `samsung-tizen` picture-off (audio-safe, verified on S90D)
- Samsung Tizen TV where the source must keep running → `samsung-tizen`
  `brightness_zero` via port 1516 IP Control G2 (audio-safe, softer panel
  change — does not pause media)
- Outputs with no DDC/CI and no audio → `kwin-dpms` is acceptable (no audio to kill)
- Outputs with audio but no DDC/CI → Tizen passthrough or `command` with an
  audio-safe external command; otherwise live with the audio loss
- macOS display with no DDC/CI support → `macos-gamma-black` (audio-safe),
  falling back to `macos-display-sleep` (not audio-safe, last resort) —
  the plan-recommended chain is `["ddcci", "macos-gamma-black",
  "macos-display-sleep"]`, see [Displays: recommended macOS
  chain](docs/src/displays.md#recommended-macos-chain)

See `docs/research/2026-07-05-kwin-dpms-verification.md` and
`docs/research/2026-07-05-s90d-verification.md` for the hardware spike data.
