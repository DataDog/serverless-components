// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Utility functions for DogStatsD operations.

/// Parses and validates a metric namespace string according to Datadog naming conventions.
///
/// A valid namespace must:
/// - Start with an ASCII letter
/// - Contain only ASCII alphanumerics, underscores, or periods
/// - Not be empty or contain only whitespace
///
/// Whitespace is automatically trimmed from the input.
///
/// # Arguments
///
/// * `namespace` - The namespace string to parse
///
/// # Returns
///
/// * `Some(String)` - The trimmed namespace if valid
/// * `None` - If the namespace is invalid or empty
///
/// # Examples
///
/// ```
/// use dogstatsd::util::parse_metric_namespace;
///
/// assert_eq!(parse_metric_namespace("myapp"), Some("myapp".to_string()));
/// assert_eq!(parse_metric_namespace("my_app.metrics"), Some("my_app.metrics".to_string()));
/// assert_eq!(parse_metric_namespace("1invalid"), None);
/// assert_eq!(parse_metric_namespace("my-app"), None);
/// ```
pub fn parse_metric_namespace(namespace: &str) -> Option<String> {
    let trimmed = namespace.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut chars = trimmed.chars();

    // Check first character is a letter
    if let Some(first_char) = chars.next() {
        if !first_char.is_ascii_alphabetic() {
            tracing::error!(
                "DD_STATSD_METRIC_NAMESPACE must start with a letter, got: '{}'. Ignoring namespace.",
                trimmed
            );
            return None;
        }
    } else {
        return None;
    }

    // Check remaining characters are valid (alphanumeric, underscore, or period)
    if let Some(invalid_char) =
        chars.find(|&ch| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '.')
    {
        tracing::error!(
            "DD_STATSD_METRIC_NAMESPACE contains invalid character '{}' in '{}'. Only ASCII alphanumerics, underscores, and periods are allowed. Ignoring namespace.",
            invalid_char, trimmed
        );
        return None;
    }

    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_metric_namespace_valid() {
        assert_eq!(parse_metric_namespace("myapp"), Some("myapp".to_string()));
        assert_eq!(parse_metric_namespace("my_app"), Some("my_app".to_string()));
        assert_eq!(parse_metric_namespace("my.app"), Some("my.app".to_string()));
        assert_eq!(
            parse_metric_namespace("myApp123"),
            Some("myApp123".to_string())
        );
        assert_eq!(
            parse_metric_namespace("a1.b2_c3"),
            Some("a1.b2_c3".to_string())
        );
    }

    #[test]
    fn test_parse_metric_namespace_with_whitespace() {
        assert_eq!(
            parse_metric_namespace("  myapp  "),
            Some("myapp".to_string())
        );
        assert_eq!(
            parse_metric_namespace("\tmyapp\n"),
            Some("myapp".to_string())
        );
    }

    #[test]
    fn test_parse_metric_namespace_empty() {
        assert_eq!(parse_metric_namespace(""), None);
        assert_eq!(parse_metric_namespace("   "), None);
        assert_eq!(parse_metric_namespace("\t\n"), None);
    }

    #[test]
    fn test_parse_metric_namespace_invalid_start() {
        assert_eq!(parse_metric_namespace("1myapp"), None);
        assert_eq!(parse_metric_namespace("_myapp"), None);
        assert_eq!(parse_metric_namespace(".myapp"), None);
        assert_eq!(parse_metric_namespace("-myapp"), None);
    }

    #[test]
    fn test_parse_metric_namespace_invalid_characters() {
        assert_eq!(parse_metric_namespace("my-app"), None);
        assert_eq!(parse_metric_namespace("my app"), None);
        assert_eq!(parse_metric_namespace("my@app"), None);
        assert_eq!(parse_metric_namespace("my#app"), None);
        assert_eq!(parse_metric_namespace("my$app"), None);
        assert_eq!(parse_metric_namespace("my!app"), None);
    }
}
