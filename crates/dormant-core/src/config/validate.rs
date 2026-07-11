//! Configuration validation: unknown-key detection, cross-reference checks,
//! credential requirements, and coherence rules.
//!
//! ## Unknown-key strategy
//!
//! serde's `deny_unknown_fields` cannot drive Warn mode (it always errors).
//! Instead, the TOML file is first parsed as a [`toml::Value`] and walked
//! against a grep-stable known-key tree.  Unknown keys are collected; in Strict
//! mode the first one becomes an error, in Warn mode they become
//! [`Warning`](super::schema::Warning)s.
//! The value is then deserialized into [`Config`] without `deny_unknown_fields`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::types::{BlankMode, SensorId, StageKind};
use crate::zone::{ZoneEngine, ZoneSpec};

use super::schema::{Config, Credentials, DisplayConfig, ValidationError};

/// A single unknown-key finding from the TOML tree walk.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownKey {
    /// Dot-separated path from the document root.
    pub key_path: String,
    /// The unrecognized key name.
    pub detail: String,
}

// ── Known-key tree ──────────────────────────────────────────────────────────────

/// Known TOML keys, organized by section depth, for unknown-key detection.
///
/// Each entry is a (`path_prefix`, `allowed_keys`) pair.  The walker visits
/// every key in the TOML tree and checks it against the nearest matching prefix.
///
/// ## Grep-stable invariant
///
/// Every key name in this tree is a literal string — never `format!`-constructed,
/// never macro-generated.  A `grep` for a config key across the codebase will
/// land here.
static KNOWN_KEYS: &[(&str, &[&str])] = &[
    // ── Top-level ───────────────────────────────────────────────────────────
    (
        "",
        &[
            "config_version",
            "daemon",
            "sensors",
            "zones",
            "displays",
            "rules",
            "wear",
            "notifications",
        ],
    ),
    // ── wear ────────────────────────────────────────────────────────────────
    (
        "wear",
        &[
            "enabled",
            "sample_interval",
            "persist_interval",
            "read_timeout",
            "grid_rows",
            "grid_cols",
            "fallback_brightness",
            "screensaver_factor",
            "short_cycle_dwell",
            "advisory_after",
        ],
    ),
    // ── notifications ──────────────────────────────────────────────────────
    (
        "notifications",
        &[
            "enabled",
            "wake_attempt_threshold",
            "cooldown",
            "notify_recovery",
        ],
    ),
    // ── daemon ──────────────────────────────────────────────────────────────
    (
        "daemon",
        &[
            "startup_holdoff",
            "stale_sensor_timeout",
            "log_level",
            "socket_path",
            "idle_time_unit",
            "idle_source",
            "reload_debounce",
            "web_port",
            "web_bind",
            "web_allow_nonloopback",
            "entity_crud_enabled",
            "pairing_enabled",
            "pair_timeout",
        ],
    ),
    // ── sensors.<id> ───────────────────────────────────────────────────────
    (
        "sensors.",
        &[
            "type",
            "kind",
            "hold_time",
            "stale_timeout",
            // mqtt
            "broker_url",
            "topic",
            "field",
            "payload_on",
            "payload_off",
            "availability_topic",
            "availability_payload_online",
            "availability_payload_offline",
            // ha
            "url",
            "entity",
            // usb-ld2410
            "port",
            "baud",
        ],
    ),
    // ── zones.<id> ─────────────────────────────────────────────────────────
    (
        "zones.",
        &[
            "mode",
            "members",
            "quorum",
            "threshold",
            "weights",
            "unavailable_policy",
        ],
    ),
    // ── zones.<id>.weights.<member> ─ (leaf, no sub-keys to check)
    // ── displays.<id> ──────────────────────────────────────────────────────
    (
        "displays.",
        &[
            "controllers",
            "blank_mode",
            "degraded_mode",
            "ladder",
            "screensaver",
            "output",
            "ddc_display",
            "host",
            "wol_mac",
            "blank_command",
            "wake_command",
            "modes",
            "ha_url",
            "blank_service",
            "blank_data",
            "wake_service",
            "wake_data",
            "command_timeout",
            "restore_brightness",
            "samsung_restore_backlight",
            "treat_unreachable_as_blanked",
            "panel_type",
        ],
    ),
    // ── rules.<id> ─────────────────────────────────────────────────────────
    (
        "rules.",
        &[
            "zone",
            "displays",
            "grace_period",
            "min_blank_time",
            "min_wake_time",
            "inhibitors",
            "activity_idle_threshold",
            "activity_poll_interval",
            "wake_retries",
            "wake_retry_backoff",
            "wake_retry_interval",
        ],
    ),
    // ── displays.<id>.ladder (array-of-tables entries) ─────────────────────
    ("displays..ladder", &["kind", "dwell"]),
    // ── displays.<id>.screensaver ─────────────────────────────────────────
    (
        "displays..screensaver",
        &[
            "trigger",
            "audio",
            "source",
            "scale_mode",
            "transition",
            "transition_duration",
            "shift_px",
            "shift_interval",
        ],
    ),
    // ── displays.<id>.screensaver.source (array-of-tables entries) ────────
    (
        "displays..screensaver.source",
        &[
            "path",
            "urls",
            "recurse",
            "shuffle",
            "order",
            "image_duration",
        ],
    ),
];

/// Walk the TOML value tree and collect every key that doesn't match the known
/// key set.
#[must_use]
pub fn collect_unknown_keys(value: &toml::Value) -> Vec<UnknownKey> {
    let mut results = Vec::new();
    walk_toml(value, "", &mut results);
    results
}

/// Recursively walk a TOML value, reporting unknown keys.
fn walk_toml(value: &toml::Value, path: &str, results: &mut Vec<UnknownKey>) {
    let toml::Value::Table(table) = value else {
        return;
    };

    // Determine the allowed-key set for this level.
    let allowed: &[&str] = known_keys_for_path(path);

    for (key, val) in table {
        // Build the full dot-path for this key.
        let key_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };

        // At a collection level (sensors, zones, displays, rules, weights), the
        // key itself is a user-chosen id — not a fixed name.  Only check it
        // against the pattern.
        if is_collection_level(path) || is_weights_level(path) {
            // The key is a dynamic id — don't flag it.
            // Walk deeper with the parent path (not key_path) so child keys
            // match against the collection's allowed set.
            walk_toml(val, &key_path, results);
            continue;
        }

        if !allowed.contains(&key.as_str()) && !is_id_level(allowed.is_empty()) {
            results.push(UnknownKey {
                key_path: key_path.clone(),
                detail: key.clone(),
            });
        }

        // Recurse into sub-tables (but not arrays or scalar values).
        // Skip passthrough data fields (blank_data, wake_data) — their
        // children are arbitrary TOML that should not be checked against the
        // known-key set.
        if val.is_table() && !is_passthrough_data_key(key) {
            walk_toml(val, &key_path, results);
        }

        // Recurse into array-of-tables entries (e.g. [[displays.d.ladder]]
        // and [[displays.d.screensaver.source]]).  Scalar arrays produce
        // non-Table elements which are skipped at the top of walk_toml, so
        // this is safe for all array values.
        if let toml::Value::Array(arr) = val {
            for item in arr {
                walk_toml(item, &key_path, results);
            }
        }
    }
}

/// Return the allowed keys for a given path prefix.
///
/// Paths ending in a dynamic id (e.g. `sensors.desk`) match the nearest
/// collection-level prefix (`sensors.`).
fn known_keys_for_path(path: &str) -> &'static [&'static str] {
    // Try exact match first.
    for (prefix, keys) in KNOWN_KEYS {
        if *prefix == path {
            return keys;
        }
    }
    // Try prefix-with-dot match (dynamic id), e.g. "sensors.desk" matches "sensors.".
    for (prefix, keys) in KNOWN_KEYS {
        if !prefix.is_empty() && path.starts_with(prefix) {
            let remainder = &path[prefix.len()..];
            // Only match if the remainder has no dots (single dynamic id).
            if !remainder.contains('.') {
                return keys;
            }
        }
    }
    // Try double-dot match — the ".." in the prefix stands for a single
    // dynamic-ID segment.  E.g. "displays.mon.ladder" matches "displays..ladder".
    for (prefix, keys) in KNOWN_KEYS {
        if let Some(dd_pos) = prefix.find("..") {
            let before = &prefix[..dd_pos]; // e.g. "displays"
            let after = &prefix[dd_pos + 1..]; // e.g. ".ladder" (includes the leading dot)
            if path.starts_with(before) && path.len() > before.len() {
                let rest = &path[before.len()..]; // e.g. ".mon.ladder"
                // rest must be non-empty and start with '.' (the collection separator).
                if rest.len() > 1 {
                    // Skip the leading dot + the dynamic ID segment (up to the next dot).
                    if let Some(id_end) = rest[1..].find('.') {
                        let after_id = &rest[1 + id_end..]; // e.g. ".ladder"
                        if after_id == after {
                            return keys;
                        }
                    }
                }
            }
        }
    }
    &[]
}

/// Is this path at a collection level where keys are dynamic ids?
/// Is this path at a collection level where keys are dynamic ids?
///
/// The root path (`""`) is NOT a collection level — root keys must be checked
/// against the top-level known set.  Only `sensors`, `zones`, `displays`, and
/// `rules` have dynamic child keys.
fn is_collection_level(path: &str) -> bool {
    path == "sensors" || path == "zones" || path == "displays" || path == "rules"
}

// ── Structural reserved names (P1/P10 single source of truth) ──────────────

/// The single source of truth for every structurally-special TOML key/id
/// name in the schema. The three predicates below (`is_weights_level`,
/// `is_array_of_tables_parent`, `is_passthrough_data_key`) each consume a
/// PER-PREDICATE subset const derived from names in this list — never one
/// shared/blanket loop (P10) — because their match semantics differ
/// (suffix-with-dot vs. exact equality) and a uniform loop would silently
/// broaden `is_passthrough_data_key` from exact-key equality into a suffix
/// match, opening a config-smuggling surface.
///
/// Drift-proofing: `dormant_web`'s `RESERVED_ENTITY_IDS` cross-check
/// (`config_patch.rs`, Task 2) asserts `RESERVED_ENTITY_IDS ⊇
/// STRUCTURAL_RESERVED_NAMES` against this REAL symbol (re-exported via
/// `config/mod.rs`). Within this crate, each predicate's own subset const is
/// pinned ⊆ `STRUCTURAL_RESERVED_NAMES` by an enumerated-by-name test in this
/// module's test suite (`predicate_name_subsets_are_reserved` and friends) —
/// a genuinely NEW 7th predicate needs its own enumerated test line added at
/// write time; this is not automatic/reflective discovery.
pub const STRUCTURAL_RESERVED_NAMES: &[&str] =
    &["weights", "source", "ladder", "blank_data", "wake_data"];

/// Suffix-matched name subset of [`STRUCTURAL_RESERVED_NAMES`] consumed by
/// [`is_weights_level`] — only `"weights"`.
const WEIGHTS_LEVEL_NAMES: &[&str] = &["weights"];

/// Is this path at a weights sub-table where keys are dynamic member ids?
fn is_weights_level(path: &str) -> bool {
    WEIGHTS_LEVEL_NAMES.iter().any(|name| {
        path.strip_suffix(name)
            .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Exact-equality name subset of [`STRUCTURAL_RESERVED_NAMES`] consumed by
/// [`is_passthrough_data_key`] — only `"blank_data"`/`"wake_data"`.
const PASSTHROUGH_DATA_KEY_NAMES: &[&str] = &["blank_data", "wake_data"];

/// Is this a passthrough data key whose children are arbitrary TOML that
/// should not be checked against the known-key set?
///
/// EXACT equality, not a suffix match — `"blank_data_extra"` must NOT match
/// (P10 — a suffix loop here would widen the passthrough surface).
fn is_passthrough_data_key(key: &str) -> bool {
    PASSTHROUGH_DATA_KEY_NAMES.contains(&key)
}

/// Is this one level deeper than an empty allowed set? (deep nesting of unknown
/// parent — don't flag recursively.)
fn is_id_level(empty_allowed: bool) -> bool {
    // If no allowed keys are defined, we're at a depth where every key is a
    // dynamic id (collection member, weight key, etc.) — skip checking.
    empty_allowed
}

// ── Cross-reference validation ─────────────────────────────────────────────────

/// Known inhibitor names for M1.
const VALID_INHIBITORS: &[&str] = &["user-activity", "manual-pause"];

/// Run all cross-reference checks on a loaded configuration.
///
/// `capabilities` maps controller name → supported [`BlankMode`]s.
/// For `"command"` and `"ha-passthrough"` controllers this map is always empty;
/// the display's `modes` list serves as its capability set.
///
/// `creds` is the loaded credentials file (or an empty default).
#[must_use]
#[allow(clippy::implicit_hasher)] // the concrete hasher type is fine for a config API
pub fn validate(
    cfg: &Config,
    capabilities: &HashMap<String, Vec<BlankMode>>,
    creds: &Credentials,
) -> Vec<ValidationError> {
    let mut errors: Vec<ValidationError> = Vec::new();

    // Build the sensor inventory from config keys.
    let sensor_inventory: Vec<SensorId> = cfg.sensors.keys().map(|k| SensorId(k.clone())).collect();
    let sensor_set: HashSet<&str> = cfg.sensors.keys().map(String::as_str).collect();
    let zone_names: HashSet<&str> = cfg.zones.keys().map(String::as_str).collect();

    // ── Zone validation ─────────────────────────────────────────────────
    validate_zones(
        cfg,
        &sensor_set,
        &zone_names,
        &sensor_inventory,
        &mut errors,
    );

    // ── Display validation ───────────────────────────────────────────────
    for (display_id, dc) in &cfg.displays {
        validate_display(display_id, dc, capabilities, creds, &mut errors);
    }

    // ── Rule validation ──────────────────────────────────────────────────
    for (rule_id, rc) in &cfg.rules {
        validate_rule(rule_id, rc, &zone_names, &cfg.displays, &mut errors);
    }

    // ── Cross-reference: ladder on rule-less display ───────────────────
    // A ladder is an auto-escalation sequence that needs a rule to drive
    // it — it could never fire without one.  A rule-less display with
    // just blank_mode is fine (manual-only control).
    let ruled: HashSet<&str> = cfg
        .rules
        .values()
        .flat_map(|r| r.displays.iter().map(String::as_str))
        .collect();
    for (display_id, dc) in &cfg.displays {
        if !dc.ladder.is_empty() && !ruled.contains(display_id.as_str()) {
            errors.push(ValidationError {
                what: "E_CONFIG_INVALID".into(),
                detail: format!(
                    "display '{display_id}' has a ladder but is in no rule; a ladder is an \
                     auto-escalation that needs a rule to drive it — use blank_mode for \
                     manual-only control, or add a rule"
                ),
            });
        }
    }

    // ── Cross-field web-UI validation ───────────────────────────────────
    if !cfg.daemon.web_bind.is_loopback() && !cfg.daemon.web_allow_nonloopback {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "web_bind {} is non-loopback; set web_allow_nonloopback = true to allow (widens unauthenticated surface)",
                cfg.daemon.web_bind
            ),
        });
    }

    // ── [daemon] validation ──────────────────────────────────────────────
    validate_daemon(cfg, &mut errors);

    // ── [wear] validation ────────────────────────────────────────────────
    validate_wear(cfg, &mut errors);

    // ── [notifications] validation ────────────────────────────────────────
    validate_notifications(cfg, &mut errors);

    // ── [sensors.<id>] mqtt availability validation ─────────────────────
    validate_sensors(cfg, &mut errors);

    errors
}

/// `Zigbee2MQTT` convention: `<topic>/availability`.
///
/// This is a local reimplementation of
/// `dormant_sensors::mqtt::availability_topic` — the `dormant-sensors` crate
/// depends on `dormant-core`, not the other way around, so the derivation
/// string form is duplicated here for validation purposes. Keep in sync with
/// the original in `crates/dormant-sensors/src/mqtt.rs`.
fn derive_availability_topic(topic: &str) -> String {
    format!("{topic}/availability")
}

/// Validate the `mqtt`-variant entries of `[sensors.<id>]`: per-sensor
/// literal/topic sanity, and per-broker cross-sensor coherence (an
/// availability topic must not collide with a state topic on the same
/// broker, and sensors sharing one availability topic on the same broker
/// must agree on the online/offline literal pair).
///
/// Broker grouping is reimplemented locally (over `SensorConfig::Mqtt`
/// variants) rather than reusing the `dormant-sensors` registry grouping,
/// for the same crate-layering reason as [`derive_availability_topic`].
fn validate_sensors(cfg: &Config, errors: &mut Vec<ValidationError>) {
    use super::schema::MqttSensorCfg;

    let mut by_broker: HashMap<&str, Vec<(&str, &MqttSensorCfg)>> = HashMap::new();

    for (sensor_id, sc) in &cfg.sensors {
        let super::schema::SensorConfig::Mqtt(m) = sc else {
            continue;
        };

        // ── Per-sensor checks ────────────────────────────────────────────
        if let Some(topic) = &m.availability_topic
            && topic.is_empty()
        {
            errors.push(ValidationError {
                what: crate::error::E_CONFIG_INVALID.into(),
                detail: format!("sensor '{sensor_id}' availability_topic is set but empty"),
            });
        }
        if m.availability_payload_online.is_empty() {
            errors.push(ValidationError {
                what: crate::error::E_CONFIG_INVALID.into(),
                detail: format!(
                    "sensor '{sensor_id}' availability_payload_online must be non-empty"
                ),
            });
        }
        if m.availability_payload_offline.is_empty() {
            errors.push(ValidationError {
                what: crate::error::E_CONFIG_INVALID.into(),
                detail: format!(
                    "sensor '{sensor_id}' availability_payload_offline must be non-empty"
                ),
            });
        }
        if m.availability_payload_online == m.availability_payload_offline {
            errors.push(ValidationError {
                what: crate::error::E_CONFIG_INVALID.into(),
                detail: format!(
                    "sensor '{sensor_id}' availability_payload_online and \
                     availability_payload_offline must differ (both are '{}')",
                    m.availability_payload_online
                ),
            });
        }

        by_broker
            .entry(m.broker_url.as_str())
            .or_default()
            .push((sensor_id.as_str(), m));
    }

    // ── Per-broker cross-sensor checks ──────────────────────────────────
    for (broker, sensors) in &by_broker {
        let state_topics: HashSet<&str> = sensors.iter().map(|(_, m)| m.topic.as_str()).collect();

        // Resolved availability topic -> [(sensor_id, online, offline)].
        let mut avail_map: HashMap<String, Vec<(&str, &str, &str)>> = HashMap::new();

        for (sensor_id, m) in sensors {
            let resolved = m
                .availability_topic
                .clone()
                .unwrap_or_else(|| derive_availability_topic(&m.topic));

            if state_topics.contains(resolved.as_str()) {
                errors.push(ValidationError {
                    what: crate::error::E_CONFIG_INVALID.into(),
                    detail: format!(
                        "sensor '{sensor_id}' availability_topic '{resolved}' collides with \
                         a state topic on broker '{broker}'"
                    ),
                });
            }

            avail_map.entry(resolved).or_default().push((
                sensor_id,
                m.availability_payload_online.as_str(),
                m.availability_payload_offline.as_str(),
            ));
        }

        for (topic, entries) in &avail_map {
            if entries.len() < 2 {
                continue;
            }
            let (first_online, first_offline) = (entries[0].1, entries[0].2);
            for (sensor_id, online, offline) in &entries[1..] {
                if *online != first_online || *offline != first_offline {
                    errors.push(ValidationError {
                        what: crate::error::E_CONFIG_INVALID.into(),
                        detail: format!(
                            "sensor '{sensor_id}' shares availability_topic '{topic}' on broker \
                             '{broker}' with divergent literals (expected online='{first_online}' \
                             offline='{first_offline}')"
                        ),
                    });
                }
            }
        }
    }
}

/// Validate the `[daemon]` section: `pair_timeout` bounds.
fn validate_daemon(cfg: &Config, errors: &mut Vec<ValidationError>) {
    let daemon = &cfg.daemon;

    if daemon.pair_timeout < Duration::from_secs(30)
        || daemon.pair_timeout > Duration::from_secs(300)
    {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "daemon.pair_timeout {:?} is out of range — allowed: 30s..=300s",
                daemon.pair_timeout
            ),
        });
    }
}

/// Validate the `[notifications]` section: threshold floor and cooldown floor.
fn validate_notifications(cfg: &Config, errors: &mut Vec<ValidationError>) {
    let notifications = &cfg.notifications;

    if notifications.wake_attempt_threshold == 0 {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "notifications.wake_attempt_threshold {} is below the minimum of 1",
                notifications.wake_attempt_threshold
            ),
        });
    }

    if notifications.cooldown < Duration::from_secs(60) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "notifications.cooldown {:?} is below the 1m floor",
                notifications.cooldown
            ),
        });
    }
}

/// Validate the `[wear]` section: duration floors, cross-field ordering
/// (`persist_interval` vs `sample_interval`), grid dimension range, and
/// fraction bounds.
#[allow(clippy::too_many_lines)] // one flat list of independent range checks; extracting helpers would scatter the logic
fn validate_wear(cfg: &Config, errors: &mut Vec<ValidationError>) {
    let wear = &cfg.wear;

    if wear.sample_interval < Duration::from_secs(5) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.sample_interval {:?} is below the 5s floor",
                wear.sample_interval
            ),
        });
    }

    if wear.persist_interval < wear.sample_interval {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.persist_interval {:?} must be >= wear.sample_interval {:?}",
                wear.persist_interval, wear.sample_interval
            ),
        });
    }

    if wear.read_timeout < Duration::from_millis(500) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.read_timeout {:?} is below the 500ms floor",
                wear.read_timeout
            ),
        });
    }

    if !(4..=64).contains(&wear.grid_rows) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.grid_rows {} is out of range — allowed: 4..=64",
                wear.grid_rows
            ),
        });
    }

    if !(4..=64).contains(&wear.grid_cols) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.grid_cols {} is out of range — allowed: 4..=64",
                wear.grid_cols
            ),
        });
    }

    if !(0.0..=1.0).contains(&wear.fallback_brightness) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.fallback_brightness {} is out of range — allowed: 0.0..=1.0",
                wear.fallback_brightness
            ),
        });
    }

    if !(0.0..=1.0).contains(&wear.screensaver_factor) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.screensaver_factor {} is out of range — allowed: 0.0..=1.0",
                wear.screensaver_factor
            ),
        });
    }

    if wear.short_cycle_dwell < Duration::from_secs(60) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.short_cycle_dwell {:?} is below the 1m floor",
                wear.short_cycle_dwell
            ),
        });
    }

    if wear.advisory_after < Duration::from_secs(3600) {
        errors.push(ValidationError {
            what: "E_CONFIG_INVALID".into(),
            detail: format!(
                "wear.advisory_after {:?} is below the 1h floor",
                wear.advisory_after
            ),
        });
    }

    // ── Per-display screensaver shift-key range checks ──────────────────
    for (display_id, dc) in &cfg.displays {
        let Some(ss) = &dc.screensaver else {
            continue;
        };
        if ss.shift_px > 8 {
            errors.push(ValidationError {
                what: "E_CONFIG_INVALID".into(),
                detail: format!(
                    "display '{display_id}' screensaver shift_px {} is out of range — \
                     allowed: 0..=8",
                    ss.shift_px
                ),
            });
        }
        if ss.shift_interval < Duration::from_secs(10) {
            errors.push(ValidationError {
                what: "E_CONFIG_INVALID".into(),
                detail: format!(
                    "display '{display_id}' screensaver shift_interval {:?} is below \
                     the 10s floor",
                    ss.shift_interval
                ),
            });
        }
    }
}

/// Validate all zones: member resolution, mode coherence, cycles, empty members.
fn validate_zones(
    cfg: &Config,
    sensor_set: &HashSet<&str>,
    zone_names: &HashSet<&str>,
    sensor_inventory: &[SensorId],
    errors: &mut Vec<ValidationError>,
) {
    // Build ZoneSpecs from zone configs (converting errors to ValidationError).
    let mut specs: Vec<ZoneSpec> = Vec::new();
    for (zone_id, zc) in &cfg.zones {
        // Empty members check.
        if zc.members.is_empty() {
            errors.push(ValidationError {
                what: "empty zone".into(),
                detail: format!("zone '{zone_id}' has no members"),
            });
            // Skip member resolution — can't validate further.
            continue;
        }

        // Resolve each member string.
        let mut member_ok = true;
        for raw in &zc.members {
            if let Some(zone_ref) = raw.strip_prefix("zone:") {
                if !zone_names.contains(zone_ref) {
                    errors.push(ValidationError {
                        what: "unknown zone reference".into(),
                        detail: format!(
                            "zone '{zone_id}' references unknown nested zone '{zone_ref}'"
                        ),
                    });
                    member_ok = false;
                }
            } else if !sensor_set.contains(raw.as_str()) {
                errors.push(ValidationError {
                    what: "unknown sensor reference".into(),
                    detail: format!("zone '{zone_id}' references unknown sensor '{raw}'"),
                });
                member_ok = false;
            }
        }

        if !member_ok {
            // Can't build a valid ZoneSpec — skip further zone validation for
            // this zone.
            continue;
        }

        match zc.to_zone_spec(zone_id) {
            Ok(spec) => specs.push(spec),
            Err(e) => {
                let msg = e.to_string();
                errors.push(ValidationError {
                    what: "invalid zone config".into(),
                    detail: msg
                        .strip_prefix("E_CONFIG_INVALID: ")
                        .unwrap_or(&msg)
                        .to_string(),
                });
            }
        }
    }

    // Use ZoneEngine::new to detect cycles and other construction-time errors.
    if !specs.is_empty() && !cfg.zones.is_empty() {
        match ZoneEngine::new(specs, sensor_inventory) {
            Ok(_) => {} // Construction succeeded.
            Err(e) => {
                let msg = e.to_string();
                let detail = msg
                    .strip_prefix("E_CONFIG_INVALID: ")
                    .or_else(|| msg.strip_prefix("E_ZONE_CYCLE: "))
                    .or_else(|| msg.strip_prefix("E_ZONE_UNKNOWN_MEMBER: "))
                    .unwrap_or(&msg)
                    .to_string();
                errors.push(ValidationError {
                    what: "zone validation error".into(),
                    detail,
                });
            }
        }
    }
}

/// Validate a single display: controllers, modes (against union), per-controller
/// required fields.
#[allow(clippy::too_many_lines)] // per-controller field checks add bulk; extracting helpers would scatter the logic
fn validate_display(
    display_id: &str,
    dc: &DisplayConfig,
    capabilities: &HashMap<String, Vec<BlankMode>>,
    creds: &Credentials,
    errors: &mut Vec<ValidationError>,
) {
    // controllers must be non-empty.
    if dc.controllers.is_empty() {
        errors.push(ValidationError {
            what: "no controllers".into(),
            detail: format!("display '{display_id}' has no controllers"),
        });
        return;
    }

    // Build the union of supported modes across all controllers.
    let mut union_caps: HashSet<BlankMode> = HashSet::new();
    let mut unknown_controllers: Vec<&str> = Vec::new();

    for controller in &dc.controllers {
        match capabilities.get(controller.as_str()) {
            Some(caps) => {
                if controller == "command" || controller == "ha-passthrough" {
                    // For command / ha-passthrough, the display's `modes` list
                    // IS the capability set. An empty list contributes nothing
                    // (treated the same as `None` — the registry-builder
                    // refuses such configs outright, but validate is the
                    // authoritative layer for cross-cutting issues and must
                    // also flag the symptom).
                    if let Some(modes) = &dc.modes {
                        union_caps.extend(modes.iter().copied());
                    }
                } else {
                    union_caps.extend(caps.iter().copied());
                }
            }
            None => {
                unknown_controllers.push(controller);
            }
        }
    }

    for controller in &unknown_controllers {
        errors.push(ValidationError {
            what: "unknown controller".into(),
            detail: format!("display '{display_id}' uses unknown controller '{controller}'"),
        });
    }

    // If the union is empty (no controller — known or otherwise — contributes
    // any mode), the display cannot blank any mode. Surface this as a single
    // operator-facing error rather than also pointing at `blank_mode` and
    // `degraded_mode` as "unsupported" — those would always fail and would
    // duplicate the root cause. Unknown controllers are still reported
    // individually above; this error is the *capability* verdict.
    if union_caps.is_empty() {
        errors.push(ValidationError {
            what: "blank-incapable display".into(),
            detail: format!(
                "display '{display_id}' has no supported blank modes \
                 (controller chain produces an empty capability set)"
            ),
        });
    } else {
        // Mode-capability checks: use the ladder if present, otherwise the
        // desugar path via blank_mode.
        if !dc.ladder.is_empty() {
            // Ladder path: validate each Controller(mode) stage against the
            // union capability set.  Render stages are not checked here —
            // they are validated in the render-stage block below.
            for stage in &dc.ladder {
                if let StageKind::Controller(mode) = stage.kind
                    && !union_caps.contains(&mode)
                {
                    errors.push(ValidationError {
                        what: "unsupported blank mode".into(),
                        detail: format!(
                            "display '{display_id}' ladder stage {mode:?} is not supported \
                             by any controller"
                        ),
                    });
                }
            }
        } else if let Some(bm) = dc.blank_mode {
            // Desugar path — check as before.
            if !union_caps.contains(&bm) {
                errors.push(ValidationError {
                    what: "unsupported blank mode".into(),
                    detail: format!(
                        "display '{display_id}' blank_mode '{bm:?}' is not supported \
                         by any controller"
                    ),
                });
            }
        }

        // Check degraded_mode against the union (only relevant without ladder,
        // per exactly-one-of rule).
        if dc.ladder.is_empty()
            && let Some(dm) = &dc.degraded_mode
            && !union_caps.contains(dm)
        {
            errors.push(ValidationError {
                what: "unsupported degraded mode".into(),
                detail: format!(
                    "display '{display_id}' degraded_mode '{dm:?}' is not supported \
                     by any controller"
                ),
            });
        }
    }

    // ── Render-stage validation (R9 feature gate) ──────────────────────────
    let ladder = dc.normalized_ladder();
    let has_render = ladder.iter().any(|s| s.kind.is_render());

    if has_render {
        // Render-eligibility check: the display must have at least one local
        // controller and must not be composed solely of remote controllers.
        if !dc.is_render_eligible() {
            errors.push(ValidationError {
                what: crate::error::E_RENDER_UNAVAILABLE.into(),
                detail: format!(
                    "display '{display_id}' uses a render stage but has only remote \
                     controllers (render stages require a local output)"
                ),
            });
        }

        // Feature-gate check.
        // Output required for render stages: a wayland layer-shell overlay
        // needs a wl_output connector name.
        if dc.output.is_none() {
            errors.push(ValidationError {
                what: crate::error::E_CONFIG_INVALID.into(),
                detail: format!(
                    "display '{display_id}' has a render stage but no 'output' field — \
                     render stages need the wl_output connector (e.g. output = \"DP-1\")"
                ),
            });
        }

        #[cfg(not(feature = "render"))]
        {
            errors.push(ValidationError {
                what: crate::error::E_RENDER_UNAVAILABLE.into(),
                detail: format!(
                    "display '{display_id}' uses a render stage but dormant was built \
                     without the render feature"
                ),
            });
        }

        // Screensaver source check.
        for stage in &ladder {
            if stage.kind == StageKind::RenderScreensaver {
                let Some(ss) = &dc.screensaver else {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "display '{display_id}' uses a RenderScreensaver stage \
                             but has no [displays.{display_id}.screensaver] section"
                        ),
                    });
                    continue;
                };
                let has_source = ss
                    .source
                    .iter()
                    .any(|s| s.path.is_some() || !s.urls.is_empty());
                if !has_source {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "display '{display_id}' screensaver has no source with \
                             a path or urls"
                        ),
                    });
                }
                // Trigger check.
                if ss.trigger != "vacancy" {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "trigger '{}' not supported — only \"vacancy\" is allowed",
                            ss.trigger
                        ),
                    });
                }
                // scale_mode value check: must be one of {fill, fit, stretch,
                // center} when set.  `None` falls back to Fill at the render
                // layer; this validator only rejects explicit unknown values.
                if let Some(ref sm) = ss.scale_mode
                    && !matches!(sm.as_str(), "fill" | "fit" | "stretch" | "center")
                {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "display '{display_id}' screensaver scale_mode '{sm}' \
                             is unknown — allowed: \"fill\", \"fit\", \"stretch\", \"center\""
                        ),
                    });
                }
                // transition value check: must be one of {crossfade, none}
                // when set.  `None` falls back to Crossfade at the render
                // layer (the production default); this validator only
                // rejects explicit unknown values.
                if let Some(ref tr) = ss.transition
                    && !matches!(tr.as_str(), "crossfade" | "none")
                {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "display '{display_id}' screensaver transition '{tr}' \
                             is unknown — allowed: \"crossfade\", \"none\""
                        ),
                    });
                }
                // transition_duration bounds: when set, must be in
                // [100 ms, 10 s].  Long blurs lose the visual cue that the
                // playlist is moving; very short blurs visibly skip.  The
                // hard upper bound also caps the worst-case blend work per
                // item switch (timer ticks * frame size).
                if let Some(d) = ss.transition_duration
                    && (d < Duration::from_millis(100) || d > Duration::from_secs(10))
                {
                    errors.push(ValidationError {
                        what: crate::error::E_SCREENSAVER_SOURCE.into(),
                        detail: format!(
                            "display '{display_id}' screensaver transition_duration \
                             {d:?} is out of range — allowed: 100ms..=10s"
                        ),
                    });
                }

                // Per-source checks.
                for (i, src) in ss.source.iter().enumerate() {
                    // Source must have exactly one of path or non-empty
                    // urls — not both AND not neither.
                    if src.path.is_none() && src.urls.is_empty() {
                        errors.push(ValidationError {
                            what: crate::error::E_SCREENSAVER_SOURCE.into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} has neither \
                                 path nor urls — each source needs exactly one"
                            ),
                        });
                    }
                    if src.path.is_some() && !src.urls.is_empty() {
                        errors.push(ValidationError {
                            what: crate::error::E_SCREENSAVER_SOURCE.into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} sets both \
                                 path and urls — pick exactly one"
                            ),
                        });
                    }
                    // shuffle + order exactly-one: validation guarantees they
                    // are never both set, so playlist.rs never hits the
                    // shuffle-wins branch at runtime.
                    if src.shuffle && src.order.is_some() {
                        errors.push(ValidationError {
                            what: crate::error::E_SCREENSAVER_SOURCE.into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} sets both \
                                 shuffle and order — pick exactly one"
                            ),
                        });
                    }
                    // order must be a known value.
                    if let Some(ref ord) = src.order
                        && !matches!(ord.as_str(), "sequential")
                    {
                        errors.push(ValidationError {
                            what: crate::error::E_SCREENSAVER_SOURCE.into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} \
                                 order '{ord}' is unknown — allowed: \"sequential\""
                            ),
                        });
                    }
                    // image_duration must be non-zero when set.
                    if let Some(d) = src.image_duration
                        && d.as_secs() == 0
                    {
                        errors.push(ValidationError {
                            what: "invalid duration".into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} \
                                 image_duration must be > 0"
                            ),
                        });
                    }
                }
            }
        }
    }

    // ── Dwell rules ────────────────────────────────────────────────────────
    // Every non-terminal stage must have a dwell.  The terminal stage's dwell
    // is ignored (optional — warn-worthy but not an error here).
    let stage_count = ladder.len();
    for (i, stage) in ladder.iter().enumerate() {
        let is_terminal = i + 1 == stage_count;
        if !is_terminal && stage.dwell.is_none() {
            errors.push(ValidationError {
                what: "missing dwell".into(),
                detail: format!(
                    "non-terminal ladder stage ({i}) for display '{display_id}' needs dwell"
                ),
            });
        }
    }

    // Empty ladder explicitly set is invalid.
    if !ladder.is_empty() && dc.ladder.is_empty() && dc.blank_mode.is_none() {
        // This case is caught by the exactly-one-of check in load_config,
        // but guard here as well for programmatic callers.
        errors.push(ValidationError {
            what: "empty ladder".into(),
            detail: format!(
                "display '{display_id}' has an empty ladder — either set ladder stages \
                 or use blank_mode"
            ),
        });
    }

    // Per-controller required field checks (still per-controller).
    for controller in &dc.controllers {
        match controller.as_str() {
            "samsung-tizen" => {
                if dc.host.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses samsung-tizen but has no 'host' field"
                        ),
                    });
                }
                // Check credentials for this host.
                if let Some(host) = &dc.host
                    && !creds.samsung.contains_key(host.as_str())
                {
                    errors.push(ValidationError {
                        what: "missing credential".into(),
                        detail: format!(
                            "display '{display_id}' (host '{host}') needs a samsung token in credentials"
                        ),
                    });
                }
            }
            "ha-passthrough" => {
                if dc.ha_url.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses ha-passthrough but has no 'ha_url' field"
                        ),
                    });
                }
                if dc.blank_service.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses ha-passthrough but has no 'blank_service' field"
                        ),
                    });
                }
                if dc.wake_service.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses ha-passthrough but has no 'wake_service' field"
                        ),
                    });
                }
                // An empty `modes` list is treated as missing — a config-validity
                // concern, not a missing-field one, so the registry builder
                // and the per-controller check agree on the wording.
                if dc.modes.as_ref().is_none_or(Vec::is_empty) {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses ha-passthrough but has no 'modes' (or modes is empty)"
                        ),
                    });
                }
                if creds.ha_token.is_none() {
                    errors.push(ValidationError {
                        what: "missing credential".into(),
                        detail: format!(
                            "display '{display_id}' uses ha-passthrough but no ha_token is configured"
                        ),
                    });
                }
            }
            "command" => {
                if dc.blank_command.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses command but has no 'blank_command' field"
                        ),
                    });
                }
                if dc.wake_command.is_none() {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses command but has no 'wake_command' field"
                        ),
                    });
                }
                if dc.modes.as_ref().is_none_or(Vec::is_empty) {
                    errors.push(ValidationError {
                        what: "missing field".into(),
                        detail: format!(
                            "display '{display_id}' uses command but has no 'modes' (or modes is empty)"
                        ),
                    });
                }
            }
            // kwin-dpms and ddcci have no required fields beyond the defaults.
            _ => {}
        }
    }

    // ── Restore-level guards ────────────────────────────────────────────
    // A zero restore level would wake to a dark panel (fail-toward-visible
    // requires at least 1). Reject here so the operator finds out at
    // config-validate time, not at the first wake after a restart.
    if dc.samsung_restore_backlight == 0 {
        errors.push(ValidationError {
            what: "invalid samsung_restore_backlight".into(),
            detail: format!(
                "display '{display_id}' samsung_restore_backlight is 0 — \
                 a zero restore level would wake to a dark panel; minimum is 1"
            ),
        });
    } else if dc.samsung_restore_backlight > 50 {
        errors.push(ValidationError {
            what: "invalid samsung_restore_backlight".into(),
            detail: format!(
                "display '{display_id}' samsung_restore_backlight {} is out of \
                 range — allowed: 1..=50 (Samsung IP-Control backlight scale)",
                dc.samsung_restore_backlight
            ),
        });
    }

    if dc.restore_brightness == 0 {
        errors.push(ValidationError {
            what: "invalid restore_brightness".into(),
            detail: format!(
                "display '{display_id}' restore_brightness is 0 — \
                 a zero restore level would wake to a dark panel; minimum is 1"
            ),
        });
    }
}

/// Validate a single rule: zone exists, displays exist, valid inhibitors, sane
/// durations.
fn validate_rule(
    rule_id: &str,
    rc: &super::schema::RuleConfig,
    zone_names: &HashSet<&str>,
    displays: &indexmap::IndexMap<String, super::schema::DisplayConfig>,
    errors: &mut Vec<ValidationError>,
) {
    if !zone_names.contains(rc.zone.as_str()) {
        errors.push(ValidationError {
            what: "rule references unknown zone".into(),
            detail: format!("rule '{rule_id}' references unknown zone '{}'", rc.zone),
        });
    }

    for display in &rc.displays {
        if !displays.contains_key(display.as_str()) {
            errors.push(ValidationError {
                what: "rule references unknown display".into(),
                detail: format!("rule '{rule_id}' references unknown display '{display}'"),
            });
        }
    }

    // Valid inhibitor names.
    for inhibitor in &rc.inhibitors {
        if !VALID_INHIBITORS.contains(&inhibitor.as_str()) {
            errors.push(ValidationError {
                what: "unknown inhibitor".into(),
                detail: format!(
                    "rule '{rule_id}' uses unknown inhibitor '{inhibitor}' (valid: {VALID_INHIBITORS:?})"
                ),
            });
        }
    }

    // Duration sanity.
    if rc.wake_retry_interval.as_secs() == 0 {
        errors.push(ValidationError {
            what: "invalid duration".into(),
            detail: format!("rule '{rule_id}' wake_retry_interval must be > 0"),
        });
    }
    if rc.activity_poll_interval.as_secs() == 0 {
        errors.push(ValidationError {
            what: "invalid duration".into(),
            detail: format!("rule '{rule_id}' activity_poll_interval must be > 0"),
        });
    }
}

// ── Known-config-path accessor ─────────────────────────────────────────────────

/// Check whether `path` is structurally valid per the known-key tree.
///
/// Collection levels (`sensors`, `zones`, `displays`, `rules`) accept any
/// dynamic-ID segment.  Array-of-tables keys (`source`, `ladder`) accept
/// an all-digit index segment before their child keys.  Empty paths return
/// `false`.
///
/// This is a pure structural check — editability is handled by a separate
/// allowlist (Task 2).
#[must_use]
pub fn is_known_config_path(path: &[&str]) -> bool {
    if path.is_empty() {
        return false;
    }
    check_valid("", path)
}

/// Does `parent` end with an array-of-tables key whose entries carry
/// child keys?  Currently `ladder` (under `displays.<id>.ladder`) and
/// `source` (under `displays.<id>.screensaver.source`) are the only
/// array-of-tables keys in the M1 schema.
///
/// Kept next to [`KNOWN_KEYS`] so the name list is grep-stable and
/// trivially auditable.
///
/// Suffix-matched name subset of [`STRUCTURAL_RESERVED_NAMES`] consumed
/// here — only `"ladder"`/`"source"` (P10 — its own const, not shared with
/// [`is_weights_level`] or [`is_passthrough_data_key`]).
const ARRAY_OF_TABLES_PARENT_NAMES: &[&str] = &["ladder", "source"];

fn is_array_of_tables_parent(parent: &str) -> bool {
    ARRAY_OF_TABLES_PARENT_NAMES.iter().any(|name| {
        parent
            .strip_suffix(name)
            .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Recursive helper: `parent` is the dot-joined path consumed so far;
/// `remaining` is the path tail to validate.
fn check_valid(parent: &str, remaining: &[&str]) -> bool {
    if remaining.is_empty() {
        // All segments were consumed — the path is valid.
        return true;
    }

    let segment = remaining[0];
    let rest = &remaining[1..];
    let allowed = known_keys_for_path(parent);

    // Build the next parent path.
    let next_parent = if parent.is_empty() {
        segment.to_string()
    } else {
        format!("{parent}.{segment}")
    };

    // 1. Segment is a literal known key at this level.
    if allowed.contains(&segment) && check_valid(&next_parent, rest) {
        return true;
    }

    // 2. At a collection or weights level — any segment is a valid dynamic id.
    if (is_collection_level(parent) || is_weights_level(parent)) && check_valid(&next_parent, rest)
    {
        return true;
    }

    // 3. Segment is all-digits after an array-of-tables key — treat as an
    //    index.  Skip it (parent stays the same) and continue matching child
    //    keys.  The guard ensures this ONLY fires for source/ladder, not for
    //    arbitrary numeric segments under non-array tables.
    if is_array_of_tables_parent(parent)
        && segment.chars().all(|c| c.is_ascii_digit())
        && !segment.is_empty()
        && !rest.is_empty()
        && check_valid(parent, rest)
    {
        return true;
    }

    false
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;
    use crate::config::Strictness;
    use crate::config::Warning;
    use crate::config::schema::{RuleConfig, ZoneConfig};
    use crate::types::BlankMode;
    use indexmap::IndexMap;
    use std::path::Path;

    // ── Unknown-key detection ──────────────────────────────────────────────

    #[test]
    fn collect_unknown_keys_finds_typo_in_rule() {
        let toml_str = r#"
config_version = 1

[rules.office]
zone = "office"
gracee_period = "60s"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(!unknown.is_empty());
        assert!(
            unknown.iter().any(|u| u.key_path.contains("gracee_period")),
            "expected gracee_period to be flagged, got {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_unknown_keys_accepts_valid_config() {
        let toml_str = include_str!("../../tests/fixtures/config/valid_full.toml");
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.is_empty(),
            "expected no unknown keys, got {:?}",
            unknown
        );
    }

    #[test]
    fn collect_unknown_keys_accepts_scale_mode_in_screensaver() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
scale_mode = "fill"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.is_empty(),
            "scale_mode must be a known key under displays..screensaver, \
             got unknown: {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_unknown_keys_flags_typo_in_screensaver_scale_mode() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
scalee_mode = "fill"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.iter().any(|u| u.key_path.contains("scalee_mode")),
            "expected scalee_mode to be flagged under the screensaver path, got {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>()
        );
    }

    // ── load_config ────────────────────────────────────────────────────────

    #[test]
    fn load_config_strict_rejects_unknown_key() {
        let dir = std::env::temp_dir().join("dormant-test-load-config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unknown_key.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1

[rules.office]
zone = "office"
displays = ["main_monitor"]
gracee_period = "60s"
"#,
        )
        .unwrap();

        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("gracee_period"),
            "expected error mentioning gracee_period, got: {err}"
        );
    }

    #[test]
    fn load_config_warn_collects_unknown_keys() {
        let dir = std::env::temp_dir().join("dormant-test-load-warn");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unknown_key.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1

[rules.office]
zone = "office"
displays = ["main_monitor"]
gracee_period = "60s"
"#,
        )
        .unwrap();

        let result = crate::config::load_config(&path, Strictness::Warn).unwrap();
        assert_eq!(result.1.len(), 1);
        assert!(result.1[0].key_path.contains("gracee_period"));
    }

    #[test]
    fn load_config_rejects_version_2() {
        let dir = std::env::temp_dir().join("dormant-test-version");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("version2.toml");
        std::fs::write(&path, "config_version = 2\n").unwrap();

        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported config_version")
        );
    }

    // ── load_credentials ──────────────────────────────────────────────────

    #[test]
    fn load_credentials_missing_file_returns_default() {
        let path = Path::new("/tmp/dormant-nonexistent-creds.toml");
        let creds = crate::config::load_credentials(path).unwrap();
        assert!(creds.ha_token.is_none());
        assert!(creds.samsung.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn load_credentials_rejects_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("dormant-test-creds-perms");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let result = crate::config::load_credentials(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsafe permissions")
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_credentials_accepts_600_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("dormant-test-creds-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        std::fs::write(&path, "ha_token = \"test-token\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let creds = crate::config::load_credentials(&path).unwrap();
        assert_eq!(creds.ha_token.as_deref(), Some("test-token"));
    }

    // ── validate ───────────────────────────────────────────────────────────

    fn test_capabilities() -> HashMap<String, Vec<BlankMode>> {
        HashMap::from([
            ("kwin-dpms".into(), vec![BlankMode::PowerOff]),
            (
                "ddcci".into(),
                vec![BlankMode::BrightnessZero, BlankMode::PowerOff],
            ),
            (
                "samsung-tizen".into(),
                vec![
                    BlankMode::ScreenOffAudioOn,
                    BlankMode::BrightnessZero,
                    BlankMode::PowerOff,
                ],
            ),
            ("ha-passthrough".into(), vec![]),
            ("command".into(), vec![]),
        ])
    }

    fn test_creds() -> Credentials {
        Credentials {
            ha_token: Some("test-ha-token".into()),
            samsung: IndexMap::from([("192.168.1.50".into(), "test-samsung-token".into())]),
            mqtt: IndexMap::new(),
        }
    }

    #[test]
    fn validate_accepts_valid_full_config() {
        let cfg = valid_full_config();
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.is_empty(),
            "expected no errors, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_detects_unsupported_blank_mode() {
        let mut cfg = valid_full_config();
        // Change tv to use BrightnessZero with a controller chain that
        // doesn't support it.  samsung-tizen now supports BrightnessZero,
        // so we point tv at kwin-dpms (which only supports PowerOff).
        cfg.displays.get_mut("tv").unwrap().controllers = vec!["kwin-dpms".into()];
        cfg.displays.get_mut("tv").unwrap().blank_mode = Some(BlankMode::BrightnessZero);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| e.what == "unsupported blank mode"),
            "expected unsupported blank mode error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_zone_cycle() {
        let mut cfg = valid_full_config();
        // Create a cycle: zone a → zone b → zone a
        cfg.zones.insert(
            "a".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec!["zone:b".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: crate::zone::UnavailablePolicy::Present,
            },
        );
        cfg.zones.insert(
            "b".into(),
            ZoneConfig {
                mode: "any".into(),
                members: vec!["zone:a".into()],
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: crate::zone::UnavailablePolicy::Present,
            },
        );
        // Remove other zones to avoid unrelated errors.
        cfg.zones.shift_remove("office");
        cfg.zones.shift_remove("media");
        cfg.zones.shift_remove("nested");

        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| e.detail.contains("cycle")),
            "expected cycle error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_rule_with_unknown_display() {
        let mut cfg = valid_full_config();
        cfg.rules.get_mut("office_blank").unwrap().displays = vec!["nonexistent".into()];
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == "rule references unknown display"),
            "expected unknown display error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_missing_samsung_credential() {
        let cfg = valid_full_config();
        // Remove the samsung credential for the tv's host.
        let creds = Credentials {
            ha_token: Some("test-ha-token".into()),
            samsung: IndexMap::new(), // empty
            mqtt: IndexMap::new(),
        };
        let errors = validate(&cfg, &test_capabilities(), &creds);
        assert!(
            errors
                .iter()
                .any(|e| e.what == "missing credential" && e.detail.contains("192.168.1.50")),
            "expected missing credential error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_quorum_without_quorum_key() {
        let mut cfg = valid_full_config();
        cfg.zones.get_mut("office").unwrap().mode = "quorum".into();
        // quorum field is None.
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| e.detail.contains("quorum")),
            "expected quorum error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_unknown_inhibitor() {
        let mut cfg = valid_full_config();
        cfg.rules.get_mut("office_blank").unwrap().inhibitors = vec!["bogus-inhibitor".into()];
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| e.what == "unknown inhibitor"),
            "expected unknown inhibitor error, got: {:?}",
            errors
        );
    }

    #[test]
    fn validate_detects_empty_zone_members() {
        let mut cfg = valid_full_config();
        cfg.zones.get_mut("office").unwrap().members = vec![];
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| e.what == "empty zone"),
            "expected empty zone error, got: {:?}",
            errors
        );
    }

    // ── Screensaver scale_mode validation ────────────────────────────────

    use std::time::Duration;

    use crate::config::schema::{DisplayConfig, ScreensaverConfig, ScreensaverSource};
    use crate::types::{LadderStage, StageKind};

    // ── Ladder on rule-less display ─────────────────────────────────

    #[test]
    fn validate_rejects_ladder_on_ruleless_display() {
        let mut cfg = valid_full_config();
        let ladder = vec![LadderStage {
            kind: StageKind::Controller(BlankMode::PowerOff),
            dwell: Some(Duration::from_secs(5)),
        }];
        // Display with a ladder but NOT referenced by any rule.
        cfg.displays.insert(
            "orphaned".into(),
            DisplayConfig {
                ladder,
                controllers: vec!["ddcci".into()],
                ..base_display_cfg()
            },
        );
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let ladder_errors: Vec<_> = errors
            .iter()
            .filter(|e| {
                e.what == "E_CONFIG_INVALID"
                    && e.detail.contains("orphaned")
                    && e.detail.contains("ladder")
            })
            .collect();
        assert_eq!(
            ladder_errors.len(),
            1,
            "expected exactly one ladder-without-rule error, got: {:?}",
            errors
                .iter()
                .map(|e| format!("{}: {}", e.what, e.detail))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_accepts_ruleless_display_with_blank_mode() {
        let mut cfg = valid_full_config();
        // Display with blank_mode, empty ladder, not in any rule — valid manual-only.
        cfg.displays.insert(
            "manual_only".into(),
            DisplayConfig {
                blank_mode: Some(BlankMode::PowerOff),
                ladder: vec![],
                controllers: vec!["ddcci".into()],
                ..base_display_cfg()
            },
        );
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let manual_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.detail.contains("manual_only") && e.detail.contains("ladder"))
            .collect();
        assert!(
            manual_errors.is_empty(),
            "expected no ladder-without-rule error for manual_only, got: {:?}",
            manual_errors.iter().map(|e| &e.detail).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_accepts_ruleful_display_with_ladder() {
        let mut cfg = valid_full_config();
        let ladder = vec![LadderStage {
            kind: StageKind::Controller(BlankMode::PowerOff),
            dwell: Some(Duration::from_secs(5)),
        }];
        // Display with ladder AND a rule referencing it.
        cfg.displays.insert(
            "office_tv".into(),
            DisplayConfig {
                ladder,
                controllers: vec!["ddcci".into()],
                ..base_display_cfg()
            },
        );
        cfg.rules.insert(
            "office_tv_rule".into(),
            RuleConfig {
                zone: "office".into(),
                displays: vec!["office_tv".into()],
                grace_period: Duration::from_secs(60),
                min_blank_time: Duration::from_secs(1),
                min_wake_time: Duration::from_secs(1),
                inhibitors: vec![],
                activity_idle_threshold: Duration::from_secs(300),
                activity_poll_interval: Duration::from_secs(5),
                wake_retries: 3,
                wake_retry_backoff: Duration::from_secs(1),
                wake_retry_interval: Duration::from_secs(2),
            },
        );
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let tv_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.detail.contains("office_tv") && e.detail.contains("ladder"))
            .collect();
        assert!(
            tv_errors.is_empty(),
            "expected no ladder-without-rule error for office_tv, got: {:?}",
            tv_errors.iter().map(|e| &e.detail).collect::<Vec<_>>()
        );
    }

    /// Build a render-eligible display with a `[controller, render_screensaver]`
    /// ladder and the given `scale_mode` value.  Used by the `scale_mode`
    /// validation tests; bypasses TOML parsing for clarity.
    fn display_with_scale_mode(scale_mode: Option<&str>) -> DisplayConfig {
        DisplayConfig {
            controllers: vec!["kwin-dpms".into()],
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![
                LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: Some(Duration::from_secs(5)),
                },
                LadderStage {
                    kind: StageKind::RenderScreensaver,
                    dwell: Some(Duration::from_secs(10)),
                },
            ],
            screensaver: Some(ScreensaverConfig {
                trigger: "vacancy".into(),
                audio: false,
                source: vec![ScreensaverSource {
                    path: Some("/tmp/img.png".into()),
                    urls: Vec::new(),
                    recurse: false,
                    shuffle: false,
                    order: None,
                    image_duration: None,
                }],
                scale_mode: scale_mode.map(str::to_string),
                transition: None,
                transition_duration: None,
                shift_px: crate::config::defaults::SHIFT_PX,
                shift_interval: crate::config::defaults::SHIFT_INTERVAL,
            }),
            output: Some("DP-1".into()),
            ..base_display_cfg()
        }
    }

    /// Minimal `DisplayConfig` "base" with sensible defaults for the non-screensaver
    /// fields.  Override fields via struct-update syntax (`..base_display_cfg()`)
    /// and the rest is filled in.
    fn base_display_cfg() -> DisplayConfig {
        use crate::config::defaults;
        DisplayConfig {
            controllers: Vec::new(),
            blank_mode: None,
            degraded_mode: None,
            ladder: Vec::new(),
            screensaver: None,
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: defaults::COMMAND_TIMEOUT,
            restore_brightness: defaults::RESTORE_BRIGHTNESS,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: crate::wear::PanelType::default(),
        }
    }

    fn config_with_scale_mode(scale_mode: Option<&str>) -> super::super::schema::Config {
        super::super::schema::Config {
            config_version: 1,
            daemon: crate::config::DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d1".into(), display_with_scale_mode(scale_mode))]),
            rules: IndexMap::new(),
            wear: super::super::schema::WearConfig::default(),
            notifications: super::super::schema::NotificationsConfig::default(),
        }
    }

    #[test]
    fn validate_accepts_all_four_scale_modes() {
        for sm in ["fill", "fit", "stretch", "center"] {
            let cfg = config_with_scale_mode(Some(sm));
            let errors = validate(&cfg, &test_capabilities(), &test_creds());
            let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
            assert!(
                !err_msgs.iter().any(|m| m.contains("scale_mode")),
                "scale_mode = '{sm}' must be accepted, got errors: {err_msgs:?}"
            );
        }
    }

    #[test]
    fn validate_accepts_absent_scale_mode() {
        let cfg = config_with_scale_mode(None);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            !err_msgs.iter().any(|m| m.contains("scale_mode")),
            "absent scale_mode must be accepted (renders as Fill), got: {err_msgs:?}"
        );
    }

    #[test]
    fn validate_rejects_unknown_scale_mode() {
        let cfg = config_with_scale_mode(Some("zoom"));
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("scale_mode 'zoom'")
                    && e.detail.contains("fill")
                    && e.detail.contains("fit")
                    && e.detail.contains("stretch")
                    && e.detail.contains("center")),
            "expected E_SCREENSAVER_SOURCE error for unknown scale_mode 'zoom' \
             naming the allowed set, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_rejects_miscased_scale_mode() {
        // Case-sensitive parsing — wrong-cased values are unknown.
        let cfg = config_with_scale_mode(Some("Fill"));
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("scale_mode 'Fill'")),
            "wrong-cased scale_mode 'Fill' must be rejected, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    // ── transition validation ────────────────────────────────────────────

    /// Build a render-eligible display with a `[controller, render_screensaver]`
    /// ladder and the given transition / duration.  Used by the transition
    /// tests; bypasses TOML parsing for clarity.
    fn display_with_transition(
        transition: Option<&str>,
        transition_duration: Option<Duration>,
    ) -> DisplayConfig {
        DisplayConfig {
            controllers: vec!["kwin-dpms".into()],
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![
                LadderStage {
                    kind: StageKind::Controller(BlankMode::PowerOff),
                    dwell: Some(Duration::from_secs(5)),
                },
                LadderStage {
                    kind: StageKind::RenderScreensaver,
                    dwell: Some(Duration::from_secs(10)),
                },
            ],
            screensaver: Some(ScreensaverConfig {
                trigger: "vacancy".into(),
                audio: false,
                source: vec![ScreensaverSource {
                    path: Some("/tmp/img.png".into()),
                    urls: Vec::new(),
                    recurse: false,
                    shuffle: false,
                    order: None,
                    image_duration: None,
                }],
                scale_mode: None,
                transition: transition.map(str::to_string),
                transition_duration,
                shift_px: crate::config::defaults::SHIFT_PX,
                shift_interval: crate::config::defaults::SHIFT_INTERVAL,
            }),
            output: Some("DP-1".into()),
            ..base_display_cfg()
        }
    }

    fn config_with_transition(
        transition: Option<&str>,
        transition_duration: Option<Duration>,
    ) -> super::super::schema::Config {
        super::super::schema::Config {
            config_version: 1,
            daemon: crate::config::DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([(
                "d1".into(),
                display_with_transition(transition, transition_duration),
            )]),
            rules: IndexMap::new(),
            wear: super::super::schema::WearConfig::default(),
            notifications: super::super::schema::NotificationsConfig::default(),
        }
    }

    #[test]
    fn validate_accepts_both_transition_values() {
        // Both canonical values must be accepted — the validation gate
        // exists ONLY to reject unknown strings.
        for tr in ["crossfade", "none"] {
            let cfg = config_with_transition(Some(tr), None);
            let errors = validate(&cfg, &test_capabilities(), &test_creds());
            let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
            assert!(
                !err_msgs.iter().any(|m| m.contains("transition")),
                "transition = '{tr}' must be accepted, got errors: {err_msgs:?}"
            );
        }
    }

    #[test]
    fn validate_accepts_absent_transition() {
        let cfg = config_with_transition(None, None);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            !err_msgs.iter().any(|m| m.contains("transition")),
            "absent transition must be accepted (renders as Crossfade), \
             got: {err_msgs:?}"
        );
    }

    #[test]
    fn validate_rejects_unknown_transition() {
        let cfg = config_with_transition(Some("dissolve"), None);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("transition 'dissolve'")
                    && e.detail.contains("crossfade")
                    && e.detail.contains("none")),
            "expected E_SCREENSAVER_SOURCE error for unknown transition 'dissolve' \
             naming the allowed set, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_rejects_miscased_transition() {
        let cfg = config_with_transition(Some("Crossfade"), None);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("transition 'Crossfade'")),
            "wrong-cased transition 'Crossfade' must be rejected, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    // ── transition_duration bounds ───────────────────────────────────────

    #[test]
    fn validate_accepts_transition_duration_within_bounds() {
        // 100 ms (lower edge) and 10 s (upper edge) plus two interior
        // values — every value in the [100ms, 10s] range must pass.
        let cases = [
            Duration::from_millis(100),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(10),
        ];
        for d in cases {
            let cfg = config_with_transition(Some("crossfade"), Some(d));
            let errors = validate(&cfg, &test_capabilities(), &test_creds());
            let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
            assert!(
                !err_msgs.iter().any(|m| m.contains("transition_duration")),
                "transition_duration = {d:?} must be accepted, got errors: {err_msgs:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_transition_duration_below_minimum() {
        // 50 ms is below the 100 ms floor — must be rejected with a
        // message that names the bound.
        let cfg = config_with_transition(Some("crossfade"), Some(Duration::from_millis(50)));
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| {
                e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("transition_duration")
                    && e.detail.contains("out of range")
                    && e.detail.contains("100ms")
                    && e.detail.contains("10s")
            }),
            "50ms transition_duration must be rejected as out of range, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_rejects_transition_duration_above_maximum() {
        // 11 s is above the 10 s ceiling — must be rejected with a
        // message that names the bound.
        let cfg = config_with_transition(Some("crossfade"), Some(Duration::from_secs(11)));
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors.iter().any(|e| {
                e.what == crate::error::E_SCREENSAVER_SOURCE
                    && e.detail.contains("transition_duration")
                    && e.detail.contains("out of range")
                    && e.detail.contains("100ms")
                    && e.detail.contains("10s")
            }),
            "11s transition_duration must be rejected as out of range, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_accepts_absent_transition_duration() {
        // Absent → None → render layer defaults to 1 s; validator
        // must accept it without flagging duration bounds.
        let cfg = config_with_transition(Some("crossfade"), None);
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let err_msgs: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            !err_msgs.iter().any(|m| m.contains("transition_duration")),
            "absent transition_duration must be accepted (renders as 1s default), \
             got: {err_msgs:?}"
        );
    }

    // ── unknown-key tree includes transition / transition_duration ──────

    #[test]
    fn collect_unknown_keys_accepts_transition_in_screensaver() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
transition = "crossfade"
transition_duration = "500ms"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.is_empty(),
            "transition + transition_duration must be known keys under \
             displays..screensaver, got unknown: {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_unknown_keys_flags_typo_in_screensaver_transition() {
        let toml_str = r#"
config_version = 1

[displays.d1]
controllers = ["kwin-dpms"]
blank_mode = "power_off"

[displays.d1.screensaver]
trigger = "vacancy"
transish = "crossfade"
[[displays.d1.screensaver.source]]
path = "/tmp/img.png"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.iter().any(|u| u.key_path.contains("transish")),
            "expected transish to be flagged under the screensaver path, got {:?}",
            unknown.iter().map(|u| &u.key_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn top_level_unknown_key_rejected() {
        let toml_str = r"
config_version = 1
typo_top = true
";
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(!unknown.is_empty());
        assert!(
            unknown.iter().any(|u| u.key_path == "typo_top"),
            "expected typo_top to be flagged, got {:?}",
            unknown
        );
    }

    #[test]
    fn passthrough_data_subkeys_not_flagged() {
        let toml_str = r#"
config_version = 1

[displays.tv]
controllers = ["ha-passthrough"]
blank_mode = "power_off"
ha_url = "http://ha.local"
blank_service = "switch.turn_off"
wake_service = "switch.turn_on"
modes = ["power_off"]

[displays.tv.wake_data]
entity_id = "switch.tv_power"
brightness = 255
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.is_empty(),
            "expected no unknown keys for passthrough data, got {:?}",
            unknown
        );
    }

    #[test]
    fn hold_time_humantime_parse() {
        let toml_str = r#"
config_version = 1

[sensors.radar]
type = "usb-ld2410"
port = "/dev/ttyUSB0"
hold_time = "5s"

[sensors.desk]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "sensors/desk"
stale_timeout = "5m"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let crate::config::schema::SensorConfig::UsbLd2410(radar) = &cfg.sensors["radar"] else {
            panic!("expected UsbLd2410")
        };
        assert_eq!(radar.hold_time, Some(std::time::Duration::from_secs(5)));

        let crate::config::schema::SensorConfig::Mqtt(desk) = &cfg.sensors["desk"] else {
            panic!("expected Mqtt")
        };
        assert_eq!(
            desk.stale_timeout,
            Some(std::time::Duration::from_secs(300))
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_mode_0700_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("dormant-test-creds-0700");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();

        let result = crate::config::load_credentials(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsafe permissions")
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_parses_mqtt_section() {
        // TOML inline table syntax for [mqtt."<url>"] sections.
        let dir = std::env::temp_dir().join("dormant-test-creds-mqtt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        let toml_content = r#"
[mqtt."mqtt://192.0.2.5:1883"]
username = "icetea"
password = "test-pass"
"#;
        std::fs::write(&path, toml_content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let creds = crate::config::load_credentials(&path).unwrap();
        let mqtt_cred = creds
            .mqtt
            .get("mqtt://192.0.2.5:1883")
            .expect("mqtt creds not found");
        assert_eq!(mqtt_cred.username, "icetea");
        assert_eq!(mqtt_cred.password, "test-pass");
    }

    #[test]
    fn credentials_no_mqtt_section_parses_empty_map() {
        // Back-compat: existing creds files without [mqtt] parse fine.
        let dir = std::env::temp_dir().join("dormant-test-creds-no-mqtt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        std::fs::write(&path, "ha_token = \"abc\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let creds = crate::config::load_credentials(&path).unwrap();
        assert_eq!(creds.ha_token.as_deref(), Some("abc"));
        assert!(
            creds.mqtt.is_empty(),
            "mqtt map should be empty when absent"
        );
    }

    #[test]
    fn credentials_mode_0400_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("dormant-test-creds-0400");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

        let result = crate::config::load_credentials(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsafe permissions")
        );
    }

    #[test]
    fn capability_union_kwin_dpms_ddcci_power_off_passes() {
        // kwin-dpms has NO modes, ddcci has PowerOff.
        // Per-controller check would fail on kwin-dpms; union must succeed.
        use crate::config::defaults;
        let caps: HashMap<String, Vec<BlankMode>> = HashMap::from([
            ("kwin-dpms".into(), vec![]),
            ("ddcci".into(), vec![BlankMode::PowerOff]),
        ]);
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([(
                "main".into(),
                DisplayConfig {
                    controllers: vec!["kwin-dpms".into(), "ddcci".into()],
                    blank_mode: Some(BlankMode::PowerOff),
                    degraded_mode: None,
                    ladder: vec![],
                    screensaver: None,
                    output: None,
                    ddc_display: None,
                    host: None,
                    wol_mac: None,
                    blank_command: None,
                    wake_command: None,
                    modes: None,
                    ha_url: None,
                    blank_service: None,
                    blank_data: None,
                    wake_service: None,
                    wake_data: None,
                    command_timeout: crate::config::defaults::COMMAND_TIMEOUT,
                    restore_brightness: 80,
                    samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
                    treat_unreachable_as_blanked: true,
                    panel_type: crate::wear::PanelType::default(),
                },
            )]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let creds = Credentials::default();
        let errors = validate(&cfg, &caps, &creds);
        assert!(
            !errors.iter().any(|e| e.what == "unsupported blank mode"),
            "expected no unsupported blank mode errors for union, got {:?}",
            errors
        );
    }

    #[test]
    fn empty_modes_display_fails_validation() {
        // Should 5 — `modes = []` on a `command` controller yields an empty
        // union; validate_display must produce a "blank-incapable display"
        // ValidationError (the operator-facing symptom) alongside the
        // per-controller "missing or empty 'modes'" check.
        use crate::config::defaults;
        let caps = test_capabilities();
        let creds = test_creds();
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([(
                "blankless".into(),
                DisplayConfig {
                    controllers: vec!["command".into()],
                    blank_mode: Some(BlankMode::PowerOff),
                    degraded_mode: None,
                    ladder: vec![],
                    screensaver: None,
                    output: None,
                    ddc_display: None,
                    host: None,
                    wol_mac: None,
                    blank_command: Some("true".into()),
                    wake_command: Some("true".into()),
                    // The empty-list case — the fix's primary target.
                    modes: Some(vec![]),
                    ha_url: None,
                    blank_service: None,
                    blank_data: None,
                    wake_service: None,
                    wake_data: None,
                    command_timeout: crate::config::defaults::COMMAND_TIMEOUT,
                    restore_brightness: 80,
                    samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
                    treat_unreachable_as_blanked: true,
                    panel_type: crate::wear::PanelType::default(),
                },
            )]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };

        let errors = validate(&cfg, &caps, &creds);
        assert!(
            errors.iter().any(|e| e.what == "blank-incapable display"),
            "expected 'blank-incapable display' error, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        assert!(
            errors
                .iter()
                .any(|e| e.what == "missing field" && e.detail.contains("modes")),
            "expected per-controller missing/empty modes error, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        // No "unsupported blank mode" error — the new branch short-circuits
        // that path when the union is empty (a duplicate root-cause noise).
        assert!(
            !errors.iter().any(|e| e.what == "unsupported blank mode"),
            "should not also flag blank_mode when the root cause is an empty union: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    // ── Web-UI config keys ──────────────────────────────────────────────────

    #[test]
    fn web_keys_known_in_strict_mode() {
        let dir = std::env::temp_dir().join("dormant-test-web-keys-known");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web_keys.toml");
        std::fs::write(
            &path,
            "config_version = 1\n[daemon]\nweb_port = 8080\nweb_bind = \"127.0.0.1\"\nweb_allow_nonloopback = false\n",
        )
        .unwrap();
        assert!(
            crate::config::load_config(&path, Strictness::Strict).is_ok(),
            "web_* keys must be in KNOWN_KEYS"
        );
    }

    #[test]
    fn nonloopback_bind_rejected_without_override() {
        let dir = std::env::temp_dir().join("dormant-test-nonloopback-reject");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nonloopback.toml");
        std::fs::write(
            &path,
            "config_version = 1\n[daemon]\nweb_port = 8080\nweb_bind = \"0.0.0.0\"\n",
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        // The non-loopback check runs in validate(), not in load_config(),
        // so the parse succeeds — the test verifies the validate gate.
        // In practice, App::build runs validate() which catches this.
        let (cfg, _) = result.unwrap();
        let errors = super::validate(
            &cfg,
            &std::collections::HashMap::new(),
            &Credentials::default(),
        );
        let joined = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("web_allow_nonloopback"),
            "expected error mentioning web_allow_nonloopback, got: {joined}"
        );
    }

    #[test]
    fn nonloopback_bind_allowed_with_override() {
        let dir = std::env::temp_dir().join("dormant-test-nonloopback-allow");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nonloopback_allow.toml");
        std::fs::write(
            &path,
            "config_version = 1\n[daemon]\nweb_port = 8080\nweb_bind = \"0.0.0.0\"\nweb_allow_nonloopback = true\n",
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = super::validate(
            &cfg,
            &std::collections::HashMap::new(),
            &Credentials::default(),
        );
        assert!(errors.is_empty());
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    fn valid_full_config() -> Config {
        let toml_str = include_str!("../../tests/fixtures/config/valid_full.toml");
        toml::from_str(toml_str).unwrap()
    }

    // ── R2 ladder desugar tests ────────────────────────────────────────────

    #[test]
    fn ladder_desugar_blank_mode_power_off() {
        use crate::config::defaults;
        let dc = DisplayConfig {
            blank_mode: Some(BlankMode::PowerOff),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            controllers: vec!["ddcci".into()],
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: crate::config::defaults::COMMAND_TIMEOUT,
            restore_brightness: 80,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: crate::wear::PanelType::default(),
        };
        let ladder = dc.normalized_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(ladder[0].kind, StageKind::Controller(BlankMode::PowerOff));
        assert_eq!(ladder[0].dwell, None);
    }

    #[test]
    fn ladder_desugar_blank_mode_screen_off_audio_on() {
        let dc = DisplayConfig {
            blank_mode: Some(BlankMode::ScreenOffAudioOn),
            ..blank_display_config()
        };
        let ladder = dc.normalized_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(
            ladder[0].kind,
            StageKind::Controller(BlankMode::ScreenOffAudioOn)
        );
    }

    #[test]
    fn ladder_desugar_blank_mode_brightness_zero() {
        let dc = DisplayConfig {
            blank_mode: Some(BlankMode::BrightnessZero),
            ..blank_display_config()
        };
        let ladder = dc.normalized_ladder();
        assert_eq!(ladder.len(), 1);
        assert_eq!(
            ladder[0].kind,
            StageKind::Controller(BlankMode::BrightnessZero)
        );
    }

    #[test]
    fn samsung_restore_backlight_in_range_accepted() {
        let mut dc = blank_display_config();
        dc.samsung_restore_backlight = 50;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .all(|e| e.what != "invalid samsung_restore_backlight"),
            "in-range samsung_restore_backlight=50 should not error: {errors:?}"
        );
    }

    #[test]
    fn samsung_restore_backlight_out_of_range_rejected() {
        let mut dc = blank_display_config();
        dc.samsung_restore_backlight = 51;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == "invalid samsung_restore_backlight"),
            "out-of-range samsung_restore_backlight=51 must error"
        );
        let detail = errors
            .iter()
            .find(|e| e.what == "invalid samsung_restore_backlight")
            .map(|e| e.detail.clone())
            .unwrap_or_default();
        assert!(
            detail.contains("1..=50"),
            "error detail should describe the 1..=50 scale: {detail}"
        );
    }

    #[test]
    fn samsung_restore_backlight_zero_rejected() {
        let mut dc = blank_display_config();
        dc.samsung_restore_backlight = 0;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let samsung_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.what == "invalid samsung_restore_backlight")
            .collect();
        assert_eq!(
            samsung_errors.len(),
            1,
            "exactly one error for samsung_restore_backlight=0, got: {errors:?}"
        );
        assert!(
            samsung_errors[0].detail.contains("dark panel"),
            "error must state the reason: {}",
            samsung_errors[0].detail
        );
    }

    #[test]
    fn samsung_restore_backlight_one_accepted() {
        let mut dc = blank_display_config();
        dc.samsung_restore_backlight = 1;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .all(|e| e.what != "invalid samsung_restore_backlight"),
            "in-range samsung_restore_backlight=1 should not error: {errors:?}"
        );
    }

    #[test]
    fn restore_brightness_zero_rejected() {
        let mut dc = blank_display_config();
        dc.restore_brightness = 0;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        let restore_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.what == "invalid restore_brightness")
            .collect();
        assert_eq!(
            restore_errors.len(),
            1,
            "exactly one error for restore_brightness=0, got: {errors:?}"
        );
        assert!(restore_errors[0].detail.contains("dark panel"));
    }

    #[test]
    fn restore_brightness_one_accepted() {
        let mut dc = blank_display_config();
        dc.restore_brightness = 1;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .all(|e| e.what != "invalid restore_brightness"),
            "in-range restore_brightness=1 should not error: {errors:?}"
        );
    }

    #[test]
    fn restore_brightness_100_accepted() {
        let mut dc = blank_display_config();
        dc.restore_brightness = 100;
        let cfg = Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            sensors: IndexMap::new(),
            zones: IndexMap::new(),
            displays: IndexMap::from([("d".into(), dc)]),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
        };
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .all(|e| e.what != "invalid restore_brightness"),
            "in-range restore_brightness=100 should not error: {errors:?}"
        );
    }

    #[test]
    fn both_blank_mode_and_ladder_rejected() {
        let dir = std::env::temp_dir().join("dormant-test-both-ladder");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("both.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
blank_mode = "power_off"
[[displays.d.ladder]]
kind = "power_off"
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("exactly one"),
            "expected 'exactly one' error, got: {err}"
        );
    }

    #[test]
    fn neither_blank_mode_nor_ladder_rejected() {
        let dir = std::env::temp_dir().join("dormant-test-neither");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neither.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("needs blank_mode or ladder"),
            "expected 'needs blank_mode or ladder' error, got: {err}"
        );
    }

    #[test]
    fn degraded_mode_with_ladder_rejected() {
        let dir = std::env::temp_dir().join("dormant-test-degraded-ladder");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("degraded.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
degraded_mode = "power_off"
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("degraded_mode alongside ladder"),
            "expected degraded+ladder error, got: {err}"
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn ladder_render_black_on_local_display_loads() {
        let dir = std::env::temp_dir().join("dormant-test-ladder-render-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("render_ok.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[sensors.s]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "sensors/test"
[zones.z]
mode = "any"
members = ["s"]
[rules.r]
zone = "z"
displays = ["d"]
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_ok(), "expected OK, got {:?}", result.err());
        let (cfg, _) = result.unwrap();
        let caps = test_capabilities();
        let errors = validate(&cfg, &caps, &Credentials::default());
        assert!(
            errors.is_empty(),
            "expected no errors on ddcci with render feature, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[cfg(not(feature = "render"))]
    #[test]
    fn ladder_render_black_without_feature_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ladder-norender");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("norender.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_ok(), "expected OK, got {:?}", result.err());
        let (cfg, _) = result.unwrap();
        let caps = test_capabilities();
        let errors = validate(&cfg, &caps, &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_RENDER_UNAVAILABLE)
                    && e.contains("without the render feature")
            }),
            "expected E_RENDER_UNAVAILABLE render-feature error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn ladder_render_on_remote_only_errors() {
        let dir = std::env::temp_dir().join("dormant-test-render-remote");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["samsung-tizen"]
host = "192.168.1.50"
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
[[displays.d.ladder]]
kind = "screen_off_audio_on"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let mut caps = test_capabilities();
        // Ensure samsung-tizen is in caps
        caps.entry("samsung-tizen".into())
            .or_insert_with(|| vec![BlankMode::ScreenOffAudioOn, BlankMode::PowerOff]);
        let creds = Credentials {
            ha_token: None,
            samsung: IndexMap::from([("192.168.1.50".into(), "test-token".into())]),
            mqtt: IndexMap::new(),
        };
        let errors = validate(&cfg, &caps, &creds);
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_RENDER_UNAVAILABLE) && e.contains("remote")
            }),
            "expected E_RENDER_UNAVAILABLE remote error, got: {:?}",
            errs
        );
    }

    #[test]
    fn ladder_non_terminal_without_dwell_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ladder-no-dwell");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nodwell.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
[[displays.d.ladder]]
kind = "power_off"
[[displays.d.ladder]]
kind = "screen_off_audio_on"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let caps = test_capabilities();
        let errors = validate(&cfg, &caps, &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter()
                .any(|e| e.contains("non-terminal") && e.contains("dwell")),
            "expected non-terminal-dwell error, got: {:?}",
            errs
        );
    }

    #[test]
    fn ladder_unknown_subkey_in_strict_mode_rejected() {
        let dir = std::env::temp_dir().join("dormant-test-ladder-unknown-key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unknown_subkey.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
[[displays.d.ladder]]
kind = "power_off"
bogus = true
"#,
        )
        .unwrap();
        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bogus"),
            "expected unknown-key error mentioning bogus, got: {err}"
        );
    }

    #[test]
    fn screensaver_trigger_unsupported_rejected() {
        let dir = std::env::temp_dir().join("dormant-test-screensaver-trigger");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trigger.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
trigger = "idle"
[[displays.d.screensaver.source]]
path = "/tmp/pics"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let caps = test_capabilities();
        let errors = validate(&cfg, &caps, &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_SCREENSAVER_SOURCE)
                    && e.contains("vacancy")
                    && e.contains("idle")
            }),
            "expected E_SCREENSAVER_SOURCE trigger error for 'idle', got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_empty_source_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-empty-source");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty_source.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter()
                .any(|e| e.contains(crate::error::E_SCREENSAVER_SOURCE)),
            "expected E_SCREENSAVER_SOURCE for empty source, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_path_and_urls_conflict_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-path-urls");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conflict.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
[[displays.d.screensaver.source]]
path = "/tmp/pics"
urls = ["https://example.com/img.jpg"]
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_SCREENSAVER_SOURCE) && e.contains("pick exactly one")
            }),
            "expected E_SCREENSAVER_SOURCE path-urls conflict error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_empty_source_neither_path_nor_urls_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-neither");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("neither.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
[[displays.d.screensaver.source]]
path = "/tmp/pics"
[[displays.d.screensaver.source]]
shuffle = true
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_SCREENSAVER_SOURCE) && e.contains("neither")
            }),
            "expected E_SCREENSAVER_SOURCE neither-path-nor-urls error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_shuffle_order_conflict_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-shuffle-order");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shuffle_order.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
[[displays.d.screensaver.source]]
path = "/tmp/pics"
shuffle = true
order = "sequential"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_SCREENSAVER_SOURCE) && e.contains("pick exactly one")
            }),
            "expected E_SCREENSAVER_SOURCE shuffle-order conflict error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_unknown_order_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-unknown-order");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unknown_order.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
[[displays.d.screensaver.source]]
path = "/tmp/pics"
order = "random"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_SCREENSAVER_SOURCE) && e.contains("random")
            }),
            "expected E_SCREENSAVER_SOURCE unknown-order error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_image_duration_zero_errors() {
        let dir = std::env::temp_dir().join("dormant-test-ss-imgdur-zero");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zero_duration.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
[[displays.d.screensaver.source]]
path = "/tmp/pics"
image_duration = "0s"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter()
                .any(|e| e.contains("invalid duration") && e.contains("image_duration")),
            "expected invalid-duration error for image_duration=0, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn screensaver_valid_config_passes() {
        let dir = std::env::temp_dir().join("dormant-test-ss-valid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("valid_ss.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[sensors.s]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "sensors/test"
[zones.z]
mode = "any"
members = ["s"]
[rules.r]
zone = "z"
displays = ["d"]
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_screensaver"
[displays.d.screensaver]
trigger = "vacancy"
audio = true
[[displays.d.screensaver.source]]
path = "/tmp/pics"
shuffle = true
image_duration = "3s"
[[displays.d.screensaver.source]]
urls = ["https://example.com/img.jpg"]
order = "sequential"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errors.is_empty(),
            "expected no errors on valid screensaver config, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn render_stage_without_output_errors() {
        let dir = std::env::temp_dir().join("dormant-test-render-no-out");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no_output.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[displays.d]
controllers = ["ddcci"]
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        let errs: Vec<_> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errs.iter().any(|e| {
                e.contains(crate::error::E_CONFIG_INVALID)
                    && e.contains('d')
                    && e.contains("output")
            }),
            "expected E_CONFIG_INVALID output-missing error, got: {:?}",
            errs
        );
    }

    #[cfg(feature = "render")]
    #[test]
    fn render_stage_with_output_validates_ok() {
        let dir = std::env::temp_dir().join("dormant-test-render-with-out");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("with_output.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[sensors.s]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "sensors/test"
[zones.z]
mode = "any"
members = ["s"]
[rules.r]
zone = "z"
displays = ["d"]
[displays.d]
controllers = ["ddcci"]
output = "DP-1"
[[displays.d.ladder]]
kind = "render_black"
dwell = "5s"
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        assert!(
            errors.is_empty(),
            "expected no errors on render black with output, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn controller_only_ladder_without_output_ok() {
        let dir = std::env::temp_dir().join("dormant-test-ctl-no-out");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ctl_no_out.toml");
        std::fs::write(
            &path,
            r#"
config_version = 1
[sensors.s]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "sensors/test"
[zones.z]
mode = "any"
members = ["s"]
[rules.r]
zone = "z"
displays = ["d"]
[displays.d]
controllers = ["ddcci"]
[[displays.d.ladder]]
kind = "power_off"
"#,
        )
        .unwrap();
        let (cfg, _) = crate::config::load_config(&path, Strictness::Strict).unwrap();
        let errors = validate(&cfg, &test_capabilities(), &Credentials::default());
        assert!(
            errors.is_empty(),
            "expected no errors on controller-only ladder without output, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    /// Minimal `DisplayConfig` with all defaults filled in, for use in
    /// desugar tests where only `blank_mode` varies.
    fn blank_display_config() -> DisplayConfig {
        use crate::config::defaults;
        DisplayConfig {
            blank_mode: None,
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            controllers: vec!["ddcci".into()],
            output: None,
            ddc_display: None,
            host: None,
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: defaults::COMMAND_TIMEOUT,
            restore_brightness: 80,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: crate::wear::PanelType::default(),
        }
    }

    // ── is_known_config_path ──────────────────────────────────────────────

    #[test]
    fn is_known_path_accepts_daemon_log_level() {
        assert!(is_known_config_path(&["daemon", "log_level"]));
    }

    #[test]
    fn is_known_path_accepts_dynamic_sensor_id() {
        assert!(is_known_config_path(&["sensors", "anything", "topic"]));
    }

    #[test]
    fn is_known_path_rejects_unknown_leaf() {
        assert!(!is_known_config_path(&["daemon", "nope"]));
    }

    #[test]
    fn is_known_path_rejects_unknown_root() {
        assert!(!is_known_config_path(&["nope"]));
    }

    #[test]
    fn is_known_path_accepts_screensaver_source_field() {
        assert!(is_known_config_path(&[
            "displays",
            "x",
            "screensaver",
            "source",
            "0",
            "shuffle"
        ]));
    }

    #[test]
    fn is_known_path_rejects_wrong_screensaver_key() {
        assert!(!is_known_config_path(&[
            "displays",
            "x",
            "screensaver",
            "source",
            "0",
            "shufle"
        ]));
    }

    #[test]
    fn is_known_path_empty_rejected() {
        assert!(!is_known_config_path(&[]));
    }

    #[test]
    fn is_known_path_bare_collection_accepted() {
        assert!(is_known_config_path(&["sensors"]));
    }

    #[test]
    fn is_known_path_ladder_index_accepted() {
        assert!(is_known_config_path(&[
            "displays", "x", "ladder", "0", "dwell"
        ]));
    }

    #[test]
    fn is_known_path_non_digit_index_rejected() {
        assert!(!is_known_config_path(&[
            "displays",
            "x",
            "screensaver",
            "source",
            "abc",
            "shuffle"
        ]));
    }

    // M1 — digit-skip must NOT fire after non-array-of-tables keys.
    #[test]
    fn is_known_path_rejects_digit_after_non_array_table() {
        assert!(!is_known_config_path(&["daemon", "0", "log_level"]));
        assert!(!is_known_config_path(&["sensors", "x", "0", "topic"]));
    }

    // N1 — bare array index (no child key) is rejected.
    // This is a deliberate stance: the fn rejects a path that ends on an
    // index because a config patch always targets a leaf key, never an
    // array slot alone.  The TOML walker would accept an empty table entry,
    // but a tighter guard here costs nothing and is defensible.
    #[test]
    fn is_known_path_rejects_bare_ladder_index() {
        assert!(!is_known_config_path(&["displays", "x", "ladder", "0"]));
    }

    // ── [wear] validation ────────────────────────────────────────────────

    fn wear_config(body: &str) -> Config {
        let toml_str = format!("config_version = 1\n[wear]\n{body}");
        toml::from_str(&toml_str).unwrap()
    }

    fn wear_validation_errors(body: &str) -> Vec<ValidationError> {
        let cfg = wear_config(body);
        validate(&cfg, &HashMap::new(), &Credentials::default())
    }

    #[test]
    fn wear_unknown_key_rejected_strict() {
        let dir = std::env::temp_dir().join("dormant-test-wear-unknown-key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wear_unknown.toml");
        std::fs::write(&path, "config_version = 1\n[wear]\nbogus = 1\n").unwrap();

        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("wear.bogus"),
            "expected error mentioning wear.bogus, got: {err}"
        );
    }

    #[test]
    fn wear_all_keys_known_in_strict_mode() {
        let dir = std::env::temp_dir().join("dormant-test-wear-known-keys");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wear_known.toml");
        std::fs::write(
            &path,
            "config_version = 1\n\
             [wear]\n\
             enabled = true\n\
             sample_interval = \"60s\"\n\
             persist_interval = \"5m\"\n\
             read_timeout = \"2s\"\n\
             grid_rows = 9\n\
             grid_cols = 16\n\
             fallback_brightness = 0.5\n\
             screensaver_factor = 0.35\n\
             short_cycle_dwell = \"10m\"\n\
             advisory_after = \"96h\"\n",
        )
        .unwrap();
        assert!(
            crate::config::load_config(&path, Strictness::Strict).is_ok(),
            "all [wear] keys must be in KNOWN_KEYS"
        );
    }

    #[test]
    fn wear_defaults_accepted_with_no_errors() {
        let errors = wear_validation_errors("");
        assert!(
            errors.is_empty(),
            "default [wear] section must validate cleanly, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn wear_sample_interval_floor_accepts_5s() {
        let errors = wear_validation_errors("sample_interval = \"5s\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("sample_interval")),
            "5s sample_interval (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_sample_interval_below_floor_rejected() {
        let errors = wear_validation_errors("sample_interval = \"4s\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("sample_interval")),
            "sample_interval below 5s floor must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_persist_shorter_than_sample_rejected() {
        let errors =
            wear_validation_errors("sample_interval = \"5m\"\npersist_interval = \"1m\"\n");
        assert!(
            errors.iter().any(|e| e.detail.contains("persist_interval")),
            "persist_interval < sample_interval must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_persist_equal_to_sample_accepted() {
        let errors =
            wear_validation_errors("sample_interval = \"5m\"\npersist_interval = \"5m\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("persist_interval")),
            "persist_interval == sample_interval must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_read_timeout_floor_accepts_500ms() {
        let errors = wear_validation_errors("read_timeout = \"500ms\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("read_timeout")),
            "500ms read_timeout (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_read_timeout_below_floor_rejected() {
        let errors = wear_validation_errors("read_timeout = \"100ms\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("read_timeout")),
            "read_timeout below 500ms floor must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_grid_rows_in_range_accepted() {
        for rows in [4, 64] {
            let errors = wear_validation_errors(&format!("grid_rows = {rows}\n"));
            assert!(
                !errors.iter().any(|e| e.detail.contains("grid_rows")),
                "grid_rows = {rows} (edge of 4..=64) must be accepted, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_grid_rows_out_of_range_rejected() {
        for rows in [3, 65] {
            let errors = wear_validation_errors(&format!("grid_rows = {rows}\n"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("grid_rows")),
                "grid_rows = {rows} (outside 4..=64) must be rejected, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_grid_cols_in_range_accepted() {
        for cols in [4, 64] {
            let errors = wear_validation_errors(&format!("grid_cols = {cols}\n"));
            assert!(
                !errors.iter().any(|e| e.detail.contains("grid_cols")),
                "grid_cols = {cols} (edge of 4..=64) must be accepted, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_grid_cols_out_of_range_rejected() {
        for cols in [3, 65] {
            let errors = wear_validation_errors(&format!("grid_cols = {cols}\n"));
            assert!(
                errors
                    .iter()
                    .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("grid_cols")),
                "grid_cols = {cols} (outside 4..=64) must be rejected, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_fallback_brightness_in_range_accepted() {
        for b in ["0.0", "1.0"] {
            let errors = wear_validation_errors(&format!("fallback_brightness = {b}\n"));
            assert!(
                !errors
                    .iter()
                    .any(|e| e.detail.contains("fallback_brightness")),
                "fallback_brightness = {b} (edge of 0.0..=1.0) must be accepted, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_fallback_brightness_out_of_range_rejected() {
        let errors = wear_validation_errors("fallback_brightness = 1.5\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("fallback_brightness")),
            "fallback_brightness outside 0.0..=1.0 must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_screensaver_factor_in_range_accepted() {
        for f in ["0.0", "1.0"] {
            let errors = wear_validation_errors(&format!("screensaver_factor = {f}\n"));
            assert!(
                !errors
                    .iter()
                    .any(|e| e.detail.contains("screensaver_factor")),
                "screensaver_factor = {f} (edge of 0.0..=1.0) must be accepted, got: {:?}",
                errors
            );
        }
    }

    #[test]
    fn wear_screensaver_factor_out_of_range_rejected() {
        let errors = wear_validation_errors("screensaver_factor = -0.1\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("screensaver_factor")),
            "screensaver_factor outside 0.0..=1.0 must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_short_cycle_dwell_floor_accepts_1m() {
        let errors = wear_validation_errors("short_cycle_dwell = \"1m\"\n");
        assert!(
            !errors
                .iter()
                .any(|e| e.detail.contains("short_cycle_dwell")),
            "1m short_cycle_dwell (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_short_cycle_dwell_below_floor_rejected() {
        let errors = wear_validation_errors("short_cycle_dwell = \"30s\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("short_cycle_dwell")),
            "short_cycle_dwell below 1m floor must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_advisory_after_floor_accepts_1h() {
        let errors = wear_validation_errors("advisory_after = \"1h\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("advisory_after")),
            "1h advisory_after (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn wear_advisory_after_below_floor_rejected() {
        let errors = wear_validation_errors("advisory_after = \"30m\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("advisory_after")),
            "advisory_after below 1h floor must be rejected, got: {:?}",
            errors
        );
    }

    // ── displays.<id>.screensaver.shift_px / shift_interval ────────────────

    fn config_with_shift(shift_px: u8, shift_interval: &str) -> Config {
        let toml_str = format!(
            "config_version = 1\n\
             [displays.d1]\n\
             controllers = [\"kwin-dpms\"]\n\
             blank_mode = \"power_off\"\n\
             [displays.d1.screensaver]\n\
             trigger = \"vacancy\"\n\
             shift_px = {shift_px}\n\
             shift_interval = \"{shift_interval}\"\n\
             [[displays.d1.screensaver.source]]\n\
             path = \"/tmp/img.png\"\n"
        );
        toml::from_str(&toml_str).unwrap()
    }

    #[test]
    fn shift_keys_known_in_strict_mode() {
        let dir = std::env::temp_dir().join("dormant-test-shift-keys-known");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shift_keys.toml");
        std::fs::write(
            &path,
            "config_version = 1\n\
             [displays.d1]\n\
             controllers = [\"kwin-dpms\"]\n\
             blank_mode = \"power_off\"\n\
             [displays.d1.screensaver]\n\
             trigger = \"vacancy\"\n\
             shift_px = 2\n\
             shift_interval = \"120s\"\n\
             [[displays.d1.screensaver.source]]\n\
             path = \"/tmp/img.png\"\n",
        )
        .unwrap();
        assert!(
            crate::config::load_config(&path, Strictness::Strict).is_ok(),
            "shift_px and shift_interval must be in KNOWN_KEYS"
        );
    }

    #[test]
    fn shift_px_in_range_accepted() {
        let cfg = config_with_shift(8, "120s");
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            !errors.iter().any(|e| e.detail.contains("shift_px")),
            "shift_px = 8 (the ceiling) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn shift_px_out_of_range_rejected() {
        let cfg = config_with_shift(20, "120s");
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("shift_px")),
            "shift_px above 8 must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn shift_interval_floor_accepts_10s() {
        let cfg = config_with_shift(2, "10s");
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            !errors.iter().any(|e| e.detail.contains("shift_interval")),
            "shift_interval = 10s (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn shift_interval_below_floor_rejected() {
        let cfg = config_with_shift(2, "5s");
        let errors = validate(&cfg, &test_capabilities(), &test_creds());
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("shift_interval")),
            "shift_interval below 10s floor must be rejected, got: {:?}",
            errors
        );
    }

    // ── [notifications] validation ──────────────────────────────────────

    fn notifications_config(body: &str) -> Config {
        let toml_str = format!("config_version = 1\n[notifications]\n{body}");
        toml::from_str(&toml_str).unwrap()
    }

    fn notifications_validation_errors(body: &str) -> Vec<ValidationError> {
        let cfg = notifications_config(body);
        validate(&cfg, &HashMap::new(), &Credentials::default())
    }

    #[test]
    fn notifications_all_keys_known_in_strict_mode() {
        let dir = std::env::temp_dir().join("dormant-test-notifications-keys-known");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notifications_full.toml");
        std::fs::write(
            &path,
            "config_version = 1\n\
             [notifications]\n\
             enabled = true\n\
             wake_attempt_threshold = 3\n\
             cooldown = \"15m\"\n\
             notify_recovery = true\n",
        )
        .unwrap();
        assert!(
            crate::config::load_config(&path, Strictness::Strict).is_ok(),
            "all [notifications] keys must be in KNOWN_KEYS"
        );
    }

    #[test]
    fn notifications_unknown_key_rejected_strict() {
        let dir = std::env::temp_dir().join("dormant-test-notifications-unknown-key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notifications_unknown.toml");
        std::fs::write(&path, "config_version = 1\n[notifications]\nbogus = 1\n").unwrap();

        let result = crate::config::load_config(&path, Strictness::Strict);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("notifications.bogus"),
            "expected error mentioning notifications.bogus, got: {err}"
        );
    }

    #[test]
    fn notifications_defaults_accepted_with_no_errors() {
        let errors = notifications_validation_errors("");
        assert!(
            errors.is_empty(),
            "default [notifications] section must validate cleanly, got: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn notifications_threshold_zero_rejected() {
        let errors = notifications_validation_errors("wake_attempt_threshold = 0\n");
        assert!(
            errors.iter().any(
                |e| e.what == "E_CONFIG_INVALID" && e.detail.contains("wake_attempt_threshold")
            ),
            "wake_attempt_threshold = 0 must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn notifications_threshold_one_accepted() {
        let errors = notifications_validation_errors("wake_attempt_threshold = 1\n");
        assert!(
            !errors
                .iter()
                .any(|e| e.detail.contains("wake_attempt_threshold")),
            "wake_attempt_threshold = 1 (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn notifications_cooldown_floor() {
        let errors = notifications_validation_errors("cooldown = \"10s\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("cooldown")),
            "cooldown = 10s (below the 1m floor) must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn notifications_cooldown_at_floor_accepted() {
        let errors = notifications_validation_errors("cooldown = \"1m\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("cooldown")),
            "cooldown = 1m (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    // ── [sensors.<id>] availability_* validation ────────────────────────────

    fn availability_config(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    fn availability_validation_errors(toml_str: &str) -> Vec<ValidationError> {
        let cfg = availability_config(toml_str);
        validate(&cfg, &HashMap::new(), &Credentials::default())
    }

    #[test]
    fn availability_literals_must_differ() {
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk"
availability_payload_online = "x"
availability_payload_offline = "x"
"#,
        );
        assert!(
            errs.iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("availability_payload")),
            "expected an availability_payload error, got: {:?}",
            errs
        );
    }

    #[test]
    fn availability_topic_colliding_with_state_topic_rejected() {
        // F10 — sensor a's state topic ("t1") is the same string as sensor b's
        // (explicit) availability topic, on the same broker.
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "t1"

[sensors.b]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "t2"
availability_topic = "t1"
"#,
        );
        assert!(
            errs.iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("availability_topic")),
            "expected an availability_topic collision error, got: {:?}",
            errs
        );
    }

    #[test]
    fn shared_availability_topic_divergent_literals_rejected_identical_accepted() {
        // F9 — two sensors on the same broker sharing one availability_topic.
        // Divergent literal pairs are rejected...
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/a"
availability_topic = "shared/avail"
availability_payload_online = "up"
availability_payload_offline = "down"

[sensors.b]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/b"
availability_topic = "shared/avail"
availability_payload_online = "online"
availability_payload_offline = "offline"
"#,
        );
        assert!(
            errs.iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("shared/avail")),
            "expected a divergent-literals error for the shared availability topic, got: {:?}",
            errs
        );

        // ...identical literal pairs are accepted.
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/a"
availability_topic = "shared/avail"
availability_payload_online = "up"
availability_payload_offline = "down"

[sensors.b]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/b"
availability_topic = "shared/avail"
availability_payload_online = "up"
availability_payload_offline = "down"
"#,
        );
        assert!(
            !errs
                .iter()
                .any(|e| e.detail.contains("shared/avail") && e.detail.contains("divergent")),
            "identical literal pairs on a shared availability_topic must not error, got: {:?}",
            errs
        );
    }

    #[test]
    fn availability_topic_empty_string_rejected() {
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "zigbee2mqtt/desk"
availability_topic = ""
"#,
        );
        assert!(
            errs.iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("availability_topic")),
            "empty availability_topic must be rejected, got: {:?}",
            errs
        );
    }

    #[test]
    fn availability_topic_collision_via_default_derivation_rejected() {
        // F10 via the DEFAULT-DERIVED path — neither sensor sets an explicit
        // `availability_topic`. Sensor a's state topic is literally the
        // string sensor b's availability topic derives to
        // (`derive_availability_topic("t1") == "t1/availability"`), so the
        // collision must be detected purely through derivation, not through
        // an explicit override. This is the case the reviewer's mutation
        // (breaking `derive_availability_topic`'s string form) must kill.
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "t1/availability"

[sensors.b]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "t1"
"#,
        );
        assert!(
            errs.iter().any(|e| e.what == "E_CONFIG_INVALID"
                && e.detail.contains("'b'")
                && e.detail.contains("t1/availability")
                && e.detail.contains("collides with a state topic")),
            "sensor b's DEFAULT-derived availability topic ('t1/availability') colliding with \
             sensor a's literal state topic must be rejected, got: {:?}",
            errs
        );
    }

    #[test]
    fn shared_derived_availability_topic_divergent_literals_rejected() {
        // F9 via the DEFAULT-DERIVED path — neither sensor sets an explicit
        // `availability_topic`; both share the same state topic
        // ("sensors/shared"), so both derive to the same availability topic
        // ("sensors/shared/availability"). A shared state topic on one
        // broker is not itself rejected by validate_sensors, so this
        // isolates the F9 divergent-literals check to the derived path.
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/shared"
availability_payload_online = "up"
availability_payload_offline = "down"

[sensors.b]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "sensors/shared"
availability_payload_online = "online"
availability_payload_offline = "offline"
"#,
        );
        assert!(
            errs.iter().any(|e| e.what == "E_CONFIG_INVALID"
                && e.detail.contains("sensors/shared/availability")),
            "two sensors sharing a purely DEFAULT-derived availability topic with divergent \
             literals must be rejected, got: {:?}",
            errs
        );
    }

    #[test]
    fn availability_topic_same_string_different_brokers_not_collided() {
        // F10 cross-broker independence — the same topic string used as
        // sensor a's state topic on broker-x and sensor b's explicit
        // availability_topic on broker-y must NOT collide: broker grouping
        // (`by_broker`) structurally isolates them. Mirrors the reviewer's
        // ad hoc probe (T1-review.md, finding S2).
        let errs = availability_validation_errors(
            r#"
config_version = 1
[sensors.a]
type = "mqtt"
broker_url = "tcp://broker-x:1883"
topic = "t1"

[sensors.b]
type = "mqtt"
broker_url = "tcp://broker-y:1883"
topic = "t2"
availability_topic = "t1"
"#,
        );
        assert!(
            errs.is_empty(),
            "same topic string on two different brokers must not collide, got: {:?}",
            errs
        );
    }

    #[test]
    fn keys_on_ha_sensor_load_and_are_ignored() {
        // F7 — the availability_* keys live in the shared `sensors.` known-key
        // bucket (not per-variant), so setting them on an `ha` sensor loads
        // without a strict-mode unknown-key rejection. HaSensorCfg has no such
        // fields, so serde simply drops them on deserialization — no
        // availability behavior applies to `ha` sensors. Pinning that HONEST
        // behavior here (not pretending the config layer rejects it).
        let toml_str = r#"
config_version = 1
[sensors.h]
type = "ha"
url = "ws://ha.local:8123/api/websocket"
entity = "binary_sensor.x"
availability_topic = "whatever"
availability_payload_online = "up"
availability_payload_offline = "down"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let unknown = collect_unknown_keys(&value);
        assert!(
            unknown.is_empty(),
            "availability_* keys must be structurally known under sensors., got {:?}",
            unknown
        );

        let cfg: Config = toml::from_str(toml_str).unwrap();
        match &cfg.sensors["h"] {
            crate::config::schema::SensorConfig::Ha(_) => {} // field dropped, as expected
            other => panic!("expected Ha sensor, got {other:?}"),
        }
    }

    // ── Task 1: STRUCTURAL_RESERVED_NAMES + predicate refactor (P1/P10) ────

    #[test]
    fn structural_reserved_names_contains_all_five() {
        for n in ["weights", "source", "ladder", "blank_data", "wake_data"] {
            assert!(
                STRUCTURAL_RESERVED_NAMES.contains(&n),
                "STRUCTURAL_RESERVED_NAMES missing {n}"
            );
        }
        assert_eq!(STRUCTURAL_RESERVED_NAMES.len(), 5);
    }

    // Byte-identical behavior pins for the refactored predicates (existing
    // unknown-key-walker tests above — passthrough_data_subkeys_not_flagged
    // et al. — already exercise these indirectly; these pin the raw fns).
    #[test]
    fn is_weights_level_matches_dot_weights_suffix_only() {
        assert!(is_weights_level("zones.media.weights"));
        assert!(!is_weights_level("zones.media.weightsx"));
        assert!(!is_weights_level("weights")); // bare root — no leading dot, not a suffix match
    }

    #[test]
    fn is_array_of_tables_parent_matches_ladder_and_source_suffixes() {
        assert!(is_array_of_tables_parent("displays.tv.ladder"));
        assert!(is_array_of_tables_parent("displays.tv.screensaver.source"));
        assert!(!is_array_of_tables_parent("displays.tv.laddery"));
    }

    #[test]
    fn is_passthrough_data_key_exact_equality_only() {
        assert!(is_passthrough_data_key("blank_data"));
        assert!(is_passthrough_data_key("wake_data"));
        assert!(!is_passthrough_data_key("blank_data_extra"));
        assert!(!is_passthrough_data_key("wake_data2"));
    }

    // Cross-predicate non-overlap pins (P10) — an accidental broadening (e.g.
    // a shared blanket loop instead of per-predicate consts) fails HERE, at
    // the unit where it would be introduced, not downstream.
    #[test]
    fn is_weights_level_rejects_ladder_suffix() {
        assert!(!is_weights_level("displays.x.ladder"));
    }

    #[test]
    fn is_array_of_tables_parent_rejects_weights_suffix() {
        assert!(!is_array_of_tables_parent("displays.x.weights"));
    }

    #[test]
    fn is_passthrough_data_key_rejects_near_miss_suffix() {
        assert!(!is_passthrough_data_key("blank_data_extra"));
        // Both broadening directions (reviewer pin): a prefix-style broadening
        // is caught above; an ends_with-style broadening is caught here.
        assert!(!is_passthrough_data_key("evil_blank_data"));
        assert!(!is_passthrough_data_key("x.wake_data"));
    }

    // Each predicate's accepted names ⊆ STRUCTURAL_RESERVED_NAMES, ENUMERATED
    // EXPLICITLY BY NAME per predicate (cold-gate Gemini Should) — a unit
    // test cannot reflectively discover a predicate function it was never
    // told about, so a genuinely NEW 7th predicate needs its own enumerated
    // test line added here at write time; this is not automatic discovery.
    #[test]
    fn weights_level_accepted_names_are_subset_of_structural_reserved_names() {
        // Single-name enumeration (not a loop, clippy::single_element_loop) —
        // still an explicit, by-name pin, not a reference to the const it
        // checks against.
        assert!(STRUCTURAL_RESERVED_NAMES.contains(&"weights"));
    }

    #[test]
    fn array_of_tables_parent_accepted_names_are_subset_of_structural_reserved_names() {
        for n in ["ladder", "source"] {
            assert!(STRUCTURAL_RESERVED_NAMES.contains(&n));
        }
    }

    #[test]
    fn passthrough_data_key_accepted_names_are_subset_of_structural_reserved_names() {
        for n in ["blank_data", "wake_data"] {
            assert!(STRUCTURAL_RESERVED_NAMES.contains(&n));
        }
    }

    // ── Task 1: daemon.entity_crud_enabled / pairing_enabled / pair_timeout ─

    // `load_str`/`load_str_strict`/`validate_str` do not exist elsewhere on
    // this branch — other tests in this module (e.g. `wear_unknown_key_
    // rejected_strict`) inline the same tempfile + load_config sequence
    // without a shared helper. Collected here per the established recipe;
    // expected to textually collide with sibling feature branches at merge
    // time (kept byte-similar intentionally, not a bug).
    fn load_str(toml_str: &str) -> Result<(Config, Vec<Warning>), crate::error::DormantError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml_str).unwrap();
        crate::config::load_config(&path, Strictness::Warn)
    }

    fn load_str_strict(
        toml_str: &str,
    ) -> Result<(Config, Vec<Warning>), crate::error::DormantError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml_str).unwrap();
        crate::config::load_config(&path, Strictness::Strict)
    }

    fn validate_str(toml_str: &str) -> Vec<ValidationError> {
        let cfg: Config = toml::from_str(toml_str).unwrap();
        validate(&cfg, &test_capabilities(), &test_creds())
    }

    #[test]
    fn daemon_section_absent_uses_literal_defaults() {
        // [daemon] entirely absent — Config::daemon's #[serde(default)] must
        // produce DaemonConfig::default(). Literal pins, NOT compared against
        // defaults::* consts — a comparison against the same const the impl
        // reads from would be a tautology unable to catch a wrong literal in
        // either the const or the Default impl.
        let (cfg, _warnings) = load_str("config_version = 1\n").unwrap();
        assert!(cfg.daemon.entity_crud_enabled);
        assert!(cfg.daemon.pairing_enabled);
        assert_eq!(cfg.daemon.pair_timeout, Duration::from_secs(120));
    }

    #[test]
    fn daemon_section_present_but_empty_uses_literal_defaults() {
        // [daemon] present with zero keys. DaemonConfig::socket_path is
        // `Option<PathBuf>` with no `#[serde(default)]` attribute (checked
        // against the live struct, per the plan's "test what's true"
        // instruction) — TOML/serde treats a missing Option field as None
        // without one (the same pattern already relied on by DisplayConfig's
        // `output`/`host`/etc. fields), so an empty `[daemon]` table still
        // deserializes cleanly.
        let (cfg, _warnings) = load_str("config_version = 1\n[daemon]\n").unwrap();
        assert!(cfg.daemon.entity_crud_enabled);
        assert!(cfg.daemon.pairing_enabled);
        assert_eq!(cfg.daemon.pair_timeout, Duration::from_secs(120));
    }

    #[test]
    fn pair_timeout_floor_29s_rejected() {
        let errors = validate_str("config_version = 1\n[daemon]\npair_timeout = \"29s\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("pair_timeout")),
            "pair_timeout below the 30s floor must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn pair_timeout_floor_30s_accepted() {
        let errors = validate_str("config_version = 1\n[daemon]\npair_timeout = \"30s\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("pair_timeout")),
            "pair_timeout = 30s (the floor) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn pair_timeout_ceiling_300s_accepted() {
        let errors = validate_str("config_version = 1\n[daemon]\npair_timeout = \"300s\"\n");
        assert!(
            !errors.iter().any(|e| e.detail.contains("pair_timeout")),
            "pair_timeout = 300s (the ceiling) must be accepted, got: {:?}",
            errors
        );
    }

    #[test]
    fn pair_timeout_ceiling_301s_rejected() {
        let errors = validate_str("config_version = 1\n[daemon]\npair_timeout = \"301s\"\n");
        assert!(
            errors
                .iter()
                .any(|e| e.what == "E_CONFIG_INVALID" && e.detail.contains("pair_timeout")),
            "pair_timeout above the 300s ceiling must be rejected, got: {:?}",
            errors
        );
    }

    #[test]
    fn daemon_new_keys_accepted_in_strict_mode() {
        let result = load_str_strict(
            "config_version = 1\n\
             [daemon]\n\
             entity_crud_enabled = false\n\
             pairing_enabled = false\n\
             pair_timeout = \"60s\"\n",
        );
        assert!(
            result.is_ok(),
            "the three new daemon keys must be in KNOWN_KEYS, got: {:?}",
            result.err()
        );
        let (cfg, _warnings) = result.unwrap();
        assert!(!cfg.daemon.entity_crud_enabled);
        assert!(!cfg.daemon.pairing_enabled);
        assert_eq!(cfg.daemon.pair_timeout, Duration::from_secs(60));
    }
}
