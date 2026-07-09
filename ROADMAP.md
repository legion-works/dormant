# Roadmap

Direction for `dormant` — the OLED-preserving presence daemon. Grouped by state, not by date. Items move down as they ship; nothing here is a dated promise.

Status: pre-`0.1.0`. The core daemon, all three control surfaces, and the audio-safe blanking path are built, cross-reviewed, and validated on the maintainer's hardware (AOC AGON AG326UZD over DisplayPort, Samsung S90D over the network). No release is tagged yet — `master` holds the last validated state, `dev` is the integration branch.

## Shipped

- **Daemon core** — config schema + strict validation (unknown-key rejection, cross-reference checks), zone fusion engine (`any`/`all`/`quorum`/`weighted`), rules engine, per-display state machine, hot reload with phase carry-over, single-instance `flock` guard. Fail-safe presence throughout: data loss makes a sensor `unavailable`, never `absent` — a room you can't see is never blanked blind.
- **Sensors** — MQTT (with native per-broker auth), Home Assistant WebSocket, USB-serial LD2410 mmWave. One module per backend; ESPHome sensors drop in over the existing MQTT path with no new code.
- **Display controllers** — `ddcci` (VCP `0xD6` power-off and brightness-zero), `samsung-tizen` (`KEY_PICTURE_OFF`, power-key, Wake-on-LAN, network pairing, plus an IP-Control G2 backlight-dim path on port 1516), `kwin-dpms`, `ha-passthrough`, and a generic `command` escape hatch. Per-display fallback chain with bounded wake-retry. The Samsung control connection tracks the TV's own heartbeat and reconnects before a send rather than firing keys into a silently-dropped idle socket.
- **Audio-safe blanking** — three paths that blank the panel without tearing down the output, so audio survives: DDC/CI `0xD6` on capable monitors, Tizen picture-off on the TV (true-off, but it cuts the source's audio), and Samsung backlight-dim on port 1516 (`brightness_zero`) which drives the panel to near-black while the source and its audio keep running — the only fully audio-safe blank on a 2024 Samsung OLED. DPMS is the documented fallback only where there is no audio to lose.
- **Render ladder** — a local Wayland layer-shell overlay: audio-safe black surface as the final blank fallback, and a libmpv-driven screensaver (folder/URL playlists, `fill`/`fit`/`stretch`/`center` scaling, crossfade transitions, muted by default). Escalation ladder blanks → dwells → powers off on a configurable schedule.
- **Control surfaces** — `dormantctl` (status, pause/resume, blank/wake, reload, validate, watch, doctor, pair), a loopback-only web dashboard (live state over WebSocket, doctor view, two-tab config editor with an atomic validated apply pipeline), and a KDE `StatusNotifierItem` tray with per-display submenus.
- **Manual-only displays** — a display in `[displays]` referenced by no rule is hand-controllable and never auto-blanked. This is how a TV joins dormant without a keep-awake dummy zone.
- **Doctor** — hardware/connectivity probes for config, MQTT, HA, USB, DDC/CI, and Samsung (reachability, power state, token).
- **Delivery** — 15-job CI matrix (fmt, clippy pedantic, tests, MSRV, Linux/macOS/Windows portability, deny, audit, taplo, typos, docs, mdBook), cargo-dist release pipeline, mdBook manual.

## Near-term — toward `0.1.0`

- **Reload-concurrency hardening** ([#9](https://github.com/legion-works/dormant/issues/9)) — a manual command issued in the narrow window during a config reload can be dropped. Narrow and self-correcting today; the fix removes the lost-command window in the reload generation swap.
- **Footprint validation** — confirm a flat resource footprint (no RSS creep) across real blank/wake/screensaver/reload cycles before calling the daemon production-stable. Watching the libmpv screensaver path (crossfade capture buffers) and per-reload generation churn in particular.
- **First tagged release** — cut `0.1.0` once the above land: promote `dev` → `master`, tag, ship installers.

## Planned

- **More display controllers** — LG webOS (network TVs), Gnome DPMS (audio-safe where the output has no sound).
- **Packaging** — `.deb` / `.rpm` alongside the shell installers; distro-friendly systemd units.
- **Config ergonomics** — full entity CRUD in the web editor (add/remove sensors, zones, displays, rules), a device-pairing wizard, and `dormantctl validate` that understands render-feature configs.
- **Doctor-assisted issue drafting** — `dormantctl doctor` already gathers the exact hardware, environment, and probe output the issue templates require. Let it write a ready-to-file bug report or feature request to disk (pre-filled with display model + connection, OS/compositor/session, controller, and probe results), captured at the moment something fails so it can be filed later without hand-reconstructing the context. A `doctor --report-issue` / `--draft-feature` that emits the filled template.
- **Global hotkeys** — bind a key to blank, wake, or pause a display (or all of them) without reaching for the tray or a terminal. On Wayland this can't be a raw global grab; it goes through the compositor's shortcut path (the XDG desktop `GlobalShortcuts` portal, or KDE's KGlobalAccel). Users can already bind their own shortcuts to `dormantctl blank`/`wake`/`pause` today — this makes it first-class, with the daemon registering the shortcuts and a config block to declare them.
  - **Emergency recovery key (priority)** — one always-bound panic key that force-wakes every display and pauses the daemon, no matter what state it thinks it's in. This is the direct answer to dormant's worst failure mode: if a controller bug, a wedged reload, or a dead network path ever leaves a panel dark, the user has a guaranteed one-key way back to a lit screen. It should lean on the most independent recovery path available (a direct wake to every controller, bypassing the normal rules/state flow) and default to enabled — the one shortcut you never want to depend on the daemon being healthy to fire.

## Exploratory — not committed

- **Input-aware display control** — use the active input source to pick a local controller (DDC/render) when the PC owns the panel and a remote controller otherwise. Parked: the maintainer's S90D exposes no local input signal, so it needs a multi-input DDC monitor, an LG webOS TV, or an HDMI-CEC adapter to be worth building. The `OwnershipGate` seam is already in place for it.
- **Multi-instance coordination** — several dormant instances arbitrating one shared display over MQTT, so a laptop and a desktop don't fight the same TV. Rides the same `OwnershipGate` seam.
- **macOS and Windows** — the codebase cross-compiles today (portability CI is green); native display control on those platforms is unbuilt.

## Non-goals

- **No telemetry, no phone-home, ever.** The daemon talks only to the sensors and displays you configure.
- **No cloud dependency.** Everything runs locally; network controllers reach your own devices on your own LAN.
- **No weakening of fail-safe presence or the wake path.** A screen that won't wake is the worst failure mode; correctness there is never traded for a feature.
