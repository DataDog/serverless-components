// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;

use crate::constants::{MAX_BATCH_ENTRIES, MAX_CONTENT_BYTES, MAX_LOG_BYTES};
use crate::errors::AggregatorError;
use crate::intake_entry::IntakeEntry;

/// In-memory log batch accumulator.
///
/// Stores pre-serialized JSON strings in a FIFO queue. A batch is "full"
/// when it reaches `MAX_BATCH_ENTRIES` entries or `MAX_CONTENT_BYTES` of
/// uncompressed content.
pub struct Aggregator {
    messages: VecDeque<String>,
    current_size_bytes: usize,
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
        }
    }

    /// Insert a log entry into the batch.
    ///
    /// Returns `Ok(true)` if the batch is now full and ready to flush.
    /// Returns `Err(AggregatorError::EntryTooLarge)` if the serialized
    /// entry exceeds `MAX_LOG_BYTES` — the entry is dropped.
    pub fn insert(&mut self, entry: &IntakeEntry) -> Result<bool, AggregatorError> {
        let serialized = serde_json::to_string(entry)?;
        let len = serialized.len();

        if len > MAX_LOG_BYTES {
            return Err(AggregatorError::EntryTooLarge {
                size: len,
                max: MAX_LOG_BYTES,
            });
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
}
