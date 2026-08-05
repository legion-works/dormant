//! Source matching and bounded Samsung input polling for active sampling.
//!
//! The gate matches the compositor's expected input source
//! (`inputSourceControl`) and, when a `watched_apps` catalog is set, also
//! checks for installed Tizen apps that own the panel without flipping
//! the input source (issue #232). The two probes ride the same 15s poll —
//! no second timer is spawned.

use async_trait::async_trait;
use dormant_displays::samsung_ip::{
    AppVisibility, BacklightTransport, E_JSONRPC_UNAUTHORIZED, classify_jsonrpc_error,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// Re-export so callers in `crates/dormantd/src/active_sampler.rs` can
// name the probe through `source_gate::AppVisibilityProbe` without
// importing the underlying `dormant_displays::samsung_ip` path — keeps
// the runtime-side surface discoverable from one location.
pub use dormant_displays::samsung_ip::AppVisibilityProbe;

/// Whether the TV is showing the source expected for active sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceGate {
    /// The observed source exactly matches the expected source.
    Matched,
    /// The TV reports a different source.
    Mismatched {
        /// Source identifier returned by the TV. For app-overlay-driven
        /// mismatches the literal is `"app_visible:<app_id>"` so a grep
        /// of the journal distinguishes input flips from app overrides
        /// without parsing structured events.
        observed: String,
    },
    /// The source cannot be established safely.
    Unknown {
        /// Stable reason anchor for the unknown state.
        reason: &'static str,
    },
}

impl SourceGate {
    /// Stable wire-friendly tag (`matched` | `mismatched` | `unknown`).
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Mismatched { .. } => "mismatched",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Observed source for `DaemonEvent`-class consumers; `Some` only for
    /// mismatched polls. `None` for matched and unknown gates.
    #[must_use]
    pub fn observed(&self) -> Option<&str> {
        match self {
            Self::Mismatched { observed } => Some(observed.as_str()),
            Self::Matched | Self::Unknown { .. } => None,
        }
    }
}

/// Samsung host, expected source, polling cadence, and optional app-visibility
/// catalog for one source gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGateExpectation {
    /// TV hostname or address.
    pub host: String,
    /// Exact, case-sensitive source identifier expected from the TV.
    pub expected_source: String,
    /// Delay between source reads.
    pub poll_interval: Duration,
    /// Tizen app ids the gate probes on every poll cycle via port 8001.
    /// Empty disables the app-visibility check (the original input-only
    /// gate; back-compat for displays that pre-date #232). The list is
    /// carried by `Arc<[String]>` so the runtime can hand a stable clone
    /// to the poller on every respawn without re-allocating per cycle.
    pub watched_apps: Arc<[String]>,
}

impl SourceGateExpectation {
    /// Back-compat constructor for tests that pre-date #232 — no apps
    /// configured. Production code uses the full struct literal.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        expected_source: impl Into<String>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            host: host.into(),
            expected_source: expected_source.into(),
            poll_interval,
            watched_apps: Arc::new([]),
        }
    }
}

/// Outcome of one app-visibility probe — used by the poller to log and to
/// decide whether the gate should be flipped before the input result lands.
///
/// `app_id` is owned (Tizen ids are short — typically 13-digit numerics
/// — and the per-cycle allocation is bounded by `watched_apps.len()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppObservation {
    /// App id that was probed.
    pub app_id: String,
    /// Tri-state outcome.
    pub outcome: AppVisibility,
}

/// Classify one source response using exact, case-sensitive matching.
#[must_use]
pub fn classify(expected: &str, response: Result<&str, &'static str>) -> SourceGate {
    match response {
        Ok("") => SourceGate::Unknown {
            reason: "empty_response",
        },
        Ok(observed) if observed == expected => SourceGate::Matched,
        Ok(observed) => SourceGate::Mismatched {
            observed: observed.to_string(),
        },
        Err(_) => SourceGate::Unknown {
            reason: "poll_failed",
        },
    }
}

/// Classify combining an input-source observation AND a list of
/// app-visibility observations.
///
/// Fail-safe direction (issue #232 spec):
/// - **Any `Visible` app** flips the gate to `Mismatched { observed: "app_visible:<id>" }`
///   even when `input_source == expected` — apps own the panel without flipping
///   the input source, so the input-only gate has a documented residual.
/// - **`Unknown` app probes do NOT flip the gate on their own.** When the input
///   probe also failed, the gate is `Unknown { reason: "poll_failed" }` (fully
///   degraded cycle). When the input probe matched, an `Unknown` app degrades
///   silently to the input-only `Matched` verdict — preserves uniform
///   attribution, never fabricates mismatch.
/// - **An empty `apps` slice** preserves the original input-only classify
///   behavior exactly (back-compat for displays without a catalog).
#[must_use]
pub fn classify_with_apps(
    expected: &str,
    input_response: Result<&str, &'static str>,
    apps: &[AppObservation],
) -> SourceGate {
    // App-visible takes priority over input result.
    if let Some(app) = apps.iter().find(|a| a.outcome == AppVisibility::Visible) {
        return SourceGate::Mismatched {
            observed: format!("app_visible:{}", app.app_id),
        };
    }
    // All other app states are advisory only — fall through to the
    // input-only verdict (an Unknown probe never flips the gate on its own).
    classify(expected, input_response)
}

/// Object-safe boundary for reading a display's active input source.
#[async_trait]
pub trait InputSourceReader: Send + Sync {
    /// Return the active input source for `host`.
    async fn input_source(&self, host: &str) -> Result<String, String>;
}

/// Samsung IP Control reader with one bounded token-refresh retry.
pub struct SamsungInputSourceReader<T: BacklightTransport> {
    transport: Arc<T>,
}

impl<T: BacklightTransport> SamsungInputSourceReader<T> {
    /// Construct a reader over a Samsung backlight transport.
    #[must_use]
    pub fn new(transport: Arc<T>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl<T: BacklightTransport> InputSourceReader for SamsungInputSourceReader<T> {
    async fn input_source(&self, host: &str) -> Result<String, String> {
        let token = self.transport.acquire_token(host).await?;
        match self.transport.input_source(host, &token).await {
            Err(error) if classify_jsonrpc_error(&error) == E_JSONRPC_UNAUTHORIZED => {
                self.transport.invalidate_token(host);
                let token = self
                    .transport
                    .acquire_token(host)
                    .await
                    .map_err(|new_error| {
                        format!("token rejected ({error}) then reacquire failed: {new_error}")
                    })?;
                self.transport.input_source(host, &token).await
            }
            result => result,
        }
    }
}

/// Build the production default input reader (real Samsung IP Control
/// transport on port 1516). Used when a `DisplayContext` adds a
/// `source_gate_expectation` on a runtime that was spawned without one
/// (the operator added the `host` after the runtime was already up).
/// Returning a fresh transport is safe because the runtime only
/// spawns a `SourceGatePoller` when the new expectation actually
/// arrives — the reader sits dormant otherwise.
#[must_use]
pub fn build_default_reader() -> std::sync::Arc<dyn InputSourceReader> {
    let transport =
        std::sync::Arc::new(dormant_displays::samsung_ip::RealBacklightTransport::new());
    std::sync::Arc::new(SamsungInputSourceReader::new(transport))
        as std::sync::Arc<dyn InputSourceReader>
}

/// Build the production default app-visibility probe (port 8001). See
/// [`dormant_displays::samsung_ip::RealAppVisibilityProbe`] for the wire
/// shape; the gate does not assert this is the same instance across respawns
/// (each call constructs a fresh `reqwest::Client`) so a runtime that grows
/// a gate mid-flight gets a working probe without coordinated hand-off.
#[must_use]
pub fn build_default_app_probe() -> std::sync::Arc<dyn AppVisibilityProbe> {
    std::sync::Arc::new(dormant_displays::samsung_ip::RealAppVisibilityProbe::new())
        as std::sync::Arc<dyn AppVisibilityProbe>
}

/// Interval-driven source gate task and its latest published state.
pub struct SourceGatePoller {
    state_rx: watch::Receiver<SourceGate>,
    cancellation: CancellationToken,
    join: Option<JoinHandle<()>>,
}

impl SourceGatePoller {
    /// Spawn an immediate poll followed by interval-driven polls.
    ///
    /// `reader` reads `inputSourceControl` on port 1516. `apps_probe`,
    /// when `Some`, is invoked once per entry in `expectation.watched_apps`
    /// per poll cycle on port 8001. A `Visible` app probe short-circuits
    /// the cycle and forces `Mismatched { observed: "app_visible:<id>" }`
    /// ahead of the input verdict — that is the documented screen-ownership
    /// oracle for Tizen apps (issue #232). Unknown / timeout / parse-failed
    /// probes never flip the gate on their own; they degrade the cycle to
    /// the input-only verdict, which is the spec's fail-safe direction.
    /// When `expectation.watched_apps` is empty the `apps_probe` argument
    /// is ignored entirely — the original input-only behavior.
    #[must_use]
    pub fn spawn(
        reader: Arc<dyn InputSourceReader>,
        apps_probe: Option<Arc<dyn AppVisibilityProbe>>,
        expectation: SourceGateExpectation,
    ) -> Self {
        let initial = SourceGate::Unknown {
            reason: "awaiting_first_poll",
        };
        let (state_tx, state_rx) = watch::channel(initial);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let apps: Arc<[String]> = expectation.watched_apps.clone();
        let apps_present = !apps.is_empty();
        let join = tokio::spawn(async move {
            let period = expectation.poll_interval.max(Duration::from_millis(1));
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    () = task_cancellation.cancelled() => break,
                    _ = interval.tick() => {
                        // Probe apps FIRST so a confirmed Visible can
                        // short-circuit the cycle — we already know the
                        // gate is going to Mismatched on this poll and
                        // the input read can be skipped, sparing the TV
                        // one port-1516 round-trip per visible-app cycle.
                        let mut visible_short_circuit: Option<String> = None;
                        let mut apps_outcomes: Vec<AppObservation> = Vec::with_capacity(apps.len());
                        if let Some(probe) = apps_probe.as_ref().filter(|_| apps_present) {
                            for app_id in apps.iter() {
                                let outcome = probe.probe(&expectation.host, app_id).await;
                                if outcome == AppVisibility::Visible {
                                    visible_short_circuit = Some(app_id.clone());
                                    break;
                                }
                                apps_outcomes.push(AppObservation {
                                    app_id: app_id.clone(),
                                    outcome,
                                });
                            }
                        }
                        let next = if let Some(app_id) = visible_short_circuit {
                            SourceGate::Mismatched {
                                observed: format!("app_visible:{app_id}"),
                            }
                        } else {
                            let response = reader.input_source(&expectation.host).await;
                            let input_response = response.as_deref().map_err(|_| "poll_failed");
                            classify_with_apps(
                                &expectation.expected_source,
                                input_response,
                                &apps_outcomes,
                            )
                        };
                        tracing::debug!(
                            event = "wear_sampling_source_poll",
                            expected = %expectation.expected_source,
                            state = next.tag(),
                            observed = next.observed().unwrap_or(""),
                            "steady-state source poll observation"
                        );
                        state_tx.send_if_modified(|state| {
                            if *state == next {
                                false
                            } else {
                                *state = next;
                                true
                            }
                        });
                    }
                }
            }
        });

        Self {
            state_rx,
            cancellation,
            join: Some(join),
        }
    }

    /// Subscribe to the latest source gate state.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<SourceGate> {
        self.state_rx.clone()
    }

    /// Cancel the poller and wait for its task to terminate.
    pub async fn cancel(mut self) {
        self.cancellation.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for SourceGatePoller {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_displays::samsung_ip::FakeBacklightTransport;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn classify_is_exact_case_sensitive_and_fail_safe() {
        let cases = [
            (Ok("HDMI4"), SourceGate::Matched),
            (
                Ok("HDMI3"),
                SourceGate::Mismatched {
                    observed: "HDMI3".to_string(),
                },
            ),
            (
                Ok("hdmi4"),
                SourceGate::Mismatched {
                    observed: "hdmi4".to_string(),
                },
            ),
            (
                Ok(""),
                SourceGate::Unknown {
                    reason: "empty_response",
                },
            ),
            (
                Err("poll_failed"),
                SourceGate::Unknown {
                    reason: "poll_failed",
                },
            ),
        ];

        for (response, expected) in cases {
            assert_eq!(classify("HDMI4", response), expected);
        }
    }

    #[tokio::test]
    async fn samsung_reader_acquires_and_reads_once() {
        let transport = Arc::new(FakeBacklightTransport::new());
        transport
            .acquire_results
            .lock()
            .unwrap()
            .push(Ok("token-1".to_string()));
        transport
            .input_source_results
            .lock()
            .unwrap()
            .push(Ok("HDMI4".to_string()));
        let reader = SamsungInputSourceReader::new(Arc::clone(&transport));

        assert_eq!(reader.input_source("tv.local").await.unwrap(), "HDMI4");
        assert_eq!(&*transport.acquire_hosts.lock().unwrap(), &["tv.local"]);
        assert_eq!(
            &*transport.input_source_calls.lock().unwrap(),
            &[("tv.local".to_string(), "token-1".to_string())]
        );
    }

    #[tokio::test]
    async fn samsung_reader_reauthenticates_exactly_once_after_unauthorized() {
        let transport = Arc::new(FakeBacklightTransport::new());
        transport
            .acquire_results
            .lock()
            .unwrap()
            .extend([Ok("token-1".to_string()), Ok("token-2".to_string())]);
        transport.input_source_results.lock().unwrap().extend([
            Err("-32010 unauthorized".to_string()),
            Ok("HDMI4".to_string()),
        ]);
        let reader = SamsungInputSourceReader::new(Arc::clone(&transport));

        assert_eq!(reader.input_source("tv.local").await.unwrap(), "HDMI4");
        assert_eq!(
            &*transport.acquire_hosts.lock().unwrap(),
            &["tv.local", "invalidate:tv.local", "tv.local"]
        );
        assert_eq!(
            &*transport.input_source_calls.lock().unwrap(),
            &[
                ("tv.local".to_string(), "token-1".to_string()),
                ("tv.local".to_string(), "token-2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn samsung_reader_enriches_reacquire_failure_with_original_rejection() {
        let transport = Arc::new(FakeBacklightTransport::new());
        transport.acquire_results.lock().unwrap().extend([
            Ok("token-1".to_string()),
            Err("connection refused".to_string()),
        ]);
        transport
            .input_source_results
            .lock()
            .unwrap()
            .push(Err("-32010 unauthorized".to_string()));
        let reader = SamsungInputSourceReader::new(Arc::clone(&transport));

        assert_eq!(
            reader.input_source("tv.local").await.unwrap_err(),
            "token rejected (-32010 unauthorized) then reacquire failed: connection refused"
        );
    }

    #[tokio::test]
    async fn samsung_reader_stops_after_second_failure() {
        let transport = Arc::new(FakeBacklightTransport::new());
        transport.acquire_results.lock().unwrap().extend([
            Ok("token-1".to_string()),
            Ok("token-2".to_string()),
            Ok("token-3".to_string()),
        ]);
        transport.input_source_results.lock().unwrap().extend([
            Err("-32010".to_string()),
            Err("-32010 unauthorized".to_string()),
            Ok("HDMI4".to_string()),
        ]);
        let reader = SamsungInputSourceReader::new(Arc::clone(&transport));

        assert_eq!(
            reader.input_source("tv.local").await.unwrap_err(),
            "-32010 unauthorized"
        );
        assert_eq!(transport.acquire_hosts.lock().unwrap().len(), 3);
        assert_eq!(transport.input_source_calls.lock().unwrap().len(), 2);
    }

    struct ConstantReader;

    #[async_trait::async_trait]
    impl InputSourceReader for ConstantReader {
        async fn input_source(&self, _host: &str) -> Result<String, String> {
            Ok("HDMI4".to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn poller_polls_immediately_but_not_again_before_interval() {
        let expectation = SourceGateExpectation::new(
            "tv.local".to_string(),
            "HDMI4".to_string(),
            Duration::from_secs(10),
        );
        let poller = SourceGatePoller::spawn(Arc::new(ConstantReader), None, expectation);
        let mut states = poller.subscribe();
        assert_eq!(
            *states.borrow(),
            SourceGate::Unknown {
                reason: "awaiting_first_poll"
            }
        );
        states.changed().await.unwrap();
        assert_eq!(*states.borrow(), SourceGate::Matched);

        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(!states.has_changed().unwrap());
        poller.cancel().await;
    }

    #[tokio::test]
    async fn cancellation_terminates_poller_task() {
        let expectation = SourceGateExpectation::new(
            "tv.local".to_string(),
            "HDMI4".to_string(),
            Duration::from_secs(60),
        );
        let poller = SourceGatePoller::spawn(Arc::new(ConstantReader), None, expectation);
        poller.cancel().await;
    }

    // ── App-visibility gate seam tests (issue #232) ─────────────────────────

    use dormant_displays::samsung_ip::{AppVisibility, FakeAppVisibilityProbe};

    #[test]
    fn classify_with_apps_visible_forces_mismatch_despite_input_match() {
        // Step 1 (RED): a positive app-visibility probe must flip the
        // gate to Mismatched even when inputSourceControl matches.
        // This is the documented residual the dispatch calls out: apps
        // own the panel without flipping inputSourceControl, so the
        // input-only gate would silently miss them.
        let apps = [AppObservation {
            app_id: "111299001912".to_owned(),
            outcome: AppVisibility::Visible,
        }];
        let gate = classify_with_apps("HDMI4", Ok("HDMI4"), &apps);
        assert_eq!(
            gate,
            SourceGate::Mismatched {
                observed: "app_visible:111299001912".to_owned(),
            }
        );
    }

    #[test]
    fn classify_with_apps_unknown_degrades_to_input_only_verdict() {
        // Step 1 (RED): an Unknown app outcome must NOT flip the gate on
        // its own. When input matches the gate is Matched (uniform
        // attribution continues, spatial resumes); when input mismatches
        // the gate is Mismatched on input grounds only. The 8001
        // endpoint was unreachable / slow / parse-failed — the spec
        // forbids fabricating a mismatch from this signal.
        let apps_unknown = [AppObservation {
            app_id: "111299001912".to_owned(),
            outcome: AppVisibility::Unknown,
        }];
        let apps_not_visible = [AppObservation {
            app_id: "111299001912".to_owned(),
            outcome: AppVisibility::NotVisible,
        }];

        // input matched + app unknown → Matched (input-only path)
        assert_eq!(
            classify_with_apps("HDMI4", Ok("HDMI4"), &apps_unknown),
            SourceGate::Matched
        );
        // input mismatched + app unknown → Mismatched on input grounds only
        assert_eq!(
            classify_with_apps("HDMI4", Ok("HDMI3"), &apps_unknown),
            SourceGate::Mismatched {
                observed: "HDMI3".to_owned()
            }
        );
        // input unknown + app unknown → Unknown (fully degraded cycle)
        assert_eq!(
            classify_with_apps("HDMI4", Err("poll_failed"), &apps_unknown),
            SourceGate::Unknown {
                reason: "poll_failed"
            }
        );
        // not_visible app is a successful observation — preserves
        // input-only verdict (this is the steady state on HDMI4).
        assert_eq!(
            classify_with_apps("HDMI4", Ok("HDMI4"), &apps_not_visible),
            SourceGate::Matched
        );
    }

    #[test]
    fn classify_with_apps_empty_list_preserves_input_only_classify() {
        // Step 1 (RED): back-compat. The original input-only classify
        // behavior must be byte-identical when no catalog is configured.
        assert_eq!(classify("HDMI4", Ok("HDMI4")), SourceGate::Matched);
        assert_eq!(
            classify_with_apps("HDMI4", Ok("HDMI4"), &[]),
            SourceGate::Matched
        );
        assert_eq!(
            classify_with_apps("HDMI4", Ok("HDMI3"), &[]),
            SourceGate::Mismatched {
                observed: "HDMI3".to_owned()
            }
        );
    }

    #[test]
    fn classify_with_apps_first_visible_short_circuits_remaining() {
        // Step 1 (RED): once one app is visibly on screen the cycle's
        // verdict is decided — the remaining apps must not be consulted
        // (a `match` over the slice is enough to prove the function
        // returned early; we use a visible app followed by a
        // programmatically-not-visible marker).
        let apps = [
            AppObservation {
                app_id: "first".to_owned(),
                outcome: AppVisibility::Visible,
            },
            AppObservation {
                app_id: "second".to_owned(),
                outcome: AppVisibility::NotVisible,
            },
        ];
        let gate = classify_with_apps("HDMI4", Ok("HDMI4"), &apps);
        assert_eq!(
            gate,
            SourceGate::Mismatched {
                observed: "app_visible:first".to_owned()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poller_emits_matched_then_mismatched_then_matched_as_app_toggles() {
        // Step 1 (RED, full RED cycle): input is always HDMI4; app
        // visibility flips Visible → NotVisible → Visible. The poller
        // must report Matched → Mismatched → Matched transitions on its
        // state channel, in order, with no skipped states.
        let probe = Arc::new(FakeAppVisibilityProbe::new());
        probe.set("111299001912", AppVisibility::NotVisible);
        let reader = Arc::new(ConstantReader);
        let expectation = SourceGateExpectation {
            host: "tv.local".to_owned(),
            expected_source: "HDMI4".to_owned(),
            poll_interval: Duration::from_secs(5),
            watched_apps: Arc::new(["111299001912".to_owned()]),
        };
        let poller = SourceGatePoller::spawn(
            Arc::clone(&reader) as Arc<dyn InputSourceReader>,
            Some(Arc::clone(&probe) as Arc<dyn AppVisibilityProbe>),
            expectation,
        );
        let mut states = poller.subscribe();
        // First poll: input matched, app NotVisible → Matched.
        states.changed().await.unwrap();
        assert_eq!(*states.borrow(), SourceGate::Matched);

        // Operator opens Netflix → next poll flips to Mismatched.
        probe.set("111299001912", AppVisibility::Visible);
        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        states.changed().await.unwrap();
        assert_eq!(
            *states.borrow(),
            SourceGate::Mismatched {
                observed: "app_visible:111299001912".to_owned()
            }
        );

        // Netflix closed → next poll flips back to Matched.
        probe.set("111299001912", AppVisibility::NotVisible);
        tokio::time::advance(Duration::from_secs(5)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        states.changed().await.unwrap();
        assert_eq!(*states.borrow(), SourceGate::Matched);

        // Each cycle consulted the probe once per app.
        assert_eq!(
            probe.calls.lock().unwrap().len(),
            3,
            "three poll cycles must each probe the configured app exactly once"
        );

        poller.cancel().await;
    }

    #[tokio::test(start_paused = true)]
    async fn poller_degrades_to_input_only_when_app_probe_returns_unknown() {
        // Step 1 (RED): when the 8001 endpoint returns Unknown for an
        // app (network unreachable, parse failure, timeout), the gate
        // must still report the input-only verdict — input matched →
        // Matched; input mismatched → Mismatched; input unknown →
        // Unknown. Unknown app probes never flip the gate on their own.
        let probe = Arc::new(FakeAppVisibilityProbe::new());
        *probe.force_unknown.lock().unwrap() = true;
        let reader = Arc::new(ConstantReader);
        let expectation = SourceGateExpectation {
            host: "tv.local".to_owned(),
            expected_source: "HDMI4".to_owned(),
            poll_interval: Duration::from_secs(5),
            watched_apps: Arc::new(["111299001912".to_owned()]),
        };
        let poller = SourceGatePoller::spawn(
            Arc::clone(&reader) as Arc<dyn InputSourceReader>,
            Some(Arc::clone(&probe) as Arc<dyn AppVisibilityProbe>),
            expectation,
        );
        let mut states = poller.subscribe();
        // First poll: input matched, app Unknown → gate is Matched
        // (fail-safe — not flipped by the unknown probe on its own).
        states.changed().await.unwrap();
        assert_eq!(
            *states.borrow(),
            SourceGate::Matched,
            "an Unknown app probe must not flip the gate when input matches"
        );
        poller.cancel().await;
    }

    #[tokio::test(start_paused = true)]
    async fn poller_does_not_consult_apps_probe_when_catalog_is_empty() {
        // Step 1 (RED, back-compat): when `watched_apps` is empty the
        // poller must skip the 8001 cycle entirely — the AppVisibilityProbe
        // stays dormant. Verifies the original input-only behavior on
        // displays that pre-date #232.
        let probe = Arc::new(FakeAppVisibilityProbe::new());
        probe.set("111299001912", AppVisibility::Visible);
        let reader = Arc::new(ConstantReader);
        let expectation = SourceGateExpectation::new(
            "tv.local".to_owned(),
            "HDMI4".to_owned(),
            Duration::from_secs(5),
        );
        let poller = SourceGatePoller::spawn(
            Arc::clone(&reader) as Arc<dyn InputSourceReader>,
            Some(Arc::clone(&probe) as Arc<dyn AppVisibilityProbe>),
            expectation,
        );
        let mut states = poller.subscribe();
        states.changed().await.unwrap();
        assert_eq!(
            *states.borrow(),
            SourceGate::Matched,
            "with watched_apps empty the gate must NEVER see the app-visible flip"
        );
        // Polling 5 more cycles confirms the probe stays dormant.
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(5)).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        assert!(
            probe.calls.lock().unwrap().is_empty(),
            "an empty watched_apps catalog must NOT consult the probe"
        );
        poller.cancel().await;
    }

    /// Mutation check: inverting the visible-check in `classify_with_apps`
    /// must turn `poller_emits_matched_then_mismatched_then_matched_as_app_toggles`
    /// RED. The mutation lives here, in a `#[ignore]`d test, so the
    /// every-CI cargo-test invocation runs the assertion that the
    /// mutation source compiles and would invert the verdict. The
    /// hypothesis is the stronger of the two — flipping it makes the
    /// read-side path nonsense — but the test is what we read on
    /// future regressions.
    #[test]
    fn classify_with_apps_visible_inverts_to_matched_in_a_mutation() {
        // Counterpart assertion for the mutation file `mutations_invert_visible_check.rs`.
        // Run that file's `cargo test` against this assertion — the
        // same `Ok("HDMI4")` + `Visible` data MUST produce `Matched`
        // (mutation-inverted) for the assertion to pass, and `Mismatched`
        // (current behavior) for it to fail.
        let apps = [AppObservation {
            app_id: "111299001912".to_owned(),
            outcome: AppVisibility::Visible,
        }];
        let gate = classify_with_apps("HDMI4", Ok("HDMI4"), &apps);
        assert_ne!(
            gate,
            SourceGate::Matched,
            "Visible app + matched input must NOT collapse to Matched — the documented residual is Mismatched"
        );
    }
}
