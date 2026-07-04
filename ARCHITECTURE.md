# Architecture

Crate map, data flow, and where-to-find-it guide for the dormant codebase.

## Crate map

| Crate | Purpose | Has binaries? |
|---|---|---|
| `dormant-core` | Domain types, traits, config schema/validation, zone fusion engine, rules engine, state machine, IPC protocol — pure logic, no I/O | No |
| `dormant-sensors` | Sensor sources: MQTT (`mqtt.rs`), Home Assistant WebSocket (`ha_ws.rs`), USB-serial LD2410 radar (`usb_ld2410.rs`), plus a shared backoff helper and a static registry | No |
| `dormant-displays` | Display controllers: arbitrary shell command (`command.rs`), DDC/CI (`ddcci.rs`), VCP operations (`vcp_ops.rs`), Home Assistant passthrough (`ha_passthrough.rs`), execution engine with fallback/retry (`executor.rs`), static registry | No |
| `dormantd` | Daemon binary: config loading, event loop, IPC server, inhibit-activity watcher, reload handling, logging | **Yes** — `dormantd` |
| `dormantctl` | CLI binary: `status`, `blank`, `wake`, `pause`, `validate`, `watch`, `doctor` subcommands | **Yes** — `dormantctl` |

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
                  │  (retry,     │      ├── kwin-dpms (planned)
                  │   escalation)│      ├── ddcci
                  └──────────────┘      ├── command
                                        ├── ha-passthrough
                                        └── samsung-tizen (planned)
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
| Add a doctor check | `dormantctl/src/cmd_doctor.rs` |
| Add a CLI subcommand | `dormantctl/src/cmd_<name>.rs` + register in `dormantctl/src/main.rs` |

## Event and error-code grep anchors

Every log event name and error code is a literal string at the definition site — never `format!`-constructed, never macro-generated. This makes them reliably greppable:

- **Error codes:** `E_CONFIG_INVALID`, `E_CONFIG_UNKNOWN_KEY`, `E_ZONE_CYCLE`, `E_ZONE_UNKNOWN_MEMBER`, `E_CREDS_PERMS`, `E_CREDS_MISSING`, `E_MODE_UNSUPPORTED`, `E_BLANK_FAILED`, `E_WAKE_FAILED`, `E_RELOAD_WAKE_FAILED`, `E_HA_AUTH`, `E_SENSOR_IO`, `E_DISPLAY_IO`, `E_IPC` — all defined in `dormant-core/src/error.rs`.
- **Log events:** grep for `event = "..."` in the source. Key events include `sensor_event`, `zone_transition`, `rule_blank`, `rule_wake`, `wake_failed`, `reload_complete`, `reload_defensive_wake`.

Config keys follow the TOML path: `daemon.log_level`, `sensors.<id>.type`, `zones.<id>.mode`, `displays.<id>.controllers`, `rules.<id>.zone`, etc. — all resolved in `dormant-core/src/config/mod.rs`.
