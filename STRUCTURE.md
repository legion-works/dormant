# Codebase Structure

## Directory Layout

```
oled-proximity/
├── crates/
│   ├── dormant-core/         # Pure domain logic: types, traits, config, rules, state machine, IPC, reload, doctor wire types
│   ├── dormant-sensors/      # Sensor sources: MQTT, HA WebSocket, USB LD2410 + registry
│   ├── dormant-displays/     # Display controllers: command, ddcci, kwin-dpms, macOS gamma/sleep, samsung-tizen (+ samsung_ip IP-Control-G2 transport with input-source query/readback), ha-passthrough + executor/registry
│   ├── dormant-doctor/       # Offline + live coalesced hardware/connectivity probes (config, mqtt, ha, usb, ddcci, samsung, wear_sampling, macOS)
│   ├── dormant-web/          # axum HTTP/WS bridge + SPA (webui/); loopback by default
│   ├── dormant-render/       # Local Wayland layer-shell render sink (black overlay + libmpv screensaver, luma cache + wear-even ordering); Linux-only I/O
│   ├── dormantd/             # Daemon: App, event loop, Unix socket or Windows named pipe, single-instance lock, activity, hooks, reload, wear
│   ├── dormantctl/           # CLI binary + library re-exporting the IPC client (status/pause/resume/blank/wake/reload/validate/watch/doctor)
│   └── dormant-tray/         # Tray applet: ksni StatusNotifierItem (Linux) + AppKit NSStatusItem (macOS), shared pure logic + IPC loop
├── scripts/                  # Development, CI, quality gates, local deployment (deploy-local.sh), and release scripts
├── changelog.d/              # Scoped changelog fragment entries consumed during release prep
├── packaging/                # Downstream packaging assets (AUR PKGBUILD)
├── vendor/                   # Vendored platform dependencies (`ddc-macos` fork)
├── docs/                     # mdBook source (src/) + research notes (research/)
├── design/                   # Design-system assets used by the web UI
├── examples/                 # Reference config.toml, credentials.toml, and ESPHome sensor configs
├── fixtures/                 # Raw byte fixtures used by sensor parsing tests
├── .github/                  # Workflows and issue templates
└── Cargo.toml                # Workspace root + member list + shared dependencies
```

## Crate Purposes

**`crates/dormant-core/`**
- Purpose: Pure-logic domain — no I/O. Every other crate depends on this.
- Contains: `types`, `traits`, `error`, `ipc_proto`, `paths` (XDG helpers + `state_dir`/`wear_state_dir` test seams), `reload`, `rules`, `state_machine`, `zone`, `config` (`schema`/`defaults`/`validate`/`mod` — includes `PublishConfig` under `[publish]`), `doctor` (wire types only), `wear` (per-panel ledger model), `spatial_grid` (`LumaGrid`/`HeatGrid`/area-preserving resampling), `ownership` (`OwnershipGate`), `coordination` (shared-display ownership and debounced observation), `observation` (causal reload identities and diagnostic observations such as `WearLedgerUnreadable`), `mqtt.rs`, `fakes` (gated by `test-fakes` feature).
- Key files: `crates/dormant-core/src/lib.rs`, `crates/dormant-core/src/traits.rs` (`DisplayController` readback and panel-identity contracts), `crates/dormant-core/src/config/schema.rs`, `crates/dormant-core/src/config/defaults.rs` (single source of truth for every timing knob, including the `WEAR_*` constants), `crates/dormant-core/src/wear.rs`, `crates/dormant-core/src/spatial_grid.rs`, `crates/dormant-core/src/rules.rs`, `crates/dormant-core/src/zone.rs`, `crates/dormant-core/src/error.rs`.
- Coordination files: `crates/dormant-core/src/{coordination,ownership,observation}.rs`; `[coordination]` is `CoordinationConfig` in `config/schema.rs`.
- Other modules: `duration.rs` and `duration/{option,serde}.rs` define the duration grammar and Serde adapters. `output_panel.rs` classifies EDID matches for `displays.<id>.output_panel`.

**`crates/dormant-sensors/`**
- Purpose: Sensor sources that emit `PresenceEvent`s. One module per sensor.
- Contains: `lib.rs`, `mqtt.rs`, `ha_ws.rs`, `usb_ld2410.rs`, `backoff.rs`, `registry.rs`.
- Key files: `crates/dormant-sensors/src/registry.rs` (explicit static registry — add new sources here).

**`crates/dormant-displays/`**
- Purpose: Display controllers that turn rules-engine `CommandSink` calls into real blank/wake operations, plus per-panel DDC locks, reload-safe blank-owner state, and serialized gamma holds.
- Contains: `lib.rs`, `registry.rs`, `command.rs`, `ddcci.rs` (Linux I²C, the vendored `ddc-macos` fork, and Windows Monitor Configuration API), `kwin_dpms.rs` (Linux), `ha_passthrough.rs`, `samsung_tizen.rs` (port 8002 WebSocket, Wake-on-LAN, and pairing), and `samsung_ip.rs` (port 1516 IP Control G2 backlight control, input readback, and token persistence). `connector_panel.rs` reads Linux DRM EDID without DDC/CI. `vcp_ops.rs` owns `VcpOps`, the physical DDC gate, panic recovery, and the five-minute handle cache. `ddc_lock.rs` gives commands priority over samplers. `executor.rs` handles fallback and retry. `blank_owner.rs` preserves controller order for wake across reloads. `test_support.rs` exposes `FakePairConnect` with `test-util`.
- macOS modules: `macos_gamma_black.rs` (Quartz gamma with the `cg:<uuid>` selector), `macos_display_catalog.rs` (CoreGraphics display lookup), `gamma_breadcrumb.rs` (gamma recovery), `macos_display_sleep.rs` (system-wide sleep and wake), and `macos_power.rs` (power assertions).
- Key files: `crates/dormant-displays/src/registry.rs` (`CONTROLLER_TYPES`, `capabilities()`, `build_controllers()` — the new controllers accept an `Arc<PanelLocks>` argument for the same bus lock to be shared across an `App` generation).

**`crates/dormant-doctor/`**
- Purpose: Probe logic + live coalesced `DoctorService`. Re-exports the wire types from `dormant_core::doctor`. The active-sampling probe is live-only because it inspects daemon-owned consent and the running portal/PipeWire session.
- Contains: `lib.rs`, `service.rs` (`DoctorService`), `draft.rs` (issue and feature drafts), and `types.rs` (`ProbeResult`, `ProbeStatus`). `probes/mod.rs` declares the probes in [Key File Locations](#key-file-locations), including live-only `wear_sampling.rs` and the read-only Windows idle probe.
- Key files: `crates/dormant-doctor/src/lib.rs` (`probe_all_offline` is the bare-doctor entry point used by `dormantctl doctor`; `probe_samsung` is the per-target dispatch).

**`crates/dormant-web/`**
- Purpose: Optional web dashboard, enabled in `dormantd` by `web-ui`. It binds to loopback by default. `routes/wear_sampling.rs` supplies daemon sampling status and disable controls.
- Contains: `server.rs` (axum router — `route_post!` derives the live `POST`-route set for the inverted origin-classification meta-test), `build.rs` (release-profile guard preventing compilation with the placeholder SPA), `event_ring.rs` (bounded in-memory ring buffer for recent `DaemonEvent`s), `routes/` (command, config, `config_apply`, **daemon** — `GET /api/daemon` process identity; doctor, events, **operations** — `GET /api/operations` single-flight guard status; **wear** — `GET /api/wear` + `GET /api/wear/<display>` reads the shared `WearHandle` directly so a fresh GET is always the truth even if the browser missed a WS nudge; **pair** — `POST`/`GET /api/pair/samsung[/<id>]`, the Samsung pairing wizard: non-blocking + single-flight via `pair_lock`, `PairId`/`PairStatus`/`Token` — the latter's `Debug`/`Display` redact to `***` — and a lazy `sweep_expired` on terminal entries older than 5 minutes; **star_nudge** — `POST /api/star-nudge/dismiss` and `/star`), `state.rs` (`WebState`/`WebStateInner`; carries the per-generation `WearHandle` from `dormantd` plus the pairing map/lock and the injected `PairConnect`/`upsert_token` seams), `config_patch.rs` (pure patch hygiene + `toml_edit` application — only `config_apply.rs` consumes it; also the entity CRUD gate: `CreateEntity`/`DeleteEntity` patch variants, the per-collection `CREATABLE_FIELDS` closed allowlist, `RESERVED_ENTITY_IDS`/`validate_entity_id` id hygiene), `assets.rs` (embedded SPA), `error.rs`, `security.rs` (`STRICT_ORIGIN_PATHS`/`ACKNOWLEDGED_WEAK_ROUTES` — the generalized strict-Origin classification set covering both `/api/config/apply` and `/api/pair/samsung`), `test_support.rs` (the seam exports `dormant-web` uses in dev-dependencies to substitute `PairConnect`/`upsert_token` for in-process pairing-route tests — gated behind the `test-util` feature, mirroring `dormant-displays/src/test_support.rs`).
- Key files: `crates/dormant-web/src/lib.rs` (`spawn` is the entry point), `crates/dormant-web/src/routes/mod.rs` (mount the routes).
- Shared display state is rendered in `webui/src/app/views/Displays.tsx`.
- Web UI assets: `crates/dormant-web/webui/src/` contains the React/Vite SPA. `main.tsx` starts the app and `App.tsx` defines routes. Routed views are `Overview`, `Displays`, `DisplayDetail`, `Switching`, `Doctor`, `Config`, and `Events`. `app/views/` also contains `OwnershipPair`, `SwitchState`, and `eventFormat.ts`. Shared widgets such as `WearCard`, `HealthChip`, and `StatusChip` live in `app/components/`. Hooks and tests live in `app/hooks/` and `__tests__/`.
- Config UI (`webui/src/app/config/`): `SettingsForm`, `ApplyBar`, `FormSection`, `SectionRail`, and `SectionRailContext` assemble the settings editor. Sections cover daemon, coordination, keymap, input filter, audio, notifications, watchdog, wear, sensors, zones, displays, and rules. `HooksInspector` lists all seven hook slots. `DisplaySamplingEditor`, `LadderEditor`, and `ScreensaverEditor` edit display subsettings. `PairingWizard`, `CreateEntityForm`, and `RollbackRecoveryCard` handle pairing, entity creation, and rollback recovery. Shared helpers are `fields.tsx`, `constants.ts`, `density.ts`, `entityCrud.ts`, `patch.ts`, and `sectionRailShared.ts`.

**`crates/dormantd/`**
- Purpose: The daemon binary — wires config → sensors → zones → rules → displays, with hot reload, inhibit-activity, the panel-wear tracker, daemon-lifetime active sampling, IPC, optional web UI. Active sampling lives in `active_sampler.rs` and `active_sampler/linux.rs`; consent persistence lives in `screencast_consent.rs`.
- Contains: `lib.rs`, `main.rs`, `main_sequence.rs`, `app.rs`, `boot.rs`, `boot_guard.rs`, `reload.rs`, `logging.rs`, `notifier.rs`, `sd_notify.rs`, `watchdog_schedule.rs`, `state_publisher.rs`, and `gamma_recovery.rs`.
- IPC and startup: `ipc.rs` serves the same protocol over a Unix socket and a Windows named pipe. `ipc_pipe_security.rs` sets a protected DACL for the current user and rejects remote pipe clients. `single_instance.rs` uses `flock` on Unix or `LockFileEx` on Windows. Startup assets: `share/` (XDG desktop entry and macOS LaunchAgent), `systemd/app-dormant.service`, and the logon task in `scripts/windows-setup.ps1`.
- Activity and hooks: `idle_source.rs`, `idle_observation.rs`, `inhibit_activity.rs`, `evdev_idle.rs`, `filtered_activity.rs`, `macos_idle.rs`, `macos_input_filter.rs`, `windows_idle.rs` (`GetLastInputInfo`), `audio_policy.rs`, `audio_source.rs`, `inhibit_audio.rs`, `activity_follow.rs`, `coordination_poll.rs`, `direct_switch.rs`, `hooks.rs` (ordered actions, timeout limits, and Windows Job Object containment), and `wake_hooks.rs` (dispatches `on_wake`).
- Wear and render: `wear_tracker.rs`, `active_sampler.rs`, `active_sampler/linux.rs`, `active_sampler/source_gate.rs`, `screencast_consent.rs`, and `retry_render_sink.rs` (retries a failed render sink build on a later show request).
- Key files: `crates/dormantd/src/main.rs` (binary entry), `crates/dormantd/src/app.rs` (runtime assembly), `crates/dormantd/src/direct_switch.rs` (local direct display switching), `crates/dormantd/src/state_publisher.rs` (MQTT state-to-record mapping), `crates/dormantd/src/wear_tracker.rs` (panel-wear tracker — pure tick + async shell split).
- Coordination files: `coordination_poll.rs` reads each shared display's local VCP `0x60` every `poll_interval` tick, refreshes brightness/power state only at the slower `state_poll_interval` cadence, and emits a contested verdict that forces not-owned during ownership flapping; `direct_switch.rs` executes local direct input switches over DDC; `activity_follow.rs` triggers switches on local activity.

**`crates/dormantctl/`**
- Purpose: CLI companion. Uses a Unix socket or a Windows named pipe. `validate` and the offline `doctor` probes need no daemon. The `client` module lets other binaries use the same IPC protocol. `emergency-wake` probes and wakes hardware directly if daemon IPC fails.
- Contains: `main.rs` (clap dispatch), `lib.rs` (`pub mod client` re-export for library users), `client.rs` (IPC client), `cmd_blank.rs` (blank + wake), `cmd_doctor.rs` (per-target subcommands: `ddcci`/`usb`/`mqtt`/`ha`/`config`/`kwin` + macOS-only `macos-idle`/`macos-display-sleep`/`macos-power` read-only arms + `exercise <display>` for control-path verification — `run_exercise_with_socket` is invoked directly from `main.rs` because it needs the resolved `--socket`), `cmd_emergency_wake.rs` (`dormantctl emergency-wake` — IPC fast path with a direct-hardware fallback when the daemon is unresponsive; also runs the macOS gamma-breadcrumb restore independently of daemon health; probes each freshly-built executor before waking), `cmd_launchd.rs` (`dormantctl launchd install`/`uninstall` — macOS-only handler that copies byte-identical embedded daemon and tray `LaunchAgent` plists from `crates/dormantd/share/com.legionworks.dormant.plist` and `crates/dormant-tray/share/com.legionworks.dormant-tray.plist` to their canonical paths under `~/Library/LaunchAgents/` via `include_bytes!`; parses on every platform so `--help` stays accurate, only the handler is `cfg(target_os = "macos")`), `cmd_pair.rs` (`dormantctl pair samsung <host>` — connects to the TV, prompts the on-screen allow, and stores the returned token via `dormant_core::config::upsert_samsung_token`), `cmd_pause.rs` (pause + resume), `cmd_status.rs`, `cmd_switch.rs` (`dormantctl switch` — write this host's input code to a shared panel locally; `--to-peer` writes the peer's code when configured), `cmd_validate.rs`, `cmd_watch.rs`, `cmd_wear.rs` (`dormantctl wear enable-sampling` / `disable-sampling` active sampling consent commands; `--display` required when multiple displays configure sampling).
- Key files: `crates/dormantctl/src/main.rs` (register new subcommands here).
- Windows: `ipc_pipe_security.rs` checks the pipe owner's SID. `cmd_doctor.rs` dispatches the read-only `windows-idle` probe.

**`crates/dormant-render/`**
- Purpose: Linux Wayland layer-shell `RenderSink` for configurable black and libmpv screensaver stages. The non-Linux stub exposes the same API and returns `E_RENDER_UNAVAILABLE` from `show`.
- Contains: `lib.rs`, `command.rs` (async-to-sync bridge), `latch.rs` (first input event), `luma.rs` (luminance grids), `wear_order.rs` (wear-even playlist order), `shift.rs` (screensaver pixel shift), `playlist.rs`, `settings.rs`, and `screensaver.rs` (Linux libmpv backend). `linux/` contains `blend.rs`, `connection.rs`, `mod.rs`, `state.rs`, `surface.rs`, and `wayland_ops.rs`. `stub.rs` is the non-Linux implementation. `examples/` contains the black-overlay smoke program.
- Key files: `crates/dormant-render/src/lib.rs` (entry points `LayerShellRenderSink` + `ScreensaverSettings` + `ShiftSettings`), `crates/dormant-render/src/settings.rs` (config-mirror types), `crates/dormant-render/src/luma.rs`, `crates/dormant-render/src/wear_order.rs`, `crates/dormant-render/src/shift.rs` (pixel-shift math).

**`crates/dormant-tray/`**
- Purpose: Tray applet. Linux uses `ksni`'s KDE `StatusNotifierItem`; macOS uses a native `AppKit` `NSStatusItem` backend (`tray_macos.rs`, `objc2`/`objc2-app-kit`/`objc2-foundation`/`dispatch2`/`png` deps, gated `cfg(target_os = "macos")`). The shared cross-platform pure logic drives both frontends from one `TrayState`. Non-Linux/macOS bins print a notice and exit 1.
- Contains (cross-platform): `lib.rs` (re-exports + `DEFAULT_WEB_PORT`), `state.rs` (pure icon-state derivation), `tooltip.rs` (pure tooltip build), `menu.rs` (pure menu model — testable without D-Bus or AppKit), `icon.rs` (pixmap construction + runtime overlays for the Linux pixmap path), `dispatch.rs` (pure action planning + `DispatchCapabilities` trait + `SystemCapabilities` impl, with cfg-gated `open_web` opener: `xdg-open` on Linux, `open` on macOS), `action_table.rs` (tagged projection of menu actions — assigns one-based `tag`s that resolve back to `Action`; the bridge for both `ksni::StandardItem` `activate` closures and AppKit `NSMenuItem` `setTag:`/`setTarget:`/`performAction:` routing), `hotkey.rs` (cross-platform global shortcut contract `HotkeyRegistrar` + `HotkeyManager` IPC watch-channel subscriber), `tray_state.rs` (`TrayState` — `socket_path` + `snapshot` + `unreachable` + `icon_state` — shared between the IPC loop and the platform frontends), `template_icon.rs` (pure monochrome pixel renderer for the macOS template icon — straight RGBA black + source alpha at `TEMPLATE_PX = 36`; state-discriminating badge shapes drawn at runtime). Linux + macOS: `ipc_loop.rs` (reconnecting event-stream reader — exposes `refresh_channel`/`RefreshSender`/`RefreshReceiver` so the platform frontends can be poked whenever `TrayState` mutates; `TickShutdown` drop-guard + `spawn_event_pump` keep the OS pump thread leak-free across reconnect cycles), `menu_refresh.rs` (testable refresh-consumption loop; its `MenuRefresher` seam drives `ksni::Handle::update()` from the IPC refresh channel without a live D-Bus session). Linux-specific: `tray.rs` (ksni `Tray` impl, action dispatch through `dispatch::execute_plan`, `request_quit`/`wait_for_quit` for the binary's main loop), `hotkey_linux.rs`/`hotkey_linux/` (XDG GlobalShortcuts portal + KDE KGlobalAccel backends; closes retained portal sessions on claim release, error, or unregistration). macOS-specific: `tray_macos.rs` (`AppKit` `NSStatusItem` + `NSMenu` + `NSMenuItem` construction from the shared menu; `MenuTarget` `objc2` class forwards `performAction:` to a tokio mpsc `Action` channel; cross-thread refresh via `dispatch2::DispatchQueue::main().exec_async` draining a `std::sync::mpsc::MainMessage` mailbox), `hotkey_macos.rs`/`hotkey_macos_common.rs` (macOS CGEventTap / RegisterEventHotKey backends). `assets/` (SVG glyphs), `systemd/` (`dormant-tray.service` user unit), `build.rs` (compile-time SVG → PNG + brand mark rasterization at four sizes for Linux, unpremultiplying StatusNotifier icon pixels for straight-alpha composition).
- Key files: `crates/dormant-tray/src/main.rs` (binary entry — three cfg-gated paths: Linux ksni, macOS `tray_macos::run()`, other-platform notice+exit), `crates/dormant-tray/src/state.rs` (testable pure logic), `crates/dormant-tray/src/lib.rs` (module surface + `DEFAULT_WEB_PORT`), `crates/dormant-tray/src/dispatch.rs` (the platform I/O seam).
- Platform assets: the Linux unit is in `systemd/dormant-tray.service`. The macOS LaunchAgent is in `share/com.legionworks.dormant-tray.plist`. `hotkey_linux/` contains `portal.rs` and `kglobalaccel.rs`.

## Key File Locations

**Workspace manifest:** `Cargo.toml` — member list + shared `[workspace.dependencies]`.
**Entry points:**
- `crates/dormantd/src/main.rs` — daemon binary entry.
- `crates/dormantctl/src/main.rs` — CLI binary entry.
- `crates/dormantctl/src/lib.rs` — library entry; re-exports the IPC `client` module so `dormant-tray` (or any out-of-process consumer) drives the same protocol.
- `crates/dormant-web/src/lib.rs`: async `spawn(bind, state)` returns the server task handle and bound address, or an I/O error.
- `crates/dormant-tray/src/main.rs` — tray binary entry (three cfg-gated paths: Linux `ksni` tray, macOS `tray_macos::run()` on the main thread, other-platform notice+exit 1).

**Configuration:**
- `crates/dormant-core/src/config/schema.rs` — TOML-mirroring structs (`Config`, `DaemonConfig`, `SensorConfig`, `DisplayConfig`, `RuleConfig`, `Credentials`, …).
- `crates/dormant-core/src/config/defaults.rs`: defaults for configurable timing.
- `crates/dormant-core/src/config/validate.rs`: cross-reference checks and the `KNOWN_KEYS` table for unknown-key detection.
- `crates/dormant-core/src/config/mod.rs` — loader; calls `validate::collect_unknown_keys` but does not itself hold the known-key tree.
- `crates/dormant-core/src/paths.rs`: Linux XDG paths, macOS config lookup in Application Support, and Windows app-data paths. See `default_socket_path`, `default_lock_path`, `state_dir`, and `wear_state_dir` for each platform's defaults.
- `examples/config.toml`, `examples/credentials.toml` — working reference configs.

**Domain logic:** `crates/dormant-core/src/{types,traits,rules,state_machine,zone,reload,ipc_proto,error,doctor,wear,coordination,ownership,observation}.rs`. `coordination.rs` holds shared-display observation state; `observation.rs` holds content revisions and diagnostic observations.
**Display executor (fallback and retry):** `crates/dormant-displays/src/executor.rs` walks the chain for `read_state`, `read_state_sampled`, `read_usage_hours`, `panel_identity`, `claim_identity`, and `wear_identity`.
**DDC/CI VCP operations:** `crates/dormant-displays/src/vcp_ops.rs` (abstract `VcpOps` trait + ddc-hi real backend + scripted fake; every transaction runs inside `spawn_blocking` with `catch_unwind` for panic recovery; per-ident handle cache with absolute five-minute revalidation, transport-vs-protocol error classification, and a process-wide physical-DDC gate spanning enumeration, transactions, and teardown).
**Per-panel DDC/CI bus lock:** `crates/dormant-displays/src/ddc_lock.rs` (`PanelLocks` registry + `PanelLock` with `VcpPriority::Command` vs `Sampler` discipline + poison recovery).
**Sensor source registry:** `crates/dormant-sensors/src/registry.rs`.
**Display controller registry:** `crates/dormant-displays/src/registry.rs`.
**Doctor probes:** `crates/dormant-doctor/src/probes/{config,ddcci,ha,input_filter,mqtt,samsung,usb,wear_sampling,macos_display_catalog,macos_idle,macos_display_sleep,macos_power,macos_power_off,windows_idle}.rs`. The macOS probes and Windows idle probe are read-only. The macOS sleep probe reads `CGDisplayIsAsleep` through `RealDisplaySleepTransport`. The macOS power probe runs `pmset -g assertions`.
**Web routes:** `crates/dormant-web/src/routes/{command,config,config_apply,daemon,dismiss_flag,doctor,events,operations,pair,star_nudge,wear,wear_sampling}.rs`. `pair.rs` is the Samsung pairing wizard.
**Web config-patch module:** `crates/dormant-web/src/config_patch.rs` — pure patch hygiene / allowlist / `toml_edit` application; the `config_apply.rs` route is the only consumer. Also owns the entity-CRUD gate (`CreateEntity`/`DeleteEntity`, `CREATABLE_FIELDS`, `RESERVED_ENTITY_IDS`) — `entity_crud_enabled` itself is enforced one level up, in `config_apply.rs`, ahead of the shared 5-stage `Set`/`Remove` pipeline.
**Wear tracker:** `crates/dormantd/src/wear_tracker.rs` — pure `tick(snapshot, samples, config, now)` advances ledgers; async shell owns file I/O, `load_or_create_ledger`, and `DaemonEvent::WearSnapshot`/`CompensationAdvisory` publication.
**Render entry:** `crates/dormant-render/src/lib.rs` (re-exports `LayerShellRenderSink` + `ScreensaverSettings`).
**Tray entry:** `crates/dormant-tray/src/main.rs` (binary) and `crates/dormant-tray/src/lib.rs` (library surface).
**Tests:**
- Unit tests live in `#[cfg(test)]` modules next to the source they cover.
- Integration tests: `crates/dormant-core/tests/`, `crates/dormant-sensors/tests/`, `crates/dormantd/tests/` (`boot_rollback.rs` is the watchdog/rollback suite — `lkg_*` + `crash_loop_*` + `config_rollback_*` end-to-end scenarios), `crates/dormant-web/webui/src/__tests__/`.
- Property regressions: `crates/dormant-core/proptest-regressions/` (`state_machine.txt` and `wear.txt` — seeds for failure cases proptest has generated in the past, including `wear::resize_grid` regression cases; total-conservation invariant is checked by a live `proptest!` block co-located in `wear.rs`).
- HTTP/SPA fixtures: `crates/dormant-sensors/fixtures/`, `crates/dormant-web/webui/src/__tests__/fixtures/`, `crates/dormant-core/tests/fixtures/config/`, `crates/dormantd/tests/fixtures/pw_dump/` (real PipeWire 1.6.7 `pw-dump` captures for the audio-poller classifier — `idle`/`idle_dirty`/`movie`/`movie_paused`/`call`/`mic_only`; `include_str!`-embedded, `README.md` documents the probe and signal table).

## Naming Conventions

**Source files:** `<concept>.rs` at the crate root or one level under a sub-module. One concept per file; soft cap ~300 lines/file.
- Sensors: `crates/dormant-sensors/src/<backend>.rs` (e.g. `mqtt.rs`, `ha_ws.rs`, `usb_ld2410.rs`).
- Controllers: `crates/dormant-displays/src/<controller>.rs` (e.g. `kwin_dpms.rs`, `samsung_tizen.rs`).
- Doctor probes: `crates/dormant-doctor/src/probes/<target>.rs`.
- Web routes: `crates/dormant-web/src/routes/<resource>.rs`.
- CLI subcommands: `crates/dormantctl/src/cmd_<verb>.rs`. Short pairs share one file (`cmd_blank.rs` handles blank+wake, `cmd_pause.rs` handles pause+resume).

**Directories:** `<area>/` mirrors the crate split (`config/`, `probes/`, `routes/`, `webui/`).

**Types:** `<Name>Source` for sensors and `<Name>Controller` for displays. Registry keys usually map hyphens to underscores in module names (`usb-ld2410` maps to `usb_ld2410.rs`). The `ha` sensor is in `ha_ws.rs`.

**Test files:** `tests/<feature>.rs` for integration; `__tests__/<Component>.test.tsx` for the SPA. Co-located `#[cfg(test)]` for unit tests.

## Where to Add New Code

**New sensor source:** Create `crates/dormant-sensors/src/<name>.rs` implementing `dormant_core::traits::SensorSource`, then add it to `crates/dormant-sensors/src/lib.rs` (`pub mod`) and register in `crates/dormant-sensors/src/registry.rs` (`SOURCE_TYPES` + `build` match arm). Add the config variant to `crates/dormant-core/src/config/schema.rs` (`SensorConfig` enum).

**New display controller:** Implement `DisplayController` in `crates/dormant-displays/src/<name>.rs`. Declare the module in `lib.rs`. Register it in `registry.rs` via `CONTROLLER_TYPES`, `capabilities()`, and `build_controllers`. Gate platform-specific I/O for the platforms that have it.

**New config key:** `crates/dormant-core/src/config/schema.rs` (struct field + serde rename) + `crates/dormant-core/src/config/defaults.rs` (default shim) + `crates/dormant-core/src/config/validate.rs` (cross-reference rule AND the `KNOWN_KEYS` known-key-tree entry — both live in this file, not `config/mod.rs`). Constants go in `crates/dormant-core/src/error.rs` (`pub const E_*`).

**New doctor probe:** Create `crates/dormant-doctor/src/probes/<target>.rs`, register in `crates/dormant-doctor/src/probes/mod.rs`, re-export in `crates/dormant-doctor/src/lib.rs`, add a CLI subcommand in `crates/dormantctl/src/cmd_doctor.rs` (`DoctorSubcommand` + dispatch).

**New web route:** Create `crates/dormant-web/src/routes/<name>.rs`, mount in `crates/dormant-web/src/routes/mod.rs`, register in the router at `crates/dormant-web/src/server.rs`. SPA view goes under `crates/dormant-web/webui/src/app/views/` with a matching route in `App.tsx`.

**New CLI subcommand:** Add `cmd_<name>.rs` (or co-locate with a sibling in an existing `cmd_*.rs`), declare the variant in `crates/dormantctl/src/main.rs` (`enum Command`), and dispatch it in the same file's `fn main`.

**New tray state/menu piece:** Add a cross-platform pure-logic module under `crates/dormant-tray/src/<name>.rs` (derive icon state, build tooltip, model menu, plan actions, etc.) so it can be unit-tested without a D-Bus session bus or an `AppKit` `NSApplication`. Global hotkey handlers implement `HotkeyRegistrar` in `crates/dormant-tray/src/hotkey.rs` and platform-specific backends under `hotkey_linux/` (Linux portal/KGlobalAccel) or `hotkey_macos.rs`/`hotkey_macos_common.rs`. Wire the platform glue into `crates/dormant-tray/src/tray.rs` (Linux ksni) or `crates/dormant-tray/src/tray_macos.rs` (macOS AppKit). If the action needs a new side effect, extend `crates/dormant-tray/src/dispatch.rs` (`DispatchPlan` variant + `DispatchCapabilities` trait method); both `SystemCapabilities` and the macOS path share the same `execute_plan` seam. Add new SVG glyphs to `crates/dormant-tray/assets/glyphs/` — `build.rs` embeds them at compile time (unpremultiplying StatusNotifier icon pixels for straight-alpha composition).

**New render ladder stage:** Add the shared pure logic to `crates/dormant-render/src/<module>.rs` (so it compiles on every platform), implement the Linux-only Wayland glue under `crates/dormant-render/src/linux/`, and update the non-Linux stub at `crates/dormant-render/src/stub.rs` to keep the unconditional `use dormant_render::…` working.

**New web SPA component:** `crates/dormant-web/webui/src/app/components/<Name>.tsx` (pure) or `crates/dormant-web/webui/src/app/views/<View>.tsx` (routed). Hooks go in `crates/dormant-web/webui/src/app/hooks/`. Tests co-locate as `__tests__/<Name>.test.tsx`.

**New error code:** `crates/dormant-core/src/error.rs` — add `pub const E_<NAME>: &str`, add a variant to `DormantError` with `#[error("E_<NAME>: ...")]`, and map it in `DormantError::code()`.

**New wear sub-feature:** Pure model goes in `crates/dormant-core/src/wear.rs` (extend `WearLedger`/`PanelType`/identity helpers; bumping `WEAR_SCHEMA_VERSION` requires a migration branch in `load_or_create_ledger`). Sampling/persistence/advisory emission lives in `crates/dormantd/src/wear_tracker.rs` (the pure `tick` first, then a `TrackerAction` for the shell to execute). Wire events are additive `DaemonEvent` variants in `crates/dormant-core/src/rules.rs` — keep the `#[serde(other)] Unknown` catch-all so older consumers keep streaming. HTTP exposure goes in `crates/dormant-web/src/routes/wear.rs`, mounted in `routes/mod.rs` and the router at `crates/dormant-web/src/server.rs`; the SPA widget is `crates/dormant-web/webui/src/app/components/WearCard.tsx` (used by `app/views/Overview.tsx`) and the config editor is `crates/dormant-web/webui/src/app/config/WearSection.tsx`.

**New MQTT state publish mapping:** Pure state-to-record mapping logic goes in `crates/dormantd/src/state_publisher.rs` (returns `PublishRecord`s for discovery, snapshot, or incremental event mapping). Configure via `[publish]` section in `crates/dormant-core/src/config/schema.rs` (`PublishConfig`).

**New controller with a DDC/CI-shaped readback:** Implement the per-controller contracts in `crates/dormant-core/src/traits.rs` (`DisplayController::read_state`/`read_state_sampled`/`read_usage_hours`/`panel_identity`/`claim_identity`); serialize physical transactions through the `PanelLock` (`crates/dormant-displays/src/ddc_lock.rs`) and the process-wide physical-DDC gate in `crates/dormant-displays/src/vcp_ops.rs`, using `VcpPriority` for command-versus-sampler access. The chain-walk overrides on `dormant-displays/src/executor.rs` ensure the sampler priority is preserved across the `CommandSink` boundary.

**Shared utilities:** Put pure logic in `crates/dormant-core/` and I/O in the crate that owns it. Use `state_dir()` from `crates/dormant-core/src/paths.rs` for daemon state. Use `state_dir_with` when the target and environment are explicit.
