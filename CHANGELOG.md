# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Allowed selector-bearing macOS controller chains to use `macos-display-sleep` as a fallback after per-display controllers.

## [0.3.0] - 2026-07-17

### Added

- Web UI v2 parity and polish: persistent boot-rollback/failure banners, global emergency wake, browser-launched control-path exercise, per-display wear heat maps and exposure summaries, guarded quick controls, exact event badges, shared confirmation dialogs, daemon-identity sidebar footer, and `GET /api/daemon`.
- macOS (M1) support, arm64 and x86_64: DDC/CI display control shared with Linux (vendored `ddc-macos` fork), the `macos-gamma-black` audio-safe Quartz gamma-table blank controller (with a daemon-independent breadcrumb-based emergency-restore path, `dormantctl emergency-wake`), the `macos-display-sleep` whole-machine `pmset` fallback controller, a CoreGraphics idle source, and read-only `dormantctl doctor macos-idle` / `macos-display-sleep` / `macos-power` diagnostics.
- `dormantctl launchd install` / `launchd uninstall` (macOS only): installs/removes the checked-in per-user `LaunchAgent` plist (`RunAtLoad`, `KeepAlive.SuccessfulExit=false`, `ThrottleInterval=10`) at the canonical `~/Library/LaunchAgents/com.legionworks.dormant.plist`, idempotently and without root.
- cargo-dist release artifacts for `aarch64-apple-darwin` and `x86_64-apple-darwin`, each bundling the checked-in `LaunchAgent` plist (`share/com.legionworks.dormant.plist`) at the same bytes `launchd install` embeds; the release-artifact smoke test now runs across all four targets (adding a `plutil -lint` check on the plist for the two macOS targets).
- `dormant-tray` is packaged for macOS but is **not functional there** — a KDE `StatusNotifierItem` applet has no macOS equivalent yet.

### Changed

- Web UI confirmation dialogs and emergency-wake styling now match the v2 design system.
- Required CI contexts include macOS test and MSRV lanes.

### Fixed

- Prevented wake-stranding executor races by waking the successful blank owner first, checking superseding dispatch tokens, and retaining blank ownership across reloads when the dispatch-relevant controller chain is unchanged.
- Re-resolve the Wayland target output when showing the render ladder so output re-creation after an input switch cannot wedge the screensaver.
- Corrected unsafe FFI in the vendored `ddc-macos` backend: the `CGDisplayIsAsleep` ABI and ARM/Intel DDC buffer handling.
- Released Core Foundation and IOKit resources through RAII wrappers in the vendored macOS backend.

## [0.2.0] - 2026-07-14

### Added

- Panel-wear tracking with brightness-weighted on-hours, local JSON ledgers, DDC/CI VCP `0xC0` seeding, `GET /api/wear`, panel-exposure cards, `wear.*` settings, and `displays.<id>.panel_type`. v1 attribution is panel-wide and advisory.
- Pixel shift for `render_screensaver`, defaulting to 2 px every 2 minutes. `displays.<id>.screensaver.shift_px = 0` disables it; `render_black` never shifts.
- Failure notifications for repeated wake failures and exhausted blank controller chains, plus recovery notices, a tray `Failure` state, and a web-dashboard failure banner. Desktop notices are configured through `notifications.*`.
- MQTT authentication through `credentials.toml`.
- MQTT retained-value handling on subscribe and reconnect; configurable `sensors.<id>.availability_topic`, `availability_payload_online`, and `availability_payload_offline`; warn-once handling for unknown availability payloads; and the `reported` sensor diagnostic.
- Watchdog + last-known-good rollback: health-gated LKG snapshots, boot rollback for invalid or crash-looping configs, and a `Type=notify` systemd unit with `WatchdogSec=150`.
- Web entity creation/deletion for sensors, zones, displays, and rules, gated server-side by `daemon.entity_crud_enabled` (default `true`).
- Samsung pairing through the web wizard and `dormantctl pair samsung <host>`, with tokens written atomically to `credentials.toml`.
- `dormantctl emergency-wake`, which tries IPC first and falls back to direct controller access when the daemon is unavailable.
- `dormantctl doctor exercise <display>` for blank/read/wake/read/restore control-path verification.
- Samsung `brightness_zero` blanking over IP Control G2 port 1516, preserving source audio while dimming the panel near-black.
- Audio- and call-aware blanking: a `pw-dump`-polling PipeWire inhibitor (`"audio-playback"` / `"call"` rule literals) that holds a display awake while a running output stream plays or a call is active, independently of and combinable with the existing user-activity inhibitor. Configured through the global `[audio]` section (`poll_interval`, `min_active`, `call_roles`, `playback_roles`, `capture_is_call`, `pw_dump_command`); fails toward blanking on any probe error (missing binary, timeout, malformed output, or a bounded-retry circuit breaker after repeated unreapable subprocesses). `capture_is_call` (microphone-as-call) defaults to `false` to avoid false positives from idling mic-capable apps.

### Changed

- MQTT validation now rejects state/availability topic collisions on the same broker and conflicting payload literals on a shared availability topic.
- Failure state survives config reload when a display's dispatch path is unchanged; changing that path voids stale failure evidence.
- Runtime footprint is bounded by two Tokio workers, `malloc_trim` after screensaver teardown, and `MALLOC_ARENA_MAX=2` in the systemd unit.
- Existing screensaver configs receive pixel shift by default; set `displays.<id>.screensaver.shift_px = 0` to retain a fixed surface.

### Fixed

- Prevented Samsung `brightness_zero` from saving a zero pre-blank value across restart. `displays.<id>.samsung_restore_backlight = 0` and `displays.<id>.restore_brightness = 0` are now rejected so wake always restores a visible level.

### Removed

- Outdated M2 web-design handoff prototypes (`*.dc.html`, `support.js`, and the handoff README). Production assets and the load-bearing design-system files remain under `design/web-ui/assets/` and `design/web-ui/_ds/`.

## [0.1.0] - 2026-07-09

### Added

- Daemon core: strict config schema, zone fusion (`any` / `all` / `quorum` / `weighted`), rules engine, per-display state machine, hot reload with phase carry-over, and a per-user single-instance `flock` guard.
- Fail-safe presence policy: sensor data loss resolves to `unavailable` (treated as present), never `absent`.
- Sensor sources: MQTT, Home Assistant WebSocket, and USB-serial LD2410 mmWave radar.
- Display controllers: `ddcci`, `samsung-tizen` picture-off/power/Wake-on-LAN, `kwin-dpms`, `ha-passthrough`, and `command`, with ordered fallback and bounded wake retry.
- Render ladder (`render` feature): Wayland black overlay and a muted libmpv screensaver with folder/URL playlists, scaling modes, crossfades, and timed escalation.
- Manual-only displays: displays referenced by no rule remain hand-controllable and are never auto-blanked.
- Control surfaces: `dormantctl`, a loopback-only web dashboard with a validated config editor, and a KDE `StatusNotifierItem` tray.
- Web UI embedded in the daemon behind the `web-ui` feature, with loopback binding and origin checks.
- Delivery: CI matrix, cargo-dist release pipeline, and an mdBook manual.

### Fixed

- Samsung control WebSocket handshake, idle-socket liveness, heartbeat tracking, and reconnect behavior.
- Samsung port-1516 request headers and protocol handling.
- Tray reconnect backoff after healthy connections and config reloads.
- Config editor serialization of absent optional fields.
- Control messages issued during a config-reload generation swap are retried instead of dropped (#9, #19).
- `dormantctl validate` no longer rejects render-ladder configs solely because the CLI binary lacks the `render` feature (#18).

### Changed

- CI runs on the `dev` integration branch; `master` is release-only.

[Unreleased]: https://github.com/legion-works/dormant/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/legion-works/dormant/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/legion-works/dormant/releases/tag/v0.1.0
