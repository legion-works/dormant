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
  "pause_changed",
  "config_reloaded",
  "wake_retry",
  "config_reload_rejected",
  "wear_snapshot",
  "wear_sampling_started",
  "wear_sampling_degraded",
  "compensation_advisory",
  "blank_failure",
  "blank_recovered",
  "wake_recovered",
  "ownership",
  "operations_changed",
  "wear_sampling_source_gate",
] as const;
export type DaemonEventTag = (typeof DAEMON_EVENT_TAGS)[number];

/** rust: wear.rs PanelType, serde(rename_all = "kebab-case") */
export const PANEL_TYPES = ["woled", "qd-oled", "unknown"] as const;
export type PanelType = (typeof PANEL_TYPES)[number];

/** rust: screensaver source ordering literals */
export const SCREENSAVER_ORDERS = ["sequential", "wear-even"] as const;
export type ScreensaverOrder = (typeof SCREENSAVER_ORDERS)[number];

/** rust: screensaver video luminance tag literals */
export const WEAR_TAGS = ["dark", "medium", "bright"] as const;
export type WearTag = (typeof WEAR_TAGS)[number];

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
  /** Shared-display coordination scope. Absent on legacy wire. */
  scope?: "private" | "shared";
  /** Shared-display ownership verdict. Absent on legacy wire. */
  owned?: boolean;
  /** Last observed shared-display input source. */
  observed_input_code?: number | null;
  /** Panel state observed alongside the shared-display input source. */
  panel_state?: PanelState | null;
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

/** rust: rules.rs RollbackStatus */
export interface RollbackStatus {
  failed_fp: string;
  lkg_fp: string;
  detail: string;
  /** Platform-specific restart command suggestion. Absent on older daemons. */
  recovery_command?: string;
}

/**
 * rust: rules.rs EmergencyWakeResult
 * serde: `error` is `#[serde(default, skip_serializing_if = "Option::is_none")]`.
 */
export interface EmergencyWakeResult {
  display: string;
  ok: boolean;
  error?: string;
}

/**
 * rust: rules.rs EmergencyWakeReport — response body of
 * `POST /api/emergency-wake`.
 * serde: field names match exactly (no rename).
 */
export interface EmergencyWakeReport {
  paused: boolean;
  displays: EmergencyWakeResult[];
}

/**
 * rust: traits.rs PowerState, serde(rename_all = "snake_case")
 * Only two variants exist on the wire — there is no "off" state.
 */
export type PowerState = "on" | "standby";

/**
 * rust: traits.rs PanelState
 * serde: both fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`.
 */
export interface PanelState {
  power?: PowerState;
  brightness?: number;
}

/** rust: rules.rs ExerciseVerdict, serde(rename_all = "snake_case") */
export type ExerciseVerdict = "confirmed" | "unconfirmable" | "failed";

/**
 * rust: rules.rs ExerciseStep
 * serde: `blank_mode`/`state_before`/`state_after`/`error` are
 * `#[serde(default, skip_serializing_if = "Option::is_none")]`.
 */
export interface ExerciseStep {
  command: string;
  blank_mode?: BlankMode;
  returned_ok: boolean;
  state_before?: PanelState;
  state_after?: PanelState;
  verdict: ExerciseVerdict;
  error?: string;
}

/**
 * rust: rules.rs ExerciseReport — response body of
 * `POST /api/doctor/exercise/:display`.
 * serde: `paused_rules` is `#[serde(default, skip_serializing_if = "Vec::is_empty")]`.
 */
export interface ExerciseReport {
  display: string;
  pre_phase: string;
  paused_rules?: string[];
  steps: ExerciseStep[];
}

/**
 * rust: dormant_web::routes::operations::OperationsStatus — response body of
 * `GET /api/operations`.
 */
export interface OperationsStatus {
  exercise_in_flight: string[];
  emergency_wake_in_flight: boolean;
}

/**
 * rust: dormant_web::routes::daemon::DaemonIdentity — response body of
 * `GET /api/daemon`.
 */
export interface DaemonIdentity {
  pid: number;
  started_epoch_s: number;
  version: string;
  socket: string;
  /** rust: DaemonIdentity::star_nudge_dismissed — whether the sidebar
   *  "Star the repo" nudge has been dismissed (flag file in config dir).
   *  Omitted by old daemons so the API client defaults it to false. */
  star_nudge_dismissed?: boolean;
  /** rust: DaemonIdentity::wear_sampling_supported — whether the daemon's
   *  active wear-sampling pipeline is **platform-capable** on this host.
   *  Derived from `wear_sampling_rx.borrow().is_some()`: non-Linux builds
   *  never spawn the active sampler (the module is
   *  `#[cfg(target_os = "linux")]`), so the watch stays `None` and this
   *  is `false`. Critically, this is independent of the user's
   *  `wear.active_sampling.enabled` config flag — that is the user's
   *  *intent*, this is the system's *capability*. The wear-card
   *  onboarding nudge (issue #186) MUST read this field, not the config
   *  flag, to decide whether to show the portal action. */
  wear_sampling_supported?: boolean;
  /** rust: DaemonIdentity::wear_sampling_nudge_dismissed — whether the
   *  wear-card onboarding nudge has been dismissed (flag file in config
   *  dir). Omitted by old daemons so the API client defaults it to false. */
  wear_sampling_nudge_dismissed?: boolean;
}

/**
 * rust: rules.rs StateSnapshot
 * serde: `displays` is `Vec<(String, DisplaySnapshot)>` → JSON array of [string, DisplaySnapshot].
 * `pending_reload` is `Option<String>` → null or string.
 */
export interface KvmStatus {
  keymap: { claim_hotkey?: string };
  switch_capable_displays: string[];
  activity_following: boolean;
  push_capable_displays: string[];
}

export interface StateSnapshot {
  sensors: SensorSnapshot[];
  zones: ZoneSnapshot[];
  displays: [string, DisplaySnapshot][];
  pending_reload: string | null;
  /** Omitted when the daemon is not running from a rollback. */
  rollback?: RollbackStatus;
  /** KVM claim presentation state; absent when coordination is unavailable. */
  kvm?: KvmStatus;
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
  | PauseChangedEvent
  | ConfigReloadedEvent
  | ConfigReloadRejectedEvent
  | WakeRetryEvent
  | WearSnapshotEvent
  | WearSamplingStartedEvent
  | WearSamplingDegradedEvent
  | CompensationAdvisoryEvent
  | BlankFailureEvent
  | BlankRecoveredEvent
  | WakeRecoveredEvent
  | OwnershipEvent
  | OperationsChangedEvent
  | WearSamplingSourceGateEvent;

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

export interface PauseChangedEvent {
  /** rust: rules.rs DaemonEvent::PauseChanged */
  event: "pause_changed";
  display: string;
  paused: boolean;
  rule: string | null;
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
  wear_attribution_mode?: "uniform" | "sampled";
}

export interface WearSamplingStartedEvent {
  event: "wear_sampling_started";
}

export interface WearSamplingDegradedEvent {
  event: "wear_sampling_degraded";
  reason: string;
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
 * rust: rules.rs DaemonEvent::Ownership
 * serde: `observed_input_code` / `verified` / `degraded` are `#[serde(default)]`.
 */
export interface OwnershipEvent {
  event: "ownership";
  display: string;
  owned: boolean;
  /** VCP 0x60 code written (write path), absent for poll-observed events. */
  written_code?: number | null;
  observed_input_code?: number | null;
  /** "pull" | "push" | "poll" | "activity_follow" | "hotkey" | "cli" | "tray" | "web" */
  cause: string;
  /** Present when cause is a write path: did readback verify the panel moved? */
  verified?: boolean | null;
  /** Set when the write path degraded (peer READ alias absent). */
  degraded?: boolean;
}

/**
 * rust: rules.rs DaemonEvent::OperationsChanged (issue #184).
 * Pushed by the HTTP layer after every exercise / emergency-wake guard mutation
 * (insert AND remove, including the detached completion monitor). Lets the
 * webui replace the 1 Hz paired poll with event-driven UI.
 */
export interface OperationsChangedEvent {
  event: "operations_changed";
  /** Display ids with a web exercise currently awaiting engine completion. */
  exercise_in_flight: string[];
  /** Whether a global web emergency wake is currently awaiting engine completion. */
  emergency_wake_in_flight: boolean;
}

/**
 * rust: rules.rs DaemonEvent::WearSamplingSourceGate
 * serde(tag = "event", rename_all = "snake_case"); `observed` is
 * `#[serde(default)]` (absent on the wire when `None` — matched and unknown
 * gates carry no observed source label). Emitted only on a full-gate
 * transition (`matched` | `mismatched` | `unknown`); steady-state polls do
 * not re-fire it. The webui refetches `GET /api/wear` on this event so the
 * row picks up the new `source_gate` from the summary, rather than
 * patching in-memory state from the event payload.
 */
export interface WearSamplingSourceGateEvent {
  event: "wear_sampling_source_gate";
  display: string;
  state: "matched" | "mismatched" | "unknown";
  observed?: string | null;
}

/**
 * rust: doctor.rs Check
 * serde: `detail` is `#[serde(default, skip_serializing_if = "Option::is_none")]`
 * `category` / `subject` are added by BG-7.
 */
export interface Check {
  name: string;
  status: CheckStatus;
  detail?: string;
  /** "config" | "sensor" | "display" | "platform" | "network" */
  category?: string;
  /** The entity this check is about — display id, sensor id, or absent. */
  subject?: string;
}

/** rust: doctor.rs DoctorReport */
export interface DoctorReport {
  checks: Check[];
}

/** rust: event_ring.rs RecentEvent — a DaemonEvent with server timestamp. */
export interface RecentEvent {
  at_epoch_ms: number;
  event: DaemonEvent;
}

/** rust: GET /api/events/recent response */
export interface RecentEventsResponse {
  events: RecentEvent[];
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
  wear?: WearConfig;
  /** rust: config/schema.rs NotificationsConfig — the `[notifications]`
   * TOML section. Optional in fixtures/older payloads, mirroring `wear`;
   * the NotificationsSection form treats absence as `{}`. */
  notifications?: Record<string, unknown>;
  /** rust: config/schema.rs WatchdogConfig — the `[watchdog]` TOML section
   * (crash-loop watchdog / last-known-good rollback). Optional in
   * fixtures/older payloads, mirroring `wear`/`notifications`; the
   * WatchdogSection form treats absence as `{}`. */
  watchdog?: Record<string, unknown>;
  /** rust: config/schema.rs AudioConfig — the `[audio]` TOML section
   * (global PipeWire audio-inhibitor config). Optional in fixtures/older
   * payloads, mirroring `wear`/`notifications`; the AudioSection form
   * treats absence as `{}`. `playback_roles` is `Option<Vec<String>>` on
   * the wire (`null` or a string array); `pw_dump_command` is rendered
   * read-only per the T7 security fold (spec §6#10). */
  audio?: Record<string, unknown>;
  /** rust: config/schema.rs CoordinationConfig — optional for older payloads. */
  coordination?: CoordinationConfig;
  /** rust: config/schema.rs KeymapConfig — optional for older payloads. */
  keymap?: KeymapConfig;
  /** rust: config/schema.rs InputFilterConfig — optional for older payloads. */
  input_filter?: InputFilterConfig;
  /** rust: config/schema.rs PublishConfig — opt-in MQTT state publish
   * (issue #105). Optional for older payloads; the publish view treats
   * absence as the default-disabled state. Always rendered without
   * credentials — the broker URL is the lookup key for
   * `creds.mqtt`, which is never serialized into the inventory. */
  publish?: Record<string, unknown>;
  sensors: Record<string, SensorConfig>;
  zones: Record<string, ZoneConfig>;
  displays: Record<string, DisplayConfig>;
  rules: Record<string, RuleConfig>;
}

/** rust: config/schema.rs ActiveSamplingConfig — `[wear.active_sampling]`.
 *
 * `sampled_displays` is the canonical multi-display field. The legacy
 * singular `sampled_display` is still accepted for backward
 * compatibility inside `config_version = 1`; the editor surfaces
 * `sampled_displays` and the server validates that exactly one of the
 * two keys is present. When `sampled_displays` is omitted (legacy
 * configs), the form keeps rendering the single `sampled_display`
 * row. */
export interface ActiveSamplingConfig {
  enabled: boolean;
  /** Legacy singular form. Mutually exclusive with `sampled_displays`. */
  sampled_display?: string | null;
  /** Canonical plural form. */
  sampled_displays?: string[];
  stream_mode: "warm" | "per-tick";
  capture_timeout: string;
  failure_threshold: number;
  circuit_reset_after: string;
}

/** rust: config/schema.rs WearConfig — the `[wear]` TOML section. */
export interface WearConfig {
  enabled?: boolean;
  active_sampling?: ActiveSamplingConfig;
  [key: string]: unknown;
}

/** rust: config/schema.rs CoordinationConfig
 *
 * Remaining fields after the KVM direct-write pivot removed the
 * owner-mediated claim protocol (Task 16).
 */
export interface CoordinationConfig {
  poll_interval?: string;
  state_poll_interval?: string;
  loss_confirmations?: number;
  reprobe_failure_threshold?: number;
  reprobe_interval?: string;
  activity_follow?: boolean;
  arm_after?: string;
  cooldown?: string;
}

/** rust: config/schema.rs KeymapConfig */
export interface KeymapConfig {
  claim_hotkey?: string | null;
}

/** rust: config/schema.rs InputFilterConfig */
export interface InputFilterConfig {
  ignore_devices?: string[];
}

/** rust: config/schema.rs HookAction */
export interface HookAction {
  command?: string[];
  mqtt?: { topic: string; payload: string };
  timeout?: string;
  blocking?: boolean;
  abort_on_failure?: boolean;
}

/** rust: config/schema.rs HookSlots */
export interface HookSlots {
  before_release?: HookAction[];
  after_release?: HookAction[];
  before_acquire?: HookAction[];
  after_acquire?: HookAction[];
  /** Actions run after observing (via VCP 0x60 poll) that a peer pulled the panel. */
  on_observed_loss?: HookAction[];
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

/** rust: config/schema.rs DisplaySamplingConfig — per-display compositor
 * sampling declaration (`[displays.<id>.sampling]` TOML table).
 *
 * All fields are optional — the table is opt-in. `source_poll_interval`
 * defaults server-side to `defaults::WEAR_SOURCE_POLL_INTERVAL` (15s); the
 * TS mirror leaves it `undefined` so the operator can clear/override it
 * explicitly. `stream_mode` reuses the same `StreamMode` enum the wear path
 * uses (kebab-case `warm` / `per-tick`); absent means "inherit the global
 * wear.active_sampling.stream_mode" and the editor surfaces an explicit
 * "Inherit global" choice in the select.
 *
 * `watched_apps` semantics (issue #232):
 * - When the key is **absent** on the wire, the server-side serde
 *   default fn seeds it with `defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS`
 *   (the fail-safe direction: a stock TV config suspends spatial
 *   attribution under installed streaming apps out of the box).
 * - When the key is present as an empty array `[]`, the operator has
 *   opted out — pure input-only gate, no app-visibility probe.
 * - When the key is present as a non-empty array, that list overrides
 *   the seed (the operator's catalog is authoritative).
 *
 * The TS mirror surfaces `undefined` for the absent case and an array
 * for the present case; the editor renders empty for both — it does NOT
 * pre-populate the seed (the daemon's seeded list is documented in
 * `docs/src/active-wear-sampling.md` and reflected in the field's help
 * text). Operators who want to extend the seed list must declare an
 * explicit array. */
export interface DisplaySamplingConfig {
  expected_source?: string | null;
  source_poll_interval?: string;
  stream_mode?: "warm" | "per-tick" | null;
  watched_apps?: string[] | null;
}

/** rust: config/schema.rs ScreensaverSource */
export interface ScreensaverSource {
  path?: string;
  urls?: string[];
  recurse?: boolean;
  shuffle?: boolean;
  order?: ScreensaverOrder;
  wear_tag?: WearTag;
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
  /** Temperature used by deterministic wear-even ordering. */
  wear_temperature?: number;
  /** Bias toward colder regions for screensaver pixel shifting. */
  shift_heat_bias?: number;
}

/** rust: config/schema.rs DisplayConfig */
export interface DisplayConfig {
  controllers: string[];
  scope?: "private" | "shared";
  shared_input_code?: number;
  shared_input_write_code?: number;
  shared_peer_input_code?: number;
  shared_peer_input_write_code?: number;
  hooks?: HookSlots;
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
  /**
   * Explicit acknowledgement of the macOS shared-DDC/CI power-off hazard
   * (issue #126). When a display's primary controller is DDC/CI, the panel
   * is shared with another machine, and the primary blank mode is
   * `power_off`, dormant emits a semantic warning at config load time and
   * the doctor flags the topology. Setting this to `true` silences the
   * warning — the operator attests they have tested physical recovery.
   * The opt-in adds NO recovery mechanism of its own. Defaults to `false`
   * so an unsuspecting operator never lands in the trap silently. The
   * editor renders a hazard-bordered checkbox that requires the
   * operator to read the warning copy before flipping it on.
   */
  power_off_opt_in?: boolean;
  /**
   * Explicit compositor output declaration for active sampling. SEPARATE
   * from the local KWin render-controller `output` key above: this names
   * the compositor output the active sampler should observe, not the
   * local KWin target. For a remote-only TV the renderer never sees the
   * panel; the operator declares this to opt the display into the
   * sampling path. Absent means no compositor source is wired.
   */
  compositor_output?: string | null;
  /**
   * Compositor-sampling declaration table. Absent when the operator has
   * not opted this display into active sampling. Field shapes live in
   * [`DisplaySamplingConfig`] above.
   */
  sampling?: DisplaySamplingConfig | null;
}

// ─── Config-apply wire types ──────────────────────────────────────────────
// rust: config_apply.rs + config_patch.rs + error.rs
// Serde: Patch uses tag="op", rename_all="lowercase".

/**
 * rust: config_patch.rs Patch, serde(tag = "op", rename_all = "lowercase")
 *
 * `CreateEntity`/`DeleteEntity` (config-crud-wizard spec §3) need an
 * EXPLICIT `#[serde(rename = "...")]` on the Rust side — `rename_all =
 * "lowercase"` would otherwise produce `"createentity"`/`"deleteentity"`
 * (it lowercases the whole variant name, not `snake_case`s it), not the
 * `"create_entity"`/`"delete_entity"` ops actually on the wire. Fields
 * are TOP-LEVEL (`collection`/`id`/`value`), NOT path-based like
 * `Set`/`Remove` — mirrors `config_patch.rs:37-60` exactly.
 */
export type ConfigPatch =
  | { op: "set"; path: string[]; value: unknown }
  | { op: "remove"; path: string[] }
  | { op: "create_entity"; collection: string; id: string; value: unknown }
  | { op: "delete_entity"; collection: string; id: string };

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
  input_wake_hold?: unknown;
}

/**
 * rust: config/schema.rs DaemonConfig — entity_crud_enabled/pairing_enabled/
 * pair_timeout (config-crud-wizard spec §10, `#[serde(default)]`, all three
 * `true`/`true`/`"120s"` by default).
 *
 * `ConfigInventory.daemon` stays `Record<string, unknown>` (the settings
 * form renders every `[daemon]` key generically, `DaemonSection.tsx`) —
 * this interface documents the three CRUD-relevant keys' shape for call
 * sites that read them explicitly (`entityCrud.ts`'s
 * `isEntityCrudEnabled`/`isPairingEnabled`); it is not itself the wire
 * type of `daemon` (which stays loose).
 */
export interface DaemonCrudFlags {
  entity_crud_enabled?: boolean;
  pairing_enabled?: boolean;
  /** humantime string, e.g. "120s". */
  pair_timeout?: string;
}

/** rust: routes/pair.rs PairRequest — POST /api/pair/samsung body. */
export interface PairRequest {
  host: string;
}

/** rust: routes/pair.rs PairAccepted — POST /api/pair/samsung 202 response. */
export interface PairAccepted {
  pair_id: string;
}

/**
 * rust: routes/pair.rs PairStatus — GET /api/pair/samsung/{id} response.
 * `state` is one of "pairing" | "paired" | "timeout" | "error". Never
 * carries a token field, by construction (spec §8/§9 invariant #5).
 */
export interface PairStatus {
  state: "pairing" | "paired" | "timeout" | "error";
  detail?: string | null;
}

/** rust: ipc_proto.rs WearSamplingStatus — token-free sampling flow status. */
export interface WearSamplingStatus {
  status: "awaiting_consent" | "granted" | "denied" | "timed_out" | "error";
  reason?: string;
}

/** rust: wear.rs WearSamplingStatus — redacted sampler lifecycle state. */
export interface WearSamplingLifecycleStatus {
  state: "disabled" | "needs_consent" | "consent_pending" | "connecting" | "streaming" | "suspended" | "cooldown";
  last_capture_age_s?: number | null;
  uniform_reason?: string | null;
  bound_display?: string | null;
  granted_at_epoch_s?: number | null;
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
  /** The `[displays.*]` config id this ledger is attributed to, when
   * known.  The frontend joins on this field first, falling back to
   * `display_name` for backward compatibility with pre-BG-8 ledgers. */
  config_display_id?: string | null;
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
  wear_attribution_mode?: "uniform" | "sampled";
  content_weighted_since?: number | null;
  /**
   * Stable source-gate tag for this display (`"matched"`, `"mismatched"`, or
   * `"unknown"`). Absent when the display has no gate configuration, or when
   * no per-display status is selected for this display (absent from a
   * populated map). Additive — older UIs keep parsing when this is absent. */
  source_gate?: string | null;
  /**
   * Stable reason the current interval is uniform while sampling is degraded
   * (e.g. `"source_mismatch"`, `"source_unknown"`). Set only when a status
   * belonging to this display reports one. Additive — absent when `None`. */
  uniform_reason?: string | null;
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
  /**
   * Maximum per-cell on-hours in the grid — the denominator the heat map
   * was zero-max-normalized against. `0` when no cell has any recorded
   * exposure (the heat map is then also all-zero). Use this to label
   * the legend with real hours — do NOT infer absolute hours from the
   * normalized `heat` (issue #108: a uniformly-worn panel collapses to
   * a flat grey / zero heat under the old min-max form).
   */
  max_cell_hours: number;
}
