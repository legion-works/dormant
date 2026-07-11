/**
 * TypeScript mirrors of the dormant-core serde wire shapes.
 *
 * Every type is hand-verified against the Rust source (single source
 * of truth).  Serde rename attributes are accounted for — enums use the
 * exact wire strings.  Newtype IDs (SensorId, DisplayId, ZoneId, RuleId)
 * are `#[serde(transparent)]` and appear as plain `string` on the wire.
 *
 * Rust sources referenced:
 *   crates/dormant-core/src/rules.rs     — StateSnapshot, DaemonEvent, DisplaySnapshot, …
 *   crates/dormant-core/src/types.rs     — SensorState, BlankMode
 *   crates/dormant-core/src/doctor.rs    — DoctorReport, Check, CheckStatus
 *   crates/dormant-core/src/zone.rs      — UnavailablePolicy
 *   crates/dormant-core/src/config/schema.rs — Config, SensorConfig, ZoneConfig, …
 *   crates/dormant-web/src/routes/config.rs  — ConfigResponse
 */

// Enums — runtime `as const` arrays are the single source; types are
//    derived from them so the drift-guard test can assert exact strings.

/** rust: SensorState, serde(rename_all = "lowercase") */
export const SENSOR_STATES = ["present", "absent", "unavailable"] as const;
export type SensorState = (typeof SENSOR_STATES)[number];

/** rust: BlankMode, serde(rename_all = "snake_case") */
export const BLANK_MODES = ["power_off", "screen_off_audio_on", "brightness_zero"] as const;
export type BlankMode = (typeof BLANK_MODES)[number];

/** rust: ControllerRole, serde(rename_all = "snake_case") */
export const CONTROLLER_ROLES = ["primary", "fallback"] as const;
export type ControllerRole = (typeof CONTROLLER_ROLES)[number];

/** rust: CheckStatus, serde(rename_all = "snake_case") */
export const CHECK_STATUSES = ["ok", "fail", "skip", "not_supported"] as const;
export type CheckStatus = (typeof CHECK_STATUSES)[number];

/** rust: UnavailablePolicy, serde(rename_all = "lowercase") */
export const UNAVAILABLE_POLICIES = ["present", "absent"] as const;
export type UnavailablePolicy = (typeof UNAVAILABLE_POLICIES)[number];

/** rust: SensorKind, serde(rename_all = "snake_case") */
export type SensorKind = "presence" | "motion";

/**
 * DaemonEvent discriminator tags.
 * rust: rules.rs DaemonEvent, serde(tag = "event", rename_all = "snake_case")
 */
export const DAEMON_EVENT_TAGS = [
  "sensor_changed",
  "zone_changed",
  "display_phase",
  "config_reloaded",
  "wake_retry",
  "config_reload_rejected",
  "wear_snapshot",
  "compensation_advisory",
  "blank_failure",
  "blank_recovered",
  "wake_recovered",
] as const;
export type DaemonEventTag = (typeof DAEMON_EVENT_TAGS)[number];

/** rust: wear.rs PanelType, serde(rename_all = "kebab-case") */
export const PANEL_TYPES = ["woled", "qd-oled", "unknown"] as const;
export type PanelType = (typeof PANEL_TYPES)[number];

/**
 * rust: rules.rs SensorSnapshot
 * serde: field names match exactly (no rename). `reported` is
 * `#[serde(default)]` — a cold-start diagnostic ("has this sensor ever
 * delivered an event since daemon start", any state counts). Modelled as
 * optional here (`stage?:` precedent) so a pre-this-feature legacy wire
 * payload that omits the key entirely still deserializes as `undefined`,
 * not a hard failure.
 */
export interface SensorSnapshot {
  id: string;
  state: SensorState;
  last_seen_secs_ago: number;
  reported?: boolean;
}

/**
 * rust: rules.rs ZoneSnapshot
 * serde: field names match exactly.
 */
export interface ZoneSnapshot {
  id: string;
  present: boolean | null; // None = unknown to engine
}

/**
 * rust: rules.rs ControllerHealth
 * serde: `detail` is `#[serde(default, skip_serializing_if = "Option::is_none")]`
 */
export interface ControllerHealth {
  name: string;
  role: ControllerRole;
  healthy: boolean;
  detail?: string;
}

/**
 * rust: rules.rs DisplaySnapshot
 * serde: `controllers` is `#[serde(default)]` (absent for legacy snapshots).
 * `stage` is `#[serde(default, skip_serializing_if = "Option::is_none")]`
 * (absent from legacy wire and omitted when None — back-compat).
 * `wake_attempts` / `last_blank_failed` are `#[serde(default)]` — mirrors
 * the `stage?:` back-compat precedent: legacy wire that omits these keys
 * deserializes as `undefined` here, not a hard failure.
 */
export interface DisplaySnapshot {
  phase: string; // grep-stable literal: "active" | "grace" | "blanking" | "blanked" | "waking" | "render_pending" | "staged"
  inhibited: boolean;
  paused: boolean;
  cmd_gen: number;
  controllers: ControllerHealth[];
  /** Current wake-retry attempt counter for this display (0 once healthy
   * or before the first attempt). Absent on legacy wire. */
  wake_attempts?: number;
  /** Whether the last blank attempt for this display exhausted its
   * controller chain and has not yet recovered. Absent on legacy wire. */
  last_blank_failed?: boolean;
  /** Present only when the display is in the `staged` phase. */
  stage?: { idx: number; kind: StageKind } | null;
}

/**
 * rust: rules.rs StateSnapshot
 * serde: `displays` is `Vec<(String, DisplaySnapshot)>` → JSON array of [string, DisplaySnapshot].
 * `pending_reload` is `Option<String>` → null or string.
 */
export interface StateSnapshot {
  sensors: SensorSnapshot[];
  zones: ZoneSnapshot[];
  displays: [string, DisplaySnapshot][];
  pending_reload: string | null;
}

/**
 * rust: rules.rs DaemonEvent, serde(tag = "event", rename_all = "snake_case")
 *
 * On the wire every event carries an `"event"` discriminator field.
 * Newtype IDs (SensorId, DisplayId, ZoneId) appear as plain strings.
 */
export type DaemonEvent =
  | SensorChangedEvent
  | ZoneChangedEvent
  | DisplayPhaseEvent
  | ConfigReloadedEvent
  | ConfigReloadRejectedEvent
  | WakeRetryEvent
  | WearSnapshotEvent
  | CompensationAdvisoryEvent
  | BlankFailureEvent
  | BlankRecoveredEvent
  | WakeRecoveredEvent;

export interface SensorChangedEvent {
  event: "sensor_changed";
  sensor: string;
  state: SensorState;
}

export interface ZoneChangedEvent {
  event: "zone_changed";
  zone: string;
  present: boolean;
  cause: string;
}

export interface DisplayPhaseEvent {
  event: "display_phase";
  display: string;
  phase: string;
  cause: string;
}

export interface ConfigReloadedEvent {
  event: "config_reloaded";
}

export interface ConfigReloadRejectedEvent {
  event: "config_reload_rejected";
  detail: string;
}

// Compile-time pin: if ConfigReloadRejectedEvent is dropped from
// the DaemonEvent union, the Extract narrows to `never` and the
// assignment of a concrete object to `never` fails tsc.
const _rejected: Extract<DaemonEvent, { event: "config_reload_rejected" }> = {
  event: "config_reload_rejected",
  detail: "",
};
void _rejected;

export interface WakeRetryEvent {
  event: "wake_retry";
  display: string;
  attempt: number;
}

/**
 * rust: rules.rs DaemonEvent::WearSnapshot
 * serde: `total_on_hours` / `sample_count` are `#[serde(default)]`.
 */
export interface WearSnapshotEvent {
  event: "wear_snapshot";
  display: string;
  total_on_hours: number;
  sample_count: number;
}

/**
 * rust: rules.rs DaemonEvent::CompensationAdvisory
 * serde: `hours_since_long_dwell` is `#[serde(default)]`.
 */
export interface CompensationAdvisoryEvent {
  event: "compensation_advisory";
  display: string;
  hours_since_long_dwell: number;
}

/**
 * rust: rules.rs DaemonEvent::BlankFailure
 * serde: `controller` / `detail` are `#[serde(default)]`. NOTE: the
 * `blank_failure` wire tag is unrelated to `DisplayPhase.phase` string
 * literals — don't conflate the two when grepping.
 */
export interface BlankFailureEvent {
  event: "blank_failure";
  display: string;
  controller: string;
  detail: string;
}

/**
 * rust: rules.rs DaemonEvent::BlankRecovered
 * serde: field names match exactly (no rename).
 */
export interface BlankRecoveredEvent {
  event: "blank_recovered";
  display: string;
}

/**
 * rust: rules.rs DaemonEvent::WakeRecovered
 * serde: `attempts` is `#[serde(default)]`.
 */
export interface WakeRecoveredEvent {
  event: "wake_recovered";
  display: string;
  attempts: number;
}

/**
 * rust: doctor.rs Check
 * serde: `detail` is `#[serde(default, skip_serializing_if = "Option::is_none")]`
 */
export interface Check {
  name: string;
  status: CheckStatus;
  detail?: string;
}

/** rust: doctor.rs DoctorReport */
export interface DoctorReport {
  checks: Check[];
}

/**
 * rust: config/routes.rs ConfigValidation
 */
export interface ConfigValidation {
  ok: boolean;
  warnings: { key_path: string; message: string }[];
  errors: { what: string; detail: string }[];
  load_error?: string;
}

/** rust: config/routes.rs DisplayRuleInfo */
export interface DisplayRuleInfo {
  rule: string;
  zone: string;
}

/**
 * rust: config/schema.rs Config (inventory)
 *
 * IndexMap serializes as a JSON object keyed by user-chosen id.
 * Sub-structs are kept loose — the Config view renders known fields
 * and tolerates new ones added by later M1 patches.
 */
export interface ConfigInventory {
  config_version: number;
  daemon: Record<string, unknown>;
  /** rust: config/schema.rs WearConfig — the `[wear]` TOML section. Optional
   * in fixtures/older payloads; the WearSection form treats absence as `{}`. */
  wear?: Record<string, unknown>;
  /** rust: config/schema.rs NotificationsConfig — the `[notifications]`
   * TOML section. Optional in fixtures/older payloads, mirroring `wear`;
   * the NotificationsSection form treats absence as `{}`. */
  notifications?: Record<string, unknown>;
  sensors: Record<string, SensorConfig>;
  zones: Record<string, ZoneConfig>;
  displays: Record<string, DisplayConfig>;
  rules: Record<string, RuleConfig>;
}

/** rust: config/schema.rs SensorConfig — internally-tagged enum, tag = "type" */
export type SensorConfig =
  | { type: "mqtt" } & MqttSensorCfg
  | { type: "ha" } & HaSensorCfg
  | { type: "usb-ld2410" } & UsbLd2410Cfg;

/** rust: config/schema.rs MqttSensorCfg */
export interface MqttSensorCfg {
  broker_url: string;
  topic: string;
  field?: string;
  payload_on?: string;
  payload_off?: string;
  kind?: SensorKind;
  hold_time?: unknown;
  stale_timeout?: unknown;
  /** Optional LWT/availability topic override; defaults to
   * `<topic>/availability` (Zigbee2MQTT convention) when absent. */
  availability_topic?: string;
  /** Payload literal marking the availability topic "online". Defaults to
   * `"online"` server-side. */
  availability_payload_online?: string;
  /** Payload literal marking the availability topic "offline". Defaults to
   * `"offline"` server-side. */
  availability_payload_offline?: string;
}

/** rust: config/schema.rs HaSensorCfg (fields: url, entity, kind, hold_time, stale_timeout) */
export interface HaSensorCfg {
  url: string;
  entity: string;
  kind?: SensorKind;
  hold_time?: unknown;
  stale_timeout?: unknown;
}

/** rust: config/schema.rs UsbLd2410Cfg (fields: port, baud, kind, hold_time, stale_timeout) */
export interface UsbLd2410Cfg {
  port: string;
  baud?: number;
  kind?: SensorKind;
  hold_time?: unknown;
  stale_timeout?: unknown;
}

/** rust: config/schema.rs ZoneConfig */
export interface ZoneConfig {
  mode: string;
  members: string[];
  quorum?: number;
  threshold?: number;
  weights: Record<string, number>;
  unavailable_policy: UnavailablePolicy;
}

/** rust: StageKind — flat serde tags (the kind field on a LadderStage) */
export const STAGE_KINDS = [
  "power_off",
  "screen_off_audio_on",
  "brightness_zero",
  "render_black",
  "render_screensaver",
] as const;
export type StageKind = (typeof STAGE_KINDS)[number];

/** rust: config/schema.rs LadderStage */
export interface LadderStage {
  kind: StageKind;
  dwell?: string;
}

/** rust: config/schema.rs ScreensaverSource */
export interface ScreensaverSource {
  path?: string;
  urls?: string[];
  recurse?: boolean;
  shuffle?: boolean;
  order?: string;
  image_duration?: string;
}

/** rust: config/schema.rs ScreensaverConfig */
export interface ScreensaverConfig {
  trigger: string;
  audio: boolean;
  /** Source-frame scaling onto the rendered output. `null`/undefined → Fill. */
  scale_mode?: "fill" | "fit" | "stretch" | "center" | null;
  /** Transition between consecutive playlist items. `null`/undefined → Crossfade. */
  transition?: "crossfade" | "none" | null;
  /** Length of the Crossfade blend. `null`/undefined → 1 second. */
  transition_duration?: string | null;
  source: ScreensaverSource[];
  /** Pixel-shift distance (px) applied periodically to reduce burn-in. Default 2. */
  shift_px?: number;
  /** Interval between successive pixel shifts. Default "120s". */
  shift_interval?: string;
}

/** rust: config/schema.rs DisplayConfig */
export interface DisplayConfig {
  controllers: string[];
  blank_mode?: BlankMode;
  degraded_mode?: BlankMode;
  ladder?: LadderStage[];
  screensaver?: ScreensaverConfig;
  output?: string;
  ddc_display?: string;
  host?: string;
  wol_mac?: string;
  blank_command?: string;
  wake_command?: string;
  modes?: BlankMode[];
  ha_url?: string;
  blank_service?: string;
  blank_data?: unknown;
  wake_service?: string;
  wake_data?: unknown;
  command_timeout?: unknown;
  restore_brightness?: number;
  treat_unreachable_as_blanked?: boolean;
  /** Panel technology classification. Default "unknown". */
  panel_type?: PanelType;
}

// ─── Config-apply wire types ──────────────────────────────────────────────
// rust: config_apply.rs + config_patch.rs + error.rs
// Serde: Patch uses tag="op", rename_all="lowercase".

/** rust: config_patch.rs Patch, serde(tag = "op", rename_all = "lowercase") */
export type ConfigPatch =
  | { op: "set"; path: string[]; value: unknown }
  | { op: "remove"; path: string[] };

/** rust: config_apply.rs ApplyRequest */
export interface ApplyRequest {
  /** Lowercase hex SHA-256 of the on-disk config bytes. */
  fingerprint: string;
  /** Ordered list of patches to apply. */
  patches: ConfigPatch[];
}

/** rust: config_apply.rs ApplyResponse */
export interface ApplyResponse {
  applied: boolean;
  /** Outcome: `"reloaded"` | `"rejected"` | `"pending"` | `"superseded"`. */
  reload: string;
  /** Human-readable detail when `reload` is `"rejected"`. */
  detail?: string;
}

/** 422 error body from `POST /api/config/apply` (`{ "errors": […] }`).
 *  rust: error.rs into_response — ValidationFailed, RedactedPathTargeted,
 *  PatchPathDenied, EntityUnknown, PatchValueRejected, PatchCapExceeded. */
export interface ApplyErrorBody {
  errors: ConfigApplyErrorDetail[];
}

/** A single error entry in the 422 `errors` array.
 *  rust: error.rs SerializableValidationError { what, detail }. */
export interface ConfigApplyErrorDetail {
  what: string;
  detail: string;
}

/** 409 error body from `POST /api/config/apply` (fingerprint mismatch).
 *  rust: error.rs into_response — FingerprintMismatch. */
export interface ApplyConflictBody {
  error: string;
}

/** rust: config/schema.rs RuleConfig */
export interface RuleConfig {
  zone: string;
  displays: string[];
  grace_period?: unknown;
  min_blank_time?: unknown;
  min_wake_time?: unknown;
  inhibitors?: string[];
  activity_idle_threshold?: unknown;
  activity_poll_interval?: unknown;
  wake_retries?: number;
  wake_retry_backoff?: unknown;
  wake_retry_interval?: unknown;
}

/**
 * rust: config/routes.rs ConfigResponse
 * Full shape of GET /api/config.
 *
 * `fingerprint` is a lowercase hex SHA-256 of the on-disk config bytes
 * (computed before redaction) — the client must send it back with every
 * `POST /api/config/apply` for optimistic-concurrency control.
 * `redacted_paths` are TOML-key paths of every value redacted from
 * `raw_toml`; array indices are decimal strings.
 */
export interface ConfigResponse {
  path: string;
  config_version: number;
  source: string;
  raw_toml: string;
  inventory: ConfigInventory;
  validation: ConfigValidation;
  display_rules: Record<string, DisplayRuleInfo>;
  /** Lowercase hex SHA-256 of the on-disk config bytes as returned by GET /api/config. */
  fingerprint: string;
  /** TOML-key paths of every value that was redacted, in discovery order. */
  redacted_paths: string[][];
}

// ─── Wear (panel-exposure) wire types ─────────────────────────────────────
// rust: dormant-web/src/routes/wear.rs

/**
 * rust: routes/wear.rs WearSummary
 *
 * `display` is the wear tracker's resolved storage key (panel identity when
 * available, else the sanitized config display key) — NOT necessarily the
 * `[displays.*]` config id.  `advisory` is server-derived (recomputed on
 * every fetch), so this is always the truth even if a WS nudge was missed.
 */
export interface WearSummary {
  display: string;
  display_name: string;
  panel_type: PanelType;
  total_on_hours: number;
  seeded_usage_hours?: number | null;
  sample_count: number;
  last_sample_at_epoch_s?: number | null;
  last_long_dwell_epoch_s?: number | null;
  advisory: boolean;
  /**
   * Hours since `max(last_long_dwell_epoch_s, advisory_baseline_epoch_s)` —
   * the same derivation the tracker uses for
   * `CompensationAdvisoryEvent.hours_since_long_dwell`. Always a real
   * number, even when `last_long_dwell_epoch_s` is null (no long dwell
   * observed yet — the common first-load case), so the client never has
   * to render a "?" day count.
   */
  hours_since_long_dwell: number;
}

/** rust: routes/wear.rs — `GET /api/wear` response envelope. */
export interface WearListResponse {
  displays: WearSummary[];
}

/** rust: routes/wear.rs WearDetail — `GET /api/wear/:display` response. */
export interface WearDetail extends WearSummary {
  grid_rows: number;
  grid_cols: number;
  cells: number[];
  heat: number[];
}
