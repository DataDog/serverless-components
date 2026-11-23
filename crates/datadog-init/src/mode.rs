// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;

/// How the process runs, chosen from its command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Runs the user's command as a child process and exits with its exit code.
    InContainer { command: Vec<OsString> },
    /// Runs next to the user's container until SIGINT or SIGTERM.
    Sidecar,
}

impl Mode {
    /// Chooses the mode from the arguments after the program name.
    pub fn from_arguments(arguments: Vec<OsString>) -> Self {
        if arguments.is_empty() {
            Mode::Sidecar
        } else {
            Mode::InContainer {
                command: split_command(arguments),
            }
        }
    }
}

/// Builds the child's command line. A single argument is split on whitespace, with no shell, so
/// quoting is lost. Several arguments are used as they are.
pub fn split_command(arguments: Vec<OsString>) -> Vec<OsString> {
    match arguments.as_slice() {
        [single] => match single.to_str() {
            Some(text) => text.split_whitespace().map(OsString::from).collect(),
            None => arguments,
        },
        _ => arguments,
    }
}

/// The environment variables set for both modes when they are unset.
const COMMON_ENVIRONMENT_DEFAULTS: &[(&str, &str)] = &[
    ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false"),
    ("DD_REMOTE_CONFIGURATION_ENABLED", "false"),
    ("DD_HOSTNAME", "none"),
    ("DD_APM_ENABLED", "true"),
    ("DD_TRACE_ENABLED", "true"),
];

/// The environment variables also set in sidecar mode when they are unset.
const SIDECAR_ENVIRONMENT_DEFAULTS: &[(&str, &str)] = &[
    ("DD_LOGS_ENABLED", "true"),
    ("DD_APM_NON_LOCAL_TRAFFIC", "true"),
    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
];

/// Returns the default environment variables of the mode that `is_set` reports as unset.
pub fn missing_environment_defaults(
    mode: &Mode,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(&'static str, &'static str)> {
    let sidecar_defaults = match mode {
        Mode::Sidecar => SIDECAR_ENVIRONMENT_DEFAULTS,
        Mode::InContainer { .. } => &[],
    };
    COMMON_ENVIRONMENT_DEFAULTS
        .iter()
        .chain(sidecar_defaults)
        .filter(|(name, _)| !is_set(name))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn mode_from_arguments_table() {
        let cases: &[(&[&str], Mode)] = &[
            (&[], Mode::Sidecar),
            (
                &["/app/server"],
                Mode::InContainer {
                    command: os_strings(&["/app/server"]),
                },
            ),
            (
                &["python app.py"],
                Mode::InContainer {
                    command: os_strings(&["python", "app.py"]),
                },
            ),
        ];
        for (arguments, expected) in cases {
            assert_eq!(
                Mode::from_arguments(os_strings(arguments)),
                *expected,
                "arguments {arguments:?}"
            );
        }
    }

    #[test]
    fn split_command_table() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["node"], &["node"]),
            (
                &["node server.js --port 80"],
                &["node", "server.js", "--port", "80"],
            ),
            (&["  node \t server.js\n"], &["node", "server.js"]),
            (&["sh -c 'echo a b'"], &["sh", "-c", "'echo", "a", "b'"]),
            (&["sh", "-c", "echo a b"], &["sh", "-c", "echo a b"]),
            (&["node", " spaced "], &["node", " spaced "]),
            (&["   "], &[]),
        ];
        for (arguments, expected) in cases {
            assert_eq!(
                split_command(os_strings(arguments)),
                os_strings(expected),
                "arguments {arguments:?}"
            );
        }
    }

    #[test]
    fn missing_environment_defaults_table() {
        let in_container = Mode::InContainer {
            command: os_strings(&["node"]),
        };
        let common = vec![
            ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false"),
            ("DD_REMOTE_CONFIGURATION_ENABLED", "false"),
            ("DD_HOSTNAME", "none"),
            ("DD_APM_ENABLED", "true"),
            ("DD_TRACE_ENABLED", "true"),
        ];
        let mut sidecar = common.clone();
        sidecar.extend([
            ("DD_LOGS_ENABLED", "true"),
            ("DD_APM_NON_LOCAL_TRAFFIC", "true"),
            ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
        ]);
        type Defaults = Vec<(&'static str, &'static str)>;
        let cases: Vec<(&Mode, &[&str], Defaults)> = vec![
            (&in_container, &[], common.clone()),
            (&Mode::Sidecar, &[], sidecar),
            (
                &in_container,
                &["DD_HOSTNAME", "DD_APM_ENABLED", "DD_LOGS_ENABLED"],
                vec![
                    ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false"),
                    ("DD_REMOTE_CONFIGURATION_ENABLED", "false"),
                    ("DD_TRACE_ENABLED", "true"),
                ],
            ),
            (
                &Mode::Sidecar,
                &[
                    "DD_INSTRUMENTATION_TELEMETRY_ENABLED",
                    "DD_REMOTE_CONFIGURATION_ENABLED",
                    "DD_HOSTNAME",
                    "DD_APM_ENABLED",
                    "DD_TRACE_ENABLED",
                    "DD_LOGS_ENABLED",
                    "DD_DOGSTATSD_NON_LOCAL_TRAFFIC",
                ],
                vec![("DD_APM_NON_LOCAL_TRAFFIC", "true")],
            ),
        ];
        for (mode, set, expected) in cases {
            let actual = missing_environment_defaults(mode, |name| set.contains(&name));
            assert_eq!(actual, expected, "mode {mode:?}, set {set:?}");
        }
    }
}
