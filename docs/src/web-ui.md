# Web UI

dormant serves an optional web dashboard — a single-page application (SPA) that shows zone status, display state, recent events, runtime configuration, and a built-in doctor report. It is embedded into the daemon binary via `rust-embed`; no separate static-file server is needed.

## Enabling

The web UI is gated behind the Cargo feature `web-ui`:

```sh
cargo build --release --features web-ui
```

When the feature is enabled, these config keys become active under `[web]`:

| Key | Default | Description |
|---|---|---|
| `web_port` | `9100` | Listen port |
| `web_bind` | `"127.0.0.1"` | Bind address — `127.0.0.1` or `0.0.0.0` |
| `web_allow_nonloopback` | `false` | Require explicit opt-in before binding to a non-loopback address |

Example:

```toml
[web]
web_port = 8080
web_bind = "127.0.0.1"
```

If the `web-ui` feature is not enabled, the `[web]` section is ignored and no HTTP server starts.

## Security posture

The web UI is **intended for single-user, loopback-only use**. There is no authentication, no user sessions, and no TLS. The design assumes the operator runs `dormantd` on their local machine and opens the dashboard from the same host.

### Loopback guard

By default the server binds to `127.0.0.1` only. Setting `web_bind = "0.0.0.0"` (or any non-loopback address) is rejected at startup unless `web_allow_nonloopback = true`. The daemon logs a prominent warning when this override is active, because it makes the dashboard reachable from the local network without authentication — anyone on the LAN could issue blank/wake commands or change zone config.

### CSRF / host guard

The server validates the `Host` header on every request. Requests with a `Host` that does not match the bound address are rejected with `421 Misdirected Request`. This blocks DNS-rebinding attacks even when bound to loopback only.

### No upstream exposure

Do not reverse-proxy the dashboard onto the public internet. If remote visibility is needed, use an SSH tunnel (`ssh -L 9100:localhost:9100 user@host`) or a VPN. The dashboard was not designed for hostile-network exposure.

## Views

The SPA has five views, selected from a left-hand navigation sidebar.

### Dashboard

The landing page. Shows every zone: its occupancy state (occupied / vacant), the fused presence score, the displays linked to it, and the active blanking rules. Occupied zones are highlighted; vacant zones that are within a grace period show a countdown.

### Displays

A table of every configured display: name, type, current power state (on / standby / off), the controller chain in priority order, and the last command result (success / error with the failing controller). Each row includes a manual blank/wake toggle for testing.

### Events

A scrolling, auto-pruning event log. Shows presence changes, display state transitions, rule evaluations, errors, and warnings. Each event is timestamped with the daemon's monotonic clock. The log is client-side only — it reflects what the daemon has emitted since the dashboard opened; reloading the page starts a fresh stream.

### Config

A read-only dump of the running configuration: every section (`[sensors]`, `[zones]`, `[displays]`, `[rules]`, `[web]`) rendered as syntax-highlighted TOML. Useful for spot-checking which config file the daemon actually parsed.

### Doctor

Runs `dormantctl doctor` and renders the report inline. Shows sensor reachability, display DDC/CI capability detection, controller capability flags, and any configuration warnings. The doctor report is fetched via the daemon's HTTP API (the daemon spawns the CLI check internally).
