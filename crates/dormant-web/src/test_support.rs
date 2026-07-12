//! Shared `#[cfg(test)]`-only test support for `dormant-web`'s route tests.
//!
//! ## Tracing-capture layer
//!
//! Multiple route test modules (`routes::pair`, `routes::config_apply`) need
//! to assert on which tracing events fired for a given handler call —
//! without leaking secrets (pairing tokens, entity values) into log lines.
//!
//! `tracing` caches each event macro callsite's `Interest` the FIRST time it
//! ever fires, process-wide, based on whichever subscriber is ambient on
//! whatever thread got there first. With `cargo test`'s default parallel
//! execution, a per-test `tracing::dispatcher::set_default` guard is racy:
//! some OTHER concurrently running test can reach the SAME callsite first
//! with NO subscriber active on ITS thread, permanently caching "never
//! interested" for that callsite before this test's guard is even
//! installed.
//!
//! The robust fix: install ONE real subscriber as the process-wide GLOBAL
//! default, exactly once (`std::sync::Once`), before any test's assertions
//! run. Every callsite's interest then resolves against a subscriber that is
//! always "interested" (our `Layer` doesn't override `register_callsite`, so
//! the default trait impl returns `Interest::always()`), regardless of which
//! thread/test hits it first. Per-test isolation of the CAPTURED events is
//! then done with a `thread_local!` buffer keyed on `Option<Vec<String>>`:
//! each `#[tokio::test]` runs on its own dedicated OS thread for its whole
//! duration (current-thread flavor), so a `None` buffer means "not
//! recording" and `Some(vec)` means "this test is recording", with no
//! cross-test interference.
//!
//! This lives in ONE shared module — deliberately NOT copy-pasted per test
//! module — because `tracing::subscriber::set_global_default` only succeeds
//! ONCE per process. `dormant-web`'s route test modules all link into the
//! same test binary; two independent `Once`-gated installs (one per module,
//! each with its own `Layer` type and its own `thread_local!` buffer) would
//! race for the single global slot. Whichever module's test runs first would
//! win, and the other module's `Layer` would never be invoked — its capture
//! would silently and permanently return empty, an easy way to ship a test
//! that always "passes" for the wrong reason. A shared module makes that
//! failure mode structurally impossible.

use std::cell::RefCell;
use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt as _;

thread_local! {
    static CAPTURE_BUF: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct FieldDumpVisitor(String);

impl tracing::field::Visit for FieldDumpVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

struct ThreadLocalCaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ThreadLocalCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        CAPTURE_BUF.with(|buf| {
            if let Some(events) = buf.borrow_mut().as_mut() {
                let mut visitor = FieldDumpVisitor(String::new());
                event.record(&mut visitor);
                events.push(visitor.0);
            }
        });
    }
}

/// Install [`ThreadLocalCaptureLayer`] as the process-wide global default
/// subscriber, exactly once. Safe to call from every test that wants to
/// capture — only the first caller's `Once::call_once` body actually runs.
fn ensure_capture_subscriber_installed() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(ThreadLocalCaptureLayer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Start recording tracing events on the CURRENT thread.
pub(crate) fn start_capturing() {
    ensure_capture_subscriber_installed();
    CAPTURE_BUF.with(|buf| *buf.borrow_mut() = Some(Vec::new()));
}

/// Stop recording and return everything captured on the CURRENT thread
/// since the matching [`start_capturing`] call.
pub(crate) fn take_captured() -> Vec<String> {
    CAPTURE_BUF.with(|buf| buf.borrow_mut().take().unwrap_or_default())
}
