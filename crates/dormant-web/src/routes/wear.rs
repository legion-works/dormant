//! `GET /api/wear` + `GET /api/wear/:display` — panel-exposure (wear) view.
//!
//! Reads directly from the shared [`dormant_core::wear::WearHandle`] the
//! wear tracker (`dormantd::wear_tracker`) populates — this route does NOT
//! go through the engine's `ControlMsg` channel, mirroring the doctor
//! route's direct-read pattern (spec §5.1's "no engine round-trip for
//! read-only diagnostics" ethos).
//!
//! `advisory` is **server-derived** here (not merely relayed from a WS
//! event) so a fresh `GET /api/wear` is always the truth, even if the
//! browser missed the `compensation_advisory` WS nudge. The formula is the
//! SAME shared implementation `dormantd::wear_tracker::tick` calls for its
//! "Advisory (observed vs baseline, ONCE latch)" check —
//! [`dormant_core::wear::advisory_active`] / [`hours_since_effective_dwell`]
//! (`dormant_core::wear::hours_since_effective_dwell`) — not an
//! independently-derived copy (see review finding W1), except this route
//! recomputes statelessly per request rather than latching once.

use axum::Json;
use axum::extract::{Path, State};
use dormant_core::wear::{
    PanelType, WearAttributionMode, WearLedger, WearSamplingStatus, advisory_active,
    hours_since_effective_dwell,
};
use std::collections::BTreeMap;

use crate::WebState;
use crate::error::WebError;

/// Per-display wear summary (spec §7.3's honesty-rule fields — no spatial
/// attribution here, just the panel-wide totals + advisory flag).
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub(crate) struct WearSummary {
    /// The [`dormant_core::wear::WearHandle`] map key (the tracker's
    /// resolved `storage_key` — panel identity when available, else the
    /// sanitized config display key).
    pub(crate) display: String,
    /// Human-readable display name.
    pub(crate) display_name: String,
    /// The `[displays.*]` config id this ledger is attributed to, when
    /// known.  The frontend joins on this field first, falling back to
    /// `display_name` for backward compatibility with pre-BG-8 ledgers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_display_id: Option<String>,
    /// Panel technology classification.
    pub(crate) panel_type: PanelType,
    /// Cumulative brightness-weighted on-hours.
    pub(crate) total_on_hours: f64,
    /// Operator-seeded prior usage, in hours, if any.
    pub(crate) seeded_usage_hours: Option<u32>,
    /// Number of samples folded into `total_on_hours`.
    pub(crate) sample_count: u64,
    /// Epoch seconds of the most recent sample, if any.
    pub(crate) last_sample_at_epoch_s: Option<u64>,
    /// Epoch seconds of the most recent long-dwell (dark) window, if any.
    pub(crate) last_long_dwell_epoch_s: Option<u64>,
    /// `true` when this display has gone longer than `[wear].advisory_after`
    /// since its last long-dwell window (or, absent one, since the ledger's
    /// creation baseline) — server-derived truth, independent of any WS
    /// nudge the client may have missed.
    pub(crate) advisory: bool,
    /// Hours since `max(last_long_dwell_epoch_s, advisory_baseline_epoch_s)`
    /// — the SAME derivation `dormantd::wear_tracker::tick` uses for
    /// `DaemonEvent::CompensationAdvisory::hours_since_long_dwell`. Exposed
    /// unconditionally (not just when `advisory` is true) so the client has
    /// a real day count even for a display that has never had an observed
    /// long dwell yet (baseline-only — the common first-load case), instead
    /// of rendering "no long standby window in ? days".
    pub(crate) hours_since_long_dwell: u64,
    /// Attribution method used for the summary's current observation.
    pub(crate) wear_attribution_mode: WearAttributionMode,
    /// Consent grant time when this display is content-weighted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_weighted_since: Option<i64>,
    /// Stable source-gate tag for this display (`"matched"`, `"mismatched"`,
    /// or `"unknown"`). `None` when the display carries no gate
    /// configuration, or when no per-display status is selected for this
    /// display (absent from a populated map). Additive: absent on the wire
    /// when `None` so older UIs keep parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_gate: Option<String>,
    /// Stable reason the current interval is uniform while sampling is
    /// degraded (e.g. `"source_mismatch"`, `"source_unknown"`). Set only
    /// when a status belonging to this display reports one. Additive:
    /// absent on the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uniform_reason: Option<String>,
}

/// `GET /api/wear` response envelope.
#[derive(serde::Serialize, Debug)]
pub(crate) struct WearListResponse {
    pub(crate) displays: Vec<WearSummary>,
}

/// `GET /api/wear/:display` response — summary plus the per-cell grid.
#[derive(serde::Serialize, Debug)]
pub(crate) struct WearDetail {
    #[serde(flatten)]
    pub(crate) summary: WearSummary,
    /// Grid row count (so the client can reshape `cells`/`heat`).
    pub(crate) grid_rows: u16,
    /// Grid column count.
    pub(crate) grid_cols: u16,
    /// Raw per-cell brightness-weighted on-hours, row-major,
    /// length `grid_rows * grid_cols`.
    pub(crate) cells: Vec<f64>,
    /// Zero-max normalized per-cell heat (`0.0..=1.0`), row-major, same
    /// length as `cells` — see [`WearLedger::heat_map`]. The denominator
    /// is `max_cell_hours` below, so the client can derive real on-hours
    /// from `heat * max_cell_hours` if needed; we expose the absolute
    /// `cells` separately so the UI never has to.
    pub(crate) heat: Vec<f32>,
    /// Maximum per-cell on-hours in the grid — the denominator the heat
    /// map was normalized against. `0.0` when no cell has any recorded
    /// exposure (i.e. the heat map is also all-zero — the "no data"
    /// signal). Exposed so the legend can label real hours instead of
    /// inferring them from the normalized heat.
    pub(crate) max_cell_hours: f64,
}

/// Current wall-clock time as epoch seconds; `0` if the clock is somehow
/// before the epoch (never in practice — defensive only).
fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn summarize(
    key: &str,
    ledger: &WearLedger,
    advisory_after: std::time::Duration,
    now_epoch_s: u64,
    per_display: &BTreeMap<String, WearSamplingStatus>,
    legacy_singular: Option<&WearSamplingStatus>,
) -> WearSummary {
    // Select this display's status by `config_display_id` — the same join
    // key `content_weighted_since` uses, never wear-storage row order (#201
    // display-name join bug class: the storage key may be panel identity,
    // not the config display id). The per-display map is authoritative; the
    // legacy singular status is a fallback ONLY when the map is empty. A
    // display absent from a NON-empty map gets `None` (no gate, no reason)
    // — falling back there would cross-attribute another display's gate.
    let selected = ledger
        .identity
        .config_display_id
        .as_deref()
        .and_then(|id| per_display.get(id))
        .or_else(|| {
            if per_display.is_empty() {
                // Empty map -> legacy singular fallback. The singular status
                // belongs to this display only when its bound_display matches
                // (uniform_reason is set solely for a status that belongs).
                legacy_singular.filter(|s| {
                    s.bound_display.as_deref() == ledger.identity.config_display_id.as_deref()
                })
            } else {
                None
            }
        });

    let content_weighted_since = selected.and_then(|status| {
        (status.bound_display.as_deref() == ledger.identity.config_display_id.as_deref())
            .then_some(status.granted_at_epoch_s)
            .flatten()
    });
    let source_gate = selected.and_then(|s| s.source_gate.clone());
    let uniform_reason = selected.and_then(|s| s.uniform_reason.clone());

    WearSummary {
        display: key.to_string(),
        display_name: ledger.identity.display_name.clone(),
        config_display_id: ledger.identity.config_display_id.clone(),
        panel_type: ledger.panel_type,
        total_on_hours: ledger.total_on_hours,
        seeded_usage_hours: ledger.seeded_usage_hours,
        sample_count: ledger.sample_count,
        last_sample_at_epoch_s: ledger.last_sample_at_epoch_s,
        last_long_dwell_epoch_s: ledger.last_long_dwell_epoch_s,
        advisory: advisory_active(
            ledger.last_long_dwell_epoch_s,
            ledger.advisory_baseline_epoch_s,
            advisory_after,
            now_epoch_s,
        ),
        hours_since_long_dwell: hours_since_effective_dwell(
            ledger.last_long_dwell_epoch_s,
            ledger.advisory_baseline_epoch_s,
            now_epoch_s,
        ),
        wear_attribution_mode: if content_weighted_since.is_some() {
            WearAttributionMode::Sampled
        } else {
            WearAttributionMode::Uniform
        },
        content_weighted_since,
        source_gate,
        uniform_reason,
    }
}

/// `GET /api/wear` — every tracked display's panel-exposure summary.
///
/// Reads the shared [`dormant_core::wear::WearHandle`] directly (a
/// `RwLock`, never expected to be poisoned in practice — a panicking
/// holder would already have brought down the wear tracker task). On the
/// defensive poison path, returns an empty list rather than propagating a
/// panic into an HTTP 500.
pub(crate) async fn get_wear(State(state): State<WebState>) -> Json<WearListResponse> {
    let advisory_after = state.inner.config_rx.borrow().wear.advisory_after;
    let per_display = state.inner.per_display_statuses_rx.borrow().clone();
    let legacy_singular = state.inner.wear_sampling_rx.borrow().clone();
    let now = now_epoch_s();

    let Ok(guard) = state.inner.wear.read() else {
        return Json(WearListResponse {
            displays: Vec::new(),
        });
    };

    let mut displays: Vec<WearSummary> = guard
        .iter()
        .map(|(key, ledger)| {
            summarize(
                key,
                ledger,
                advisory_after,
                now,
                &per_display,
                legacy_singular.as_ref(),
            )
        })
        .collect();
    // Deterministic ordering for a stable UI list / test assertions.
    displays.sort_by(|a, b| a.display.cmp(&b.display));

    Json(WearListResponse { displays })
}

/// `GET /api/wear/:display` — one display's summary plus its wear grid.
///
/// # Errors
///
/// Returns [`WebError::UnknownDisplay`] (404) when `display` is not a
/// known [`dormant_core::wear::WearHandle`] key.
pub(crate) async fn get_wear_detail(
    State(state): State<WebState>,
    Path(display): Path<String>,
) -> Result<Json<WearDetail>, WebError> {
    let advisory_after = state.inner.config_rx.borrow().wear.advisory_after;
    let now = now_epoch_s();

    let Ok(guard) = state.inner.wear.read() else {
        return Err(WebError::UnknownDisplay(display));
    };

    let ledger = guard
        .get(&display)
        .ok_or_else(|| WebError::UnknownDisplay(display.clone()))?;

    let per_display = state.inner.per_display_statuses_rx.borrow().clone();
    let legacy_singular = state.inner.wear_sampling_rx.borrow().clone();
    let summary = summarize(
        &display,
        ledger,
        advisory_after,
        now,
        &per_display,
        legacy_singular.as_ref(),
    );
    let cells: Vec<f64> = ledger.cells.iter().map(|c| c.wear_hours).collect();
    // Compute the max once and reuse it: it's the denominator the
    // heat map was normalized against, AND it's what the legend
    // labels (a single fold keeps the value bit-identical to the
    // heat map's own max computation — see `heat_map`).
    let max_cell_hours = cells.iter().copied().fold(0.0_f64, f64::max);
    let heat = ledger.heat_map();

    Ok(Json(WearDetail {
        summary,
        grid_rows: ledger.grid_rows,
        grid_cols: ledger.grid_cols,
        cells,
        heat,
        max_cell_hours,
    }))
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use dormant_core::config::schema::{Config, Credentials, DaemonConfig, WearConfig};
    use dormant_core::wear::{WearIdentity, WearLedger};
    use indexmap::IndexMap;
    use std::collections::{BTreeMap, HashMap};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use tower::util::ServiceExt;

    fn ledger_with(
        key: &str,
        display_name: &str,
        panel_type: PanelType,
        advisory_baseline_epoch_s: u64,
        last_long_dwell_epoch_s: Option<u64>,
    ) -> WearLedger {
        let mut ledger = WearLedger::new(
            WearIdentity {
                key: key.to_string(),
                display_name: display_name.to_string(),
                config_display_id: None,
            },
            panel_type,
            2,
            3,
            advisory_baseline_epoch_s,
        );
        ledger.attribute_uniform(Duration::from_secs(3600), 1.0);
        ledger.last_sample_at_epoch_s = Some(advisory_baseline_epoch_s + 10);
        ledger.last_long_dwell_epoch_s = last_long_dwell_epoch_s;
        ledger
    }

    #[test]
    fn content_weighted_since_requires_matching_bound_display() {
        let mut ledger = ledger_with("desk", "Desk", PanelType::Woled, 100, None);
        ledger.identity.config_display_id = Some("desk".to_owned());
        let matching = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(5),
            uniform_reason: None,
            bound_display: Some("desk".to_owned()),
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: None,
        };
        let non_matching = dormant_core::wear::WearSamplingStatus {
            bound_display: Some("other".to_owned()),
            ..matching.clone()
        };
        let absent_consent = dormant_core::wear::WearSamplingStatus {
            granted_at_epoch_s: None,
            ..matching.clone()
        };

        // Per-display map keyed by the ledger's config_display_id — the
        // join the route uses. `non_matching` is keyed under "desk" but
        // bound to "other", so the bound_display gate still suppresses
        // content_weighted_since.
        let map_for = |status: dormant_core::wear::WearSamplingStatus| {
            let mut m = BTreeMap::new();
            m.insert("desk".to_string(), status);
            m
        };

        assert_eq!(
            summarize(
                "desk",
                &ledger,
                Duration::from_secs(1),
                200,
                &map_for(matching.clone()),
                None
            )
            .content_weighted_since,
            Some(1_700_000_000)
        );
        assert!(
            summarize(
                "desk",
                &ledger,
                Duration::from_secs(1),
                200,
                &map_for(non_matching),
                None
            )
            .content_weighted_since
            .is_none()
        );
        assert!(
            summarize(
                "desk",
                &ledger,
                Duration::from_secs(1),
                200,
                &map_for(absent_consent),
                None
            )
            .content_weighted_since
            .is_none()
        );
    }

    fn test_state_with(
        wear: HashMap<String, WearLedger>,
        wear_cfg: WearConfig,
        bind: SocketAddr,
    ) -> WebState {
        test_state_with_sampling(wear, wear_cfg, bind, BTreeMap::new(), None)
    }

    /// Like [`test_state_with`] but also injects the per-display sampler
    /// status map and the legacy singular status. The per-display map is
    /// the authoritative source the wear route joins on; the legacy
    /// singular status is the fallback ONLY when the per-display map is
    /// empty (issue #185 cycle B / Task 11).
    #[allow(clippy::type_complexity)]
    fn test_state_with_sampling(
        wear: HashMap<String, WearLedger>,
        wear_cfg: WearConfig,
        bind: SocketAddr,
        per_display: BTreeMap<String, dormant_core::wear::WearSamplingStatus>,
        legacy_singular: Option<dormant_core::wear::WearSamplingStatus>,
    ) -> WebState {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) =
            mpsc::channel::<dormant_core::reload::ReloadRequest>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
            coordination: dormant_core::config::CoordinationConfig::default(),
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: wear_cfg,
            notifications: dormant_core::config::schema::NotificationsConfig::default(),
            watchdog: dormant_core::config::schema::WatchdogConfig::default(),
            audio: dormant_core::config::schema::AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
            keymap: dormant_core::config::KeymapConfig::default(),
            input_filter: dormant_core::config::InputFilterConfig::default(),
            publish: dormant_core::config::PublishConfig::default(),
        });
        let creds = Arc::new(Credentials::default());
        let (config_tx, config_rx) = watch::channel(config);
        let (creds_tx, creds_rx) = watch::channel(creds);
        let cancel = CancellationToken::new();

        std::mem::forget(reload_tx);
        std::mem::forget(config_tx);
        std::mem::forget(creds_tx);

        let doctor =
            dormant_doctor::DoctorService::new(ctl_tx.clone(), config_rx.clone(), creds_rx.clone());

        WebState::new(crate::state::WebStateInner::new_for_test(
            crate::state::WebStateInnerParams {
                ctl_tx,
                reload_requester: dormant_core::reload::ReloadRequester::new(reload_trigger_tx),
                reload_rx,
                config_rx,
                creds_rx,
                config_path: std::path::PathBuf::from("/dev/null"),
                creds_path: std::path::PathBuf::from("/dev/null"),
                doctor,
                wear: Arc::new(RwLock::new(wear)),
                web_bind: bind,
                cancel,
                reload_timeout: Duration::from_secs(10),
                wear_sampling_rx: tokio::sync::watch::channel(legacy_singular).1,
                per_display_statuses_rx: tokio::sync::watch::channel(per_display).1,
            },
        ))
    }

    const BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

    // ── GET /api/wear — shape + advisory derivation ────────────────────────

    #[tokio::test]
    async fn get_wear_returns_summary_shape_for_each_display() {
        let now = now_epoch_s();
        let mut wear = HashMap::new();
        wear.insert(
            "ddc-aoc-1234".to_string(),
            ledger_with(
                "ddc-aoc-1234",
                "Living Room TV",
                PanelType::QdOled,
                now,
                None,
            ),
        );
        let state = test_state_with(wear, WearConfig::default(), BIND);

        let Json(resp) = get_wear(State(state)).await;
        assert_eq!(resp.displays.len(), 1);
        let s = &resp.displays[0];
        assert_eq!(s.display, "ddc-aoc-1234");
        assert_eq!(s.display_name, "Living Room TV");
        assert_eq!(s.panel_type, PanelType::QdOled);
        assert!((s.total_on_hours - 1.0).abs() < 1e-9);
        assert_eq!(s.seeded_usage_hours, None);
        assert_eq!(s.sample_count, 1);
        assert!(s.last_sample_at_epoch_s.is_some());
        assert_eq!(s.last_long_dwell_epoch_s, None);
        assert!(
            !s.advisory,
            "freshly-baselined ledger must not be advisory yet"
        );
        assert_eq!(
            s.hours_since_long_dwell, 0,
            "a ledger baselined at `now` must report ~0 hours since reference"
        );
    }

    #[tokio::test]
    async fn get_wear_hours_since_long_dwell_uses_baseline_when_no_dwell_observed_yet() {
        // T8 review Should-fix: a display that has NEVER had an observed
        // long dwell (baseline-only — the common first-load case) must
        // still report a real `hours_since_long_dwell` count derived from
        // `advisory_baseline_epoch_s`, not merely omit/null the field.
        let now = now_epoch_s();
        let old_baseline = now.saturating_sub(3 * 3600); // 3h ago, no dwell ever observed
        let mut wear = HashMap::new();
        wear.insert(
            "baseline-only".to_string(),
            ledger_with(
                "baseline-only",
                "Baseline Only Panel",
                PanelType::Unknown,
                old_baseline,
                None,
            ),
        );
        let state = test_state_with(wear, WearConfig::default(), BIND);

        let Json(resp) = get_wear(State(state)).await;
        assert_eq!(resp.displays.len(), 1);
        assert_eq!(
            resp.displays[0].hours_since_long_dwell, 3,
            "baseline-only ledger must derive hours_since_long_dwell from advisory_baseline_epoch_s"
        );
        assert_eq!(resp.displays[0].last_long_dwell_epoch_s, None);
    }

    #[tokio::test]
    async fn get_wear_hours_since_long_dwell_uses_observed_dwell_when_more_recent_than_baseline() {
        let now = now_epoch_s();
        let old_baseline = now.saturating_sub(10 * 3600);
        let recent_dwell = now.saturating_sub(2 * 3600);
        let mut wear = HashMap::new();
        wear.insert(
            "observed".to_string(),
            ledger_with(
                "observed",
                "Observed Panel",
                PanelType::Woled,
                old_baseline,
                Some(recent_dwell),
            ),
        );
        let state = test_state_with(wear, WearConfig::default(), BIND);

        let Json(resp) = get_wear(State(state)).await;
        assert_eq!(
            resp.displays[0].hours_since_long_dwell, 2,
            "a more-recent observed dwell must win over the older baseline"
        );
    }

    #[tokio::test]
    async fn get_wear_advisory_true_when_baseline_older_than_advisory_after() {
        let cfg = WearConfig {
            advisory_after: Duration::from_secs(3600), // 1h
            ..WearConfig::default()
        };
        let now = now_epoch_s();
        // Baseline 2h in the past, no observed long-dwell -> since_s = 2h > 1h.
        let old_baseline = now.saturating_sub(7200);
        let mut wear = HashMap::new();
        wear.insert(
            "stale".to_string(),
            ledger_with("stale", "Stale Panel", PanelType::Woled, old_baseline, None),
        );
        let state = test_state_with(wear, cfg, BIND);

        let Json(resp) = get_wear(State(state)).await;
        assert_eq!(resp.displays.len(), 1);
        assert!(
            resp.displays[0].advisory,
            "ledger older than advisory_after with no recent long-dwell must be advisory=true"
        );
    }

    #[tokio::test]
    async fn get_wear_advisory_false_when_recent_long_dwell_resets_reference() {
        let cfg = WearConfig {
            advisory_after: Duration::from_secs(3600),
            ..WearConfig::default()
        };
        let now = now_epoch_s();
        let old_baseline = now.saturating_sub(7200);
        let mut wear = HashMap::new();
        wear.insert(
            "recovered".to_string(),
            ledger_with(
                "recovered",
                "Recovered Panel",
                PanelType::Unknown,
                old_baseline,
                Some(now), // long-dwell observed just now
            ),
        );
        let state = test_state_with(wear, cfg, BIND);

        let Json(resp) = get_wear(State(state)).await;
        assert!(
            !resp.displays[0].advisory,
            "a fresh long-dwell observation must reset the advisory reference point"
        );
    }

    #[tokio::test]
    async fn get_wear_empty_handle_returns_empty_list() {
        let state = test_state_with(HashMap::new(), WearConfig::default(), BIND);
        let Json(resp) = get_wear(State(state)).await;
        assert!(resp.displays.is_empty());
    }

    // ── GET /api/wear/:display — detail + 404 ──────────────────────────────

    #[tokio::test]
    async fn get_wear_detail_returns_cells_and_heat_matching_grid_dims() {
        let now = now_epoch_s();
        let mut wear = HashMap::new();
        wear.insert(
            "panel-a".to_string(),
            ledger_with("panel-a", "Panel A", PanelType::QdOled, now, None),
        );
        let state = test_state_with(wear, WearConfig::default(), BIND);

        let result = get_wear_detail(State(state), Path("panel-a".to_string())).await;
        let Json(detail) = result.expect("known display must resolve");
        assert_eq!(detail.grid_rows, 2);
        assert_eq!(detail.grid_cols, 3);
        let expected_len = usize::from(detail.grid_rows) * usize::from(detail.grid_cols);
        assert_eq!(
            detail.cells.len(),
            expected_len,
            "cells length must equal rows*cols"
        );
        assert_eq!(
            detail.heat.len(),
            expected_len,
            "heat length must equal rows*cols"
        );
        assert_eq!(detail.summary.display, "panel-a");
    }

    // T11 (#108): the detail response carries BOTH the raw absolute
    // `cells` (clients still need real on-hours, not a normalized
    // value) AND the new `max_cell_hours` field (so the UI legend can
    // label real hours instead of inferring them from the normalized
    // heat). Pinning the coexistence here so a future refactor that
    // drops one or the other breaks the test, not the dashboard.
    #[tokio::test]
    async fn get_wear_detail_response_includes_cells_heat_and_max_cell_hours() {
        let now = now_epoch_s();
        // Hand-built cells so the maximum is an exact, predictable
        // value (4.0) — attribute_uniform would put the same number
        // in every cell, which is a degenerate max for this check.
        let mut ledger = WearLedger::new(
            WearIdentity {
                key: "panel-mix".to_string(),
                display_name: "Mixed Panel".to_string(),
                config_display_id: None,
            },
            PanelType::QdOled,
            1,
            3,
            now,
        );
        ledger.cells[0].wear_hours = 0.0;
        ledger.cells[1].wear_hours = 2.0;
        ledger.cells[2].wear_hours = 4.0;

        let mut wear = HashMap::new();
        wear.insert("panel-mix".to_string(), ledger);
        let state = test_state_with(wear, WearConfig::default(), BIND);

        let result = get_wear_detail(State(state), Path("panel-mix".to_string())).await;
        let Json(detail) = result.expect("known display must resolve");

        // absolute hours preserved (clients still need this)
        assert_eq!(detail.cells, vec![0.0, 2.0, 4.0]);
        // zero-max normalized heat (varied grid starting at 0 reads the
        // same as the old min-max form: 0, 0.5, 1)
        assert_eq!(detail.heat, vec![0.0, 0.5, 1.0]);
        // max_cell_hours exposes the denominator the legend needs
        assert!(
            (detail.max_cell_hours - 4.0).abs() < 1e-9,
            "max_cell_hours must equal the max of `cells` (4.0), got {}",
            detail.max_cell_hours
        );
    }

    #[tokio::test]
    async fn get_wear_detail_unknown_display_returns_404_error() {
        let state = test_state_with(HashMap::new(), WearConfig::default(), BIND);

        let result = get_wear_detail(State(state), Path("bogus".to_string())).await;
        match result {
            Err(WebError::UnknownDisplay(name)) => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownDisplay, got {other:?}"),
        }
    }

    // ── Per-display source gate (Task 11: per-display status watch) ────────

    /// A TV whose source gate is mismatched (e.g. on Netflix, not our HDMI):
    /// attribution degrades to uniform tagged `source_mismatch`.
    fn tv_mismatched_status() -> dormant_core::wear::WearSamplingStatus {
        dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(5),
            uniform_reason: Some("source_mismatch".to_owned()),
            bound_display: Some("tv".to_owned()),
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: Some("mismatched".to_owned()),
        }
    }

    /// A monitor whose source gate is matched (the expected input).
    fn monitor_matched_status() -> dormant_core::wear::WearSamplingStatus {
        dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(5),
            uniform_reason: None,
            bound_display: Some("monitor".to_owned()),
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: Some("matched".to_owned()),
        }
    }

    /// Ledger attributed to `[displays.<id>]` — the join key the per-display
    /// status map is indexed by. `config_display_id` is set so selection is
    /// by stable config id, never by wear-storage row order (#201).
    fn ledger_for(id: &str, display_name: &str, panel_type: PanelType) -> WearLedger {
        let mut ledger = WearLedger::new(
            WearIdentity {
                key: id.to_string(),
                display_name: display_name.to_string(),
                config_display_id: Some(id.to_string()),
            },
            panel_type,
            2,
            3,
            now_epoch_s(),
        );
        ledger.attribute_uniform(Duration::from_secs(3600), 1.0);
        ledger
    }

    /// Index a wear response by `config_display_id` so assertions are
    /// independent of the (sorted) wire order.
    fn by_config_id(resp: &WearListResponse) -> BTreeMap<&str, &WearSummary> {
        resp.displays
            .iter()
            .map(|s| (s.config_display_id.as_deref().unwrap(), s))
            .collect()
    }

    #[tokio::test]
    async fn per_display_source_gate_maps_tv_mismatch_and_monitor_matched() {
        let mut wear = HashMap::new();
        wear.insert(
            "tv".to_string(),
            ledger_for("tv", "Living Room TV", PanelType::QdOled),
        );
        wear.insert(
            "monitor".to_string(),
            ledger_for("monitor", "Desk Monitor", PanelType::Woled),
        );
        let mut per_display = BTreeMap::new();
        per_display.insert("tv".to_string(), tv_mismatched_status());
        per_display.insert("monitor".to_string(), monitor_matched_status());

        let state = test_state_with_sampling(wear, WearConfig::default(), BIND, per_display, None);
        let Json(resp) = get_wear(State(state)).await;
        let by_id = by_config_id(&resp);

        assert_eq!(by_id["tv"].source_gate.as_deref(), Some("mismatched"));
        assert_eq!(
            by_id["tv"].uniform_reason.as_deref(),
            Some("source_mismatch")
        );
        assert_eq!(by_id["monitor"].source_gate.as_deref(), Some("matched"));
        assert!(
            by_id["monitor"].uniform_reason.is_none(),
            "matched gate must carry no uniform_reason"
        );
    }

    #[tokio::test]
    async fn per_display_source_gate_independent_of_insertion_order() {
        // Reverse both the ledger map and the status map insertion order.
        // Selection is by config_display_id, not row order, so the mapping
        // must be identical to the forward-order case.
        let mut wear = HashMap::new();
        wear.insert(
            "monitor".to_string(),
            ledger_for("monitor", "Desk Monitor", PanelType::Woled),
        );
        wear.insert(
            "tv".to_string(),
            ledger_for("tv", "Living Room TV", PanelType::QdOled),
        );
        let mut per_display = BTreeMap::new();
        per_display.insert("monitor".to_string(), monitor_matched_status());
        per_display.insert("tv".to_string(), tv_mismatched_status());

        let state = test_state_with_sampling(wear, WearConfig::default(), BIND, per_display, None);
        let Json(resp) = get_wear(State(state)).await;
        let by_id = by_config_id(&resp);

        assert_eq!(by_id["tv"].source_gate.as_deref(), Some("mismatched"));
        assert_eq!(
            by_id["tv"].uniform_reason.as_deref(),
            Some("source_mismatch")
        );
        assert_eq!(by_id["monitor"].source_gate.as_deref(), Some("matched"));
        assert!(by_id["monitor"].uniform_reason.is_none());
    }

    #[tokio::test]
    async fn per_display_source_gate_legacy_singular_fallback_when_map_empty() {
        // Per-display map empty -> legacy singular status is the fallback.
        // The singular status is bound to "tv", so tv inherits its gate and
        // reason; monitor does not belong (bound_display != config_display_id)
        // and gets no gate — absent-in-empty-map falls back, absent-in-
        // populated-map does not, but here the map is empty so the legacy
        // slot is the only source.
        let mut wear = HashMap::new();
        wear.insert(
            "tv".to_string(),
            ledger_for("tv", "Living Room TV", PanelType::QdOled),
        );
        wear.insert(
            "monitor".to_string(),
            ledger_for("monitor", "Desk Monitor", PanelType::Woled),
        );
        let legacy = tv_mismatched_status();

        let state = test_state_with_sampling(
            wear,
            WearConfig::default(),
            BIND,
            BTreeMap::new(),
            Some(legacy),
        );
        let Json(resp) = get_wear(State(state)).await;
        let by_id = by_config_id(&resp);

        assert_eq!(by_id["tv"].source_gate.as_deref(), Some("mismatched"));
        assert_eq!(
            by_id["tv"].uniform_reason.as_deref(),
            Some("source_mismatch")
        );
        assert!(
            by_id["monitor"].source_gate.is_none(),
            "monitor is not bound by the legacy singular status -> no gate"
        );
        assert!(by_id["monitor"].uniform_reason.is_none());
    }

    #[tokio::test]
    async fn per_display_source_gate_absent_in_populated_map_is_none() {
        // Trap guard: a display absent from a NON-empty per-display map gets
        // no gate (NOT a legacy fallback). Only an EMPTY map falls back.
        let mut wear = HashMap::new();
        wear.insert(
            "tv".to_string(),
            ledger_for("tv", "Living Room TV", PanelType::QdOled),
        );
        wear.insert(
            "orphan".to_string(),
            ledger_for("orphan", "Orphan Panel", PanelType::Unknown),
        );
        let mut per_display = BTreeMap::new();
        per_display.insert("tv".to_string(), tv_mismatched_status());

        let state = test_state_with_sampling(wear, WearConfig::default(), BIND, per_display, None);
        let Json(resp) = get_wear(State(state)).await;
        let by_id = by_config_id(&resp);

        assert_eq!(by_id["tv"].source_gate.as_deref(), Some("mismatched"));
        assert!(
            by_id["orphan"].source_gate.is_none(),
            "absent-in-populated-map must NOT fall back to legacy -> None"
        );
        assert!(by_id["orphan"].uniform_reason.is_none());
    }

    #[tokio::test]
    async fn per_display_source_gate_absent_in_populated_map_ignores_legacy_singular() {
        // T11 review Should (carried forward): the sibling above pins the
        // absent-in-populated-map guard with `legacy_singular: None`. This
        // test pins the stronger property — when the per-display map is
        // populated AND a legacy singular status IS present, a display
        // absent from the map still yields `None` rather than inheriting
        // the legacy fallback. The legacy status is bound to the ABSENT
        // display ("orphan") so the test is meaningful: if the
        // `per_display.is_empty()` guard were dropped, orphan would match
        // on `bound_display` and inherit the legacy gate — this catches
        // that regression.
        let mut wear = HashMap::new();
        wear.insert(
            "tv".to_string(),
            ledger_for("tv", "Living Room TV", PanelType::QdOled),
        );
        wear.insert(
            "orphan".to_string(),
            ledger_for("orphan", "Orphan Panel", PanelType::Unknown),
        );
        let mut per_display = BTreeMap::new();
        per_display.insert("tv".to_string(), tv_mismatched_status());
        // Legacy singular status bound to the ABSENT display — a broken
        // fallback (no is_empty guard) would attribute this gate to orphan;
        // the correct path returns None because the map is non-empty.
        let legacy = dormant_core::wear::WearSamplingStatus {
            state: dormant_core::wear::WearSamplingState::Streaming,
            last_capture_age_s: Some(5),
            uniform_reason: Some("source_mismatch".to_owned()),
            bound_display: Some("orphan".to_owned()),
            granted_at_epoch_s: Some(1_700_000_000),
            source_gate: Some("mismatched".to_owned()),
        };

        let state =
            test_state_with_sampling(wear, WearConfig::default(), BIND, per_display, Some(legacy));
        let Json(resp) = get_wear(State(state)).await;
        let by_id = by_config_id(&resp);

        assert_eq!(by_id["tv"].source_gate.as_deref(), Some("mismatched"));
        assert!(
            by_id["orphan"].source_gate.is_none(),
            "absent-in-populated-map must NOT inherit the legacy singular -> None"
        );
        assert!(by_id["orphan"].uniform_reason.is_none());
    }

    // ── Router-level: guard + HTTP status ──────────────────────────────────

    #[tokio::test]
    async fn wear_route_rejects_foreign_host() {
        let state = test_state_with(HashMap::new(), WearConfig::default(), BIND);
        let router = crate::server::build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/wear")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn wear_detail_route_unknown_display_is_http_404() {
        let state = test_state_with(HashMap::new(), WearConfig::default(), BIND);
        let router = crate::server::build_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/wear/bogus")
            .header("Host", "127.0.0.1")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
