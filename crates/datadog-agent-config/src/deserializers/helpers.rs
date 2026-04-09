use serde::{Deserialize, Deserializer};
use serde_aux::prelude::deserialize_bool_from_anything;
use serde_json::Value;

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;
use tracing::warn;

use crate::TracePropagationStyle;

pub fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(s) => Ok(Some(s)),
        other => {
            warn!(
                "Failed to parse value, expected a string, got: {}, ignoring",
                other
            );
            Ok(None)
        }
    }
}

pub fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
        Value::Number(n) => Ok(Some(n.to_string())),
        _ => {
            warn!("Failed to parse value, expected a string or an integer, ignoring");
            Ok(None)
        }
    }
}

pub fn deserialize_optional_bool_from_anything<'de, D>(
    deserializer: D,
) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    // First try to deserialize as Option<_> to handle null/missing values
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(None),
        Some(value) => match deserialize_bool_from_anything(value) {
            Ok(bool_result) => Ok(Some(bool_result)),
            Err(e) => {
                warn!("Failed to parse bool value: {}, ignoring", e);
                Ok(None)
            }
        },
    }
}

/// Parse a single "key:value" string into a (key, value) tuple
/// Returns None if the string is invalid (e.g., missing colon, empty key/value)
fn parse_key_value_tag(tag: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = tag.splitn(2, ':').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        warn!(
            "Failed to parse tag '{}', expected format 'key:value', ignoring",
            tag
        );
        None
    }
}

pub fn deserialize_key_value_pairs<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct KeyValueVisitor;

    impl serde::de::Visitor<'_> for KeyValueVisitor {
        type Value = HashMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string in format 'key1:value1,key2:value2' or 'key1:value1'")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let mut map = HashMap::new();
            for tag in value.split(&[',', ' ']) {
                if tag.is_empty() {
                    continue;
                }
                if let Some((key, val)) = parse_key_value_tag(tag) {
                    map.insert(key, val);
                }
            }

            Ok(map)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            warn!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            warn!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            warn!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            warn!(
                "Failed to parse tags: expected string in format 'key:value', got boolean {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }
    }

    deserializer.deserialize_any(KeyValueVisitor)
}

pub fn deserialize_array_from_comma_separated_string<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    Ok(s.split(',')
        .map(|feature| feature.trim().to_string())
        .filter(|feature| !feature.is_empty())
        .collect())
}

pub fn deserialize_key_value_pair_array_to_hashmap<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let array: Vec<String> = match Vec::deserialize(deserializer) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to deserialize tags array: {e}, ignoring");
            return Ok(HashMap::new());
        }
    };
    let mut map = HashMap::new();
    for s in array {
        if let Some((key, val)) = parse_key_value_tag(&s) {
            map.insert(key, val);
        }
    }
    Ok(map)
}

/// Deserialize APM filter tags from space-separated "key:value" pairs, also support key-only tags
pub fn deserialize_apm_filter_tags<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => {
            let tags: Vec<String> = s
                .split_whitespace()
                .filter_map(|pair| {
                    let parts: Vec<&str> = pair.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let key = parts[0].trim();
                        let value = parts[1].trim();
                        if key.is_empty() {
                            None
                        } else if value.is_empty() {
                            Some(key.to_string())
                        } else {
                            Some(format!("{key}:{value}"))
                        }
                    } else if parts.len() == 1 {
                        let key = parts[0].trim();
                        if key.is_empty() {
                            None
                        } else {
                            Some(key.to_string())
                        }
                    } else {
                        None
                    }
                })
                .collect();

            if tags.is_empty() {
                Ok(None)
            } else {
                Ok(Some(tags))
            }
        }
    }
}

pub fn deserialize_option_lossless<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    match Option::<T>::deserialize(deserializer) {
        Ok(value) => Ok(value),
        Err(e) => {
            warn!("Failed to deserialize optional value: {}, ignoring", e);
            Ok(None)
        }
    }
}

/// Gracefully deserialize any field, falling back to `T::default()` on error.
///
/// This ensures that a single field with the wrong type never fails the entire
/// struct extraction. Works for any `T` that implements `Deserialize + Default`:
/// - `Option<T>` defaults to `None`
/// - `Vec<T>` defaults to `[]`
/// - `HashMap<K,V>` defaults to `{}`
/// - Structs with `#[derive(Default)]` use their default
pub fn deserialize_with_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    match T::deserialize(deserializer) {
        Ok(value) => Ok(value),
        Err(e) => {
            warn!("Failed to deserialize field: {}, using default", e);
            Ok(T::default())
        }
    }
}

pub fn deserialize_optional_duration_from_microseconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    match Option::<u64>::deserialize(deserializer) {
        Ok(opt) => Ok(opt.map(Duration::from_micros)),
        Err(e) => {
            warn!("Failed to deserialize duration (microseconds): {e}, ignoring");
            Ok(None)
        }
    }
}

pub fn deserialize_optional_duration_from_seconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    // Deserialize into a generic Value first to avoid propagating type errors,
    // then try to extract a duration from it.
    match Value::deserialize(deserializer) {
        Ok(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(Duration::from_secs(u)))
            } else if let Some(i) = n.as_i64() {
                if i < 0 {
                    warn!("Failed to parse duration: negative durations are not allowed, ignoring");
                    Ok(None)
                } else {
                    Ok(Some(Duration::from_secs(i as u64)))
                }
            } else if let Some(f) = n.as_f64() {
                if f < 0.0 {
                    warn!("Failed to parse duration: negative durations are not allowed, ignoring");
                    Ok(None)
                } else {
                    Ok(Some(Duration::from_secs_f64(f)))
                }
            } else {
                warn!("Failed to parse duration: unsupported number format, ignoring");
                Ok(None)
            }
        }
        Ok(Value::Null) => Ok(None),
        Ok(other) => {
            warn!("Failed to parse duration: expected number, got {other}, ignoring");
            Ok(None)
        }
        Err(e) => {
            warn!("Failed to deserialize duration: {e}, ignoring");
            Ok(None)
        }
    }
}

// Like deserialize_optional_duration_from_seconds(), but return None if the value is 0
pub fn deserialize_optional_duration_from_seconds_ignore_zero<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    let duration: Option<Duration> = deserialize_optional_duration_from_seconds(deserializer)?;
    if duration.is_some_and(|d| d.as_secs() == 0) {
        return Ok(None);
    }
    Ok(duration)
}

pub fn deserialize_trace_propagation_style<'de, D>(
    deserializer: D,
) -> Result<Vec<TracePropagationStyle>, D::Error>
where
    D: Deserializer<'de>,
{
    use std::str::FromStr;
    let s: String = match String::deserialize(deserializer) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to deserialize trace propagation style: {e}, ignoring");
            return Ok(Vec::new());
        }
    };

    Ok(s.split(',')
        .filter_map(
            |style| match TracePropagationStyle::from_str(style.trim()) {
                Ok(parsed_style) => Some(parsed_style),
                Err(e) => {
                    warn!("Failed to parse trace propagation style: {e}, ignoring");
                    None
                }
            },
        )
        .collect())
}
