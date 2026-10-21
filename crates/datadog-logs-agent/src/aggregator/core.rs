// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;

use tracing::warn;

use crate::constants::{MAX_BATCH_ENTRIES, MAX_CONTENT_BYTES, MAX_LOG_BYTES};
use crate::errors::AggregatorError;
use crate::intake_entry::IntakeEntry;

/// What the aggregator does with a log entry larger than `MAX_LOG_BYTES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum OversizePolicy {
    /// Drop the entry: `insert` returns `EntryTooLarge`, and the aggregator service logs
    /// one warning per dropped entry.
    #[default]
    Drop,
    /// Keep the entry, with one warning, as long as it still fits in a payload on its own.
    /// Entries too big for any payload are dropped, with one warning from the service.
    SendWithWarning,
}

/// In-memory log batch accumulator.
///
/// Stores pre-serialized JSON strings in a FIFO queue. A batch is "full"
/// when it reaches `MAX_BATCH_ENTRIES` entries or `MAX_CONTENT_BYTES` of
/// uncompressed content.
pub struct Aggregator {
    messages: VecDeque<String>,
    current_size_bytes: usize,
    oversize_policy: OversizePolicy,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Aggregator {
    /// Create a new, empty aggregator.
    pub fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            current_size_bytes: 0,
            oversize_policy: OversizePolicy::default(),
        }
    }

    /// Set how entries larger than `MAX_LOG_BYTES` are handled. Default: drop them.
    #[must_use]
    pub fn with_oversize_policy(mut self, policy: OversizePolicy) -> Self {
        self.oversize_policy = policy;
        self
    }

    /// Insert a log entry into the batch.
    ///
    /// Returns `Ok(true)` if the batch is now full and ready to flush.
    /// Returns `Err(AggregatorError::EntryTooLarge)` if the entry is dropped under the
    /// oversize policy.
    pub fn insert(&mut self, entry: &IntakeEntry) -> Result<bool, AggregatorError> {
        let serialized = serde_json::to_string(entry)?;
        self.insert_serialized(serialized)
    }

    /// Insert an entry that is already serialized as a JSON object. It is batched as-is,
    /// without re-serializing or validating it: the caller must pass valid JSON, or the
    /// whole payload it lands in is rejected by the intake.
    ///
    /// Same return values as [`Aggregator::insert`].
    pub fn insert_serialized(&mut self, serialized: String) -> Result<bool, AggregatorError> {
        let len = serialized.len();

        if len > MAX_LOG_BYTES {
            match self.oversize_policy {
                OversizePolicy::Drop => {
                    return Err(AggregatorError::EntryTooLarge {
                        size: len,
                        max: MAX_LOG_BYTES,
                    });
                }
                OversizePolicy::SendWithWarning => {
                    // The entry must fit in a payload on its own: `[` + entry + `]`.
                    let max = MAX_CONTENT_BYTES - 2;
                    if len > max {
                        return Err(AggregatorError::EntryTooLarge { size: len, max });
                    }
                    warn!(
                        "log entry of {len} bytes exceeds {MAX_LOG_BYTES} bytes; sending it anyway"
                    );
                }
            }
        }

        self.messages.push_back(serialized);
        self.current_size_bytes += len;

        Ok(self.is_full())
    }

    /// Returns `true` if the batch has reached its entry count or byte limit.
    ///
    /// The byte check accounts for JSON framing: `[` + `]` (2 bytes) plus one
    /// comma per entry after the first (`N - 1` bytes).
    pub fn is_full(&self) -> bool {
        let n = self.messages.len();
        // framing: 2 bytes for `[`/`]` + (n - 1) commas
        let framing = if n == 0 { 0 } else { 2 + (n - 1) };
        n >= MAX_BATCH_ENTRIES || self.current_size_bytes + framing >= MAX_CONTENT_BYTES
    }

    /// Returns `true` if no log entries are buffered.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns the number of log entries currently buffered.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Drain up to `MAX_BATCH_ENTRIES` entries and return them as a JSON
    /// array (`[entry1,entry2,...]`) encoded as UTF-8 bytes.
    ///
    /// Returns `None` if the aggregator is empty.
    pub fn get_batch(&mut self) -> Option<Vec<u8>> {
        if self.messages.is_empty() {
            return None;
        }

        let mut output = Vec::new();
        output.push(b'[');
        let mut bytes_in_batch: usize = 0;
        let mut count: usize = 0;

        loop {
            if count >= MAX_BATCH_ENTRIES {
                break;
            }

            let msg_len = match self.messages.front() {
                Some(m) => m.len(),
                None => break,
            };

            // Account for the comma separator and the 2-byte `[`/`]` framing.
            // Total wire size = bytes_in_batch + (separator) + msg_len + 2 (for `[` and `]`)
            let separator = if count == 0 { 0 } else { 1 };
            if bytes_in_batch + separator + msg_len + 2 > MAX_CONTENT_BYTES {
                break;
            }

            // Safe: we just confirmed front() is Some and we hold &mut self
            let msg = match self.messages.pop_front() {
                Some(m) => m,
                None => break,
            };

            if count > 0 {
                output.push(b',');
                bytes_in_batch += 1;
            }

            self.current_size_bytes = self.current_size_bytes.saturating_sub(msg.len());
            bytes_in_batch += msg.len();
            count += 1;
            output.extend_from_slice(msg.as_bytes());
        }

        output.push(b']');
        Some(output)
    }

    /// Drain all entries and return them as a `Vec` of JSON array batches.
    /// May return multiple batches if the queue exceeds a single batch limit.
    pub fn get_all_batches(&mut self) -> Vec<Vec<u8>> {
        let mut batches = Vec::new();
        while let Some(batch) = self.get_batch() {
            batches.push(batch);
        }
        batches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake_entry::IntakeEntry;

    fn make_entry(msg: &str) -> IntakeEntry {
        IntakeEntry::from_message(msg, 1_700_000_000_000)
    }

    #[test]
    fn test_new_aggregator_is_empty() {
        let agg = Aggregator::new();
        assert!(agg.is_empty());
        assert_eq!(agg.len(), 0);
    }

    #[test]
    fn test_insert_single_entry() {
        let mut agg = Aggregator::new();
        let full = agg.insert(&make_entry("hello")).expect("insert failed");
        assert!(!full, "single entry should not fill the batch");
        assert_eq!(agg.len(), 1);
        assert!(!agg.is_empty());
    }

    #[test]
    fn test_get_batch_returns_valid_json_array() {
        let mut agg = Aggregator::new();
        agg.insert(&make_entry("line 1")).expect("insert");
        agg.insert(&make_entry("line 2")).expect("insert");

        let batch = agg.get_batch().expect("should have a batch");
        let parsed: serde_json::Value = serde_json::from_slice(&batch).expect("valid JSON");

        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["message"], "line 1");
        assert_eq!(parsed[1]["message"], "line 2");
    }

    #[test]
    fn test_get_batch_drains_aggregator() {
        let mut agg = Aggregator::new();
        agg.insert(&make_entry("log")).expect("insert");

        let _ = agg.get_batch();
        assert!(agg.is_empty(), "aggregator should be empty after get_batch");
        assert!(
            agg.get_batch().is_none(),
            "second get_batch should return None"
        );
    }

    #[test]
    fn test_entry_too_large_returns_error() {
        let mut agg = Aggregator::new();
        // Construct an entry whose JSON serialization exceeds MAX_LOG_BYTES
        let big_message = "x".repeat(crate::constants::MAX_LOG_BYTES + 1);
        let entry = make_entry(&big_message);
        let result = agg.insert(&entry);
        assert!(result.is_err(), "oversized entry should be rejected");
        assert!(
            agg.is_empty(),
            "aggregator should stay empty after rejection"
        );
    }

    #[test]
    fn test_insert_returns_true_when_batch_full_by_count() {
        use crate::constants::MAX_BATCH_ENTRIES;
        let mut agg = Aggregator::new();
        let entry = make_entry("x");

        for _ in 0..(MAX_BATCH_ENTRIES - 1) {
            let full = agg.insert(&entry).expect("insert");
            assert!(!full, "should not be full until last entry");
        }
        let full = agg.insert(&entry).expect("insert");
        assert!(full, "should be full after MAX_BATCH_ENTRIES entries");
        assert!(agg.is_full());
    }

    #[test]
    fn test_get_all_batches_splits_large_queue() {
        use crate::constants::MAX_BATCH_ENTRIES;
        let mut agg = Aggregator::new();
        let entry = make_entry("x");

        for _ in 0..(MAX_BATCH_ENTRIES + 5) {
            let _ = agg.insert(&entry);
        }

        let batches = agg.get_all_batches();
        assert_eq!(batches.len(), 2, "should produce 2 batches");

        let first: serde_json::Value = serde_json::from_slice(&batches[0]).expect("json");
        let second: serde_json::Value = serde_json::from_slice(&batches[1]).expect("json");
        assert_eq!(first.as_array().unwrap().len(), MAX_BATCH_ENTRIES);
        assert_eq!(second.as_array().unwrap().len(), 5);
    }

    #[test]
    fn test_get_all_batches_empty_returns_empty_vec() {
        let mut agg = Aggregator::new();
        assert!(agg.get_all_batches().is_empty());
    }

    #[test]
    fn test_batch_never_exceeds_max_content_bytes() {
        // Fill with entries whose sizes sum to just under MAX_CONTENT_BYTES so
        // that the framing bytes (`[`, `]`, commas) would push a naive
        // implementation over the limit.
        let mut agg = Aggregator::new();
        // Each entry's serialized JSON is roughly 50 bytes; pack enough entries
        // that their raw sum approaches MAX_CONTENT_BYTES.
        let entry = make_entry("x");
        for _ in 0..1000 {
            let _ = agg.insert(&entry);
        }

        for batch in agg.get_all_batches() {
            assert!(
                batch.len() <= MAX_CONTENT_BYTES,
                "batch size {} exceeds MAX_CONTENT_BYTES {}",
                batch.len(),
                MAX_CONTENT_BYTES
            );
        }
    }

    /// An entry whose JSON is exactly MAX_LOG_BYTES is accepted; one byte more is rejected.
    #[test]
    fn test_entry_at_max_log_bytes_is_accepted_and_one_more_byte_is_rejected() {
        let overhead = serde_json::to_string(&make_entry(""))
            .expect("serialize")
            .len();
        let mut agg = Aggregator::new();

        let exact = make_entry(&"a".repeat(MAX_LOG_BYTES - overhead));
        assert_eq!(
            serde_json::to_string(&exact).expect("serialize").len(),
            MAX_LOG_BYTES
        );
        assert!(agg.insert(&exact).is_ok());

        let too_big = make_entry(&"a".repeat(MAX_LOG_BYTES - overhead + 1));
        assert!(matches!(
            agg.insert(&too_big),
            Err(AggregatorError::EntryTooLarge { size, max })
                if size == MAX_LOG_BYTES + 1 && max == MAX_LOG_BYTES
        ));
        assert_eq!(agg.len(), 1);
    }

    /// A JSON string of exactly `len` bytes, for size tests.
    fn json_string_of_len(len: usize) -> String {
        format!("\"{}\"", "a".repeat(len - 2))
    }

    #[test]
    fn test_insert_serialized_keeps_entry_bytes_as_is() {
        let mut agg = Aggregator::new();
        let raw = r#"{"message":{"message":"x","lambda":{"arn":"a","request_id":null}},"ddsource":"lambda"}"#;
        agg.insert_serialized(raw.to_string()).expect("insert");
        assert_eq!(
            agg.get_batch().expect("batch"),
            format!("[{raw}]").into_bytes()
        );
    }

    #[test]
    fn test_insert_serialized_drops_oversized_entry_by_default() {
        let mut agg = Aggregator::new();
        let result = agg.insert_serialized(json_string_of_len(MAX_LOG_BYTES + 1));
        assert!(matches!(
            result,
            Err(AggregatorError::EntryTooLarge { size, max })
                if size == MAX_LOG_BYTES + 1 && max == MAX_LOG_BYTES
        ));
        assert!(agg.is_empty());
    }

    #[test]
    fn test_send_with_warning_keeps_entry_over_max_log_bytes() {
        let mut agg = Aggregator::new().with_oversize_policy(OversizePolicy::SendWithWarning);
        let big = json_string_of_len(2 * MAX_LOG_BYTES);
        agg.insert_serialized(big.clone()).expect("kept");
        assert_eq!(
            agg.get_batch().expect("batch"),
            format!("[{big}]").into_bytes()
        );
    }

    #[test]
    fn test_send_with_warning_drops_only_entries_that_cannot_fit_a_payload() {
        let mut agg = Aggregator::new().with_oversize_policy(OversizePolicy::SendWithWarning);
        let largest = MAX_CONTENT_BYTES - 2;
        agg.insert_serialized(json_string_of_len(largest))
            .expect("fits exactly");
        assert_eq!(agg.get_batch().expect("batch").len(), MAX_CONTENT_BYTES);

        let result = agg.insert_serialized(json_string_of_len(largest + 1));
        assert!(matches!(
            result,
            Err(AggregatorError::EntryTooLarge { size, max })
                if size == largest + 1 && max == largest
        ));
    }

    /// Three 2 MiB entries cannot share one 5 MiB payload: they are split across batches,
    /// every batch stays within the limit, and nothing is lost.
    #[test]
    fn test_send_with_warning_splits_oversized_entries_across_batches() {
        let mut agg = Aggregator::new().with_oversize_policy(OversizePolicy::SendWithWarning);
        for _ in 0..3 {
            agg.insert_serialized(json_string_of_len(2 * MAX_LOG_BYTES))
                .expect("kept");
        }
        agg.insert_serialized(r#"{"after":1}"#.to_string())
            .expect("kept");

        let batches = agg.get_all_batches();
        assert!(
            batches.len() >= 2,
            "three 2 MiB entries need at least two payloads"
        );
        assert!(batches.iter().all(|b| b.len() <= MAX_CONTENT_BYTES));
        let entries: usize = batches
            .iter()
            .map(|b| {
                serde_json::from_slice::<Vec<serde_json::Value>>(b)
                    .expect("json")
                    .len()
            })
            .sum();
        assert_eq!(entries, 4);
        assert!(agg.is_empty());
    }

    #[test]
    fn test_insert_entry_honors_send_with_warning() {
        let mut agg = Aggregator::new().with_oversize_policy(OversizePolicy::SendWithWarning);
        agg.insert(&make_entry(&"a".repeat(MAX_LOG_BYTES + 1)))
            .expect("kept");
        assert_eq!(agg.len(), 1);
    }
}
