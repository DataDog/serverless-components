// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::env::VarError;
use std::fmt;

/// The raw value of an environment variable that is not a boolean accepted by Go `strconv.ParseBool`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidBool(pub String);

impl fmt::Display for InvalidBool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid boolean value {:?}", self.0)
    }
}

impl std::error::Error for InvalidBool {}

/// Parses a boolean the way Go `strconv.ParseBool` does.
/// An unset value returns `default`.
pub fn parse_bool(value: Option<&str>, default: bool) -> Result<bool, InvalidBool> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(InvalidBool(value.to_string())),
    }
}

/// Reads a boolean environment variable. An unset variable, or one with an invalid value,
/// returns `default`. An invalid value also logs a warning.
pub fn env_bool(name: &str, default: bool) -> bool {
    let value = match std::env::var(name) {
        Ok(value) => Some(value),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => {
            tracing::warn!(
                "Environment variable {name} is not valid unicode, using default {default}"
            );
            return default;
        }
    };
    match parse_bool(value.as_deref(), default) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!("Environment variable {name} has an {error}, using default {default}");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_table() {
        let cases: &[(Option<&str>, bool, Result<bool, InvalidBool>)] = &[
            (Some("1"), false, Ok(true)),
            (Some("t"), false, Ok(true)),
            (Some("T"), false, Ok(true)),
            (Some("TRUE"), false, Ok(true)),
            (Some("true"), false, Ok(true)),
            (Some("True"), false, Ok(true)),
            (Some("0"), true, Ok(false)),
            (Some("f"), true, Ok(false)),
            (Some("F"), true, Ok(false)),
            (Some("FALSE"), true, Ok(false)),
            (Some("false"), true, Ok(false)),
            (Some("False"), true, Ok(false)),
            (None, true, Ok(true)),
            (None, false, Ok(false)),
            (
                Some(" true "),
                false,
                Err(InvalidBool(" true ".to_string())),
            ),
            (Some("true "), false, Err(InvalidBool("true ".to_string()))),
            (Some("yes"), true, Err(InvalidBool("yes".to_string()))),
            (Some("tRuE"), true, Err(InvalidBool("tRuE".to_string()))),
            (Some(""), true, Err(InvalidBool(String::new()))),
        ];
        for (value, default, expected) in cases {
            assert_eq!(parse_bool(*value, *default), *expected, "value {value:?}");
        }
    }

    #[test]
    fn env_bool_reads_environment_and_falls_back_on_invalid() {
        const NAME: &str = "DATADOG_SERVERLESS_CORE_TEST_ENV_BOOL";
        // SAFETY: this is the only test that touches this variable.
        unsafe {
            std::env::remove_var(NAME);
            assert!(env_bool(NAME, true));
            std::env::set_var(NAME, "false");
            assert!(!env_bool(NAME, true));
            std::env::set_var(NAME, "yes");
            assert!(env_bool(NAME, true));
            assert!(!env_bool(NAME, false));
            std::env::remove_var(NAME);
        }
    }
}
