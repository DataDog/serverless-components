// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

pub mod aggregator;
pub mod config;
pub mod constants;
pub mod errors;
pub mod flusher;
pub mod log_entry;

#[cfg(test)]
mod tests {
    use crate::constants::{MAX_BATCH_ENTRIES, MAX_CONTENT_BYTES, MAX_LOG_BYTES};

    #[test]
    fn test_constants_are_sane() {
        assert_eq!(MAX_BATCH_ENTRIES, 1_000);
        assert_eq!(MAX_CONTENT_BYTES, 5 * 1_024 * 1_024);
        assert_eq!(MAX_LOG_BYTES, 1_024 * 1_024);
        assert!(MAX_LOG_BYTES < MAX_CONTENT_BYTES);
    }
}
