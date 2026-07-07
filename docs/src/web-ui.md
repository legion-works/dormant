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

Two tabs: **Settings** (a form editor for live config changes without touching the TOML file) and **Raw TOML** (the original read-only syntax-highlighted viewer with inventory sidebar and a reload button).

#### Settings tab

The Settings form presents the running config as editable sections: Daemon, Sensors, Zones, Rules, and Displays. Each field shows the current value from `GET /api/config`; edits are accumulated in a client-side patch store and submitted together via `POST /api/config/apply`.

**What is editable (v1):**
- Leaf string, number, and duration values (e.g. `grace_period`, `wake_command`, `hold_time`).
- Whole arrays (e.g. a rule's `displays` list, a display's `controllers` or `modes` list, the `ladder` array-of-tables). Setting an array replaces it wholesale.
- A limited set of optional keys can be *removed* via the Remove op: `blank_mode`, `degraded_mode`, `dwell`, `order`, `image_duration`, `scale_mode`, `transition`, `transition_duration`, `hold_time`, `stale_timeout`, `ddc_display`, `output`, `wol_mac`, `host`.

**What is not editable:**
- **Locked leaves** — `type`, `blank_data`, `wake_data`: never writable through the patch API. The form renders these with a 🔒 icon and a tooltip explaining the restriction (redacted path ancestor, or a hard-locked config key).
- **Credentials-redacted fields** — URLs carrying inline userinfo (e.g. MQTT `broker_url` with `user:pass@`) are redacted by the server before the response is sent. The form locks these fields (🔒) and a redacted-path-ancestor lock cascades to their descendants (e.g. all fields under a screensaver `source` entry whose `urls` contain userinfo).
- **Entity add/remove** — adding or removing a sensor, zone, display, or rule table from the config is file-only. The patch API can only modify existing entities.

**Apply → reload flow:**

1. The form computes the patch delta from the user's dirty edits.
2. `POST /api/config/apply` sends the current `fingerprint` plus the patches.
3. The server re-reads the file, checks the fingerprint, validates and applies the patches, writes the new file, backs up the old file, and subscribes to the daemon's reload outcome.
4. The daemon's config-file watcher detects the write, waits through the `reload_debounce` window, then reloads the runtime.
5. The reload outcome is reported back in the apply response.

#### Outcome banners

The apply bar displays one of four outcomes after the request completes:

| Banner | Meaning |
|---|---|
| **✓ Reloaded** | The daemon accepted the new config and rebuilt the runtime successfully. The form clears its dirty state and re-fetches the new fingerprint. |
| **✕ Rejected** | The config was valid at write time but the daemon's reload failed (e.g. assembly error, removed-display verified-wake failure). A detail message names the cause. *The old config is still running; the patched file is on disk.* |
| **pending** | The apply handler waited for the reload outcome but it did not arrive within the timeout (10 s). Normal when `reload_debounce` is large — the daemon coalesces the event and will reload shortly. The form re-fetches immediately; the file was already written. |
| **superseded** | Another writer (a second browser tab, `dormantctl`, or a direct file edit) landed *after* your apply wrote the file. The reload outcome belongs to their write, not yours. |

#### Conflict dialog (409)

If the fingerprint in the apply request does not match the on-disk file (someone else edited the config between your last `GET` and your `POST`), the server returns `409 Conflict`. The Settings form shows a red conflict dialog:

> Config changed on disk — your edits are against an outdated version. Reload the form to get the latest config, or keep editing (your changes will be lost).

**Reload form** discards your edits, re-fetches the fresh config, and refreshes the form. **Keep editing** dismisses the dialog and leaves your dirty edits in place — you can then re-apply (the next attempt will use the current fingerprint, not the stale one).

#### Unsaved-changes guard

While the form has dirty edits, a `beforeunload` browser guard prevents accidental navigation away from the page. Switching from the Settings tab to the Raw TOML tab also triggers a confirmation dialog: *"Discard N unsaved changes?"*. The guard is removed once all edits are applied or discarded.

#### Backups

Every `POST /api/config/apply` that succeeds (the file is written and fsync'd) creates a backup of the previous config file before the atomic rename. Backups are stored in `<config-dir>/backups/` with names derived from the current UTC time plus a random 4-hex-digit suffix:

```
config.toml.2026-07-07T14:22:03Z.a3f1
```

The directory is created with mode `0o700` (owner-only). A rotation policy keeps at most **5** newest backups (sorted by filename, which encodes an RFC 3339 timestamp); older files are deleted after each new backup.

The config-file watcher uses `RecursiveMode::NonRecursive` on the config directory — writes inside `backups/` do *not* trigger a reload.

### Doctor

Runs the same diagnostic checks as `dormantctl doctor` on demand. The SPA calls `POST /api/doctor`, which invokes the shared `DoctorService` directly (the same service instance the daemon's IPC server uses — no subprocess is spawned). Results include a summary bar (passing / skipped / failing counts) and per-check rows with status chips and detail messages.
