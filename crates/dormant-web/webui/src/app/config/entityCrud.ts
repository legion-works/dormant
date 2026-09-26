/**
 * Client-side mirrors of the Rust config-CRUD security/gating consts and
 * pure functions (spec `.opencode/specs/2026-07-10-dormant-config-crud-wizard.md`
 * §4/§5/§6). These are UX-only — instant feedback so a user doesn't
 * fill out a whole create form before hitting a 422. The SERVER
 * (`crates/dormant-web/src/config_patch.rs`) is the real security
 * boundary and re-checks every one of these independently; a mirror
 * drifting from the Rust source degrades the UX (a wrong hint) but can
 * never widen what the server accepts.
 */
import type { ConfigPatch } from "../../api/types";

/** rust: crates/dormant-web/src/config_patch.rs CRUD_COLLECTIONS (:474) */
export const CRUD_COLLECTIONS = ["sensors", "zones", "displays", "rules"] as const;
export type CrudCollection = (typeof CRUD_COLLECTIONS)[number];

/**
 * rust: crates/dormant-web/src/config_patch.rs CREATABLE_FIELDS (:488-545)
 *
 * Per-collection closed top-level field allowlist for `CreateEntity`
 * payloads. Copied EXACTLY (order preserved) — sensors 13 fields, zones
 * 4, displays 10, rules 11. `displays` deliberately OMITS
 * `blank_command`/`wake_command` (cold-gate M3 Must-1 — daemon-executed
 * `sh -c` commands are not web-creatable in v1). A change to the Rust
 * array must be mirrored here by hand; `entityCrud.test.ts` pins the
 * exact arrays so drift fails the build, not silently.
 */
export const CREATABLE_FIELDS: Record<CrudCollection, readonly string[]> = {
  sensors: [
    "type", "kind", "hold_time", "stale_timeout",
    // mqtt
    "broker_url", "topic", "field", "payload_on", "payload_off",
    // ha
    "url", "entity",
    // usb-ld2410
    "port", "baud",
  ],
  zones: ["mode", "members", "unavailable_policy", "weights"],
  displays: [
    "controllers", "host", "blank_mode", "output", "ddc_display", "wol_mac",
    "samsung_restore_backlight", "restore_brightness",
    "treat_unreachable_as_blanked", "command_timeout",
    "shared_input_code", "shared_input_write_code",
    "shared_peer_input_code", "shared_peer_input_write_code",
  ],
  rules: [
    "zone", "displays", "grace_period", "inhibitors",
    "min_blank_time", "min_wake_time", "activity_idle_threshold",
    "activity_poll_interval", "wake_retries", "wake_retry_backoff",
    "wake_retry_interval",
  ],
};

/**
 * rust: crates/dormant-web/src/config_patch.rs RESERVED_ENTITY_IDS (:576-600)
 *
 * Every entity id string ANY gate special-cases by literal name, across
 * BOTH `config_patch.rs` and `dormant-core::config::validate`'s
 * `is_known_config_path` internals (`STRUCTURAL_RESERVED_NAMES`,
 * `LOCKED_LEAVES`, `REMOVABLE_KEYS`). Copied EXACTLY, same order as the
 * Rust source, purely for instant client feedback — `validate_entity_id`
 * server-side is the real boundary.
 */
export const RESERVED_ENTITY_IDS: readonly string[] = [
  // STRUCTURAL_RESERVED_NAMES (dormant-core, re-exported)
  "weights",
  "source",
  "ladder",
  "blank_data",
  "wake_data",
  // LOCKED_LEAVES (only "type" is new here — blank_data/wake_data above)
  "type",
  // REMOVABLE_KEYS
  "blank_mode",
  "degraded_mode",
  "dwell",
  "order",
  "image_duration",
  "scale_mode",
  "transition",
  "transition_duration",
  "hold_time",
  "stale_timeout",
  "ddc_display",
  "output",
  "wol_mac",
  "host",
  "playback_roles",
];

/**
 * rust: crates/dormant-core/src/config/validate.rs VALID_INHIBITORS (:392)
 *
 * Composed from the single-definition-site consts in `rules.rs` plus
 * the local `"manual-pause"` literal (accepted-but-unwired). Kept in
 * the same order as the Rust recomposition:
 * INHIBITOR_USER_ACTIVITY / INHIBITOR_AUDIO_PLAYBACK / INHIBITOR_CALL /
 * "manual-pause".
 */
export const VALID_INHIBITORS = ["user-activity", "audio-playback", "call", "manual-pause"] as const;

/** rust: crates/dormant-displays/src/samsung_tizen.rs SamsungTizenController::NAME (:770) */
export const SAMSUNG_TIZEN_CONTROLLER = "samsung-tizen";

/**
 * rust: crates/dormant-displays/src/registry.rs CONTROLLER_TYPES (:44-52)
 *
 * The platform superset (`ddcci` is available on Linux and macOS); listed here
 * regardless since the web UI has no way to know the daemon's platform
 * and the server's `capabilities()`/collection check is the real gate —
 * an unsupported controller on a non-Linux daemon fails the
 * daemon-identical `validate()` at apply time, same as any other bad
 * create.
 */
export const DISPLAY_CONTROLLER_OPTIONS = [
  "command",
  "ddcci",
  "ha-passthrough",
  "kwin-dpms",
  "macos-display-sleep",
  "macos-gamma-black",
  "samsung-tizen",
] as const;

export interface EntityIdValidation {
  ok: boolean;
  reason?: string;
}

/**
 * rust: crates/dormant-web/src/config_patch.rs validate_entity_id (:607-636)
 *
 * Charset `[a-z0-9_-]`, first char `[a-z]`, length 1-64, plus the
 * `RESERVED_ENTITY_IDS` ban. Mirrors the server function's rejection
 * order and reason-text shape (not the exact wording — the server's
 * message is authoritative for the error banner) for a stable
 * "reserved" substring assertion in tests.
 */
export function validateEntityId(id: string): EntityIdValidation {
  if (id.length === 0) {
    return { ok: false, reason: "entity id must not be empty" };
  }
  if ([...id].length > 64) {
    return { ok: false, reason: `entity id '${id}' exceeds the maximum length of 64` };
  }
  const first = id[0];
  if (!/^[a-z]$/.test(first)) {
    return { ok: false, reason: `entity id '${id}' must start with a lowercase ASCII letter` };
  }
  if (!/^[a-z0-9_-]+$/.test(id)) {
    return { ok: false, reason: `entity id '${id}' contains characters outside [a-z0-9_-]` };
  }
  if (RESERVED_ENTITY_IDS.includes(id)) {
    return { ok: false, reason: `entity id '${id}' is a reserved config key name` };
  }
  return { ok: true };
}

/**
 * rust: crates/dormant-core/src/config/schema.rs DaemonConfig
 * `entity_crud_enabled` (spec §10, default `true`).
 *
 * `ConfigInventory.daemon` is loosely typed (`Record<string, unknown>`)
 * since the settings form renders every `[daemon]` key generically —
 * this reader centralizes "absent/non-boolean means the Rust default
 * (true)" so every call site treats an old fixture or a pre-feature
 * config identically.
 */
export function isEntityCrudEnabled(daemon: Record<string, unknown> | undefined): boolean {
  const v = daemon?.["entity_crud_enabled"];
  return typeof v === "boolean" ? v : true;
}

/**
 * rust: crates/dormant-core/src/config/schema.rs DaemonConfig
 * `pairing_enabled` (spec §10, default `true`).
 */
export function isPairingEnabled(daemon: Record<string, unknown> | undefined): boolean {
  const v = daemon?.["pairing_enabled"];
  return typeof v === "boolean" ? v : true;
}

/**
 * rust: crates/dormant-core/src/config/schema.rs DaemonConfig
 * `hook_edit_enabled` (BG-6, default `false`).
 *
 * Defaults to false when the key is absent — no argv-writing form
 * on an unauthenticated loopback surface without explicit operator
 * consent.  The server rejects hook-path patches when this flag is
 * off, matching the UI's hidden affordance.
 */
export function isHookEditEnabled(daemon: Record<string, unknown> | undefined): boolean {
  const v = daemon?.["hook_edit_enabled"];
  return typeof v === "boolean" ? v : false;
}

/**
 * Client-side UX gate for fail-unsafe config writes. The Rust server is
 * NOT consulted (every listed value is a normal, accepted value the
 * server validates normally) — this is purely a "you may not realise
 * what you're doing" warning that lives at the Apply button, so an
 * operator gets one last chance to back out before writing a value that
 * is at odds with the daemon's deliberate defaults.
 *
 * Each entry is a path-pattern predicate applied to a `ConfigPatch`
 * (see [`detectUnsafePatches`]). When the predicate matches, the
 * matching patch is paired with a plain-language `consequence` string
 * that the apply dialog renders verbatim.
 *
 * Open-ended extension point: the list is data-driven so adding a new
 * fail-unsafe value is a one-line change with tests. Each entry should
 * be grounded in a fail-safe design commitment in
 * `crates/dormant-core/src/...` (e.g. zone policy, inhibitor policy) so
 * the consequence text describes the project's deliberate default, not
 * an opinion.
 */
export interface FailUnsafeSetting {
  /** Path-pattern predicate. Set patches: walk-and-match against `path`. CreateEntity patches: walk into `value` via the same pattern. Return true to flag. */
  matches: (path: string[]) => boolean;
  /** Match a concrete value; the test sets the predicate and value predicate together. */
  matchesValue: (value: unknown) => boolean;
  /** Plain-language consequence rendered in the apply confirmation. */
  consequence: string;
}

const FAIL_UNSAFE_SETTINGS: readonly FailUnsafeSetting[] = [
  // zones.<id>.unavailable_policy = "absent" — fail-unsafe presence policy.
  // Source of the fail-safe default: `crates/dormant-core/src/zone.rs:37-45`
  // (`UnavailablePolicy::Present` is the `Default` impl); the rule is
  // documented at docs/src/configuration.md:139-145 ("present (default,
  // fail-safe) — treat unavailable sensors as occupied").
  {
    matches: (path) => path.length === 3 && path[0] === "zones" && path[2] === "unavailable_policy",
    matchesValue: (value) => value === "absent",
    consequence:
      "If this zone's sensors go offline, dormant will treat the room as empty and may blank a screen while someone is there.",
  },
];

export interface UnsafePatchHit {
  patch: ConfigPatch;
  consequence: string;
}

/**
 * Walk the pending patches and return each one that sets a fail-unsafe
 * value (config-package-agnostic — the same detector used at Apply time
 * is used by tests that drive the form directly without the
 * ConfigForm/SettingsForm wrapper).
 */
export function detectUnsafePatches(patches: readonly ConfigPatch[]): UnsafePatchHit[] {
  const hits: UnsafePatchHit[] = [];
  for (const patch of patches) {
    if ("path" in patch && patch.op === "set") {
      for (const rule of FAIL_UNSAFE_SETTINGS) {
        if (rule.matches(patch.path) && rule.matchesValue(patch.value)) {
          hits.push({ patch, consequence: rule.consequence });
        }
      }
    } else if ("path" in patch && patch.op === "remove") {
      // A Remove restores the daemon's default for that key — for
      // zones.<id>.unavailable_policy that is `"present"` (fail-safe),
      // so a Remove is never unsafe.
    } else if (patch.op === "create_entity" && patch.collection === "zones") {
      const valueObj = patch.value as Record<string, unknown> | null | undefined;
      if (valueObj && "unavailable_policy" in valueObj) {
        for (const rule of FAIL_UNSAFE_SETTINGS) {
          // Synthesize the path the same pattern would see if the
          // entity were being edited in place after creation.
          const synthPath = ["zones", patch.id, "unavailable_policy"];
          if (rule.matches(synthPath) && rule.matchesValue(valueObj["unavailable_policy"])) {
            hits.push({ patch, consequence: rule.consequence });
          }
        }
      }
    } else if (patch.op === "delete_entity") {
      // Deleting an entity can never introduce a fail-unsafe value.
    }
  }
  return hits;
}

/**
 * One-line guidance for every creatable config field, grounded in
 * `docs/src/configuration.md` (the section-table descriptions) plus
 * `sensors.md`/`displays.md` where they elaborate a single field.
 *
 * Each entry is short on purpose — a per-field hint that disappears when
 * the operator already knows the meaning.  Keys are EXACTLY the field
 * names from [`CREATABLE_FIELDS`], so a new creatable field with no
 * `FIELD_HELP` entry will be flagged by the test below as a docs gap
 * rather than silently rendering a bare input.
 */
export const FIELD_HELP: Record<CrudCollection, Record<string, string>> = {
  sensors: {
    // From docs/src/configuration.md:36 — the discriminator.
    type: "Sensor backend: mqtt (broker subscriber), ha (Home Assistant WebSocket), or usb-ld2410 (USB-serial radar).",
    // From docs/src/configuration.md:42-44 — common fields.
    kind: "Sensor semantics: \"presence\" (binary occupied/vacant) or \"motion\" (transient, stretched by hold_time).",
    hold_time: "How long occupancy persists after the sensor's last on — an off inside the window is deferred. \u20140s\u201D disables.",
    stale_timeout: "How long before no data means the sensor is unavailable.",
    // docs/src/configuration.md:50-58 — mqtt.
    broker_url: "MQTT broker URL the sensor connects to (e.g., tcp://localhost:1883).",
    topic: "MQTT topic to subscribe to for this sensor's state.",
    field: "JSON pointer (RFC 6901) into the MQTT payload that holds the on/off value (default: /occupancy).",
    payload_on: "Override for the on payload value (default: JSON true). Use when the broker publishes raw text like ON.",
    payload_off: "Override for the off payload value (default: JSON false).",
    // docs/src/configuration.md:91-93 — ha.
    url: "Home Assistant WebSocket URL (e.g., ws://ha.local:8123/api/websocket).",
    entity: "HA entity id to track (e.g., binary_sensor.couch_presence).",
    // docs/src/configuration.md:107-109 — usb-ld2410.
    port: "Serial port path the LD2410 is attached to (e.g., /dev/ttyUSB0).",
    baud: "Serial baud rate (default 256000).",
  },
  zones: {
    mode: "Fusion mode: any (any member present) / all (all members present) / quorum (N members) / weighted (weight fraction).",
    members: "Sensor/zone ids in this zone. Prefix zone ids with \"zone:\" to nest zones.",
    unavailable_policy: "How unavailable members are treated. present (default, fail-safe) keeps the room on when blind; absent blanks on sensor failure \u2014 confirm to opt in.",
    weights: "Per-member float weights for \"weighted\" mode (the member weight when present).",
  },
  displays: {
    controllers: "Ordered list of controllers to try \u2014 first one that supports the requested blank mode wins.",
    blank_mode: "Primary blank mode: screen_off_audio_on / power_off / brightness_zero.",
    output: "Display selector: KWin output name (e.g., \"DP-1\" for kwin-dpms); \"cg:<uuid>\" for macos-gamma-black; omit for macos-display-sleep.",
    ddc_display: "DDC/CI display identifier (e.g., \"1\" for the first monitor /dev/i2c-1).",
    host: "Hostname or IP for network-controllable displays (Samsung, HA passthrough).",
    wol_mac: "MAC address for Wake-on-LAN when the display's network stack supports it.",
    samsung_restore_backlight: "Samsung IP Control G2 backlight (1\u201350) restored when no saved value exists.",
    restore_brightness: "DDC/CI brightness (1\u2013100) restored on wake.",
    treat_unreachable_as_blanked: "True (fail-safe): if a controller is unreachable, assume the display is blanked.",
    command_timeout: "Timeout for a single blank/wake command (default 10s).",
    shared_input_code: "VCP input-source hex code this machine reads when the display is shared.",
    shared_input_write_code: "Optional VCP input-source hex code this machine writes to claim the display.",
    shared_peer_input_code: "Optional VCP code the peer reads when displaying on this machine.",
    shared_peer_input_write_code: "Optional VCP code the peer writes when claiming this machine.",
  },
  rules: {
    zone: "Zone id whose state drives this rule.",
    displays: "Display ids this rule blanks/wakes.",
    grace_period: "Time the zone must stay stable in its target state before blanking/waking (default 60s).",
    min_blank_time: "Minimum time a display stays blanked before waking (debounces on/off cycling, default 10s).",
    min_wake_time: "Minimum time a display stays awake before blanking (default 10s).",
    inhibitors: "Hold the blank while any listed inhibitor is active \u2014 user-activity, audio-playback, call, manual-pause (no-op).",
    activity_idle_threshold: "How long without input before user-activity inhibitor considers the user idle (default 2m).",
    activity_poll_interval: "How often to poll activity state (default 5s).",
    wake_retries: "Number of wake retries before escalating (default 3).",
    wake_retry_backoff: "Backoff before the first wake retry (default 2s).",
    wake_retry_interval: "Interval between successive wake retries (default 60s).",
  },
};

/**
 * Per-field placeholder / example values, shown in the form's text
 * inputs as a hint.  Numbers and durations use their TOML form
 * (humantime for durations: "2s", "1m 30s").  Sensors/displays look
 * the same as in `docs/src/configuration.md` so a README-style
 * example reads directly into the field.
 */
export const FIELD_EXAMPLE: Record<CrudCollection, Record<string, string>> = {
  sensors: {
    hold_time: "2s",
    stale_timeout: "300s",
    broker_url: "mqtt://host:1883",
    topic: "zigbee2mqtt/desk-sensor",
    field: "/occupancy",
    payload_on: "ON",
    payload_off: "OFF",
    url: "ws://ha.local:8123/api/websocket",
    entity: "binary_sensor.couch_presence",
    port: "/dev/ttyUSB0",
    baud: "256000",
  },
  zones: {
    members: "[\"desk_mmwave\", \"living_motion\"]",
  },
  displays: {
    blank_mode: "power_off",
    output: "DP-1",
    ddc_display: "1",
    host: "192.0.2.10",
    wol_mac: "00:11:22:33:44:55",
    samsung_restore_backlight: "30",
    restore_brightness: "80",
    command_timeout: "10s",
  },
  rules: {
    grace_period: "60s",
    min_blank_time: "10s",
    min_wake_time: "10s",
    activity_idle_threshold: "2m",
    activity_poll_interval: "5s",
    wake_retries: "3",
    wake_retry_backoff: "2s",
    wake_retry_interval: "60s",
  },
};

/** Minimal inventory shape `referencingEntities` needs — a subset of `ConfigInventory`. */
export interface CrudInventoryRefs {
  zones: Record<string, { members?: string[] }>;
  rules: Record<string, { zone?: string; displays?: string[] }>;
}

/**
 * Compute a human-readable list of entities referencing `id` in
 * `collection`, for the delete-confirm warning (spec §7). A
 * client-side pre-check only — the server's daemon-identical
 * `validate()` at apply time is the real reference-integrity gate
 * (invariant #1); this exists purely so the confirm dialog can name
 * what would break, before the user commits to Apply.
 */
export function referencingEntities(
  collection: CrudCollection,
  id: string,
  inv: CrudInventoryRefs,
): string[] {
  const refs: string[] = [];
  if (collection === "zones") {
    for (const [ruleId, rule] of Object.entries(inv.rules)) {
      if (rule.zone === id) refs.push(`rule "${ruleId}"`);
    }
  }
  if (collection === "sensors") {
    for (const [zoneId, zone] of Object.entries(inv.zones)) {
      if (zone.members?.includes(id)) refs.push(`zone "${zoneId}"`);
    }
  }
  if (collection === "displays") {
    for (const [ruleId, rule] of Object.entries(inv.rules)) {
      if (rule.displays?.includes(id)) refs.push(`rule "${ruleId}"`);
    }
  }
  return refs;
}
