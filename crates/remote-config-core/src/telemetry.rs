//! Telemetry helpers for Remote Configuration.
//!
//! This module provides drop-in implementations of [`RemoteConfigTelemetry`] that
//! make it easy to surface metrics or integrate with external monitoring systems.
//! Consumers can either use the provided counting primitives to expose their own
//! metrics or wrap them in application-specific emitters.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::service::{
    OrgStatusSnapshot, RemoteConfigTelemetry, ServiceError, WebsocketCheckResult,
};

/// Aggregated telemetry counters (all values use relaxed atomics).
#[derive(Debug, Default)]
pub struct TelemetryCounters {
    refresh_success: AtomicU64,
    refresh_error: AtomicU64,
    bypass_enqueued: AtomicU64,
    bypass_timeout: AtomicU64,
    bypass_rejected: AtomicU64,
    bypass_rate_limited: AtomicU64,
    websocket_success: AtomicU64,
    websocket_failure: AtomicU64,
    org_enabled: AtomicU64,
    org_authorized: AtomicU64,
}

impl TelemetryCounters {
    /// Captures a point-in-time snapshot of the counters.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            refresh_success: self.refresh_success.load(Ordering::Relaxed),
            refresh_error: self.refresh_error.load(Ordering::Relaxed),
            bypass_enqueued: self.bypass_enqueued.load(Ordering::Relaxed),
            bypass_timeout: self.bypass_timeout.load(Ordering::Relaxed),
            bypass_rejected: self.bypass_rejected.load(Ordering::Relaxed),
            bypass_rate_limited: self.bypass_rate_limited.load(Ordering::Relaxed),
            websocket_success: self.websocket_success.load(Ordering::Relaxed),
            websocket_failure: self.websocket_failure.load(Ordering::Relaxed),
            org_enabled: self.org_enabled.load(Ordering::Relaxed),
            org_authorized: self.org_authorized.load(Ordering::Relaxed),
        }
    }
}

/// Plain data representation of [`TelemetryCounters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetrySnapshot {
    /// Number of successful refresh operations.
    pub refresh_success: u64,
    /// Number of refresh errors.
    pub refresh_error: u64,
    /// Number of bypass requests enqueued.
    pub bypass_enqueued: u64,
    /// Number of bypass requests that timed out.
    pub bypass_timeout: u64,
    /// Number of bypass requests that were rejected.
    pub bypass_rejected: u64,
    /// Number of bypass requests that were rate limited.
    pub bypass_rate_limited: u64,
    /// Number of successful websocket echo checks.
    pub websocket_success: u64,
    /// Number of failed websocket echo checks.
    pub websocket_failure: u64,
    /// Latest organisation enabled flag (0 = disabled, 1 = enabled).
    pub org_enabled: u64,
    /// Latest organisation authorised flag (0 = unauthorized, 1 = authorized).
    pub org_authorized: u64,
}

impl fmt::Display for TelemetrySnapshot {
    /// Formats the snapshot metrics into a comma-separated list for logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refresh_success={}, refresh_error={}, bypass_enqueued={}, bypass_timeout={}, bypass_rejected={}, bypass_rate_limited={}, websocket_success={}, websocket_failure={}, org_enabled={}, org_authorized={}",
            self.refresh_success,
            self.refresh_error,
            self.bypass_enqueued,
            self.bypass_timeout,
            self.bypass_rejected,
            self.bypass_rate_limited,
            self.websocket_success,
            self.websocket_failure,
            self.org_enabled,
            self.org_authorized
        )
    }
}

/// Telemetry implementation that maintains atomic counters for every signal.
///
/// This is useful when embedding the service in environments that prefer collecting
/// metrics externally: the caller can expose the [`TelemetryCounters`] via statsd,
/// OpenTelemetry, or any other metric backend.
#[derive(Debug, Clone)]
pub struct CountingTelemetry {
    counters: Arc<TelemetryCounters>,
}

impl CountingTelemetry {
    /// Creates a new telemetry instance backed by the provided counter set.
    pub fn new(counters: Arc<TelemetryCounters>) -> Self {
        Self { counters }
    }

    /// Returns the underlying counter set.
    pub fn counters(&self) -> Arc<TelemetryCounters> {
        self.counters.clone()
    }
}

impl Default for CountingTelemetry {
    /// Builds a counting telemetry instance backed by fresh counters.
    fn default() -> Self {
        Self::new(Arc::new(TelemetryCounters::default()))
    }
}

impl RemoteConfigTelemetry for CountingTelemetry {
    /// Records a refresh success by incrementing the relevant counter.
    fn on_refresh_success(&self, _next_interval: Duration) {
        self.counters
            .refresh_success
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a refresh failure for external visibility.
    fn on_refresh_error(&self, _error: &ServiceError) {
        self.counters.refresh_error.fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a bypass was enqueued for processing.
    fn on_bypass_enqueued(&self) {
        self.counters
            .bypass_enqueued
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a bypass operation exceeded its waiting window.
    fn on_bypass_timeout(&self) {
        self.counters.bypass_timeout.fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a bypass request was rejected.
    fn on_bypass_rejected(&self) {
        self.counters
            .bypass_rejected
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a bypass request was rate limited.
    fn on_bypass_rate_limited(&self) {
        self.counters
            .bypass_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Classifies websocket echo outcomes as success or failure counters.
    fn on_websocket_check(&self, result: WebsocketCheckResult) {
        // Categorise the echo result so callers can export separate success/failure metrics.
        match result {
            WebsocketCheckResult::Success => {
                self.counters
                    .websocket_success
                    .fetch_add(1, Ordering::Relaxed);
            }
            WebsocketCheckResult::Failure => {
                self.counters
                    .websocket_failure
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Captures org status changes emitted by the telemetry handle.
    fn on_org_status_change(
        &self,
        _previous: Option<OrgStatusSnapshot>,
        current: OrgStatusSnapshot,
    ) {
        self.counters
            .org_enabled
            .store(current.enabled as u64, Ordering::Relaxed);
        self.counters
            .org_authorized
            .store(current.authorized as u64, Ordering::Relaxed);
    }
}

/// Telemetry implementation that forwards events to multiple observers.
///
/// This allows applications to combine counters with logging or custom sinks.
pub struct CompositeTelemetry {
    observers: Vec<Arc<dyn RemoteConfigTelemetry>>,
}

impl CompositeTelemetry {
    /// Creates an empty dispatcher.
    pub fn new() -> Self {
        Self {
            observers: Vec::new(),
        }
    }

    /// Adds a telemetry observer to the dispatcher.
    pub fn with_observer(mut self, telemetry: Arc<dyn RemoteConfigTelemetry>) -> Self {
        self.observers.push(telemetry);
        self
    }

    /// Extends the dispatcher with additional observers.
    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = Arc<dyn RemoteConfigTelemetry>>,
    {
        self.observers.extend(iter);
    }
}

impl Default for CompositeTelemetry {
    /// Builds an empty composite dispatcher.
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CompositeTelemetry {
    /// Emits a debug struct containing the observer count.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeTelemetry")
            .field("observer_count", &self.observers.len())
            .finish()
    }
}

impl RemoteConfigTelemetry for CompositeTelemetry {
    /// Notifies all observers about a successful refresh.
    fn on_refresh_success(&self, next_interval: Duration) {
        for observer in &self.observers {
            observer.on_refresh_success(next_interval);
        }
    }

    /// Notifies observers about a refresh failure.
    fn on_refresh_error(&self, error: &ServiceError) {
        for observer in &self.observers {
            observer.on_refresh_error(error);
        }
    }

    /// Forwards bypass timeout notifications to observers.
    fn on_bypass_timeout(&self) {
        for observer in &self.observers {
            observer.on_bypass_timeout();
        }
    }

    /// Forwards bypass rejection notifications to observers.
    fn on_bypass_rejected(&self) {
        for observer in &self.observers {
            observer.on_bypass_rejected();
        }
    }

    /// Forwards bypass rate-limit notifications.
    fn on_bypass_rate_limited(&self) {
        for observer in &self.observers {
            observer.on_bypass_rate_limited();
        }
    }

    /// Forwards bypass enqueue notifications to observers.
    fn on_bypass_enqueued(&self) {
        for observer in &self.observers {
            observer.on_bypass_enqueued();
        }
    }

    /// Forwards websocket echo results to observers.
    fn on_websocket_check(&self, result: WebsocketCheckResult) {
        for observer in &self.observers {
            // Forward the classification to each observer so that composite sinks remain in sync.
            observer.on_websocket_check(result);
        }
    }

    /// Helper used by the async test to capture status transitions.
    fn on_org_status_change(
        &self,
        previous: Option<OrgStatusSnapshot>,
        current: OrgStatusSnapshot,
    ) {
        for observer in &self.observers {
            observer.on_org_status_change(previous, current);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::service::{OrgStatusSnapshot, WebsocketCheckResult};
    use std::sync::Arc;

    /// Confirms counters increment for every telemetry callback.
    #[test]
    fn counting_telemetry_tracks_all_events() {
        let telemetry = CountingTelemetry::default();
        telemetry.on_refresh_success(Duration::from_secs(1));
        telemetry.on_refresh_error(&ServiceError::InvalidRequest("".into()));
        telemetry.on_bypass_enqueued();
        telemetry.on_bypass_timeout();
        telemetry.on_bypass_rejected();
        telemetry.on_bypass_rate_limited();
        telemetry.on_websocket_check(WebsocketCheckResult::Success);
        telemetry.on_websocket_check(WebsocketCheckResult::Failure);
        telemetry.on_org_status_change(
            None,
            OrgStatusSnapshot {
                enabled: true,
                authorized: false,
                fetched_at: tokio::time::Instant::now(),
            },
        );

        let snapshot = telemetry.counters().snapshot();
        assert_eq!(snapshot.refresh_success, 1);
        assert_eq!(snapshot.refresh_error, 1);
        assert_eq!(snapshot.bypass_enqueued, 1);
        assert_eq!(snapshot.bypass_timeout, 1);
        assert_eq!(snapshot.bypass_rejected, 1);
        assert_eq!(snapshot.bypass_rate_limited, 1);
        assert_eq!(snapshot.websocket_success, 1);
        assert_eq!(snapshot.websocket_failure, 1);
        assert_eq!(snapshot.org_enabled, 1);
        assert_eq!(snapshot.org_authorized, 0);
    }

    /// Ensures composite telemetry broadcasts to every observer.
    #[test]
    fn composite_telemetry_forwards_calls() {
        let primary = Arc::new(CountingTelemetry::default());
        let secondary = Arc::new(CountingTelemetry::default());

        let composite = CompositeTelemetry::new()
            .with_observer(primary.clone())
            .with_observer(secondary.clone());

        composite.on_bypass_enqueued();
        composite.on_bypass_rate_limited();
        composite.on_websocket_check(WebsocketCheckResult::Failure);

        let snapshot_primary = primary.counters().snapshot();
        let snapshot_secondary = secondary.counters().snapshot();

        assert_eq!(snapshot_primary.bypass_enqueued, 1);
        assert_eq!(snapshot_secondary.bypass_enqueued, 1);
        assert_eq!(snapshot_primary.bypass_rate_limited, 1);
        assert_eq!(snapshot_secondary.bypass_rate_limited, 1);
        assert_eq!(snapshot_primary.websocket_failure, 1);
        assert_eq!(snapshot_secondary.websocket_failure, 1);
    }

    /// Validates that the telemetry counters start at zero and snapshot reports zeros.
    #[test]
    fn telemetry_counters_default_to_zero() {
        let counters = TelemetryCounters::default();
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.refresh_success, 0);
        assert_eq!(snapshot.refresh_error, 0);
        assert_eq!(snapshot.bypass_enqueued, 0);
        assert_eq!(snapshot.bypass_timeout, 0);
        assert_eq!(snapshot.bypass_rejected, 0);
        assert_eq!(snapshot.bypass_rate_limited, 0);
        assert_eq!(snapshot.websocket_success, 0);
        assert_eq!(snapshot.websocket_failure, 0);
        assert_eq!(snapshot.org_enabled, 0);
        assert_eq!(snapshot.org_authorized, 0);
    }

    /// Ensures CountingTelemetry::new preserves the external counter set.
    #[test]
    fn counting_telemetry_uses_external_counters() {
        let counters = Arc::new(TelemetryCounters::default());
        let telemetry = CountingTelemetry::new(counters.clone());
        assert!(
            Arc::ptr_eq(&counters, &telemetry.counters()),
            "CountingTelemetry did not retain the provided Arc"
        );
    }

    /// Ensures TelemetrySnapshot::fmt prints all fields.
    #[test]
    fn telemetry_snapshot_display_includes_all_fields() {
        let snapshot = TelemetrySnapshot {
            refresh_success: 1,
            refresh_error: 2,
            bypass_enqueued: 3,
            bypass_timeout: 4,
            bypass_rejected: 5,
            bypass_rate_limited: 6,
            websocket_success: 7,
            websocket_failure: 8,
            org_enabled: 1,
            org_authorized: 0,
        };
        let formatted = snapshot.to_string();
        for segment in [
            "refresh_success=1",
            "refresh_error=2",
            "bypass_enqueued=3",
            "bypass_timeout=4",
            "bypass_rejected=5",
            "bypass_rate_limited=6",
            "websocket_success=7",
            "websocket_failure=8",
            "org_enabled=1",
            "org_authorized=0",
        ] {
            assert!(
                formatted.contains(segment),
                "missing segment '{segment}' in '{formatted}'"
            );
        }
    }

    /// Ensures CompositeTelemetry::extend accepts iterators and debug output reports observer count.
    #[test]
    fn composite_telemetry_extend_and_debug() {
        let mut composite = CompositeTelemetry::default();
        let first = Arc::new(CountingTelemetry::default());
        let second = Arc::new(CountingTelemetry::default());
        composite.extend(vec![
            first.clone() as Arc<dyn RemoteConfigTelemetry>,
            second.clone() as Arc<dyn RemoteConfigTelemetry>,
        ]);

        composite.on_bypass_timeout();
        composite.on_bypass_rate_limited();
        composite.on_websocket_check(WebsocketCheckResult::Success);
        composite.on_org_status_change(
            None,
            OrgStatusSnapshot {
                enabled: false,
                authorized: true,
                fetched_at: tokio::time::Instant::now(),
            },
        );

        for snapshot in [first.counters().snapshot(), second.counters().snapshot()] {
            assert_eq!(snapshot.bypass_timeout, 1);
            assert_eq!(snapshot.bypass_rate_limited, 1);
            assert_eq!(snapshot.websocket_success, 1);
            assert_eq!(snapshot.org_enabled, 0);
            assert_eq!(snapshot.org_authorized, 1);
        }

        let debug_output = format!("{composite:?}");
        assert!(
            debug_output.contains("observer_count"),
            "debug output should contain observer count field"
        );
    }
}
