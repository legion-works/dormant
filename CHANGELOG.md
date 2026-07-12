# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Commits use [Conventional Commits](https://www.conventionalcommits.org/), which drive the release-generated notes; this file is the curated, human-readable summary.

## [Unreleased]

### Added

- Panel-wear tracking: a daemon-lifetime tracker samples brightness-weighted on-time for displays that expose a real readback (`ddcci`, `samsung-tizen`), persists a per-display JSON ledger under `$XDG_STATE_HOME/dormant/wear` (fallback `~/.local/state/dormant/wear`), and seeds prior usage from a DDC/CI panel's own VCP `0xC0` counter where available. Config-only gate (`[wear] enabled`, default `true`); zero new Cargo features.
- Compensation advisory: a "no long standby window in N days" nudge in the web UI when a display has gone longer than `wear.advisory_after` (default 96h) without a qualifying blanked dwell. Strictly advisory — v1 makes no dwell-enforcement changes to the state machine.
- Web UI: a per-display "Panel exposure" card (`GET /api/wear`, `GET /api/wear/<display>`) honestly labeled as a v1 uniform-exposure ledger — no spatial/heat-map attribution yet. `WearSummary` now also carries `hours_since_long_dwell`, so the advisory line always shows a real day count, even for a display that has never had an observed long dwell yet (the common first-load case), instead of falling back to "?".
- Config: new `[wear]` section (`enabled`, `sample_interval`, `persist_interval`, `read_timeout`, `grid_rows`, `grid_cols`, `fallback_brightness`, `screensaver_factor`, `short_cycle_dwell`, `advisory_after`) and a per-display `panel_type` key (`woled` / `qd-oled` / `unknown`, config-declared only — never auto-detected). `panel_type` is recorded on the ledger as a v2 bridge; v1 attribution does not yet branch on it.
- `DaemonEvent` gains additive `WearSnapshot` and `CompensationAdvisory` variants, plus a `#[serde(other)] Unknown` catch-all so older `dormantctl`/tray/WebUI builds keep streaming past event tags they don't recognize instead of erroring.
- Docs: new "OLED health" mdBook chapter covering what wear tracking does and does not track, the ledger file location, the advisory's meaning, and its all-local/no-telemetry privacy story.
- Wake/blank-failure desktop notifications: a display whose wake command fails `notifications.wake_attempt_threshold` consecutive times (default `3`), or whose blank command exhausts its whole controller chain (one-shot, no threshold), fires a critical-urgency `org.freedesktop.Notifications` notice over the session D-Bus; a Normal-urgency recovery notice follows once the display succeeds again, gated by `notifications.notify_recovery`. Repeat notices for the same display are cooldown-limited (`notifications.cooldown`, default `15m`). Config-only gate (`[notifications] enabled`, default `true`); disabled means no notifier task is spawned and zero D-Bus I/O.
- `DaemonEvent` gains additive `BlankFailure`, `BlankRecovered`, and `WakeRecovered` variants (wire tags `blank_failure`, `blank_recovered`, `wake_recovered`), covered by the same `#[serde(other)] Unknown` catch-all as the wear events.
- Tray: a `Failure` icon state (outranks `Paused`) with a red badge and tooltip detail, driven by the same per-display `wake_attempts`/`last_blank_failed` snapshot fields the notifier and web UI read — independent of whether desktop notifications are enabled.
- Web UI: a Dashboard failure banner listing every currently-failing display, an event-log entry for each new failure/blank/recovery event, and a "Notifications" settings section exposing all four `[notifications]` keys in the config editor.
- Config: new `[notifications]` section (`enabled`, `wake_attempt_threshold`, `cooldown`, `notify_recovery`), validated with a threshold floor of `1` and a cooldown floor of `1m`.
- Reload-window failure-state carry-over: `wake_attempts`/`last_blank_failed` survive a config reload for an unchanged display, but are deliberately zeroed (voided) for a display whose dispatch-relevant config changed (controllers, blank/degraded mode, ladder, output/DDC target, host/WoL MAC, blank/wake command or service+data, `modes`, command timeout, unreachable-as-blanked) or that was added/removed — stale failure evidence from a superseded dispatch path is not carried forward as if it still applied.
- Docs: new "Failure notifications" mdBook chapter covering what fires and when, how to silence desktop notifications without affecting the tray/dashboard, why wake failures use critical urgency, reload carry-over and voiding semantics, the daemon-restart limitation, and the session-bus-only privacy story.
- MQTT: retained messages are now verified and observable — a corrected module doc documents that retained publishes are dispatched exactly like live ones (on initial connect and every reconnect), a debug line (`mqtt: retained publish on '<topic>' dispatched`) logs each one, and a live-broker integration test proves a retained occupancy/availability value is delivered on subscribe without a fresh publish.
- MQTT: three new optional per-sensor keys — `availability_topic`, `availability_payload_online` (default `"online"`), `availability_payload_offline` (default `"offline"`) — let a sensor override the derived Zigbee2MQTT-style `<topic>/availability` convention and its literal payloads (e.g. for Tasmota's LWT topic). Availability routing is membership-based, so an override topic that doesn't end in `/availability` (like a Tasmota LWT topic) is no longer silently misrouted into occupancy parsing.
- MQTT: an unrecognized availability payload now warns once per `(topic, sensor)` pair instead of being silently dropped forever.
- Sensor snapshots gain `reported: bool` — has this sensor delivered at least one event since the daemon started? Distinguishes a genuinely-just-seeded-`unavailable` sensor from one that later went offline. Carried across a config reload for a sensor whose own binding is unchanged; reset to `false` when that sensor's config changed or it was newly added. The web dashboard's sensor list renders a "no data since start" hint for `unavailable` sensors with `reported == false`.
- Daemon: a new startup- and reload-time warning (`tracing::warn!(event = "unavailable_absent_mqtt", zone, sensor, …)`) fires once per installed generation for every zone that pairs `unavailable_policy = "absent"` with an MQTT sensor — this combination can strand a blanked screen if the MQTT bridge doesn't republish state on availability recovery (see the docs warning below).
- Docs: new "Retained values and availability" section in the sensors mdBook chapter — retained-message semantics and the stale-clock bound, the Zigbee2MQTT per-device `retain` setting operators need to flip on, the three new availability keys with a Tasmota example, the `reported` diagnostic, a worked single-sensor-zone timeline showing the ~60s blank-then-wake residual risk on a stale retained-vacant restart, and a strongly-worded warning that `unavailable_policy = "absent"` + MQTT is unsafe until you've personally verified your bridge republishes state on recovery. `examples/config.toml` gains the commented-out keys on the MQTT sensor example.

- Web UI: entity create/delete for sensors, zones, displays, and rules from the Settings form (`CreateEntity`/`DeleteEntity` patch ops on the existing `POST /api/config/apply` pipeline — same fingerprint check, backup, atomic rename, and daemon-identical validate-at-apply reference-integrity net as every other edit). Entity ids are restricted to `[a-z0-9_-]`, must start with a lowercase letter, and a fixed set of names (`type`, `blank_data`, `wake_data`, `source`, `ladder`, `weights`, and the 14 removable-leaf names) is reserved and can never be used as an id. Per-collection creatable-field lists are closed enumerations, not an open set — `displays` deliberately excludes `wake_command`/`blank_command` (daemon-executed shell commands) from what a web-created display can carry. Cross-reference fields (`zone`, `displays`, `members`, `inhibitors`) are now editable dropdowns instead of read-only. Gated by a new `daemon.entity_crud_enabled` config flag (default `true`), enforced server-side — a request sent with the flag off is rejected `403 feature_disabled` regardless of what the UI shows.
- Web UI: a Samsung TV pairing wizard (`POST /api/pair/samsung`, `GET /api/pair/samsung/<id>`) — enter a host, accept the "Allow" prompt on the TV, and the granted token is stored in `credentials.toml` (mode `0600`, atomic) the same way `dormantctl pair` does, with an optional hand-off into creating the paired display entity. The route is non-blocking (returns 202 immediately, status is polled) and single-flight (a concurrent second pairing attempt gets an immediate `409 pairing_in_progress` rather than queueing). The token is never present in any HTTP response body or log line. Gated by a new `daemon.pairing_enabled` flag (default `true`, server-enforced) and bounded by `daemon.pair_timeout` (default `120s`, `30s`..`300s`).
- Security: both new write routes (`/api/config/apply` and the new `/api/pair/samsung`) now share a single, generalized strict-Origin check (`STRICT_ORIGIN_PATHS`) instead of a single hardcoded route string — the Origin header must be present and match the bound loopback address and port exactly, with no allowance for an absent Origin. A build-time test derives every registered `POST` route from the router and asserts each one is explicitly classified into the strict set or an acknowledged-weaker set, so a future write route can no longer silently default to the weaker same-origin check by omission. `/api/pair/samsung` also carries its own 4 KiB body-size cap.
- Docs: the "Web UI" mdBook chapter gains an "Entity create/delete" section and a "Pairing wizard" section, plus an expanded Security posture covering the Origin/CSRF guard and an explicit statement of what the loopback-only threat model does and doesn't defend against.

### Changed

- **Upgrade note:** the new per-sensor availability validation (`validate_sensors`) can reject a config that previously loaded. Two new cross-sensor rules fire as `E_CONFIG_INVALID`: sensors on the same broker must not resolve an availability topic that collides with any sensor's state topic, and sensors sharing a resolved availability topic must declare identical `availability_payload_online`/`availability_payload_offline` literals. If a reload or restart after upgrading suddenly reports `E_CONFIG_INVALID` for a config that ran fine before, check for exactly these two collisions among your MQTT sensors.

### Fixed

- Wake-path brightness-zero restart poison: a daemon restart while the panel is dimmed could save the blank-residue reading (0) as the operator-chosen brightness level, causing every subsequent wake to "restore" 0 (permanently dim). Controllers now refuse to save a zero pre-blank reading, and config validation rejects `samsung_restore_backlight = 0` and `restore_brightness = 0` — the fail-toward-visible fallback is always at least 1.

## [0.1.0] - 2026-07-09

### Added

- Daemon core: config schema with strict validation, zone fusion engine (`any`/`all`/`quorum`/`weighted`), rules engine, per-display state machine, hot reload with phase carry-over, and a per-user single-instance `flock` guard.
- Fail-safe presence policy: sensor data loss resolves to `unavailable` (treated as present), never `absent`.
- Sensor sources: MQTT (with native per-broker authentication via the credentials file), Home Assistant WebSocket, and USB-serial LD2410 mmWave radar.
- Display controllers: `ddcci` (VCP `0xD6` power-off, brightness-zero), `samsung-tizen` (picture-off, power key, Wake-on-LAN, network pairing, plus a port-1516 IP Control G2 `backlightControl` path for the audio-safe `brightness_zero` dim), `kwin-dpms`, `ha-passthrough`, and a generic `command` controller. Per-display fallback chain with bounded wake-retry.
- Render ladder (`render` feature): Wayland layer-shell black overlay as the audio-safe final blank stage, plus a libmpv screensaver — folder/URL playlists, `fill`/`fit`/`stretch`/`center` scaling, crossfade transitions, muted by default — on a configurable blank → dwell → power-off escalation.
- Manual-only displays: a display referenced by no rule is built and hand-controllable, never auto-blanked.
- Control surfaces: `dormantctl` (status, pause/resume, blank/wake, reload, validate, watch, doctor, pair), a loopback-only web dashboard (live WebSocket state, doctor view, validated two-tab config editor), and a KDE `StatusNotifierItem` tray with per-display submenus.
- `dormantctl pair samsung <host>` and a real `doctor samsung` probe (reachability, power state, token presence).
- Web UI over an axum HTTP/WebSocket bridge, embedded into the daemon behind the `web-ui` feature; origin-guarded and loopback-bound.
- Delivery: 15-job CI matrix, cargo-dist release pipeline, and an mdBook manual.

### Fixed

- Samsung control WebSocket: generate the handshake headers via `IntoClientRequest` — a bare request was rejected by the TV (`missing sec-websocket-key`), so blank/wake never reached the panel.
- Samsung control WebSocket: liveness-check a cached socket before sending, so a key is never written into a socket the TV silently dropped while idle (which reported success while doing nothing).
- Samsung control WebSocket: durable reconnect — the connection tracks the TV's own heartbeat and reconnects before a send rather than firing keys into a silently-dropped idle socket.
- Samsung port-1516: send `Accept: application/json` on `backlightControl` calls (the TV 400s on `*/*`).
- Samsung port-1516: title-case HTTP headers (`Host`, `Content-Length`, `Content-Type`, `Connection`); the TV 400s on lowercase.
- Samsung port-1516: match the real protocol (no-params token fetch, `backlightControl` read, token persistence, readback-confirm) so dim and restore survive a daemon restart.
- Tray: reset the reconnect backoff on any healthy connection and reconnect promptly after a config-reload stream close, so the menu no longer blanks for up to 30 seconds.
- Config editor: strip absent optional fields instead of serializing `null`, which the daemon rejected (`422`) on a bare-dwell ladder edit.
- Reload-window control message: `forward_ctl` now retries across the generation swap, so a `blank`/`wake`/`pause`/`resume` issued in the narrow window during a config reload is not dropped (#9, #19).
- `dormantctl validate` against render-ladder configs: no longer false-rejects with `E_RENDER_UNAVAILABLE` when the `render` feature is off — validation is feature-agnostic, the runtime gate is what enforces it (#18).

### Changed

- CI runs on the `dev` integration branch; `master` is release-only.
- Runtime footprint: tokio worker pool capped to `worker_threads = 2` (down from the default `num_cpus`), `malloc_trim` runs after every screensaver teardown to release the libmpv crossfade buffers, and the systemd unit sets `MALLOC_ARENA_MAX=2` so freed render heap is returned to the OS instead of retained per-thread — the post-screensaver RSS floor drops from ~265 MB to ~90 MB and idle RSS stays flat across blank/wake/screensaver/reload cycles (#16).

[Unreleased]: https://github.com/legion-works/dormant/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/legion-works/dormant/releases/tag/v0.1.0
