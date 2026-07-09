# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Commits use [Conventional Commits](https://www.conventionalcommits.org/), which drive the release-generated notes; this file is the curated, human-readable summary.

## [Unreleased]

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
