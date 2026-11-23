// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Shutdown signals and the flush budget.
//!
//! Once [`ShutdownSignal::register`] returns, tokio keeps the operating system handler for the
//! life of the process, and signals that arrive before `wait` is called are buffered, not lost.
//! After the caller stops waiting, later signals are ignored, so the binary must exit itself
//! after the flush budget.

use std::future::Future;
use std::time::Duration;

use tracing::warn;

/// The platform event that asked the process to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
    /// SIGTERM on Unix.
    Terminate,
    /// SIGINT on Unix.
    Interrupt,
    /// A Windows console control event. The value is the event name, such as `ctrl_close`.
    WindowsCtrl(&'static str),
}

/// Handlers for the signals that ask the process to stop.
///
/// Until a handler is registered, the default action of the signal ends the process with no
/// flush. Call [`ShutdownSignal::register`] at startup, before any slow work.
#[cfg(unix)]
pub struct ShutdownSignal {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignal {
    /// Registers the handlers now.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system refuses to install a handler.
    ///
    /// # Panics
    ///
    /// Panics outside a tokio runtime, or in a runtime built without the input and output
    /// driver (`enable_io` or `enable_all`).
    pub fn register() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Waits for SIGTERM or SIGINT.
    pub async fn wait(&mut self) -> ShutdownReason {
        tokio::select! {
            _ = self.terminate.recv() => ShutdownReason::Terminate,
            _ = self.interrupt.recv() => ShutdownReason::Interrupt,
        }
    }
}

/// Handlers for the console control events that ask the process to stop.
///
/// Until a handler is registered, the default action of the event ends the process with no
/// flush. Call [`ShutdownSignal::register`] at startup, before any slow work.
///
/// On `ctrl_close`, Windows ends the process after about 5 seconds, and on `ctrl_shutdown`
/// after the system shutdown timeout, whether or not the flush has finished. So the flushes
/// must run concurrently (see [`FlushBudget`]). `ctrl_shutdown` reaches only services, and
/// console processes that do not load `user32`.
#[cfg(windows)]
pub struct ShutdownSignal {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl ShutdownSignal {
    /// Registers the handlers now.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system refuses to install a handler.
    ///
    /// # Panics
    ///
    /// Panics outside a tokio runtime, or in a runtime built without the input and output
    /// driver (`enable_io` or `enable_all`).
    pub fn register() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
            ctrl_close: ctrl_close()?,
            ctrl_shutdown: ctrl_shutdown()?,
        })
    }

    /// Waits for `ctrl_c`, `ctrl_break`, `ctrl_close` or `ctrl_shutdown`.
    pub async fn wait(&mut self) -> ShutdownReason {
        tokio::select! {
            _ = self.ctrl_c.recv() => ShutdownReason::WindowsCtrl("ctrl_c"),
            _ = self.ctrl_break.recv() => ShutdownReason::WindowsCtrl("ctrl_break"),
            _ = self.ctrl_close.recv() => ShutdownReason::WindowsCtrl("ctrl_close"),
            _ = self.ctrl_shutdown.recv() => ShutdownReason::WindowsCtrl("ctrl_shutdown"),
        }
    }
}

/// Registers the handlers and waits for the platform to ask the process to stop.
///
/// The handlers are registered only when this future is first polled. Prefer
/// [`ShutdownSignal::register`] at startup, before any slow work.
///
/// # Errors
///
/// Returns an error when the operating system refuses to install a handler.
pub async fn wait_for_shutdown_signal() -> std::io::Result<ShutdownReason> {
    Ok(ShutdownSignal::register()?.wait().await)
}

/// How long each kind of telemetry may take to flush at shutdown.
///
/// Run the flushes concurrently, so the worst case is the longest budget (about 2.5 seconds).
/// One after another the defaults add up to 6 seconds, which is more than Windows allows after
/// `ctrl_close` (about 5 seconds) before it ends the process.
///
/// The stats flush shares the `traces` budget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlushBudget {
    pub traces: Duration,
    pub metrics: Duration,
    pub logs: Duration,
}

impl Default for FlushBudget {
    fn default() -> Self {
        Self {
            traces: Duration::from_millis(2500),
            metrics: Duration::from_millis(2000),
            logs: Duration::from_millis(1500),
        }
    }
}

/// Whether a flush finished inside its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    Completed,
    TimedOut,
}

/// Runs `fut` for at most `budget`, and logs a warning when it runs out of time.
///
/// On timeout, `fut` is dropped (cancelled), so the in-flight requests and the buffered data
/// inside it are lost.
pub async fn run_with_budget<F: Future<Output = ()>>(
    name: &'static str,
    budget: Duration,
    fut: F,
) -> FlushOutcome {
    match tokio::time::timeout(budget, fut).await {
        Ok(()) => FlushOutcome::Completed,
        Err(_) => {
            warn!("{name} flush exceeded {budget:?}");
            FlushOutcome::TimedOut
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn run_with_budget_completes_fast_future() {
        let outcome = run_with_budget("test", Duration::from_millis(100), async {}).await;
        assert_eq!(outcome, FlushOutcome::Completed);
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_budget_times_out_slow_future() {
        let outcome = run_with_budget(
            "test",
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_secs(5)),
        )
        .await;
        assert_eq!(outcome, FlushOutcome::TimedOut);
    }

    #[test]
    fn flush_budget_default_values() {
        let budget = FlushBudget::default();
        assert_eq!(
            budget,
            FlushBudget {
                traces: Duration::from_millis(2500),
                metrics: Duration::from_millis(2000),
                logs: Duration::from_millis(1500),
            }
        );
    }

    // The tests that raise SIGTERM call `register` before the raise, and tokio keeps the handler
    // installed for the life of the process, so the raised SIGTERM cannot kill the test binary.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_for_shutdown_signal_returns_terminate_on_sigterm() {
        let mut signal = ShutdownSignal::register().expect("signal registration failed");
        // SAFETY: raising a signal for which a handler is registered.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(signal.wait().await, ShutdownReason::Terminate);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_receives_repeated_signals_through_one_registration() {
        let mut signal = ShutdownSignal::register().expect("signal registration failed");
        // SAFETY: raising a signal for which a handler is registered.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(signal.wait().await, ShutdownReason::Terminate);
        // SAFETY: the handler is still registered.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(signal.wait().await, ShutdownReason::Terminate);
    }
}
