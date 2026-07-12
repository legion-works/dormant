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
];

/**
 * rust: crates/dormant-core/src/config/validate.rs VALID_INHIBITORS (:392)
 *
 * THIS BRANCH ONLY (`feat/config-crud-wizard`) — a sibling branch
 * (feature 03, audio-aware-blanking) adds audio-playback inhibitor
 * literals that do NOT exist on this branch's `validate.rs`. Do not add
 * them here; this mirror must match what THIS branch's server actually
 * accepts, not a future merge.
 */
export const VALID_INHIBITORS = ["user-activity", "manual-pause"] as const;

/** rust: crates/dormant-displays/src/samsung_tizen.rs SamsungTizenController::NAME (:770) */
export const SAMSUNG_TIZEN_CONTROLLER = "samsung-tizen";

/**
 * rust: crates/dormant-displays/src/registry.rs CONTROLLER_TYPES (:44-52)
 *
 * The Linux superset (`ddcci` is Linux-only server-side); listed here
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
