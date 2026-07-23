# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Multi-machine shared-display coordination: local DDC/CI input ownership,
  opt-in mDNS discovery, and SPAKE2-protected dormant-instance pairing.

## [0.5.0] - 2026-07-19

### Added

- `daemon.generation_barrier_ack_timeout` (default `2s`): bounds how long a config reload waits for the running engine to acknowledge the generation barrier before the daemon force-restarts itself, so a wedged engine can never hang a reload indefinitely (#104).

### Fixed

- Config-reload input handling is now exactly-once and correlated: control/watcher/web reload requests are routed through a single causal coordinator, front-door inputs are paused-and-queued across a generation swap and released after install (no more dropped-or-duplicated commands during a reload), and reload outcomes carry causal receipts so a caller can tell which reload its request completed. Resolves the long-standing reload-race behind the intermittent `config_watch` test failure (#92, #104).
- MQTT sensor reconnects are more robust: each connection uses a unique client ID (no silent broker-side takeover when a stale session lingers) and subscription acknowledgements are validated, so a rejected subscription surfaces instead of silently dropping presence updates (#107).
- The `real_ddcutil_reports_not_installed_in_this_sandbox` doctor test no longer false-fails local pre-push on developer machines that have `ddcutil` installed; it skips the not-installed assertion when `ddcutil` is on `PATH` while preserving the assertion in CI (#110).

### Changed

- CI/test infrastructure hardened (workflow-only, no runtime behavior change): nextest `ci`/`stress`/`soak` profiles with `flaky-result = fail` (a retry-pass now fails the run), a tracked flake-incident ledger with a proving-test anchor requirement, shared gate scripts so local Lefthook hooks and CI run identical commands (parity-enforced), per-job timeouts and pinned tool versions, changed-test cross-platform stress jobs, a nightly high-risk soak workflow, and rejection of same-SHA CI reruns so a red required check can't be re-run green (#104, #107, #109).

### Docs

- Reworked the LD2410C example config and sensor guide around a tested ESP32-C6 build, documenting the MQTT-vs-USB wiring choice and the fail-safe availability symmetry between them (#106).

## [0.4.0] - 2026-07-18

### Added

- `dormantctl doctor --report-issue [PATH]` and `--draft-feature [PATH]` now generate prefilled GitHub issue drafts from doctor probe results, with value-based redaction of config and credential secrets and IPv4 scrubbing in the draft text (#93).
- The release pipeline now publishes Homebrew formulas to `legion-works/homebrew-tap`; install binaries with `brew install legion-works/tap/<binary>` (#91).
- The `dormant-bin` AUR package is now published automatically after each release announcement (#90, #97).
- Linux release tarballs now include the systemd user units (#90).

### Fixed

- Wear-ledger persistence no longer collides between processes: temporary files use unique PID-and-sequence names, and stale temporary files are pruned at startup (#95).
- Retired three macOS timing races in the daemon smoke tests (#95).

## [0.3.1] - 2026-07-17

### Fixed

- Allowed selector-bearing macOS controller chains to use `macos-display-sleep` as a fallback after per-display controllers — the documented recommended macOS chain `["ddcci", "macos-gamma-black", "macos-display-sleep"]` now passes validation.
- The web UI version label now shows the running daemon's actual version (from `GET /api/daemon`) instead of a hardcoded "pre-alpha" literal.
- macOS startup gamma-restore events (`gamma_stale_breadcrumb_restored` and siblings) are deferred until logging is initialised instead of being lost — the crash-recovery restore is now visible in the log.
- Two macOS CI-lane test flakes retired: the wear shutdown-persist test now awaits the daemon join instead of racing it, and the LKG sidecar test tolerates a reload-armed first candidate under scheduler pressure.

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

[Unreleased]: https://github.com/legion-works/dormant/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/legion-works/dormant/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/legion-works/dormant/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/legion-works/dormant/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/legion-works/dormant/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/legion-works/dormant/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/legion-works/dormant/releases/tag/v0.1.0
