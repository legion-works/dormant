# Changelog

All notable changes to `dormant` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims at [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Commits use [Conventional Commits](https://www.conventionalcommits.org/), which drive the release-generated notes; this file is the curated, human-readable summary.

## [Unreleased]

Pre-`0.1.0`. Everything below is built and validated but not yet tagged for release.

### Added

- Daemon core: config schema with strict validation, zone fusion engine (`any`/`all`/`quorum`/`weighted`), rules engine, per-display state machine, hot reload with phase carry-over, and a per-user single-instance `flock` guard.
- Fail-safe presence policy: sensor data loss resolves to `unavailable` (treated as present), never `absent`.
- Sensor sources: MQTT (with native per-broker authentication via the credentials file), Home Assistant WebSocket, and USB-serial LD2410 mmWave radar.
- Display controllers: `ddcci` (VCP `0xD6` power-off, brightness-zero), `samsung-tizen` (picture-off, power key, Wake-on-LAN, network pairing), `kwin-dpms`, `ha-passthrough`, and a generic `command` controller. Per-display fallback chain with bounded wake-retry.
- Render ladder (`render` feature): Wayland layer-shell black overlay as the audio-safe final blank stage, plus a libmpv screensaver — folder/URL playlists, `fill`/`fit`/`stretch`/`center` scaling, crossfade transitions, muted by default — on a configurable blank → dwell → power-off escalation.
- Manual-only displays: a display referenced by no rule is built and hand-controllable, never auto-blanked.
- Control surfaces: `dormantctl` (status, pause/resume, blank/wake, reload, validate, watch, doctor, pair), a loopback-only web dashboard (live WebSocket state, doctor view, validated two-tab config editor), and a KDE `StatusNotifierItem` tray with per-display submenus.
- `dormantctl pair samsung <host>` and a real `doctor samsung` probe (reachability, power state, token presence).
- Web UI over an axum HTTP/WebSocket bridge, embedded into the daemon behind the `web-ui` feature; origin-guarded and loopback-bound.
- Delivery: 15-job CI matrix, cargo-dist release pipeline, and an mdBook manual.

### Fixed

- Samsung control WebSocket: generate the handshake headers via `IntoClientRequest` — a bare request was rejected by the TV (`missing sec-websocket-key`), so blank/wake never reached the panel.
- Samsung control WebSocket: liveness-check a cached socket before sending, so a key is never written into a socket the TV silently dropped while idle (which reported success while doing nothing).
- Tray: reset the reconnect backoff on any healthy connection and reconnect promptly after a config-reload stream close, so the menu no longer blanks for up to 30 seconds.
- Config editor: strip absent optional fields instead of serializing `null`, which the daemon rejected (`422`) on a bare-dwell ladder edit.

### Changed

- CI runs on the `dev` integration branch; `master` is release-only.

[Unreleased]: https://github.com/legion-works/dormant/commits/dev
