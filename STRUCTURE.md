# Codebase Structure

## Directory Layout

```
oled-proximity/
├── crates/
│   ├── dormant-core/         # Pure domain logic: types, traits, config, rules, state machine, IPC, reload, doctor wire types
│   ├── dormant-sensors/      # Sensor sources: MQTT, HA WebSocket, USB LD2410 + registry
│   ├── dormant-displays/     # Display controllers: command, ddcci, kwin-dpms, samsung-tizen (+ samsung_ip IP-Control-G2 transport), ha-passthrough + executor/registry
│   ├── dormant-doctor/       # Offline + live coalesced hardware/connectivity probes (config, mqtt, ha, usb, ddcci, samsung)
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
- Contains: `types`, `traits`, `error`, `ipc_proto`, `paths`, `reload`, `rules`, `state_machine`, `zone`, `config` (`schema`/`defaults`/`validate`/`mod`), `doctor` (wire types only), `fakes` (gated by `test-fakes` feature).
- Key files: `crates/dormant-core/src/lib.rs`, `crates/dormant-core/src/config/schema.rs`, `crates/dormant-core/src/rules.rs`, `crates/dormant-core/src/zone.rs`, `crates/dormant-core/src/error.rs`.

**`crates/dormant-sensors/`**
- Purpose: Sensor sources that emit `PresenceEvent`s. One module per sensor.
- Contains: `mqtt.rs`, `ha_ws.rs`, `usb_ld2410.rs`, `backoff.rs`, `registry.rs`.
- Key files: `crates/dormant-sensors/src/registry.rs` (explicit static registry — add new sources here).

**`crates/dormant-displays/`**
- Purpose: Display controllers that turn rules-engine `CommandSink` calls into real blank/wake operations.
- Contains: `command.rs`, `ddcci.rs` (Linux-only), `kwin_dpms.rs` (Linux-only), `samsung_tizen.rs` (port 8002 WebSocket remote control + Wake-on-LAN + network pairing), `samsung_ip.rs` (port 1516 IP Control G2 JSON-RPC — `backlightControl` for the audio-safe `brightness_zero` blank path), `ha_passthrough.rs`, `vcp_ops.rs`, `executor.rs` (per-display fallback chain + retry), `registry.rs`.
- Key files: `crates/dormant-displays/src/registry.rs` (`CONTROLLER_TYPES`, `capabilities()`, `build_controllers()`).

**`crates/dormant-doctor/`**
- Purpose: Probe logic + live coalesced `DoctorService`. Re-exports the wire types from `dormant_core::doctor`.
- Contains: `probes/` (config, ddcci, ha, mqtt, samsung, usb — one file per probe; `samsung.rs` covers reachability on ports 8001/8002, REST power-state, and `credentials.samsung.<host>` token presence), `service.rs` (singleflight `DoctorService`), `types.rs` (`ProbeResult`/`ProbeStatus`).
- Key files: `crates/dormant-doctor/src/lib.rs` (`probe_all_offline` is the bare-doctor entry point used by `dormantctl doctor`; `probe_samsung` is the per-target dispatch).

**`crates/dormant-web/`**
- Purpose: Optional loopback-only web dashboard. Gated behind the `web-ui` Cargo feature of `dormantd`; when off, zero web code compiles.
- Contains: `server.rs` (axum router), `routes/` (command, config, doctor, events), `state.rs` (`WebState`/`WebStateInner`), `assets.rs` (embedded SPA), `error.rs`, `security.rs`.
- Key files: `crates/dormant-web/src/lib.rs` (`spawn` is the entry point), `crates/dormant-web/src/routes/mod.rs` (mount the routes).
- Web UI assets: `crates/dormant-web/webui/src/` (SPA — React/Vite — `App.tsx`, `main.tsx`, `app/views/*`, `app/components/*`, `app/hooks/*`, `__tests__/`).

**`crates/dormantd/`**
- Purpose: The daemon binary — wires config → sensors → zones → rules → displays, with hot reload, inhibit-activity, IPC, optional web UI.
- Contains: `app.rs` (`App`, `AppHandle`, generation assembly + reload + teardown), `inhibit_activity.rs`, `idle_source.rs`, `ipc.rs`, `single_instance.rs` (per-user-session `flock` guard — non-config-overridable), `reload.rs`, `logging.rs`, `lib.rs`, `main.rs`. Also: `systemd/dormant.service` (unit file), `tests/` (`daemon_smoke.rs`, `ipc_roundtrip.rs`, `web_config_apply.rs`).
- Key files: `crates/dormantd/src/main.rs` (binary entry), `crates/dormantd/src/app.rs` (runtime assembly).

**`crates/dormantctl/`**
- Purpose: CLI companion. Talks to a running daemon over the Unix socket; some subcommands are offline (validate, doctor). Also exposes an IPC `client` library entry for out-of-binary consumers (e.g. `dormant-tray`). Emergency-wake falls back to direct-hardware probing + waking through the registry when the daemon is wedged or unreachable.
- Contains: `main.rs` (clap dispatch), `lib.rs` (`pub mod client` re-export for library users), `client.rs` (IPC client), `cmd_blank.rs` (blank + wake), `cmd_doctor.rs` (per-target subcommands + `exercise` for control-path verification), `cmd_emergency_wake.rs` (`dormantctl emergency-wake` — IPC fast path with a direct-hardware fallback when the daemon is unresponsive; probes each freshly-built executor before waking), `cmd_pair.rs` (`dormantctl pair samsung <host>` — connects to the TV, prompts the on-screen allow, and stores the returned token via `dormant_core::config::upsert_samsung_token`), `cmd_pause.rs` (pause + resume), `cmd_status.rs`, `cmd_validate.rs`, `cmd_watch.rs`.
- Key files: `crates/dormantctl/src/main.rs` (register new subcommands here).

**`crates/dormant-render/`**
- Purpose: Local Wayland layer-shell `RenderSink` — software-blank black overlay (final ladder fallback) and libmpv-driven screensaver overlay. Linux-only Wayland I/O; non-Linux stub exposes the same surface so `use dormant_render::LayerShellRenderSink` compiles everywhere.
- Contains: `lib.rs` (cross-platform re-export), `command.rs` (async→sync bridge encode), `latch.rs` (T7 first-input-event latch), `playlist.rs` (screensaver item list), `settings.rs` (`ScreensaverSettings`, `ScaleMode`, `TransitionMode` — re-exported), `screensaver.rs` (Linux-only libmpv backend), `linux/` (real Wayland layer-shell impl: `blend.rs`, …), `stub.rs` (non-Linux no-op sink), `examples/`.
- Key files: `crates/dormant-render/src/lib.rs` (entry points `LayerShellRenderSink` + `ScreensaverSettings`), `crates/dormant-render/src/settings.rs` (config-mirror types).

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
- `crates/dormant-core/src/config/validate.rs` — cross-reference validation rules.
- `crates/dormant-core/src/config/mod.rs` — loader + known-key tree for unknown-key detection.
- `crates/dormant-core/src/paths.rs` — XDG path resolution (`config_path`, `socket_path`, `sibling_credentials`).
- `examples/config.toml`, `examples/credentials.toml` — working reference configs.

**Domain logic:** `crates/dormant-core/src/{types,traits,rules,state_machine,zone,reload,ipc_proto,error,doctor}.rs`.
**Display executor (fallback + retry):** `crates/dormant-displays/src/executor.rs`.
**Sensor source registry:** `crates/dormant-sensors/src/registry.rs`.
**Display controller registry:** `crates/dormant-displays/src/registry.rs`.
**Doctor probes:** `crates/dormant-doctor/src/probes/{config,ddcci,ha,mqtt,samsung,usb}.rs`.
**Web routes:** `crates/dormant-web/src/routes/{command,config,config_apply,doctor,events}.rs`.
**Web config-patch module:** `crates/dormant-web/src/config_patch.rs` — pure patch hygiene / allowlist / `toml_edit` application; the `config_apply.rs` route is the only consumer.
**Render entry:** `crates/dormant-render/src/lib.rs` (re-exports `LayerShellRenderSink` + `ScreensaverSettings`).
**Tray entry:** `crates/dormant-tray/src/main.rs` (binary) and `crates/dormant-tray/src/lib.rs` (library surface).
**Tests:**
- Co-located `#[cfg(test)] mod tests` in every source file.
- Integration tests: `crates/dormant-core/tests/`, `crates/dormant-sensors/tests/`, `crates/dormantd/tests/`, `crates/dormant-web/webui/src/__tests__/`.
- Property regressions: `crates/dormant-core/proptest-regressions/`.
- HTTP/SPA fixtures: `crates/dormant-sensors/fixtures/`, `crates/dormant-web/webui/src/__tests__/fixtures/`, `crates/dormant-core/tests/fixtures/config/`.

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

**New config key:** `crates/dormant-core/src/config/schema.rs` (struct field + serde rename) + `crates/dormant-core/src/config/defaults.rs` (default shim) + `crates/dormant-core/src/config/validate.rs` (cross-reference rule) + `crates/dormant-core/src/config/mod.rs` (known-key tree). Constants go in `crates/dormant-core/src/error.rs` (`pub const E_*`).

**New doctor probe:** Create `crates/dormant-doctor/src/probes/<target>.rs`, register in `crates/dormant-doctor/src/probes/mod.rs`, re-export in `crates/dormant-doctor/src/lib.rs`, add a CLI subcommand in `crates/dormantctl/src/cmd_doctor.rs` (`DoctorSubcommand` + dispatch).

**New web route:** Create `crates/dormant-web/src/routes/<name>.rs`, mount in `crates/dormant-web/src/routes/mod.rs`, register in the router at `crates/dormant-web/src/server.rs`. SPA view goes under `crates/dormant-web/webui/src/app/views/` with a matching route in `App.tsx`.

**New CLI subcommand:** Add `cmd_<name>.rs` (or co-locate with a sibling in an existing `cmd_*.rs`), declare the variant in `crates/dormantctl/src/main.rs` (`enum Command`), and dispatch it in the same file's `fn main`.

**New tray state/menu piece:** Add a pure-logic module under `crates/dormant-tray/src/<name>.rs` (derive icon state, build tooltip, model menu) so it can be unit-tested without a D-Bus session bus. Wire the Linux-only glue into `crates/dormant-tray/src/tray.rs` and `crates/dormant-tray/src/ipc_loop.rs`. Add new SVG glyphs to `crates/dormant-tray/assets/glyphs/` — `build.rs` embeds them at compile time.

**New render ladder stage:** Add the shared pure logic to `crates/dormant-render/src/<module>.rs` (so it compiles on every platform), implement the Linux-only Wayland glue under `crates/dormant-render/src/linux/`, and update the non-Linux stub at `crates/dormant-render/src/stub.rs` to keep the unconditional `use dormant_render::…` working.

**New web SPA component:** `crates/dormant-web/webui/src/app/components/<Name>.tsx` (pure) or `crates/dormant-web/webui/src/app/views/<View>.tsx` (routed). Hooks go in `crates/dormant-web/webui/src/app/hooks/`. Tests co-locate as `__tests__/<Name>.test.tsx`.

**New error code:** `crates/dormant-core/src/error.rs` — add `pub const E_<NAME>: &str`, add a variant to `DormantError` with `#[error("E_<NAME>: ...")]`, and map it in `DormantError::code()`.

**Shared utilities:** A new pure-logic helper belongs in `crates/dormant-core/` under a topical module. I/O helpers go in the relevant crate (`dormant-sensors`, `dormant-displays`, `dormant-doctor`, `dormant-web`).