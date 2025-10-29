//! Generated protobuf models for the Datadog Remote Configuration service.
//!
//! This crate compiles the `datadog.remoteconfig` protobuf definitions and
//! re-exports the generated modules. All message types derive `serde`
//! serialization to ease JSON bridging in higher-level services.

#![doc = include_str!("../README.md")]

// Base64 serialization for single Vec<u8> field (required)
pub(crate) mod serde_base64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(value);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

// Base64 serialization for optional Vec<u8> field
pub(crate) mod serde_base64_option {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(bytes) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                serializer.serialize_some(&encoded)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        opt.map(|s| {
            base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(serde::de::Error::custom)
        })
        .transpose()
    }
}

// Base64 serialization for Vec<Vec<u8>> field
pub(crate) mod serde_base64_vec {
    use base64::Engine;
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[Vec<u8>], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(value.len()))?;
        for item in value {
            let encoded = base64::engine::general_purpose::STANDARD.encode(item);
            seq.serialize_element(&encoded)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let strings = Vec::<String>::deserialize(deserializer)?;
        strings
            .into_iter()
            .map(|s| {
                base64::engine::general_purpose::STANDARD
                    .decode(s.as_bytes())
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

// Legacy module for backwards compatibility (redirects to serde_base64_vec)
pub(crate) mod serde_bytes_vec {
    pub use super::serde_base64_vec::{deserialize, serialize};
}

// Custom deserializer that treats null as an empty vector
pub(crate) mod null_as_default {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Default + Deserialize<'de>,
    {
        let opt = Option::<T>::deserialize(deserializer)?;
        Ok(opt.unwrap_or_default())
    }
}

// Helper to mimic Go's `omitempty` behavior for protobuf enums/ints.
// Prevents serde from emitting fields whose value is just the default zero.
pub(crate) mod skip_zero_i32 {
    pub fn is_zero(value: &i32) -> bool {
        *value == 0
    }
}

// Skip serializing empty byte buffers (Vec<u8>) to mirror Go's omitempty behavior.
pub(crate) mod skip_empty_bytes {
    pub fn is_empty(value: &[u8]) -> bool {
        value.is_empty()
    }
}

// Skip serializing empty Vec<T> collections just like Go's json omitempty tags.
pub(crate) mod skip_vec {
    pub fn is_empty<T>(value: &[T]) -> bool {
        value.is_empty()
    }
}

#[allow(clippy::all)]
#[allow(missing_docs)]
pub mod remoteconfig {
    include!(concat!(env!("OUT_DIR"), "/datadog.config.rs"));
}

pub use remoteconfig::*;
