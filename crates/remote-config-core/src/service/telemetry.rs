//! Service-level telemetry abstractions.
//!
//! Embedders can implement [`RemoteConfigTelemetry`] to observe refresh
//! outcomes and cache bypass activity without depending on the internal
//! service state.  A no-op implementation is provided for callers that do not
//! require instrumentation.

use std::time::Duration;

use super::state::{OrgStatusSnapshot, WebsocketCheckResult};
use super::ServiceError;

/// Telemetry hook invoked on significant service events.
pub trait RemoteConfigTelemetry: Send + Sync {
    /// Called after a successful refresh with the computed interval before the next attempt.
    fn on_refresh_success(&self, _next_interval: Duration) {}
    /// Called when a refresh attempt fails.
    fn on_refresh_error(&self, _error: &ServiceError) {}
    /// Called when a cache-bypass request times out waiting for completion.
    fn on_bypass_timeout(&self) {}
    /// Called when a cache-bypass request cannot be enqueued because the budget is exhausted.
    fn on_bypass_rejected(&self) {}
    /// Called when a cache-bypass request is skipped due to rate limiting.
    fn on_bypass_rate_limited(&self) {}
    /// Called when a cache-bypass refresh is enqueued.
    fn on_bypass_enqueued(&self) {}
    /// Called when the websocket echo task reports a connectivity result.
    fn on_websocket_check(&self, _result: WebsocketCheckResult) {}
    /// Called whenever the organisation status changes.
    fn on_org_status_change(
        &self,
        _previous: Option<OrgStatusSnapshot>,
        _current: OrgStatusSnapshot,
    ) {
    }
}

/// Default telemetry implementation that performs no-ops.
#[derive(Debug, Default)]
pub(crate) struct NoopTelemetry;

impl RemoteConfigTelemetry for NoopTelemetry {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::state::{OrgStatusSnapshot, WebsocketCheckResult};
    use crate::service::ServiceError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;

    /// Simple telemetry implementation that counts invocations for assertions.
    #[derive(Default)]
    struct CountingTelemetry {
        refresh_success: AtomicUsize,
        refresh_error: AtomicUsize,
        bypass_enqueued: AtomicUsize,
        bypass_timeout: AtomicUsize,
        bypass_rejected: AtomicUsize,
        bypass_rate_limited: AtomicUsize,
        websocket_checks: AtomicUsize,
        org_status: AtomicUsize,
    }

    impl RemoteConfigTelemetry for CountingTelemetry {
        /// Records a refresh success to confirm hooks are wired.
        fn on_refresh_success(&self, _next_interval: Duration) {
            self.refresh_success.fetch_add(1, Ordering::Relaxed);
        }

        /// Captures refresh errors for later assertions.
        fn on_refresh_error(&self, _error: &ServiceError) {
            self.refresh_error.fetch_add(1, Ordering::Relaxed);
        }

        /// Notes when bypass requests exceed their wait deadline.
        fn on_bypass_timeout(&self) {
            self.bypass_timeout.fetch_add(1, Ordering::Relaxed);
        }

        /// Tracks bypass requests rejected due to queue pressure.
        fn on_bypass_rejected(&self) {
            self.bypass_rejected.fetch_add(1, Ordering::Relaxed);
        }

        /// Records rate-limited bypass requests.
        fn on_bypass_rate_limited(&self) {
            self.bypass_rate_limited.fetch_add(1, Ordering::Relaxed);
        }

        /// Registers enqueue operations so tests can assert telemetry flow.
        fn on_bypass_enqueued(&self) {
            self.bypass_enqueued.fetch_add(1, Ordering::Relaxed);
        }

        /// Tallies websocket health check reports.
        fn on_websocket_check(&self, _result: WebsocketCheckResult) {
            self.websocket_checks.fetch_add(1, Ordering::Relaxed);
        }

        /// Captures organisation status transitions.
        fn on_org_status_change(
            &self,
            _previous: Option<OrgStatusSnapshot>,
            _current: OrgStatusSnapshot,
        ) {
            self.org_status.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Ensures telemetry implementors can observe every callback without panicking.
    #[test]
    fn telemetry_callbacks_increment_counters() {
        let telemetry = CountingTelemetry::default();
        telemetry.on_refresh_success(Duration::from_secs(1));
        telemetry.on_refresh_error(&ServiceError::InvalidRequest("boom".into()));
        telemetry.on_bypass_enqueued();
        telemetry.on_bypass_timeout();
        telemetry.on_bypass_rejected();
        telemetry.on_bypass_rate_limited();
        telemetry.on_websocket_check(WebsocketCheckResult::Success);
        let snapshot = OrgStatusSnapshot {
            enabled: true,
            authorized: true,
            fetched_at: Instant::now(),
        };
        telemetry.on_org_status_change(None, snapshot);

        assert_eq!(telemetry.refresh_success.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.refresh_error.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.bypass_enqueued.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.bypass_timeout.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.bypass_rejected.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.bypass_rate_limited.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.websocket_checks.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.org_status.load(Ordering::Relaxed), 1);
    }

    /// Verifies the default no-op telemetry accepts invocations without panicking.
    #[test]
    fn noop_telemetry_is_safe_to_call() {
        let telemetry = NoopTelemetry::default();
        telemetry.on_refresh_success(Duration::from_secs(1));
        telemetry.on_bypass_enqueued();
        telemetry.on_websocket_check(WebsocketCheckResult::Failure);
        let snapshot = OrgStatusSnapshot {
            enabled: false,
            authorized: false,
            fetched_at: Instant::now(),
        };
        telemetry.on_org_status_change(None, snapshot);
    }
}
