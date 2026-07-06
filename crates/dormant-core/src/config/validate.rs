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
            "treat_unreachable_as_blanked",
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
    ("displays..screensaver", &["trigger", "audio", "source"]),
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

/// Is this path at a weights sub-table where keys are dynamic member ids?
fn is_weights_level(path: &str) -> bool {
    path.ends_with(".weights")
}

/// Is this a passthrough data key whose children are arbitrary TOML that
/// should not be checked against the known-key set?
fn is_passthrough_data_key(key: &str) -> bool {
    key == "blank_data" || key == "wake_data"
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

    errors
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
                what: "render unavailable".into(),
                detail: format!(
                    "display '{display_id}' uses a render stage but has only remote \
                     controllers (render stages require a local output)"
                ),
            });
        }

        // Feature-gate check.
        #[cfg(not(feature = "render"))]
        {
            errors.push(ValidationError {
                what: "render unavailable".into(),
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
                        what: "missing screensaver config".into(),
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
                        what: "screensaver source missing".into(),
                        detail: format!(
                            "display '{display_id}' screensaver has no source with \
                             a path or urls"
                        ),
                    });
                }
                // Per-source path-xor-urls check.
                for (i, src) in ss.source.iter().enumerate() {
                    if src.path.is_some() && !src.urls.is_empty() {
                        errors.push(ValidationError {
                            what: "screensaver source conflict".into(),
                            detail: format!(
                                "display '{display_id}' screensaver source {i} sets both \
                                 path and urls — pick exactly one"
                            ),
                        });
                    }
                }
                // Trigger check.
                if ss.trigger != "vacancy" {
                    errors.push(ValidationError {
                        what: "unsupported trigger".into(),
                        detail: format!(
                            "trigger '{}' not supported in this release (vacancy only)",
                            ss.trigger
                        ),
                    });
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

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;
    use crate::config::Strictness;
    use crate::config::schema::ZoneConfig;
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
                vec![BlankMode::ScreenOffAudioOn, BlankMode::PowerOff],
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
        // Change tv to use PowerOff with samsung-tizen (it supports it, so use
        // BrightnessZero which samsung-tizen does NOT support).
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
                    treat_unreachable_as_blanked: true,
                },
            )]),
            rules: IndexMap::new(),
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
                    treat_unreachable_as_blanked: true,
                },
            )]),
            rules: IndexMap::new(),
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
            treat_unreachable_as_blanked: true,
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
        assert!(
            !errors.iter().any(|e| e.what.contains("render")),
            "expected no render errors on ddcci, got: {:?}",
            errors
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
            errs.iter()
                .any(|e| e.contains("render") && e.contains("without the render feature")),
            "expected render-feature error, got: {:?}",
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
            errs.iter()
                .any(|e| e.contains("render") && e.contains("remote")),
            "expected render-on-remote error, got: {:?}",
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
            errs.iter()
                .any(|e| e.contains("not supported in this release")),
            "expected unsupported trigger error, got: {:?}",
            errs
        );
    }

    /// Minimal `DisplayConfig` with all defaults filled in, for use in
    /// desugar tests where only `blank_mode` varies.
    fn blank_display_config() -> DisplayConfig {
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
            command_timeout: crate::config::defaults::COMMAND_TIMEOUT,
            restore_brightness: 80,
            treat_unreachable_as_blanked: true,
        }
    }
}
