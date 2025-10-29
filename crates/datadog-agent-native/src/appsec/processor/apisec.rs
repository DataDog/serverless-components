//! API Security sampling for schema extraction.
//!
//! This module implements time-based, per-endpoint sampling to control the rate
//! at which API schemas are extracted and sent to Datadog. Without sampling, high-
//! traffic APIs would generate excessive overhead and data volume.
//!
//! # Sampling Strategy
//!
//! - **Per-Endpoint**: Each unique `(method, route, status)` tuple is tracked separately
//! - **Time-Based**: Sample at most once per interval (default: 30s) per endpoint
//! - **LRU Eviction**: When capacity is reached, oldest endpoints are evicted
//! - **Fast Hashing**: Uses FNV hash for endpoint signature computation
//!
//! # Example
//!
//! ```text
//! Endpoint: GET /users/{id} → 200
//! - First request at T=0s: SAMPLE (schema extracted)
//! - Request at T=15s: SKIP (within 30s interval)
//! - Request at T=35s: SAMPLE (interval elapsed)
//! ```
//!
//! # Memory Footprint
//!
//! Default capacity: 4,096 endpoints × 24 bytes/entry = ~96KB
//!
//! # Thread Safety
//!
//! This sampler is not thread-safe (uses `&mut self`). In the processor, it's
//! wrapped in `Arc<Mutex<>>` for concurrent access across async tasks.

use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::num::NonZero;
use std::time::Duration;

#[cfg(test)]
use mock_instant::global::Instant;
#[cfg(not(test))]
use std::time::Instant;

use fnv::FnvBuildHasher;
use ordered_hash_map::OrderedHashMap;

/// Time-based, per-endpoint sampler for API schema extraction.
///
/// This sampler implements the algorithm described in the
/// [API Security Sampling Design Doc](https://docs.google.com/document/d/1PYoHms9PPXR8V_5_T5-KXAhoFDKQYA8mTnmS12xkGOE/edit?pli=1&tab=t.0#heading=h.rnd972k0hiye).
///
/// The implementation is optimized for single-threaded access (no internal locking)
/// as the serverless extension does not present significant risk of contention.
///
/// # Fields
///
/// - `interval`: Minimum time between samples for the same endpoint
/// - `data`: LRU map of endpoint hashes to last sample timestamps
/// - `hasher_builder`: FNV hasher factory for fast endpoint signature computation
///
/// # Algorithm
///
/// 1. Compute hash of `(method, route, status_code)` tuple
/// 2. Check if endpoint exists in map:
///    - If yes and interval elapsed: SAMPLE and update timestamp
///    - If yes and interval not elapsed: SKIP
///    - If no and map not full: SAMPLE and insert
///    - If no and map full: Evict oldest, SAMPLE and insert
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::apisec::Sampler;
/// use std::time::Duration;
///
/// let mut sampler = Sampler::with_interval(Duration::from_secs(30));
///
/// // First request: sampled
/// assert!(sampler.decision_for("GET", "/users/{id}", "200"));
///
/// // Same endpoint, 10s later: not sampled
/// assert!(!sampler.decision_for("GET", "/users/{id}", "200"));
///
/// // Different endpoint: sampled (different hash)
/// assert!(sampler.decision_for("POST", "/users", "201"));
/// ```
#[derive(Debug)]
pub(crate) struct Sampler {
    /// Minimum duration between samples for the same endpoint.
    interval: Duration,

    /// LRU map of endpoint hashes to sampling state.
    /// Ordered for efficient eviction of oldest entries.
    data: OrderedHashMap<u64, SamplerState, BuildIdentityHasher>,

    /// Hash builder for computing endpoint signatures.
    /// Uses FNV for fast hashing of endpoint strings.
    hasher_builder: FnvBuildHasher,
}
impl Sampler {
    /// Creates a new sampler with the given interval and a default capacity.
    pub(crate) fn with_interval(interval: Duration) -> Self {
        Self::with_interval_and_capacity(interval, unsafe { NonZero::new_unchecked(4_096) })
    }

    /// Creates a new sampler with the given interval and capacity.
    pub(crate) fn with_interval_and_capacity(interval: Duration, capacity: NonZero<usize>) -> Self {
        Self {
            interval,
            data: OrderedHashMap::with_capacity_and_hasher(
                capacity.get(),
                BuildIdentityHasher::default(),
            ),
            hasher_builder: FnvBuildHasher::default(),
        }
    }

    /// Makes a sampling decision for a given endpoint signature.
    ///
    /// This method determines whether to sample (extract schema) for an endpoint
    /// based on the time since the last sample for this specific endpoint.
    ///
    /// # Arguments
    ///
    /// * `method` - HTTP method (e.g., "GET", "POST")
    /// * `route` - Route pattern (e.g., "/users/{id}")
    /// * `status_code` - HTTP status code as string (e.g., "200", "404")
    ///
    /// # Returns
    ///
    /// * `true` - Sample this request (extract and send schema)
    /// * `false` - Skip this request (within sampling interval)
    ///
    /// # Algorithm
    ///
    /// 1. Compute FNV hash of `method\0route\0status_code`
    /// 2. Check if endpoint exists in LRU map:
    ///    - **Exists + interval elapsed**: SAMPLE, update timestamp, move to back
    ///    - **Exists + interval not elapsed**: SKIP, move to back (mark as recent)
    ///    - **Not exists + map has space**: SAMPLE, insert new entry
    ///    - **Not exists + map full**: Evict oldest, SAMPLE, insert new entry
    ///
    /// # Side Effects
    ///
    /// - Updates internal state (last sample timestamps)
    /// - May evict oldest endpoint when at capacity
    /// - Moves accessed endpoints to back of LRU queue
    pub(crate) fn decision_for(&mut self, method: &str, route: &str, status_code: &str) -> bool {
        // Compute FNV hash of endpoint signature: method\0route\0status_code
        // Decision: Use null bytes as delimiters to avoid collision (e.g., "GETa" + "b" vs "GET" + "ab")
        let mut hasher = self.hasher_builder.build_hasher();
        hasher.write(method.as_bytes());
        hasher.write_u8(0); // Delimiter
        hasher.write(route.as_bytes());
        hasher.write_u8(0); // Delimiter
        hasher.write(status_code.as_bytes());
        let hash = hasher.finish();

        // Check if this endpoint has been seen before
        if let Some(state) = self.data.get_mut(&hash) {
            // Endpoint exists - check if sampling interval has elapsed
            // Decision: Sample if enough time has passed since last sample
            if state.last_sample.elapsed() >= self.interval {
                // Interval elapsed - sample this request
                state.last_sample = Instant::now();

                // Move entry to back of LRU queue (mark as most recently used)
                // Decision: Prevents eviction of active endpoints
                self.data.move_to_back(&hash);
                true // SAMPLE
            } else {
                // Interval not elapsed - skip this request
                // Move entry to back of LRU queue anyway (mark as accessed)
                // Decision: Keep active endpoints in memory even when not sampled
                self.data.move_to_back(&hash);
                false // SKIP
            }
        } else {
            // New endpoint - check if we need to evict oldest entry
            // Decision: Evict before insert to maintain capacity constraint
            if self.data.len() >= self.data.capacity() {
                // Map is full - evict oldest (front of LRU queue)
                // Decision: FIFO eviction ensures fairness across endpoints
                self.data.pop_front();
            }

            // Insert new endpoint with current timestamp
            self.data.insert(
                hash,
                SamplerState {
                    last_sample: Instant::now(),
                },
            );

            true // SAMPLE (first time seeing this endpoint)
        }
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
struct SamplerState {
    last_sample: Instant,
}

type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

/// A [Hasher] that forwards the value that is being hashed without any additonal processing.
#[derive(Debug, Default, Clone, Copy)]
struct IdentityHasher(u64);
#[allow(clippy::cast_sign_loss)] // This is not relevant in this Hasher implementation
impl Hasher for IdentityHasher {
    #[cfg_attr(coverage_nightly, coverage(off))] // Unsupported
    fn write(&mut self, _: &[u8]) {
        unimplemented!("IdentityHasher does not support hashing arbitrary data")
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_u8(&mut self, v: u8) {
        self.0 = u64::from(v);
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_u16(&mut self, v: u16) {
        self.0 = u64::from(v);
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_u32(&mut self, v: u32) {
        self.0 = u64::from(v);
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_usize(&mut self, v: usize) {
        self.0 = v as u64;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_i8(&mut self, v: i8) {
        self.0 = v as u64;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_i16(&mut self, v: i16) {
        self.0 = v as u64;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_i32(&mut self, v: i32) {
        self.0 = v as u64;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_i64(&mut self, v: i64) {
        self.0 = v as u64;
    }
    #[cfg_attr(coverage_nightly, coverage(off))] // Supported, but unused
    fn write_isize(&mut self, v: isize) {
        self.0 = v as u64;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
mod tests {
    use mock_instant::global::MockClock;

    use super::*;

    #[test]
    fn test_default_sampler() {
        let mut sampler = Sampler::with_interval(Duration::from_secs(30));
        // First call should be sampled
        assert!(sampler.decision_for("GET", "/", "200"));

        MockClock::advance(Duration::from_secs(15));
        // Second call should not be sampled (less than 30 seconds have passed)
        assert!(!sampler.decision_for("GET", "/", "200"));

        MockClock::advance(Duration::from_secs(15));
        // 30 seconds have passed, the call should be sampled in again!
        assert!(sampler.decision_for("GET", "/", "200"));
    }

    #[test]
    fn test_sampler_capacity() {
        let mut sampler = Sampler::with_interval_and_capacity(Duration::from_secs(30), unsafe {
            NonZero::new_unchecked(1)
        });

        for i in 0..100 {
            // All requests should be sampled in here since we run with a capacity of 1 and use a differen route each time...
            assert!(sampler.decision_for("GET", &format!("/{i}"), "200"));
        }
    }
}
