//! Test-support fakes for `dormant-displays`, gated behind the `test-util`
//! Cargo feature so other crates in the workspace (e.g. `dormant-web`'s
//! pairing wizard, Task 5) can inject them in their own tests without
//! touching real hardware or network.
//!
//! This module is intentionally NOT `#[cfg(test)]`-only: it must be
//! reachable as an ordinary item from a *different* crate's `[dev-dependencies]`
//! (`dormant-displays = { path = "...", features = ["test-util"] }`), which
//! `#[cfg(test)]` — scoped to this crate's own test builds — cannot provide.
//! The `test-util` feature is the seam that keeps it out of prod builds
//! instead.

use std::time::Duration;

use dormant_core::error::DormantError;

use crate::samsung_tizen::PairConnect;

/// A scripted [`PairConnect`] fake for pairing-wizard tests.
///
/// Construct with [`FakePairConnect::yielding`] (or
/// [`FakePairConnect::yielding_after`]) to return a fixed token, or
/// [`FakePairConnect::never`] to simulate a TV that never accepts the
/// pairing prompt — paired with a short timeout passed to
/// [`crate::samsung_tizen::pair_with_connect`], this exercises the timeout
/// error path deterministically and quickly, with no real I/O.
#[derive(Debug)]
pub struct FakePairConnect {
    /// What `connect` should do when called.
    behavior: Behavior,
}

/// Internal scripted behavior for [`FakePairConnect`]. Not `pub` — callers
/// only interact with it through the named constructors.
#[derive(Debug, Clone)]
enum Behavior {
    /// Resolve with `token` after waiting `delay` (zero for "immediately").
    Yield {
        /// The token `connect` returns.
        token: String,
        /// How long to wait before returning it.
        delay: Duration,
    },
    /// Never resolve — `connect`'s returned future is pending forever, so
    /// only the caller's own timeout (not this fake) can end the wait.
    Never,
}

impl FakePairConnect {
    /// A fake that resolves immediately with `token`.
    #[must_use]
    pub fn yielding(token: impl Into<String>) -> Self {
        Self::yielding_after(token, Duration::ZERO)
    }

    /// A fake that resolves with `token` after waiting `delay`.
    #[must_use]
    pub fn yielding_after(token: impl Into<String>, delay: Duration) -> Self {
        Self {
            behavior: Behavior::Yield {
                token: token.into(),
                delay,
            },
        }
    }

    /// A fake whose `connect` never resolves — for exercising the caller's
    /// timeout path (e.g. [`crate::samsung_tizen::pair_with_connect`])
    /// without waiting for a real, long timeout duration.
    #[must_use]
    pub fn never() -> Self {
        Self {
            behavior: Behavior::Never,
        }
    }
}

#[async_trait::async_trait]
impl PairConnect for FakePairConnect {
    async fn connect(&self, _host: &str, _timeout: Duration) -> Result<String, DormantError> {
        match &self.behavior {
            Behavior::Yield { token, delay } => {
                if !delay.is_zero() {
                    tokio::time::sleep(*delay).await;
                }
                Ok(token.clone())
            }
            Behavior::Never => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::samsung_tizen::pair_with_connect;

    #[tokio::test]
    async fn yielding_returns_the_scripted_token_immediately() {
        let fake: Arc<dyn PairConnect> = Arc::new(FakePairConnect::yielding("tok-abc"));
        let result = pair_with_connect("192.0.2.1", Duration::from_secs(5), fake).await;
        assert_eq!(result.unwrap(), "tok-abc");
    }

    #[tokio::test]
    async fn never_is_bounded_by_the_callers_timeout_not_its_own() {
        let fake: Arc<dyn PairConnect> = Arc::new(FakePairConnect::never());
        let start = tokio::time::Instant::now();
        let result = pair_with_connect("192.0.2.1", Duration::from_millis(30), fake).await;
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
