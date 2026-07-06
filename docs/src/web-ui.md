# Web UI

dormant serves an optional web dashboard — a single-page application (SPA) that shows zone status, display state, recent events, runtime configuration, and a built-in doctor report. It is embedded into the daemon binary via `rust-embed`; no separate static-file server is needed.

## Enabling

The web UI is gated behind the Cargo feature `web-ui`:

```sh
cargo build --release --features web-ui
```

When the feature is enabled, these daemon config keys control the web server:

| Key | Default | Description |
|---|---|---|
| `daemon.web_port` | (unset) | TCP port. Set to a port number to enable the web UI; unset (the default) leaves it disabled. |
| `daemon.web_bind` | `"127.0.0.1"` | Bind address — `127.0.0.1` or `0.0.0.0` |
| `daemon.web_allow_nonloopback` | `false` | Require explicit opt-in before binding to a non-loopback address |

Example:

```toml
[daemon]
web_port = 8080
web_bind = "127.0.0.1"
```

If `daemon.web_port` is not set (the default), no HTTP server starts — even when the binary was compiled with the `web-ui` feature. The keys live under `[daemon]`, not a separate `[web]` section.

## Security posture

The web UI is **intended for single-user, loopback-only use**. There is no authentication, no user sessions, and no TLS. The design assumes the operator runs `dormantd` on their local machine and opens the dashboard from the same host.

### Loopback guard

By default the server binds to `127.0.0.1` only. Setting `daemon.web_bind = "0.0.0.0"` (or any non-loopback address) is rejected at startup unless `daemon.web_allow_nonloopback = true`. The daemon logs a prominent warning when this override is active, because it makes the dashboard reachable from the local network without authentication — anyone on the LAN could issue blank/wake commands or change zone config.

### Host guard

The server validates the `Host` header on every request. Requests with a `Host` that does not match the configured bind address are rejected with `403 Forbidden` (the daemon logs a `web_reject_host` event). This blocks DNS-rebinding attacks even when bound to loopback only.

### No upstream exposure

Do not reverse-proxy the dashboard onto the public internet. If remote visibility is needed, use an SSH tunnel (`ssh -L 9100:localhost:9100 user@host`) or a VPN. The dashboard was not designed for hostile-network exposure.

## Views

The SPA has five views, selected from a left-hand navigation sidebar.

### Dashboard

The landing page. Shows a stat row (displays count with active/blanked split, sensor online/unavailable split, zone occupied/vacant split, OLED guard status), a three-column signal-flow grid (Sensors → Zones → Displays), and a recent-activity feed.

Each sensor row shows its id, type label (MQTT / HA WebSocket / LD2410 radar), state (present/absent/unavailable), and last-seen age. Zone rows show occupancy (present/absent/unavailable), the fusion mode (ANY/ALL/QUORUM/WEIGHTED), and member sensor names. Display rows show phase chips, blank mode label, controller chain, and blank/wake action buttons.

### Displays

A per-display card list. Each card shows a screen preview glyph (ON / grace / … / OFF / wake), phase and paused/inhibited status chips, the blank mode label, the driving zone and rule, the command generation counter, and the controller chain rendered as HealthChips (each controller's name, role — primary/fallback — and health status). Action buttons let the operator force-blank, force-wake, and pause or resume the governing rule.

### Events

A scrolling, auto-pruning event log. Shows presence changes, display phase transitions, wake retry attempts, and config reloads — each with a type-colored badge and a human-readable message. Events are timestamped with the browser's local clock at the moment the WebSocket message arrives (client-side arrival time, not the daemon's clock). The log is client-side only — it reflects what the daemon has emitted since the dashboard opened; reloading the page starts a fresh stream.

### Config

A two-column layout. The left column renders the running config file as syntax-highlighted TOML (keys, string values, numbers, comments, and section headers each colored distinctly). The right column shows a parsed inventory (sensor, zone, display, and rule counts with their names), validation issues (load errors, schema errors, and warnings) with detail messages, and a reload button for hot-reloading config from disk.

### Doctor

Runs the same diagnostic checks as `dormantctl doctor` on demand. The SPA calls `POST /api/doctor`, which invokes the shared `DoctorService` directly (the same service instance the daemon's IPC server uses — no subprocess is spawned). Results include a summary bar (passing / skipped / failing counts) and per-check rows with status chips and detail messages.
