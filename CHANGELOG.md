# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-07-27

### Highlights

**Web UI v3** — edit every config section (coordination, keymap, input filter, hooks) from the web Settings editor, inspect shared-display ownership with live pull/push feedback on the new Switching view, and keep event history across page reloads. See [the web UI chapter](./docs/src/web-ui.md).

### Added

- six-tab Settings layout (Daemon, Sensors, Zones, Rules, Displays, Coordination). Switching view surfaces `Ownership` daemon events over the WebSocket and shows both machines' input codes, the panel state, poll cadence, and agreement verdict. `/api/events/recent` seeds the event log on page load so history survives a reload. Doctor checks are grouped by subject. Panel-wear detail now splits wear time into seeded (pre-existing) and measured (new samples) with a per-cell heat map. The rollback banner surfaces the recovery command when one is configured. A security-posture card honestly states what the Host and Origin guards defend, and what they don't. Hook editing is gated behind the new `daemon.hook_edit_enabled` flag (default off); without it hooks are read-only everywhere.

### Fixed

- Web UI matches the v3 design: corrected type scale and panel styling, the mock's Switching and display-detail layouts, and literal `\uXXXX` escape sequences that rendered as raw text in several views.
- Web Apply no longer falsely rejects shared-display configs (the apply route validated with an empty input-source-reader set, unlike the daemon). Also a batch of operator-reported UI fixes: event badge overflow, the history sentinel rendering as a raw row, unstyled action buttons, the shared-blank warning displacing the actions row, and display-detail card padding.

## [0.7.1] - 2026-07-26

### Fixed

- Corrected documentation that had drifted from shipped behavior: multi-machine ownership and switching flows, hook blocking defaults, the `/api/push` route in the security-surface lists, and DDC retry semantics.
- The LD2410C ESPHome example now ships hardware-calibrated defaults — `still_energy_floor` 30 → 28 and `near_cutoff` 2.5 m → 1.6 m — so the distance fallback covers the desk chair without latching onto seating behind it.
- The daemon smoke tests no longer carry a race condition where the coordinator's batch watcher could observe a stale file state during repeated config reloads, causing spurious test failures (#143).

## [0.7.0] - 2026-07-26

### Breaking

- `coordination.enabled` and eleven sibling keys were removed. Delete them from your `config.toml` and run `dormantctl validate` before upgrading — strict unknown-key validation rejects a config that still carries them.

### Highlights

**Soft KVM — share one monitor between two machines.** Press a hotkey on either machine to pull the panel to it, or use `dormantctl switch`, the tray, or the web dashboard. An opt-in mode follows local input activity. See [Multi-machine](./docs/src/multi-machine.md).

### Added

- The LD2410C example config exposes all 18 per-gate sensitivity thresholds (gates 0-8, moving and static), an energy-gated `desk_seated` presence template, and a tunable still-energy floor. Gates 0 and 1 have no settable static sensitivity in firmware, so a person sitting still within ~75 cm cannot be detected as a still target — per-gate tuning alone cannot fix close-range seated presence, and the energy-gated template is the mitigation. Requires reflashing the device and repointing the sensor's MQTT topic; see `docs/src/sensors.md` ([#135](https://github.com/legion-works/dormant/issues/135)).

### Changed

- Shared-display switching is now a direct local DDC write (`dormantctl switch` writes the local input code; `dormantctl switch --to-peer` writes the peer's code when `shared_peer_input_write_code` is configured).
- Ownership is an observation from VCP `0x60` polling, not a claim-negotiated authority — nothing consults ownership before writing.
- Surviving `[coordination]` keys: `poll_interval`, `state_poll_interval`, `loss_confirmations`, `activity_follow`, `arm_after`, `cooldown`.
- New `on_observed_loss` hook slot: fires after the poller commits an ownership loss (post-hoc, fire-and-forget).
- Activity-follow pulls are gated by `activity_follow` (default `false`), `arm_after` (default `7s`), and `cooldown` (default `3s`).

### Fixed

- Input-source write verification no longer reports `E_DISPLAY_IO` for a switch that succeeded. The readback is retried three times with a 200 ms delay on transport errors; a clean read carrying the wrong value still fails immediately, so a silently-ignored VCP `0x60` write is detected. Measured on a shared panel, the verification read garbles roughly one time in three ([#138](https://github.com/legion-works/dormant/issues/138)).
- Coordination poll reads that decode outside the display's configured code set (`shared_input_code` / `shared_peer_input_code`) are treated as transport failures, holding the last verdict, instead of disagreements that reset the debounce. On the reference panel ~30% of reads under concurrent DDC access decode to a plausible-but-wrong input, including codes that exist in the panel's capability string ([#138](https://github.com/legion-works/dormant/issues/138)).
- Exit codes for a failed switch now agree between the pull and push paths ([#138](https://github.com/legion-works/dormant/issues/138)).
- A verified local pull feeds ownership immediately instead of waiting for the debounced poll to rediscover the machine's own write, removing ~7.5 s of visible wake lag on the acquiring machine. The debounce still governs loss and poll-observed gain; ownership remains an observation and the poll still causes no writes ([#139](https://github.com/legion-works/dormant/issues/139)).
- Hook children inherit five session environment variables when present — `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DISPLAY`, `XDG_SESSION_TYPE`, `DBUS_SESSION_BUS_ADDRESS` — via a documented allowlist. Without them a compositor command such as `kscreen-doctor` aborts, so no such hook had ever executed successfully. `PATH` remains hard-coded ([#140](https://github.com/legion-works/dormant/issues/140)).
- Both `hook_timeout` log arms now carry the `reason` field, which was previously discarded — a non-timeout failure could be logged as a timeout with no way to tell what actually failed ([#140](https://github.com/legion-works/dormant/issues/140)).
- Render-only ladders are no longer reload-rejected with `E_MODE_UNSUPPORTED`. Hardware blank-mode validation is skipped when the normalised ladder contains no controller stage; ladders that contain one are validated unchanged ([#122](https://github.com/legion-works/dormant/issues/122)).
- The DDC/CI D6 power-control probe retries three times with a 50 ms backoff, so a transient bus error no longer drops `PowerOff` from a display's supported modes for the lifetime of the generation ([#123](https://github.com/legion-works/dormant/issues/123)).
- `dormantctl doctor --report-issue` writes a draft when the configuration fails to load, rendering a config-failed probe row and skipping the dependent probes, instead of producing nothing ([#117](https://github.com/legion-works/dormant/issues/117)).
- The draft's configuration-validity line reflects only the config probe, rather than every probe result — a failing MQTT or DDC probe no longer makes the draft claim the configuration is invalid ([#116](https://github.com/legion-works/dormant/issues/116)).
- MQTT usernames shorter than the minimum secret length are redacted in issue drafts. They are collected separately and re-injected after the length filter, so short strings in general are still not over-redacted ([#118](https://github.com/legion-works/dormant/issues/118)).
- Draft files are created with `create_new` and a retry on collision, closing a check-then-write race where concurrent runs could overwrite each other's output ([#119](https://github.com/legion-works/dormant/issues/119)).

### Removed

- The owner-mediated claim protocol (mDNS discovery, SPAKE2 pairing, Ed25519 signed frames, TCP transport, claim-engine state machine, peer store). Replaced by direct local DDC writes — each machine writes its own input code over its own DDC bus with no network protocol, peer connection, handshake, or crypto. See `docs/src/multi-machine.md`.
- The instance-pairing web routes (`/api/pair/instance`, `/api/pair/instance/join`, `/api/pair/instance/:id/cancel`). Samsung TV pairing (`/api/pair/samsung`) is unaffected.

## [0.6.0] - 2026-07-23

### Added

- Multi-machine shared-display coordination: opt-in `scope = "shared"` displays arbitrate ownership through DDC/CI VCP `0x60` (active input source) — a daemon reads the live input, polls and caches ownership verdicts, reconciles a panel on ownership acquisition, exposes ownership on the IPC wire, and preserves coordination state across config reloads. Configured through the new `[coordination]` section; the ownership poller self-disables when no shared displays remain. Tray, web, and doctor surfaces report shared-panel ownership and recovery.
- SPAKE2-protected dormant-instance pairing: window-gated mDNS discovery of dormant peers, a code-confirmed SPAKE2 pairing handshake with a hardened frame transport (bounded concurrent connections to resist a slow-loris window, strict-origin on parameterized routes, honest 409/cancel/expiry semantics), a persistent paired-instance peer store, and operator surfaces in `dormantctl pair instance`, web routes, and a web pairing wizard.
- Native macOS menu-bar tray (`NSStatusItem`) with launchd autostart, replacing the non-functional KDE-`StatusNotifierItem`-only tray on macOS (#115).

### Fixed

- NVIDIA DDC polling no longer convoys the RM driver lock or busy-loops a core: the ddc-hi display handle is cached across VCP ops (serialized under panel lock, with VCP error classification and a bounded/absolute handle max-age), DDC enumeration is gated behind a process-wide physical-DDC barrier, and input-poll and state-poll cadences are split (`state_poll_interval` derives from `poll_interval`). Eliminates the desktop-wide compositor stutter and drops the userspace busy-loop from ~40% to ~6% of a core (#127, resolves #120).
- Instance-pairing hardening: bounded concurrent pairing connections resist a slow-loris window, the pairing code is surfaced in the open response with hardened frame-order handling and dedup identity derivation, parameterized pairing routes enforce strict-origin with honest 409/cancel/expiry semantics and wizard lifecycle parity, the peer store caps load size and zeroizes write temporaries, and the pairing wizard drops an unused import and null-guards its inputs.
- Coordination correctness: a panel is reconciled on ownership acquisition, the ownership poller is skipped when no shared displays remain, and mDNS validation enforces non-empty `display_name` (with a corrected coordination default comment).
- The web IPC bridge's unix-socket path is gated for Windows portability (#128).
- `dormantctl doctor` gates the `probe_ddcci_with_locks` probe to Linux + macOS (#128).
- CI/test harness: nextest JUnit output is resolved from the manifest workspace root, and changed-test stress handles the vendored workspace and crate-root ownership (#128). The macOS soak jobs drop a Linux-only watcher-delivery filter (#113).
- macOS: clippy pedantic debt cleared in the vendored `ddc-macos` backend (cfg-gated linux items, raw-ref ffi pointers).

### Docs

- Multi-machine coordination and instance-pairing documentation: protocol ratification (SPAKE2 + mdns-sd), a folded pairing-protocol security review (pinning spake2 0.4.0, transcript/zeroize/peer-store invariants), and structure-docs registration of the pairing wizard.

### Known issues

- macOS over USB-C: `blank_mode = "power_off"` with a `ddcci`-led chain is a one-way door — ddcci D6 standby makes the panel's DDC service vanish, and `macos-gamma-black` is ineligible in PowerOff mode (it registers only BrightnessZero), so wake cannot re-enumerate the display and may require physical intervention. Prefer a non-`power_off` blank mode on macOS USB-C until #126 is resolved.

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

[Unreleased]: https://github.com/legion-works/dormant/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/legion-works/dormant/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/legion-works/dormant/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/legion-works/dormant/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/legion-works/dormant/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/legion-works/dormant/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/legion-works/dormant/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/legion-works/dormant/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/legion-works/dormant/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/legion-works/dormant/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/legion-works/dormant/releases/tag/v0.1.0
