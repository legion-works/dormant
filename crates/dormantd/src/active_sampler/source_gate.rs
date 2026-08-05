//! Source matching and bounded Samsung input polling for active sampling.

use async_trait::async_trait;
use dormant_displays::samsung_ip::{
    BacklightTransport, E_JSONRPC_UNAUTHORIZED, classify_jsonrpc_error,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Whether the TV is showing the source expected for active sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceGate {
    /// The observed source exactly matches the expected source.
    Matched,
    /// The TV reports a different source.
    Mismatched {
        /// Source identifier returned by the TV.
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

/// Samsung host, expected source, and polling cadence for one source gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGateExpectation {
    /// TV hostname or address.
    pub host: String,
    /// Exact, case-sensitive source identifier expected from the TV.
    pub expected_source: String,
    /// Delay between source reads.
    pub poll_interval: Duration,
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

/// Build the production default reader (real Samsung IP Control
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

/// Interval-driven source gate task and its latest published state.
pub struct SourceGatePoller {
    state_rx: watch::Receiver<SourceGate>,
    cancellation: CancellationToken,
    join: Option<JoinHandle<()>>,
}

impl SourceGatePoller {
    /// Spawn an immediate poll followed by interval-driven polls.
    #[must_use]
    pub fn spawn(reader: Arc<dyn InputSourceReader>, expectation: SourceGateExpectation) -> Self {
        let initial = SourceGate::Unknown {
            reason: "awaiting_first_poll",
        };
        let (state_tx, state_rx) = watch::channel(initial);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let join = tokio::spawn(async move {
            let period = expectation.poll_interval.max(Duration::from_millis(1));
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    () = task_cancellation.cancelled() => break,
                    _ = interval.tick() => {
                        let response = reader.input_source(&expectation.host).await;
                        let next = classify(
                            &expectation.expected_source,
                            response.as_deref().map_err(|_| "poll_failed"),
                        );
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
        let expectation = SourceGateExpectation {
            host: "tv.local".to_string(),
            expected_source: "HDMI4".to_string(),
            poll_interval: Duration::from_secs(10),
        };
        let poller = SourceGatePoller::spawn(Arc::new(ConstantReader), expectation);
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
        let expectation = SourceGateExpectation {
            host: "tv.local".to_string(),
            expected_source: "HDMI4".to_string(),
            poll_interval: Duration::from_secs(60),
        };
        let poller = SourceGatePoller::spawn(Arc::new(ConstantReader), expectation);
        poller.cancel().await;
    }
}
