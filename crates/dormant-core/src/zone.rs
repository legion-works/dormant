//! Zone fusion engine: combines sensor states into zone-level presence booleans
//! using configurable fusion modes (any/all/quorum/weighted), supports nested zones
//! with cycle detection, and emits change events when a zone's resolved presence flips.
//!
//! The engine is pure logic — no I/O. Sensor state is driven externally via
//! [`ZoneEngine::apply`], which accepts [`PresenceEvent`]s and returns the set of
//! zones whose presence changed as a result.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::DormantError;
use crate::types::{PresenceEvent, SensorId, SensorState, Timestamp, ZoneId};

// ── Public types ───────────────────────────────────────────────────────────────

/// How a zone fuses its member booleans into one presence signal.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FusionMode {
    /// At least one member is present.
    Any,
    /// Every member is present.
    All,
    /// At least `n` members are present.
    ///
    /// `Quorum(0)` is always present (trivially satisfied).
    /// `Quorum(n)` where `n` exceeds the member count is never present.
    Quorum(u32),
    /// Weighted sum of present members divided by total weight >= `threshold`.
    Weighted {
        /// Fraction (0.0 – 1.0) that must be met or exceeded.
        threshold: f32,
    },
}

/// How a zone treats sensors in the [`SensorState::Unavailable`] state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnavailablePolicy {
    /// Treat unavailable sensors as present (fail-safe — don't blank when blind).
    #[default]
    Present,
    /// Treat unavailable sensors as absent (opt-in aggressive blanking).
    Absent,
}

/// A member of a zone: either a sensor or a nested zone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "id")]
pub enum ZoneMember {
    /// A sensor member.
    Sensor(SensorId),
    /// A nested zone member.
    Zone(ZoneId),
}

/// Specification for a zone — the static config that defines its identity, fusion
/// mode, members, weights, and unavailable policy.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ZoneSpec {
    /// Unique zone identifier.
    pub id: ZoneId,
    /// How members are fused into one presence boolean.
    pub mode: FusionMode,
    /// Ordered list of members (sensors and/or nested zones).
    pub members: Vec<ZoneMember>,
    /// Per-member weights for [`FusionMode::Weighted`].
    /// Keys are the string representation of the member's id (sensor or zone id).
    /// Members without an entry default to weight 1.0.
    pub weights: HashMap<String, f32>,
    /// How unavailable sensors are treated in this zone.
    pub unavailable_policy: UnavailablePolicy,
}

/// A change event emitted when a zone's resolved presence flips.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneChange {
    /// The zone whose presence changed.
    pub zone: ZoneId,
    /// The new presence value.
    pub present: bool,
    /// The sensor whose event triggered this change.
    pub cause: SensorId,
}

// ── ZoneEngine ─────────────────────────────────────────────────────────────────

/// The zone fusion engine.
///
/// Maintains per-sensor state (all sensors start `Unavailable` — fail-safe from
/// t=0), resolves zones bottom-up in topological order, and emits [`ZoneChange`]
/// events only when a zone's presence actually flips.
#[derive(Debug)]
pub struct ZoneEngine {
    /// Per-sensor state, keyed by [`SensorId`].
    sensor_states: HashMap<SensorId, (SensorState, Timestamp)>,
    /// Zone specs indexed by id.
    specs: HashMap<ZoneId, ZoneSpec>,
    /// Topological evaluation order (leaf zones first).
    eval_order: Vec<ZoneId>,
    /// Current resolved presence for each zone.
    zone_presence: HashMap<ZoneId, bool>,
    /// Set of sensor ids that have already been warned about (unknown event).
    unknown_sensor_warnings: HashSet<SensorId>,
}

impl ZoneEngine {
    /// Create a new zone engine from zone specs and the known sensor inventory.
    ///
    /// # Errors
    ///
    /// - [`DormantError::ZoneUnknownMember`] if a `ZoneMember::Sensor` is not in
    ///   `sensor_inventory` or a `ZoneMember::Zone` references an undeclared zone.
    /// - [`DormantError::ConfigInvalid`] if two specs share the same zone id.
    /// - [`DormantError::ZoneCycle`] if zone-to-zone references form a cycle.
    pub fn new(specs: Vec<ZoneSpec>, sensor_inventory: &[SensorId]) -> Result<Self, DormantError> {
        // Build id→spec map, check duplicates.
        let mut spec_map: HashMap<ZoneId, ZoneSpec> = HashMap::new();
        for spec in specs {
            if spec_map.contains_key(&spec.id) {
                return Err(DormantError::ConfigInvalid {
                    detail: format!("duplicate zone id '{}'", spec.id),
                });
            }
            spec_map.insert(spec.id.clone(), spec);
        }

        let inventory_set: HashSet<&SensorId> = sensor_inventory.iter().collect();

        // Validate all members.
        for spec in spec_map.values() {
            for member in &spec.members {
                match member {
                    ZoneMember::Sensor(sid) => {
                        if !inventory_set.contains(sid) {
                            return Err(DormantError::ZoneUnknownMember {
                                zone: spec.id.to_string(),
                                member: sid.to_string(),
                            });
                        }
                    }
                    ZoneMember::Zone(zid) => {
                        if !spec_map.contains_key(zid) {
                            return Err(DormantError::ZoneUnknownMember {
                                zone: spec.id.to_string(),
                                member: zid.to_string(),
                            });
                        }
                    }
                }
            }
        }

        // Validate weighted-mode config (NaN, out-of-range threshold, non-finite/negative weights).
        for spec in spec_map.values() {
            if let FusionMode::Weighted { threshold } = &spec.mode {
                if !threshold.is_finite() || !(0.0..=1.0).contains(threshold) {
                    return Err(DormantError::ConfigInvalid {
                        detail: format!(
                            "zone '{}': weighted threshold must be finite and in 0.0..=1.0, got {}",
                            spec.id, threshold
                        ),
                    });
                }
                for (key, weight) in &spec.weights {
                    if !weight.is_finite() || *weight < 0.0 {
                        return Err(DormantError::ConfigInvalid {
                            detail: format!(
                                "zone '{}': weight for '{}' must be finite and >= 0.0, got {}",
                                spec.id, key, weight
                            ),
                        });
                    }
                }
            }
        }

        // Topological sort (Kahn's algorithm) — also detects cycles.
        let eval_order = topo_sort(&spec_map)?;

        // All inventory sensors start Unavailable at now().
        let now = Timestamp::now();
        let mut sensor_states: HashMap<SensorId, (SensorState, Timestamp)> = HashMap::new();
        for sid in sensor_inventory {
            sensor_states.insert(sid.clone(), (SensorState::Unavailable, now));
        }

        // Initial zone presence: resolve bottom-up.
        let mut zone_presence: HashMap<ZoneId, bool> = HashMap::new();
        for zid in &eval_order {
            let spec = &spec_map[zid];
            let present = resolve_zone(spec, &sensor_states, &zone_presence);
            zone_presence.insert(zid.clone(), present);
        }

        Ok(Self {
            sensor_states,
            specs: spec_map,
            eval_order,
            zone_presence,
            unknown_sensor_warnings: HashSet::new(),
        })
    }

    /// Apply a presence event, updating sensor state and re-resolving affected
    /// zones bottom-up.
    ///
    /// Returns one [`ZoneChange`] per zone whose resolved presence flipped.
    /// Events for sensors not in the inventory are silently ignored (with a
    /// one-time `tracing::warn!` per unknown sensor id).
    pub fn apply(&mut self, ev: &PresenceEvent) -> Vec<ZoneChange> {
        if !self.sensor_states.contains_key(&ev.sensor_id) {
            if self.unknown_sensor_warnings.insert(ev.sensor_id.clone()) {
                tracing::warn!(
                    sensor_id = %ev.sensor_id,
                    "ignoring event from unknown sensor"
                );
            }
            return Vec::new();
        }

        // Update sensor state.
        self.sensor_states
            .insert(ev.sensor_id.clone(), (ev.state, ev.at));

        // Re-resolve zones bottom-up, collecting flips.
        let mut changes: Vec<ZoneChange> = Vec::new();
        for zid in &self.eval_order {
            let spec = &self.specs[zid];
            let new_present = resolve_zone(spec, &self.sensor_states, &self.zone_presence);
            let old_present = self.zone_presence[zid];
            if new_present != old_present {
                self.zone_presence.insert(zid.clone(), new_present);
                changes.push(ZoneChange {
                    zone: zid.clone(),
                    present: new_present,
                    cause: ev.sensor_id.clone(),
                });
            }
        }

        changes
    }

    /// Query the current resolved presence for a zone.
    ///
    /// Returns `None` if the zone id is unknown.
    #[must_use]
    pub fn is_present(&self, id: &ZoneId) -> Option<bool> {
        self.zone_presence.get(id).copied()
    }

    /// Read-only view of all sensor states.
    #[must_use]
    pub fn sensor_states(&self) -> &HashMap<SensorId, (SensorState, Timestamp)> {
        &self.sensor_states
    }

    /// Iterate over all known zone ids (in evaluation order).
    pub fn known_zone_ids(&self) -> impl Iterator<Item = &ZoneId> {
        self.eval_order.iter()
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Topological sort of zones using Kahn's algorithm.
///
/// Returns zones in evaluation order (leaves first). Detects cycles and returns
/// [`DormantError::ZoneCycle`] if one exists.
fn topo_sort(specs: &HashMap<ZoneId, ZoneSpec>) -> Result<Vec<ZoneId>, DormantError> {
    // Build adjacency: for each zone, list of zones that depend on it (its parents).
    // Also compute in-degree (number of zone children that must be resolved first).
    let mut in_degree: HashMap<&ZoneId, usize> = HashMap::new();
    let mut dependents: HashMap<&ZoneId, Vec<&ZoneId>> = HashMap::new();

    for spec in specs.values() {
        in_degree.entry(&spec.id).or_insert(0);
        for member in &spec.members {
            if let ZoneMember::Zone(zid) = member
                && specs.contains_key(zid)
            {
                // zid must be resolved before spec.id, so spec.id depends on zid.
                // zid's out-edge: spec.id is a dependent of zid.
                dependents.entry(zid).or_default().push(&spec.id);
                *in_degree.entry(&spec.id).or_insert(0) += 1;
            }
        }
    }

    // Start with zones that have no zone dependencies (in-degree 0).
    let mut queue: VecDeque<&ZoneId> = VecDeque::new();
    for (zid, deg) in &in_degree {
        if *deg == 0 {
            queue.push_back(zid);
        }
    }

    let mut order: Vec<ZoneId> = Vec::with_capacity(specs.len());
    while let Some(zid) = queue.pop_front() {
        order.push((*zid).clone());
        if let Some(deps) = dependents.get(zid) {
            for parent in deps {
                let deg = in_degree.get_mut(parent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(parent);
                }
            }
        }
    }

    if order.len() != specs.len() {
        // Find a zone involved in the cycle for the error message.
        let cycle_zone = specs
            .keys()
            .find(|zid| !order.contains(*zid))
            .map_or_else(|| "unknown".into(), ToString::to_string);
        return Err(DormantError::ZoneCycle { zone: cycle_zone });
    }

    Ok(order)
}

/// Resolve a zone's presence given current sensor states and already-resolved
/// child zone presence.
fn resolve_zone(
    spec: &ZoneSpec,
    sensor_states: &HashMap<SensorId, (SensorState, Timestamp)>,
    zone_presence: &HashMap<ZoneId, bool>,
) -> bool {
    if spec.members.is_empty() {
        // An empty zone has no members to assert presence.
        // This is a config smell — validation will flag it later.
        return false;
    }

    // Build the boolean for each member.
    let member_bools: Vec<bool> = spec
        .members
        .iter()
        .map(|member| member_effective_bool(member, spec, sensor_states, zone_presence))
        .collect();

    match &spec.mode {
        FusionMode::Any => member_bools.iter().any(|&b| b),
        FusionMode::All => member_bools.iter().all(|&b| b),
        FusionMode::Quorum(n) => {
            #[allow(clippy::cast_possible_truncation)]
            let count = member_bools.iter().filter(|&&b| b).count() as u32;
            count >= *n
        }
        FusionMode::Weighted { threshold } => {
            let total_weight: f32 = spec.members.iter().map(|m| member_weight(m, spec)).sum();
            if total_weight <= 0.0 {
                tracing::debug!(zone = %spec.id, "total weight is zero, zone resolves to absent");
                return false;
            }
            let present_weight: f32 = spec
                .members
                .iter()
                .zip(&member_bools)
                .filter(|&(_, present)| *present)
                .map(|(m, _)| member_weight(m, spec))
                .sum();
            present_weight / total_weight >= *threshold
        }
    }
}

/// Get the effective boolean for a zone member.
fn member_effective_bool(
    member: &ZoneMember,
    spec: &ZoneSpec,
    sensor_states: &HashMap<SensorId, (SensorState, Timestamp)>,
    zone_presence: &HashMap<ZoneId, bool>,
) -> bool {
    match member {
        ZoneMember::Sensor(sid) => {
            let state = sensor_states
                .get(sid)
                .map_or(SensorState::Unavailable, |(s, _)| *s);
            match state {
                SensorState::Present => true,
                SensorState::Absent => false,
                SensorState::Unavailable => match spec.unavailable_policy {
                    UnavailablePolicy::Present => true,
                    UnavailablePolicy::Absent => false,
                },
            }
        }
        ZoneMember::Zone(zid) => zone_presence.get(zid).copied().unwrap_or(false),
    }
}

/// Sensors that participate, as a direct member, in a zone whose
/// `unavailable_policy` is [`UnavailablePolicy::Absent`] and that use the
/// `mqtt` sensor type — the aggressive-blanking hazard case where a broker
/// hiccup or LWT flap (which only ever produces [`SensorState::Unavailable`],
/// never a real absence) would blank the display instead of being treated
/// fail-safe.
///
/// Returns one `(zone, sensor)` pair per matching direct zone membership; a
/// sensor referenced by several absent-policy zones appears once per zone.
/// Only direct `ZoneMember::Sensor` members are considered — nested zones are
/// not recursively expanded, since a zone's own `unavailable_policy` governs
/// its own direct sensor members, and a nested zone's members are covered by
/// that nested zone's own entry in `cfg.zones`.
#[must_use]
pub fn absent_mqtt_hazards(cfg: &crate::config::Config) -> Vec<(ZoneId, SensorId)> {
    let mut hazards = Vec::new();

    for (zone_id, zc) in &cfg.zones {
        if zc.unavailable_policy != UnavailablePolicy::Absent {
            continue;
        }
        for raw in &zc.members {
            if raw.starts_with("zone:") {
                continue;
            }
            if let Some(crate::config::SensorConfig::Mqtt(_)) = cfg.sensors.get(raw) {
                hazards.push((ZoneId(zone_id.clone()), SensorId(raw.clone())));
            }
        }
    }

    hazards
}

/// Get the weight for a zone member (defaults to 1.0).
fn member_weight(member: &ZoneMember, spec: &ZoneSpec) -> f32 {
    let key = match member {
        ZoneMember::Sensor(sid) => sid.to_string(),
        ZoneMember::Zone(zid) => zid.to_string(),
    };
    spec.weights.get(&key).copied().unwrap_or(1.0)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use crate::error::E_ZONE_CYCLE;
    use crate::error::E_ZONE_UNKNOWN_MEMBER;

    /// Helper: create a sensor id.
    fn sid(name: &str) -> SensorId {
        SensorId(name.into())
    }

    /// Helper: create a zone id.
    fn zid(name: &str) -> ZoneId {
        ZoneId(name.into())
    }

    /// Helper: a presence event at a fixed timestamp.
    fn event(sensor: &str, state: SensorState) -> PresenceEvent {
        PresenceEvent {
            sensor_id: sid(sensor),
            state,
            confidence: 1.0,
            at: Timestamp::now(),
        }
    }

    /// Helper: a zone spec builder.
    fn spec(id: &str, mode: FusionMode, members: Vec<ZoneMember>) -> ZoneSpec {
        ZoneSpec {
            id: zid(id),
            mode,
            members,
            weights: HashMap::new(),
            unavailable_policy: UnavailablePolicy::Present,
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[test]
    fn any_zone_present_when_one_sensor_present() {
        let inventory = [sid("a"), sid("b")];
        let mut engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
            )],
            &inventory,
        )
        .unwrap();

        // Both start unavailable → present (fail-safe).
        assert!(engine.is_present(&zid("z")).unwrap());

        // Sensor a goes absent.
        let changes = engine.apply(&event("a", SensorState::Absent));
        // b is still unavailable → present, so zone stays present.
        assert!(changes.is_empty());
        assert!(engine.is_present(&zid("z")).unwrap());

        // Both absent.
        engine.apply(&event("b", SensorState::Absent));
        assert!(!engine.is_present(&zid("z")).unwrap());

        // a returns.
        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].zone, zid("z"));
        assert!(changes[0].present);
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn unavailable_defaults_to_present_failsafe() {
        let inventory = [sid("a")];
        let engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Sensor(sid("a"))],
            )],
            &inventory,
        )
        .unwrap();

        // Sensor starts Unavailable → treated as Present by default policy.
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn unavailable_policy_absent_lets_zone_clear() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Any,
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Sensor starts Unavailable → treated as Absent → zone absent.
        assert!(!engine.is_present(&zid("z")).unwrap());

        // Sensor reports present.
        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
        assert!(changes[0].present);
        assert!(engine.is_present(&zid("z")).unwrap());

        // Sensor goes unavailable again.
        let changes = engine.apply(&event("a", SensorState::Unavailable));
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].present);
        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn quorum_two_of_three() {
        let inventory = [sid("a"), sid("b"), sid("c")];
        let mut engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Quorum(2),
                vec![
                    ZoneMember::Sensor(sid("a")),
                    ZoneMember::Sensor(sid("b")),
                    ZoneMember::Sensor(sid("c")),
                ],
            )],
            &inventory,
        )
        .unwrap();

        // All start unavailable → present (fail-safe), so quorum met.
        assert!(engine.is_present(&zid("z")).unwrap());

        // Make all absent.
        engine.apply(&event("a", SensorState::Absent));
        engine.apply(&event("b", SensorState::Absent));
        engine.apply(&event("c", SensorState::Absent));
        assert!(!engine.is_present(&zid("z")).unwrap());

        // One present → still not quorum(2).
        engine.apply(&event("a", SensorState::Present));
        assert!(!engine.is_present(&zid("z")).unwrap());

        // Two present → quorum met.
        engine.apply(&event("b", SensorState::Present));
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn weighted_threshold_crossing_emits_change() {
        let inventory = [sid("a"), sid("b")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.6 },
                members: vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
                weights: [("a".into(), 2.0), ("b".into(), 1.0)].into(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Both start unavailable → absent (policy), so zone absent.
        assert!(!engine.is_present(&zid("z")).unwrap());

        // a present (weight 2) / total 3 = 0.666 >= 0.6 → present.
        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
        assert!(changes[0].present);
        assert!(engine.is_present(&zid("z")).unwrap());

        // a absent, b present: weight 1 / 3 = 0.333 < 0.6 → absent.
        let changes = engine.apply(&event("a", SensorState::Absent));
        assert_eq!(changes.len(), 1);
        assert!(!changes[0].present);
        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn nested_zone_propagates_flip_to_parent() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![
                ZoneSpec {
                    id: zid("child"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(sid("a"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Absent,
                },
                ZoneSpec {
                    id: zid("parent"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("child"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
            ],
            &inventory,
        )
        .unwrap();

        // child: unavailable → absent (policy). parent: child absent → parent absent.
        assert!(!engine.is_present(&zid("child")).unwrap());
        assert!(!engine.is_present(&zid("parent")).unwrap());

        // Sensor present → child present → parent present.
        let changes = engine.apply(&event("a", SensorState::Present));
        // Both child and parent flip.
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.zone == zid("child") && c.present));
        assert!(changes.iter().any(|c| c.zone == zid("parent") && c.present));
    }

    #[test]
    fn cycle_detection_rejects_self_referencing_zones() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![
                ZoneSpec {
                    id: zid("a"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("b"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
                ZoneSpec {
                    id: zid("b"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("a"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
            ],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().starts_with(E_ZONE_CYCLE),
            "expected E_ZONE_CYCLE error"
        );
    }

    #[test]
    fn apply_returns_only_flipped_zones() {
        let inventory = [sid("a"), sid("b")];
        let mut engine = ZoneEngine::new(
            vec![
                ZoneSpec {
                    id: zid("z1"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(sid("a"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Absent,
                },
                ZoneSpec {
                    id: zid("z2"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(sid("b"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Absent,
                },
            ],
            &inventory,
        )
        .unwrap();

        // Both start absent.
        assert!(!engine.is_present(&zid("z1")).unwrap());
        assert!(!engine.is_present(&zid("z2")).unwrap());

        // Event for sensor a — only z1 flips.
        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].zone, zid("z1"));
        assert!(changes[0].present);

        // Same event again — no flip.
        let changes = engine.apply(&event("a", SensorState::Present));
        assert!(changes.is_empty());
    }

    #[test]
    fn never_seen_sensor_is_unavailable_failsafe_from_start() {
        let inventory = [sid("a")];
        let engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Sensor(sid("a"))],
            )],
            &inventory,
        )
        .unwrap();

        // Sensor never received an event → Unavailable → Present (fail-safe).
        assert!(engine.is_present(&zid("z")).unwrap());

        // Check sensor_states view.
        let states = engine.sensor_states();
        let (state, _ts) = &states[&sid("a")];
        assert_eq!(*state, SensorState::Unavailable);
    }

    #[test]
    fn unknown_member_ref_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Sensor(sid("nonexistent"))],
            )],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with(E_ZONE_UNKNOWN_MEMBER),
            "expected E_ZONE_UNKNOWN_MEMBER error"
        );
    }

    #[test]
    fn unknown_event_sensor_ignored_without_panic() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Sensor(sid("a"))],
            )],
            &inventory,
        )
        .unwrap();

        // Event for sensor not in inventory.
        let changes = engine.apply(&event("unknown", SensorState::Present));
        assert!(changes.is_empty());

        // Known sensor still works — first make it absent so we get a flip.
        engine.apply(&event("a", SensorState::Absent));
        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn empty_members_any_zone_is_absent() {
        let inventory: [SensorId; 0] = [];
        let engine = ZoneEngine::new(vec![spec("z", FusionMode::Any, vec![])], &inventory).unwrap();

        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn empty_members_all_zone_is_absent() {
        let inventory: [SensorId; 0] = [];
        let engine = ZoneEngine::new(vec![spec("z", FusionMode::All, vec![])], &inventory).unwrap();

        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn duplicate_zone_id_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![
                spec("z", FusionMode::Any, vec![ZoneMember::Sensor(sid("a"))]),
                spec("z", FusionMode::All, vec![ZoneMember::Sensor(sid("a"))]),
            ],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn unknown_zone_member_ref_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::Any,
                vec![ZoneMember::Zone(zid("nonexistent"))],
            )],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with(E_ZONE_UNKNOWN_MEMBER)
        );
    }

    #[test]
    fn self_loop_zone_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("a"),
                mode: FusionMode::Any,
                members: vec![ZoneMember::Zone(zid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().starts_with(E_ZONE_CYCLE));
    }

    #[test]
    fn weighted_zero_total_weight_resolves_absent() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: [("a".into(), 0.0)].into(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Total weight is 0 → zone absent.
        assert!(!engine.is_present(&zid("z")).unwrap());

        // Even with sensor present, total weight still 0.
        engine.apply(&event("a", SensorState::Present));
        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn all_zone_requires_all_present() {
        let inventory = [sid("a"), sid("b")];
        let mut engine = ZoneEngine::new(
            vec![spec(
                "z",
                FusionMode::All,
                vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
            )],
            &inventory,
        )
        .unwrap();

        // Both start unavailable → present (fail-safe) → all present.
        assert!(engine.is_present(&zid("z")).unwrap());

        // One absent → all absent.
        engine.apply(&event("a", SensorState::Absent));
        assert!(!engine.is_present(&zid("z")).unwrap());

        // Both present.
        engine.apply(&event("a", SensorState::Present));
        engine.apply(&event("b", SensorState::Present));
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn cause_is_event_sensor_id() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Any,
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Start absent (unavailable → absent policy).
        assert!(!engine.is_present(&zid("z")).unwrap());

        let changes = engine.apply(&event("a", SensorState::Present));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, sid("a"));
    }

    // ── Must: NaN / out-of-range weighted config rejection ──────────────────

    #[test]
    fn weighted_nan_threshold_rejected_at_construction() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted {
                    threshold: f32::NAN,
                },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn weighted_nan_weight_rejected_at_construction() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: [("a".into(), f32::NAN)].into(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn weighted_negative_weight_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: [("a".into(), -1.0)].into(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn weighted_threshold_above_one_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 1.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn weighted_infinite_threshold_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted {
                    threshold: f32::INFINITY,
                },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    #[test]
    fn weighted_infinite_weight_rejected() {
        let inventory = [sid("a")];
        let result = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: [("a".into(), f32::INFINITY)].into(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("E_CONFIG_INVALID")
        );
    }

    // ── Should: weighted edge cases ─────────────────────────────────────────

    #[test]
    fn weighted_exact_threshold_equality_is_present() {
        let inventory = [sid("a")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.5 },
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(), // a defaults to 1.0
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // a present → 1.0/1.0 = 1.0 >= 0.5 → present.
        engine.apply(&event("a", SensorState::Present));
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn weighted_threshold_zero_always_present_with_any_member() {
        let inventory = [sid("a"), sid("b")];
        let engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 0.0 },
                members: vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Both absent → 0.0/2.0 = 0.0 >= 0.0 → present (trivially satisfied).
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn weighted_threshold_one_requires_all() {
        let inventory = [sid("a"), sid("b")];
        let mut engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Weighted { threshold: 1.0 },
                members: vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Both absent → 0.0 < 1.0 → absent.
        assert!(!engine.is_present(&zid("z")).unwrap());

        // One present → 0.5 < 1.0 → absent.
        engine.apply(&event("a", SensorState::Present));
        assert!(!engine.is_present(&zid("z")).unwrap());

        // Both present → 1.0 >= 1.0 → present.
        engine.apply(&event("b", SensorState::Present));
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    // ── Should: quorum edge cases ───────────────────────────────────────────

    #[test]
    fn quorum_zero_is_always_present() {
        let inventory = [sid("a")];
        let engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Quorum(0),
                members: vec![ZoneMember::Sensor(sid("a"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Absent,
            }],
            &inventory,
        )
        .unwrap();

        // Sensor absent, but quorum(0) is trivially satisfied.
        assert!(engine.is_present(&zid("z")).unwrap());
    }

    #[test]
    fn quorum_exceeding_member_count_never_present() {
        let inventory = [sid("a"), sid("b")];
        let engine = ZoneEngine::new(
            vec![ZoneSpec {
                id: zid("z"),
                mode: FusionMode::Quorum(10),
                members: vec![ZoneMember::Sensor(sid("a")), ZoneMember::Sensor(sid("b"))],
                weights: HashMap::new(),
                unavailable_policy: UnavailablePolicy::Present,
            }],
            &inventory,
        )
        .unwrap();

        // Both present (fail-safe), but 2 < 10.
        assert!(!engine.is_present(&zid("z")).unwrap());
    }

    // ── Should: diamond graph ───────────────────────────────────────────────

    #[test]
    fn diamond_zone_graph_is_accepted() {
        // a → b, a → c, b → d, c → d  (diamond: no cycle)
        let inventory = [sid("s1")];
        let engine = ZoneEngine::new(
            vec![
                ZoneSpec {
                    id: zid("d"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("b")), ZoneMember::Zone(zid("c"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
                ZoneSpec {
                    id: zid("b"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("a"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
                ZoneSpec {
                    id: zid("c"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Zone(zid("a"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
                ZoneSpec {
                    id: zid("a"),
                    mode: FusionMode::Any,
                    members: vec![ZoneMember::Sensor(sid("s1"))],
                    weights: HashMap::new(),
                    unavailable_policy: UnavailablePolicy::Present,
                },
            ],
            &inventory,
        );
        assert!(engine.is_ok());
    }

    // ── absent_mqtt_hazards ─────────────────────────────────────────────────

    #[test]
    fn absent_mqtt_hazards_matrix() {
        use crate::config::Config;
        use crate::config::schema::{HaSensorCfg, MqttSensorCfg, SensorConfig, SensorKind};
        use indexmap::IndexMap;

        fn mqtt_cfg(topic: &str) -> MqttSensorCfg {
            MqttSensorCfg {
                broker_url: "tcp://localhost:1883".into(),
                topic: topic.into(),
                field: "/occupancy".into(),
                payload_on: None,
                payload_off: None,
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
                availability_topic: None,
                availability_payload_online: "online".into(),
                availability_payload_offline: "offline".into(),
            }
        }

        fn zone_cfg(
            members: Vec<&str>,
            policy: UnavailablePolicy,
        ) -> crate::config::schema::ZoneConfig {
            crate::config::schema::ZoneConfig {
                mode: "any".into(),
                members: members.into_iter().map(String::from).collect(),
                quorum: None,
                threshold: None,
                weights: IndexMap::new(),
                unavailable_policy: policy,
            }
        }

        let mut sensors: IndexMap<String, SensorConfig> = IndexMap::new();
        sensors.insert("mq1".into(), SensorConfig::Mqtt(mqtt_cfg("t1")));
        sensors.insert(
            "ha1".into(),
            SensorConfig::Ha(HaSensorCfg {
                url: "ws://ha.local:8123/api/websocket".into(),
                entity: "binary_sensor.x".into(),
                kind: SensorKind::Presence,
                hold_time: None,
                stale_timeout: None,
            }),
        );

        let mut zones = IndexMap::new();
        zones.insert(
            "absent_mqtt".to_string(),
            zone_cfg(vec!["mq1"], UnavailablePolicy::Absent),
        );
        zones.insert(
            "default_mqtt".to_string(),
            zone_cfg(vec!["mq1"], UnavailablePolicy::Present),
        );
        zones.insert(
            "absent_ha_only".to_string(),
            zone_cfg(vec!["ha1"], UnavailablePolicy::Absent),
        );

        let cfg = Config {
            config_version: 1,
            daemon: crate::config::DaemonConfig::default(),
            sensors,
            zones,
            displays: IndexMap::new(),
            rules: IndexMap::new(),
            wear: crate::config::schema::WearConfig::default(),
            notifications: crate::config::schema::NotificationsConfig::default(),
            audio: crate::config::schema::AudioConfig::default(),
        };

        let hazards = absent_mqtt_hazards(&cfg);
        assert_eq!(
            hazards,
            vec![(ZoneId("absent_mqtt".into()), SensorId("mq1".into()))],
            "expected exactly one hazard pair (absent-policy zone + mqtt sensor), got {hazards:?}"
        );
    }
}
