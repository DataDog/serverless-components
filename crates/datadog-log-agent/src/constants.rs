// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

/// Maximum number of log entries per batch.
pub const MAX_BATCH_ENTRIES: usize = 1_000;

/// Maximum total uncompressed payload size per batch (5 MB).
pub const MAX_CONTENT_BYTES: usize = 5 * 1_024 * 1_024;

/// Maximum allowed size for a single serialized log entry (1 MB).
pub const MAX_LOG_BYTES: usize = 1_024 * 1_024;

/// Default Datadog site for log intake.
pub const DEFAULT_SITE: &str = "datadoghq.com";

/// Default flush timeout in seconds.
pub const DEFAULT_FLUSH_TIMEOUT_SECS: u64 = 5;

/// Negative values enable ultra-fast modes. Level 3 is the zstd library default.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;
