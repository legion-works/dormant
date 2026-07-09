# Architecture

Crate map, data flow, and where-to-find-it guide for the dormant codebase.

## Crate map

| Crate | Purpose | Has binaries? |
|---|---|---|
| `dormant-core` | Domain types, traits, config schema/validation, zone fusion engine, rules engine, state machine, IPC protocol, reload types, doctor wire types (`DoctorReport`/`Check`/`CheckStatus`) — pure logic, no I/O | No |
| `dormant-sensors` | Sensor sources: MQTT (`mqtt.rs`), Home Assistant WebSocket (`ha_ws.rs`), USB-serial LD2410 radar (`usb_ld2410.rs`), plus a shared backoff helper and a static registry | No |
| `dormant-displays` | Display controllers: arbitrary shell command (`command.rs`), DDC/CI (`ddcci.rs`), VCP operations (`vcp_ops.rs`), Home Assistant passthrough (`ha_passthrough.rs`), Samsung Tizen (`samsung_tizen.rs` — port 8002 WebSocket + Wake-on-LAN + pairing) plus its audio-safe IP Control G2 JSON-RPC transport (`samsung_ip.rs` — port 1516 `backlightControl` for `brightness_zero`), execution engine with fallback/retry (`executor.rs`), static registry | No |
| `dormant-doctor` | Hardware/connectivity health checks: probes for config, MQTT, HA WebSocket, USB LD2410, DDC/CI, Samsung Tizen (reachability + power state + token presence); live coalesced `DoctorService` for the daemon + web UI. Wire types live in `dormant_core::doctor` to avoid a cycle | No |
| `dormant-web` | Loopback-only web dashboard: axum HTTP/WS bridge that reads live engine state and serves a SPA (`crates/dormant-web/webui/`). Optional dependency of `dormantd`, gated behind the `web-ui` Cargo feature — when off, zero web code is compiled | No |
| `dormant-render` | Local Wayland layer-shell `RenderSink`: software-blank black overlay (final ladder fallback when every display controller failed) and libmpv-driven screensaver overlay (last-resort idle surface). Wayland I/O is `target_os = "linux"`-gated; non-Linux builds expose a no-op stub with the same `LayerShellRenderSink` surface so callers compile unconditionally | No |
| `dormantd` | Daemon binary: config loading, event loop, IPC server, inhibit-activity watcher, single-instance flock, reload handling, optional web UI spawn, logging | **Yes** — `dormantd` |
| `dormantctl` | CLI binary: `status`, `pause`, `resume`, `blank`, `wake`, `reload`, `validate`, `watch`, `doctor` (per-target subcommands including `samsung`), `pair samsung <host>` subcommands. Also re-exports its IPC `client` module as a library entry (`crates/dormantctl/src/lib.rs`) so `dormant-tray` can drive the same protocol without forking the socket glue | **Yes** — `dormantctl` |
| `dormant-tray` | KDE `StatusNotifierItem` tray applet (`ksni`): live icon (Normal / Attention / Paused / Unreachable), pause/resume/blank/wake menu items, tooltip, reconnecting IPC event-stream reader. Linux-only — non-Linux bin prints a notice and exits 1 so cross-platform `cargo check` stays green | **Yes** — `dormant-tray` |

Each crate follows the convention: one module per concept, one file per sensor/controller, explicit static registry with no proc-macro magic.

## Data flow

```
                  ┌──────────────┐
  MQTT ──────────▶│              │
  HA WebSocket ──▶│  Sensors     │──▶ PresenceEvent ──▶
  USB LD2410 ────▶│  (registry)  │                     │
                  └──────────────┘                     │
                                                       ▼
                  ┌──────────────┐           ┌──────────────────┐
  Config ────────▶│  Zone Engine │──▶ Zone   │  Rules Engine /  │
  (schema.rs)     │  (fusion)    │    State  │  State Machine    │
                  └──────────────┘           └──────┬───────────┘
                                                    │
                                          Blank / Wake commands
                                                    │
                                                    ▼
                  ┌──────────────┐
                  │  Executor    │──▶ Controller chain (fallback)
                  │  (retry,     │      ├── kwin-dpms (fallback, audio-unsafe)
                  │   escalation)│      ├── ddcci
                  └──────────────┘      ├── command
                                        ├── ha-passthrough
                                        └── samsung-tizen
```

1. **Sensors** produce `PresenceEvent` values (occupied / vacant) and push them to the zone engine.
2. The **zone engine** fuses events from multiple sensors per zone using the configured mode (`any`, `all`, `quorum`, `weighted`). Unavailable sensors are treated as *present* (fail-safe — never blank a room you can't see).
3. The **rules engine** maps zone state to display commands, applying grace periods, min-blank/min-wake floors, and inhibitor checks (user activity, manual pause).
4. The **display executor** walks an ordered controller chain per display: tries the first controller, falls back on failure, retries wakes with bounded backoff, and escalates to the next controller if all retries are exhausted.

## Where do I look for X?

| Task | Where |
|---|---|
| Add a sensor | `dormant-sensors/src/<name>.rs` (new module) + `dormant-sensors/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config variant) |
| Add a display controller | `dormant-displays/src/<name>.rs` (new module) + `dormant-displays/src/registry.rs` (register it) + `dormant-core/src/config/schema.rs` (config fields if needed) |
| Add a config key | `dormant-core/src/config/schema.rs` (struct field + serde) + `dormant-core/src/config/defaults.rs` (default value) + `dormant-core/src/config/validate.rs` (validation rule) + `dormant-core/src/config/mod.rs` (known-key tree for unknown-key detection) |
| Add an error code | `dormant-core/src/error.rs` (`pub const E_*` + variant in `DormantError`) |
| Change timing defaults | `dormant-core/src/config/defaults.rs` (single source of truth) |
| Add a doctor probe | `dormant-doctor/src/probes/<name>.rs` (new probe) + `dormant-doctor/src/probes/mod.rs` + `dormant-doctor/src/lib.rs` (re-export) + `dormantctl/src/cmd_doctor.rs` (CLI dispatch + subcommand) |
| Wire doctor into the daemon | `dormantd/src/app.rs` (construct one `DoctorService` shared by IPC + web UI) + `dormant-doctor/src/service.rs` (coalesced singleflight logic) |
| Add a web route | `dormant-web/src/routes/<name>.rs` (new module) + `dormant-web/src/routes/mod.rs` (mount) + `dormant-web/src/server.rs` (router) |
| Add a CLI subcommand | `dormantctl/src/cmd_<name>.rs` (new file) + register variant + dispatch in `dormantctl/src/main.rs`. Short commands that share a handler (e.g. `pause`+`resume`, `blank`+`wake`) co-locate in one `cmd_*.rs` |
| Add a render ladder stage | `dormant-render/src/<module>.rs` (shared pure logic — `command`, `latch`, `settings`, `playlist`) + `dormant-render/src/linux/<impl>.rs` for the Wayland implementation (Linux-only). The cross-platform `LayerShellRenderSink` re-export is in `dormant-render/src/lib.rs` |
| Add a tray menu/state piece | `dormant-tray/src/<module>.rs` (pure logic — `state`, `tooltip`, `menu`, `icon`; unit-tested without D-Bus); Linux-only glue lives in `dormant-tray/src/tray.rs` and `dormant-tray/src/ipc_loop.rs` |
| Drive the daemon from another binary | Reuse `dormantctl::client` (re-exported via `crates/dormantctl/src/lib.rs`) — `dormant-tray` is the canonical example |

## Event and error-code grep anchors

Every log event name and error code is a literal string at the definition site — never `format!`-constructed, never macro-generated. This makes them reliably greppable:

- **Error codes:** `E_CONFIG_INVALID`, `E_CONFIG_UNKNOWN_KEY`, `E_ZONE_CYCLE`, `E_ZONE_UNKNOWN_MEMBER`, `E_CREDS_PERMS`, `E_CREDS_MISSING`, `E_MODE_UNSUPPORTED`, `E_BLANK_FAILED`, `E_WAKE_FAILED`, `E_RELOAD_WAKE_FAILED`, `E_HA_AUTH`, `E_SENSOR_IO`, `E_DISPLAY_IO`, `E_IPC` — all defined in `dormant-core/src/error.rs`.
- **Log events:** grep for `event = "..."` in the source. Key events include `sensor_event`, `zone_transition`, `rule_blank`, `rule_wake`, `wake_failed`, `reload_complete`, `reload_defensive_wake`.

Config keys follow the TOML path: `daemon.log_level`, `sensors.<id>.type`, `zones.<id>.mode`, `displays.<id>.controllers`, `rules.<id>.zone`, etc. — all resolved in `dormant-core/src/config/mod.rs`.

## Audio-safe blanking

DPMS-based blanking (including `kwin-dpms`) disables the DRM/KMS output,
which tears down the associated ALSA audio sink — audio dies along with the
picture. This is architectural, not a config setting.

Three display-controller modes blank without tearing down the output,
preserving audio:

- **`ddcci`** — VCP `0xD6` sends a "display power off" command over I2C.
  The monitor blanks its panel internally; the OS output and ALSA device
  remain active. Only works on DDC/CI-capable monitors that support D6.
- **`samsung-tizen`** — `KEY_PICTURE_OFF` blanks the TV panel over WebSocket.
  The TV continues rendering audio; the HDMI output remains active.
- **`samsung-tizen`** (`brightness_zero`) — Samsung IP Control G2 JSON-RPC on
  port 1516 calls `backlightControl` to dim the panel to 0 via `samsung_ip`.
  Source and audio keep running; the HDMI output stays active. Used when
  `KEY_PICTURE_OFF` would cut the source or pause media.

Per-display strategy:
- DDC/CI monitor → `ddcci` power_off (audio-safe, verified on AOC AG326UZD)
- Samsung Tizen TV → `samsung-tizen` picture-off (audio-safe, verified on S90D)
- Samsung Tizen TV where the source must keep running → `samsung-tizen`
  `brightness_zero` via port 1516 IP Control G2 (audio-safe, softer panel
  change — does not pause media)
- Outputs with no DDC/CI and no audio → `kwin-dpms` is acceptable (no audio to kill)
- Outputs with audio but no DDC/CI → Tizen passthrough or `command` with an
  audio-safe external command; otherwise live with the audio loss

See `docs/research/2026-07-05-kwin-dpms-verification.md` and
`docs/research/2026-07-05-s90d-verification.md` for the hardware spike data.
