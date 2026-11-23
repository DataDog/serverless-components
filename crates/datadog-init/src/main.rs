// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(
    not(test),
    deny(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

#[cfg(unix)]
mod agent_telemetry;
#[cfg(unix)]
mod cgroup_cpu;
#[cfg(unix)]
mod components;
#[cfg(unix)]
mod enhanced_metrics;
#[cfg(unix)]
mod file_tailer;
#[cfg(unix)]
mod gcp_metadata;
#[cfg(unix)]
mod health;
#[cfg(unix)]
mod logging;
#[cfg(unix)]
mod logs_agent;
#[cfg(unix)]
mod metrics_agent;
#[cfg(unix)]
mod mode;
#[cfg(unix)]
mod platform;
#[cfg(unix)]
mod settings;
#[cfg(unix)]
mod trace_agent;
#[cfg(unix)]
mod wrapper;

#[cfg(unix)]
fn main() {
    use mode::Mode;

    let mode = Mode::from_arguments(std::env::args_os().skip(1).collect());
    let defaults =
        mode::missing_environment_defaults(&mode, |name| std::env::var_os(name).is_some());
    for (name, value) in defaults {
        // SAFETY: no other thread exists yet, so nothing reads the environment concurrently.
        unsafe { std::env::set_var(name, value) };
    }
    logging::init();

    let exit_code = match tokio::runtime::Runtime::new() {
        Ok(runtime) => {
            let exit_code = runtime.block_on(run(mode));
            // The flush is over or out of time. Dropping the runtime would wait for blocking
            // tasks, such as a DNS lookup, past the flush budget.
            runtime.shutdown_background();
            exit_code
        }
        Err(error) => {
            tracing::error!("Failed to start the async runtime: {error}");
            1
        }
    };
    std::process::exit(exit_code);
}

/// Runs the components and the mode until shutdown, flushes, and returns the process exit code.
/// The signal handlers are registered before the components start, so a signal during startup
/// is handled by the mode instead of ending the process.
///
/// The shutdown signal handlers stay registered until the process exits, so a SIGTERM or SIGINT
/// during the shutdown flush is ignored, as in Go. The flush budget bounds the shutdown.
#[cfg(unix)]
async fn run(mode: mode::Mode) -> i32 {
    use datadog_serverless_core::shutdown::ShutdownSignal;
    use mode::Mode;

    match mode {
        Mode::InContainer { command } => {
            let signals = match wrapper::register_forwarded_signals() {
                Ok(signals) => signals,
                Err(error) => {
                    tracing::error!("Failed to register the signal handlers: {error}");
                    return 1;
                }
            };
            // Every registered handler receives every signal, so the order of registration does
            // not matter and the forwarding handlers still forward SIGTERM and SIGINT. These
            // handlers only keep SIGTERM and SIGINT from ending the process once the child has
            // exited and forwarding has stopped.
            let _shutdown_signal = match ShutdownSignal::register() {
                Ok(shutdown_signal) => shutdown_signal,
                Err(error) => {
                    tracing::error!("Failed to register the shutdown signal handlers: {error}");
                    return 1;
                }
            };
            let components = match components::start(&Mode::InContainer {
                command: command.clone(),
            })
            .await
            {
                Ok(components) => components,
                Err(message) => return fatal(message),
            };
            tracing::info!("Arguments provided, launching in Init mode");
            let exit_code = wrapper::exit_code(
                &wrapper::run_child(command, signals, components.log_sink()).await,
            );
            components.flush_at_shutdown().await;
            exit_code
        }
        Mode::Sidecar => {
            let mut shutdown_signal = match ShutdownSignal::register() {
                Ok(shutdown_signal) => shutdown_signal,
                Err(error) => {
                    tracing::error!("Failed to register the shutdown signal handlers: {error}");
                    return 1;
                }
            };
            let components = match components::start(&Mode::Sidecar).await {
                Ok(components) => components,
                Err(message) => return fatal(message),
            };
            tracing::info!(
                "No arguments provided, launching in Sidecar mode, waiting for SIGINT or SIGTERM"
            );
            let reason = shutdown_signal.wait().await;
            tracing::info!("Received {reason:?}, shutting down");
            components.flush_at_shutdown().await;
            0
        }
    }
}

/// Prints a fatal error as Go's standard logger does, without its date prefix, and gives the exit
/// code of Go's `log.Fatalf`.
#[cfg(unix)]
fn fatal(message: &str) -> i32 {
    eprintln!("{message}");
    1
}

#[cfg(not(unix))]
fn main() {
    eprintln!("datadog-init is not supported on this platform");
    std::process::exit(1);
}
