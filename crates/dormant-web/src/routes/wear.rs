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
//! browser missed the `compensation_advisory` WS nudge. The formula
//! mirrors the tracker's own latch condition exactly (`dormantd::
//! wear_tracker::tick`'s "Advisory (observed vs baseline, ONCE latch)"
//! section) — `now - max(observed, baseline) > advisory_after` — except
//! this is a stateless per-request recomputation, not a one-shot latch.

use axum::Json;
use axum::extract::{Path, State};
use dormant_core::wear::{PanelType, WearLedger};

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
    /// Min-max normalized per-cell heat (`0.0..=1.0`), row-major, same
    /// length as `cells` — see [`WearLedger::heat_map`].
    pub(crate) heat: Vec<f32>,
}

/// Current wall-clock time as epoch seconds; `0` if the clock is somehow
/// before the epoch (never in practice — defensive only).
fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Same advisory formula as `dormantd::wear_tracker::tick`'s latch check,
/// recomputed statelessly: `now - max(observed, baseline) > advisory_after`.
fn is_advisory(ledger: &WearLedger, advisory_after_s: u64, now_epoch_s: u64) -> bool {
    let baseline = ledger.advisory_baseline_epoch_s;
    let observed = ledger.last_long_dwell_epoch_s.unwrap_or(0);
    let reference = observed.max(baseline);
    let since_s = now_epoch_s.saturating_sub(reference);
    since_s > advisory_after_s
}

fn summarize(
    key: &str,
    ledger: &WearLedger,
    advisory_after_s: u64,
    now_epoch_s: u64,
) -> WearSummary {
    WearSummary {
        display: key.to_string(),
        display_name: ledger.identity.display_name.clone(),
        panel_type: ledger.panel_type,
        total_on_hours: ledger.total_on_hours,
        seeded_usage_hours: ledger.seeded_usage_hours,
        sample_count: ledger.sample_count,
        last_sample_at_epoch_s: ledger.last_sample_at_epoch_s,
        last_long_dwell_epoch_s: ledger.last_long_dwell_epoch_s,
        advisory: is_advisory(ledger, advisory_after_s, now_epoch_s),
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
    let advisory_after_s = state.inner.config_rx.borrow().wear.advisory_after.as_secs();
    let now = now_epoch_s();

    let Ok(guard) = state.inner.wear.read() else {
        return Json(WearListResponse {
            displays: Vec::new(),
        });
    };

    let mut displays: Vec<WearSummary> = guard
        .iter()
        .map(|(key, ledger)| summarize(key, ledger, advisory_after_s, now))
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
    let advisory_after_s = state.inner.config_rx.borrow().wear.advisory_after.as_secs();
    let now = now_epoch_s();

    let Ok(guard) = state.inner.wear.read() else {
        return Err(WebError::UnknownDisplay(display));
    };

    let ledger = guard
        .get(&display)
        .ok_or_else(|| WebError::UnknownDisplay(display.clone()))?;

    let summary = summarize(&display, ledger, advisory_after_s, now);
    let cells = ledger.cells.iter().map(|c| c.wear_hours).collect();
    let heat = ledger.heat_map();

    Ok(Json(WearDetail {
        summary,
        grid_rows: ledger.grid_rows,
        grid_cols: ledger.grid_cols,
        cells,
        heat,
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
    use std::collections::HashMap;
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

    fn test_state_with(
        wear: HashMap<String, WearLedger>,
        wear_cfg: WearConfig,
        bind: SocketAddr,
    ) -> WebState {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<dormant_core::rules::ControlMsg>(8);
        let (reload_trigger_tx, _reload_trigger_rx) = mpsc::channel::<()>(8);
        let (reload_tx, reload_rx) = tokio::sync::broadcast::channel(16);
        let config = Arc::new(Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: wear_cfg,
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays: IndexMap::default(),
            rules: IndexMap::default(),
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

        WebState::new(crate::state::WebStateInner {
            ctl_tx,
            reload_trigger: reload_trigger_tx,
            reload_rx,
            config_rx,
            creds_rx,
            config_path: std::path::PathBuf::from("/dev/null"),
            creds_path: std::path::PathBuf::from("/dev/null"),
            apply_lock: tokio::sync::Mutex::new(()),
            doctor,
            wear: Arc::new(RwLock::new(wear)),
            web_bind: bind,
            cancel,
            reload_timeout: Duration::from_secs(10),
        })
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

    #[tokio::test]
    async fn get_wear_detail_unknown_display_returns_404_error() {
        let state = test_state_with(HashMap::new(), WearConfig::default(), BIND);

        let result = get_wear_detail(State(state), Path("bogus".to_string())).await;
        match result {
            Err(WebError::UnknownDisplay(name)) => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownDisplay, got {other:?}"),
        }
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
