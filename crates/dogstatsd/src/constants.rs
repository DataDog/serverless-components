// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

/// The maximum tags that a `Metric` may hold.
pub const MAX_TAGS: usize = 100;

pub const CONTEXTS: usize = 10_240;

pub static MAX_CONTEXTS: usize = 65_536; // 2**16, arbitrary

/// Width in seconds of the time buckets that metrics are aggregated into.
pub(crate) const BUCKET_SECONDS: u32 = 10;

const MB: u64 = 1_024 * 1_024;

pub(crate) const MAX_ENTRIES_SINGLE_METRIC: usize = 1_000;

pub(crate) const MAX_SIZE_BYTES_SINGLE_METRIC: u64 = 5 * MB;

pub(crate) const MAX_ENTRIES_SKETCH_METRIC: usize = 1_000;

pub(crate) const MAX_SIZE_SKETCH_METRIC: u64 = 62 * MB;
