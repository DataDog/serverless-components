// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;

use signal_hook::consts::signal::{
    SIGABRT, SIGALRM, SIGCONT, SIGHUP, SIGINT, SIGIO, SIGPIPE, SIGPROF, SIGQUIT, SIGTERM, SIGTSTP,
    SIGTTIN, SIGTTOU, SIGURG, SIGUSR1, SIGUSR2, SIGVTALRM, SIGWINCH, SIGXCPU, SIGXFSZ,
};
use signal_hook::iterator::Signals;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error};

use crate::logs_agent::{LineSplitter, LogSink};

/// The signals forwarded to the child: every catchable signal except SIGCHLD, which reports the
/// child's own state changes. SIGKILL and SIGSTOP cannot be caught. SIGBUS, SIGFPE, SIGILL,
/// SIGSEGV, SIGSYS and SIGTRAP report faults of this process, so they keep their default action.
const FORWARDED_SIGNALS: &[libc::c_int] = &[
    SIGABRT, SIGALRM, SIGCONT, SIGHUP, SIGINT, SIGIO, SIGPIPE, SIGPROF, SIGQUIT, SIGTERM, SIGTSTP,
    SIGTTIN, SIGTTOU, SIGURG, SIGUSR1, SIGUSR2, SIGVTALRM, SIGWINCH, SIGXCPU, SIGXFSZ,
];

/// The process exit code for the child's outcome: the child's exit code, 1 when the child could
/// not start, and 255 when a signal killed the child, as Go serverless-init does.
pub fn exit_code(outcome: &io::Result<ExitStatus>) -> i32 {
    match outcome {
        Ok(status) => status.code().unwrap_or(255),
        Err(_) => 1,
    }
}

/// Registers the handlers for every signal in [`FORWARDED_SIGNALS`]. Call it before anything
/// else starts, so that a signal that arrives early is held and forwarded once the child starts,
/// instead of ending this process.
pub fn register_forwarded_signals() -> io::Result<Signals> {
    Signals::new(FORWARDED_SIGNALS)
}

/// Runs the command as a child process that shares this process's stdin, stdout and stderr, and
/// waits for it to exit. With a log sink, the child's stdout and stderr go through pipes that copy
/// them unchanged to this process's own, and to the sink; the wait then also waits for the end of
/// both pipes, as Go serverless-init does. Every signal that `signals` catches is forwarded to the
/// child, and the child is never killed.
///
/// The forwarding thread starts before the child. If it cannot start, the child is not started
/// either, because the signals would then be caught and dropped.
pub async fn run_child(
    command: Vec<OsString>,
    mut signals: Signals,
    log_sink: Option<LogSink>,
) -> io::Result<ExitStatus> {
    let signals_handle = signals.handle();
    let outcome = async {
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let (pid_sender, pid_receiver) = std::sync::mpsc::channel::<libc::pid_t>();
        std::thread::Builder::new()
            .name("signal-forwarding".to_string())
            .spawn(move || {
                // The sender is dropped without a process ID when the child fails to start.
                let Ok(pid) = pid_receiver.recv() else {
                    return;
                };
                for signal in signals.forever() {
                    // SAFETY: kill has no memory safety requirements.
                    unsafe { libc::kill(pid, signal) };
                }
            })
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to start the signal forwarding thread: {error}"),
                )
            })?;
        debug!("Launching subprocess {command:?}");
        let mut child_command = tokio::process::Command::new(program);
        child_command.args(arguments);
        if log_sink.is_some() {
            child_command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        }
        let mut child = child_command.spawn()?;
        if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            let _ = pid_sender.send(pid);
        }
        let mut copies = Vec::new();
        if let Some(sink) = log_sink {
            if let Some(stdout) = child.stdout.take() {
                copies.push(tokio::spawn(copy_output(
                    stdout,
                    tokio::io::stdout(),
                    sink.clone(),
                    false,
                )));
            }
            if let Some(stderr) = child.stderr.take() {
                copies.push(tokio::spawn(copy_output(
                    stderr,
                    tokio::io::stderr(),
                    sink,
                    true,
                )));
            }
        }
        let status = child.wait().await;
        for copy in copies {
            let _ = copy.await;
        }
        status
    }
    .await;
    signals_handle.close();
    if let Err(error) = &outcome {
        error!("Failed to execute command {command:?}: {error}");
    }
    outcome
}

/// Copies the child's output to `output` unchanged, and sends it to `sink` until the pipe ends:
/// stdout one message per line, and stderr one message per read, newlines included, as Go
/// serverless-init does.
async fn copy_output(
    mut pipe: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    sink: LogSink,
    is_error: bool,
) {
    let mut buffer = vec![0u8; 32 * 1024];
    let mut lines = LineSplitter::default();
    loop {
        let chunk = match pipe.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => &buffer[..read],
            Err(error) => {
                debug!("Failed to read the child's output: {error}");
                return;
            }
        };
        if output.write_all(chunk).await.is_ok() {
            let _ = output.flush().await;
        }
        if is_error {
            sink.send(chunk, true);
        } else {
            for line in lines.push(chunk) {
                sink.send(&line, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn exit_code_table() {
        let cases: Vec<(io::Result<ExitStatus>, i32)> = vec![
            (Ok(ExitStatus::from_raw(0)), 0),
            (Ok(ExitStatus::from_raw(3 << 8)), 3),
            (Ok(ExitStatus::from_raw(255 << 8)), 255),
            (Ok(ExitStatus::from_raw(libc::SIGKILL)), 255),
            (Ok(ExitStatus::from_raw(libc::SIGTERM)), 255),
            (Err(io::Error::from(io::ErrorKind::NotFound)), 1),
            (Err(io::Error::from(io::ErrorKind::PermissionDenied)), 1),
        ];
        for (outcome, expected) in &cases {
            assert_eq!(exit_code(outcome), *expected, "outcome {outcome:?}");
        }
    }
}
