// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Agent-side score sampler.
//!
//! 1:1 port of the Go trace agent's `ScoreSampler`/`ErrorsSampler`
//! (`pkg/trace/sampler/coresampler.go` + `scoresampler.go`). Seen traces are
//! counted per signature in a circular buffer of `NUM_BUCKETS` buckets of
//! `BUCKET_DURATION_SECS`. The sampler distributes a target TPS uniformly across
//! signatures; the bucket with the maximum count over the window drives the rate.

use std::collections::{HashMap, HashSet};

use crate::signature::{Signature, compute_signature_with_root_and_env, sample_by_rate};
use crate::{ErrorSamplerConfig, SampleDecision, TraceView};

const BUCKET_DURATION_SECS: i64 = 5;
const NUM_BUCKETS: usize = 6;
const MAX_RATE_INCREASE: f64 = 1.2;

// Max signature cardinality before shrinking (see `scoresampler.go`).
const SHRINK_CARDINALITY: u64 = 200;

/// The core counting/rate-computation engine shared by every score sampler.
struct Sampler {
    /// Per-signature counts in a circular buffer of `NUM_BUCKETS`.
    seen: HashMap<Signature, [f32; NUM_BUCKETS]>,
    /// Counts of all signatures combined, in the same circular buffer.
    all_sigs_seen: [f32; NUM_BUCKETS],
    /// Index of the last bucket on which traces were counted.
    last_bucket_id: i64,
    /// Sampling rate in [0, 1] per signature.
    rates: HashMap<Signature, f64>,
    /// Lowest rate across all signatures.
    lowest_rate: f64,
    /// Maximum total number of traces per second to sample.
    target_tps: f64,
    /// Extra raw sampling rate applied on top of the computed rate.
    extra_rate: f64,
}

impl Sampler {
    fn new(extra_rate: f64, target_tps: f64) -> Self {
        Sampler {
            seen: HashMap::new(),
            all_sigs_seen: [0.0; NUM_BUCKETS],
            last_bucket_id: 0,
            rates: HashMap::new(),
            lowest_rate: 0.0,
            target_tps,
            extra_rate,
        }
    }

    /// Counts a trace against its signature and updates rates when buckets rotate.
    fn count_weighted_sig(&mut self, now_unix_secs: i64, signature: Signature, n: f32) -> bool {
        let bucket_id = now_unix_secs / BUCKET_DURATION_SECS;
        let prev_bucket_id = self.last_bucket_id;
        self.last_bucket_id = bucket_id;

        // Pass through each bucket, zero expired ones and adjust sampling rates.
        let update_rates = prev_bucket_id != bucket_id;
        if update_rates {
            self.update_rates(prev_bucket_id, bucket_id);
        }

        let idx = bucket_index(bucket_id);
        let buckets = self.seen.entry(signature).or_insert([0.0; NUM_BUCKETS]);
        buckets[idx] += n;
        self.all_sigs_seen[idx] += n;

        update_rates
    }

    /// Distributes TPS across each signature and applies it to the moving max of
    /// seen buckets. Rate increases are bounded to 20% per evaluation.
    fn update_rates(&mut self, previous_bucket: i64, new_bucket: i64) {
        if self.seen.is_empty() {
            return;
        }

        let mut seen_tps: Vec<f64> = Vec::with_capacity(self.seen.len());
        let mut sigs: Vec<Signature> = Vec::with_capacity(self.seen.len());
        for (&sig, buckets) in self.seen.iter_mut() {
            let (max_bucket, rotated) = zero_and_get_max(*buckets, previous_bucket, new_bucket);
            *buckets = rotated;
            seen_tps.push(f64::from(max_bucket) / BUCKET_DURATION_SECS as f64);
            sigs.push(sig);
        }
        let (_, all_sigs_seen) = zero_and_get_max(self.all_sigs_seen, previous_bucket, new_bucket);
        self.all_sigs_seen = all_sigs_seen;

        let tps_per_sig = compute_tps_per_sig(self.target_tps, &seen_tps);

        let mut rates: HashMap<Signature, f64> = HashMap::with_capacity(sigs.len());
        self.lowest_rate = 1.0;
        for (i, &sig) in sigs.iter().enumerate() {
            let seen = seen_tps[i];
            let mut rate = 1.0;
            if tps_per_sig < seen && seen > 0.0 {
                rate = tps_per_sig / seen;
            }
            // Cap the rate increase to 20%.
            if let Some(&prev_rate) = self.rates.get(&sig)
                && prev_rate != 0.0
                && rate / prev_rate > MAX_RATE_INCREASE
            {
                rate = prev_rate * MAX_RATE_INCREASE;
            }
            if rate > 1.0 {
                rate = 1.0;
            }
            // No traffic on this signature, clean it up from the sampler.
            if rate == 1.0 && seen == 0.0 {
                self.seen.remove(&sig);
                continue;
            }
            if rate < self.lowest_rate {
                self.lowest_rate = rate;
            }
            rates.insert(sig, rate);
        }
        self.rates = rates;
    }

    /// Returns the sampling rate to apply to a signature.
    fn get_signature_sample_rate(&self, sig: Signature) -> f64 {
        match self.rates.get(&sig) {
            Some(&rate) => rate * self.extra_rate,
            None => self.default_rate(),
        }
    }

    /// Returns the sampling rate for every known signature, plus the default rate.
    #[cfg(test)]
    fn get_all_signature_sample_rates(&self) -> (HashMap<Signature, f64>, f64) {
        let rates = self
            .rates
            .iter()
            .map(|(&sig, &val)| (sig, val * self.extra_rate))
            .collect();
        (rates, self.default_rate())
    }

    /// Rate applied to unknown signatures, from the moving max of all signatures
    /// seen and the lowest stored rate.
    fn default_rate(&self) -> f64 {
        if self.target_tps == 0.0 {
            return 0.0;
        }

        let max_seen = self.all_sigs_seen.iter().copied().fold(0.0_f32, f32::max);
        let seen_tps = f64::from(max_seen) / BUCKET_DURATION_SECS as f64;

        let mut rate = 1.0;
        if self.target_tps < seen_tps && seen_tps > 0.0 {
            rate = self.target_tps / seen_tps;
        }
        if self.lowest_rate < rate && self.lowest_rate != 0.0 {
            return self.lowest_rate;
        }
        rate
    }

    fn size(&self) -> u64 {
        self.seen.len() as u64
    }
}

/// Bucket index for a bucket id, matching Go's `bucketID % numBuckets`.
fn bucket_index(bucket_id: i64) -> usize {
    bucket_id.rem_euclid(NUM_BUCKETS as i64) as usize
}

/// Distributes TPS across signatures. By default it spreads the target uniformly;
/// low-volume signatures that do not use their share have the remainder spread
/// across the others.
fn compute_tps_per_sig(target_tps: f64, seen: &[f64]) -> f64 {
    let mut sorted = seen.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut target_tps = target_tps;
    let mut sig_target = target_tps / sorted.len() as f64;

    for (i, &c) in sorted.iter().enumerate() {
        if c >= sig_target || i == sorted.len() - 1 {
            break;
        }
        target_tps -= c;
        sig_target = target_tps / (sorted.len() - i - 1) as f64;
    }
    sig_target
}

/// Zeroes expired buckets and returns the max count over the live window.
fn zero_and_get_max(
    mut buckets: [f32; NUM_BUCKETS],
    previous_bucket: i64,
    new_bucket: i64,
) -> (f32, [f32; NUM_BUCKETS]) {
    let mut max_bucket = 0.0_f32;
    let mut i = previous_bucket + 1;
    while i <= previous_bucket + NUM_BUCKETS as i64 {
        let index = bucket_index(i);

        // If a complete rotation happened between previous and new, all buckets
        // are zeroed.
        if i < new_bucket {
            buckets[index] = 0.0;
            i += 1;
            continue;
        }

        let value = buckets[index];
        if value > max_bucket {
            max_bucket = value;
        }

        // Zero after accounting for the previous value overridden by this rotation.
        if i == new_bucket {
            buckets[index] = 0.0;
        }
        i += 1;
    }
    (max_bucket, buckets)
}

/// Score sampler dedicated to catching traces containing spans with errors.
///
/// Rates are applied on the trace ID so that, for a given trace ID, error chunks
/// are kept together: `P(chunk1 kept and chunk2 kept) = min(P1, P2)`.
pub struct ErrorsSampler {
    sampler: Sampler,
    disabled: bool,
    /// Snapshot of active signatures taken when cardinality is exceeded.
    shrink_allow_list: Option<HashSet<Signature>>,
}

impl ErrorsSampler {
    /// Creates an error sampler from config. `target_tps == 0` disables it (every
    /// trace is dropped, i.e. never rescued).
    pub fn new(config: ErrorSamplerConfig) -> Self {
        ErrorsSampler {
            sampler: Sampler::new(config.extra_sample_rate, config.target_tps),
            disabled: config.target_tps == 0.0,
            shrink_allow_list: None,
        }
    }

    /// Counts an incoming trace and decides whether to rescue it.
    ///
    /// `now_unix_secs` drives the rolling-window rotation and is passed in (not
    /// read from a clock) to keep the crate dependency-free and deterministically
    /// testable.
    ///
    /// This does not check that the trace actually contains an error span, which
    /// mirrors the Go agent's `ScoreSampler.Sample` (it only guards `disabled`
    /// and empty traces). Filtering to errored traces is the caller's job: the
    /// integration site feeds only errored P0 chunks into the error sampler, so
    /// the error TPS budget is never spent on non-error traces.
    pub fn sample(&mut self, now_unix_secs: i64, trace: &TraceView) -> SampleDecision {
        // A malformed chunk (empty, or root_index past the end) cannot be scored;
        // do not rescue it. Guards the slice indexing in the signature computation.
        if self.disabled || trace.root_index >= trace.spans.len() {
            return SampleDecision::Drop;
        }

        let signature =
            compute_signature_with_root_and_env(trace.spans, trace.root_index, trace.env);
        let signature = self.shrink(signature);
        // Update sampler state by counting this trace.
        self.sampler
            .count_weighted_sig(now_unix_secs, signature, weight_root(trace));

        let rate = self.sampler.get_signature_sample_rate(signature);
        self.apply_sample_rate(trace, rate)
    }

    fn apply_sample_rate(&self, trace: &TraceView, rate: f64) -> SampleDecision {
        // No clamping, mirroring the Go agent's `applySampleRate`. A missing
        // global rate is 1.0 by convention (see `TraceView::root_global_sample_rate`),
        // and `sample_by_rate` already treats any `new_rate >= 1.0` as always-keep,
        // so the product is well-defined without bounding it to [0, 1].
        let new_rate = trace.root_global_sample_rate * rate;
        if sample_by_rate(trace.trace_id, new_rate) {
            SampleDecision::Keep { errors_sr: rate }
        } else {
            SampleDecision::Drop
        }
    }

    /// Limits the number of stored signatures. Once cardinality exceeds
    /// `SHRINK_CARDINALITY / 2`, new signatures fold onto a fixed set of values;
    /// previously active signatures are unaffected.
    fn shrink(&mut self, sig: Signature) -> Signature {
        if self.sampler.size() < SHRINK_CARDINALITY / 2 {
            self.shrink_allow_list = None;
            return sig;
        }
        let sampler = &self.sampler;
        let allow_list = self
            .shrink_allow_list
            .get_or_insert_with(|| sampler.rates.keys().copied().collect());
        if allow_list.contains(&sig) {
            return sig;
        }
        sig % (SHRINK_CARDINALITY / 2)
    }
}

/// Weight of the root span: the inverse of the sampling already applied upstream.
///
/// Mirrors Go's `weightRoot`, using the root's global sample rate as the client
/// rate (clamped to `(0, 1]`). Serverless has no agent pre-sampler, so the
/// pre-sampler rate is always 1.
fn weight_root(trace: &TraceView) -> f32 {
    let mut client_rate = trace.root_global_sample_rate;
    if client_rate <= 0.0 || client_rate > 1.0 {
        client_rate = 1.0;
    }
    (1.0 / client_rate) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpanView;

    const BUCKET: i64 = BUCKET_DURATION_SECS;

    fn cfg(target_tps: f64) -> ErrorSamplerConfig {
        ErrorSamplerConfig {
            target_tps,
            extra_sample_rate: 1.0,
        }
    }

    fn error_trace<'a>(trace_id: u64, spans: &'a [SpanView<'a>]) -> TraceView<'a> {
        TraceView {
            env: "testEnv",
            trace_id,
            root_index: 0,
            root_global_sample_rate: 1.0,
            spans,
        }
    }

    // Ports TestZeroAndGetMaxBuckets.
    #[test]
    fn zero_and_get_max_buckets() {
        // same bucket: nothing zeroed, max over all.
        let (max, buckets) = zero_and_get_max([10.0, 11.0, 12.0, 13.0, 14.0, 15.0], 0, 0);
        assert_eq!(max, 15.0);
        assert_eq!(buckets, [10.0, 11.0, 12.0, 13.0, 14.0, 15.0]);

        // full rotation: everything zeroed. Test each slot plus extra rotations.
        for i in 0..(NUM_BUCKETS as i64 * 2) {
            let (max, buckets) = zero_and_get_max(
                [10.0, 11.0, 12.0, 13.0, 14.0, 15.0],
                i,
                (NUM_BUCKETS as i64 + 1) + i,
            );
            assert_eq!(max, 0.0);
            assert_eq!(buckets, [0.0; NUM_BUCKETS]);
        }

        // 3 zeroes.
        let (max, buckets) = zero_and_get_max([10.0, 11.0, 17.0, 13.0, 14.0, 19.0], 3, 7);
        assert_eq!(max, 17.0);
        assert_eq!(buckets, [0.0, 0.0, 17.0, 13.0, 0.0, 0.0]);

        // 4 zeroes.
        let (max, buckets) = zero_and_get_max([10.0, 11.0, 10.0, 13.0, 14.0, 19.0], 3, 8);
        assert_eq!(max, 13.0);
        assert_eq!(buckets, [0.0, 0.0, 0.0, 13.0, 0.0, 0.0]);

        // 4 zeroes, max is the new window.
        let (max, buckets) = zero_and_get_max([10.0, 11.0, 129_191.0, 13.0, 14.0, 19.0], 3, 8);
        assert_eq!(max, 129_191.0);
        assert_eq!(buckets, [0.0, 0.0, 0.0, 13.0, 0.0, 0.0]);
    }

    // Ports TestComputeTPSPerSig.
    #[test]
    fn compute_tps_per_sig_cases() {
        let eps = 1e-8;
        assert_eq!(compute_tps_per_sig(0.0, &[0.0, 10.0, 100.0, 3.0, 0.0]), 0.0);
        assert!((compute_tps_per_sig(2.0, &[0.0, 10.0, 100.0, 3.0, 0.0]) - 2.0 / 3.0).abs() < eps);
        assert!((compute_tps_per_sig(10.0, &[0.0, 10.0, 100.0, 3.0, 0.0]) - 3.5).abs() < eps);
        assert!((compute_tps_per_sig(23.5, &[10.0, 0.0, 100.0, 3.0, 0.0]) - 10.5).abs() < eps);
        assert!((compute_tps_per_sig(53.5, &[10.0, 0.0, 100.0, 3.0, 0.0]) - 40.5).abs() < eps);
    }

    // Ports TestDefaultRate.
    #[test]
    fn default_rate() {
        let mut s = Sampler::new(1.0, 10.0);
        s.count_weighted_sig(0, 0, 1000.0);
        let (_, default_rate) = s.get_all_signature_sample_rates();
        assert_eq!(default_rate, 1.0 / 20.0);
        assert_eq!(s.get_signature_sample_rate(100), 1.0 / 20.0);
    }

    // Ports TestRateIncrease: rate recovers toward 1.0 capped at 20% per evaluation.
    #[test]
    fn rate_increase_capped() {
        let target_tps = 7.0;
        let initial_tps = 21.0;
        let mut s = Sampler::new(1.0, target_tps);

        let test_sig: Signature = 25;
        s.count_weighted_sig(0, test_sig, (initial_tps * BUCKET as f64) as f32);
        // Force rate evaluation by rotating one bucket.
        s.count_weighted_sig(BUCKET, test_sig, 0.0);

        // Move to the last bucket of the max window: the initial count is still
        // the moving max on the first evaluation, then decays out afterwards.
        let base = NUM_BUCKETS as i64 * BUCKET;
        let mut expected_rate = target_tps / initial_tps;
        for i in 0..=10 {
            s.count_weighted_sig(base + i * BUCKET, 0, 1.0);
            let (rates, _) = s.get_all_signature_sample_rates();
            let rate = *rates.get(&test_sig).expect("test sig present");
            assert!(
                (rate - expected_rate).abs() < 1e-9,
                "i={i} rate={rate} expected={expected_rate}"
            );
            expected_rate *= MAX_RATE_INCREASE;
            if expected_rate > 1.0 {
                break;
            }
        }
    }

    // Ports TestOldSigEviction: a signature with no traffic is eventually evicted.
    #[test]
    fn old_sig_eviction() {
        let target_tps = 7.0;
        let initial_tps = 21.0;
        let mut s = Sampler::new(1.0, target_tps);

        let test_sig: Signature = 25;
        s.count_weighted_sig(0, test_sig, (initial_tps * BUCKET as f64) as f32);
        s.count_weighted_sig(BUCKET, test_sig, 0.0);

        let base = NUM_BUCKETS as i64 * BUCKET;
        for i in 0..=20 {
            s.count_weighted_sig(base + i * BUCKET, 0, 1.0);
            if i < 5 {
                let (rates, _) = s.get_all_signature_sample_rates();
                assert!(rates.contains_key(&test_sig), "i={i}");
                assert!(s.seen.contains_key(&test_sig), "i={i}");
            }
        }
        let (rates, default_rate) = s.get_all_signature_sample_rates();
        assert!(!rates.contains_key(&test_sig));
        assert_eq!(default_rate, 1.0);
        assert!(!s.seen.contains_key(&test_sig));
    }

    // Ports the rate-computation half of TestTargetTPSPerSigUpdate: after counting
    // several signatures and rotating a bucket, each rate settles at
    // target_tps_per_sig / seenTPS, and the default rate tracks the busiest sig.
    #[test]
    fn per_sig_rates_settle() {
        let target_tps = 10.0;
        let mut s = Sampler::new(1.0, target_tps);

        let initial: [f32; 5] = [37.0, 3.0, 21.0, 2921.0, 5.0];
        for (i, &c) in initial.iter().enumerate() {
            s.count_weighted_sig(0, i as Signature, c * BUCKET as f32);
        }
        // Trigger rate computation via a bucket rotation.
        s.count_weighted_sig(BUCKET, 0, 0.0);

        let eps = 1e-10;
        let (rates, default_rate) = s.get_all_signature_sample_rates();
        // With target 10 across 5 sigs, low-volume sigs (3, 5) do not use their
        // 2.0 share, so the surplus cascades and tps_per_sig lands at 2.0.
        let expected: [f64; 5] = [2.0 / 37.0, 2.0 / 3.0, 2.0 / 21.0, 2.0 / 2921.0, 2.0 / 5.0];
        for (i, &e) in expected.iter().enumerate() {
            let r = *rates.get(&(i as Signature)).expect("sig present");
            assert!(
                (r - e).abs() < eps * e.max(1.0),
                "sig={i} rate={r} expected={e}"
            );
        }
        assert!((default_rate - 2.0 / 2921.0).abs() < eps);
    }

    // Ports TestDisable: target_tps == 0 always drops.
    #[test]
    fn disabled_always_drops() {
        let mut s = ErrorsSampler::new(cfg(0.0));
        let spans = [SpanView {
            service: "mcnulty",
            name: "web",
            resource: "/",
            error: true,
            http_status_code: None,
            error_type: None,
        }];
        for _ in 0..100 {
            assert_eq!(s.sample(0, &error_trace(42, &spans)), SampleDecision::Drop);
        }
    }

    // A keep stamps errors_sr with the *signature* rate, not new_rate
    // (= root_global_sample_rate * signature_rate). With a global rate of 0.5 the
    // two differ, so this catches a bug that stamped new_rate instead.
    #[test]
    fn keep_stamps_signature_rate_not_new_rate() {
        let mut s = ErrorsSampler::new(cfg(10.0));
        let spans = [SpanView {
            service: "mcnulty",
            name: "web",
            resource: "/",
            error: true,
            http_status_code: None,
            error_type: None,
        }];
        // Low volume => signature rate stays 1.0. trace_id 2 is kept at new_rate 0.5
        // (2 * SAMPLER_HASHER < 2^63). errors_sr must be the signature rate 1.0,
        // not new_rate 0.5.
        let trace = TraceView {
            env: "testEnv",
            trace_id: 2,
            root_index: 0,
            root_global_sample_rate: 0.5,
            spans: &spans,
        };
        match s.sample(0, &trace) {
            SampleDecision::Keep { errors_sr } => assert_eq!(errors_sr, 1.0),
            SampleDecision::Drop => panic!("trace_id 2 should be kept at rate 0.5"),
        }
    }

    // Once a signature earns a fractional map rate under load, a keep stamps that
    // exact rate (including extra_sample_rate), exercising the rates-map branch
    // rather than the default-rate fallback.
    #[test]
    fn keep_stamps_fractional_map_rate() {
        use crate::signature::compute_signature_with_root_and_env;

        let mut s = ErrorsSampler::new(ErrorSamplerConfig {
            target_tps: 1.0,
            extra_sample_rate: 0.5,
        });
        let spans = [SpanView {
            service: "busy",
            name: "web",
            resource: "/",
            error: true,
            http_status_code: None,
            error_type: None,
        }];
        let sig = compute_signature_with_root_and_env(&spans, 0, "testEnv");

        // Drive volume of this one signature in bucket 0: 50 hits => seenTPS 10 =>
        // signature rate 1/10, times extra_sample_rate 0.5 => reported 0.05, high
        // enough that keeps are plentiful across the trace ids below.
        for i in 0..50u64 {
            s.sample(0, &error_trace(i, &spans));
        }
        // Rotate a bucket to trigger rate computation.
        s.sample(BUCKET, &error_trace(999, &spans));

        let rate = s.sampler.get_signature_sample_rate(sig);
        assert!(rate < 1.0, "expected a throttled rate, got {rate}");
        // extra_sample_rate 0.5 must be folded into the reported rate.
        assert!(rate <= 0.5);

        // Same bucket (no further rotation), so the rate is stable. Every keep must
        // stamp exactly that rate.
        let mut saw_keep = false;
        for i in 0..400u64 {
            let id = i.wrapping_mul(2_654_435_761);
            if let SampleDecision::Keep { errors_sr } =
                s.sample(BUCKET + 1, &error_trace(id, &spans))
            {
                assert_eq!(errors_sr, rate);
                saw_keep = true;
            }
        }
        assert!(saw_keep, "expected at least one keep across 300 trace ids");
    }

    // Ports TestTargetTPS: under heavy load the keep throughput approximates
    // target_tps.
    #[test]
    fn target_tps_effectiveness() {
        let target_tps = 10.0;
        let generated_tps = 200.0;
        let mut s = ErrorsSampler::new(cfg(target_tps));

        let init_periods = 2;
        let periods = 10;
        let traces_per_period = (generated_tps * BUCKET as f64) as i64;

        // Pre-intern the 50 distinct service names once, rather than leaking a
        // fresh String per trace.
        let services: Vec<String> = (0..50).map(|n| format!("svc-{n}")).collect();

        let mut sampled_count = 0i64;
        let mut now = 0i64;
        for period in 0..(init_periods + periods) {
            now += BUCKET;
            for i in 0..traces_per_period {
                // Vary signature and trace id to spread load, like the Go test.
                let spans = [SpanView {
                    service: &services[(i % 50) as usize],
                    name: "web",
                    resource: "/",
                    error: true,
                    http_status_code: None,
                    error_type: None,
                }];
                let trace = TraceView {
                    env: "testEnv",
                    trace_id: (i as u64).wrapping_mul(2_654_435_761),
                    root_index: 0,
                    root_global_sample_rate: 1.0,
                    spans: &spans,
                };
                let kept = matches!(s.sample(now, &trace), SampleDecision::Keep { .. });
                if period > init_periods && kept {
                    sampled_count += 1;
                }
            }
        }

        let kept_tps = sampled_count as f64 / (periods as f64 * BUCKET as f64);
        // Go's TestTargetTPS asserts within 20% (InEpsilon 0.2). This port drives a
        // deterministic pseudo-random load rather than Go's RNG, so it allows a
        // looser 30% band; it still fails on any gross rate-computation regression.
        assert!(
            kept_tps > target_tps * 0.7 && kept_tps < target_tps * 1.3,
            "kept_tps={kept_tps} target={target_tps}"
        );
    }

    // Shrink: below the cardinality threshold, signatures pass through unchanged;
    // once the allow-list snapshot is taken, unknown signatures fold onto
    // [0, SHRINK_CARDINALITY/2).
    #[test]
    fn shrink_folds_new_signatures() {
        let mut s = ErrorsSampler::new(cfg(10.0));
        // Below threshold: passthrough and no allow-list.
        assert_eq!(s.shrink(123_456), 123_456);
        assert!(s.shrink_allow_list.is_none());

        // Fill the sampler past SHRINK_CARDINALITY/2 distinct signatures, populating
        // both `seen` (drives the size threshold) and `rates` (the allow-list source,
        // as production does via update_rates).
        let half = SHRINK_CARDINALITY / 2;
        for sig in 0..half + 10 {
            s.sampler.seen.insert(sig, [1.0; NUM_BUCKETS]);
            s.sampler.rates.insert(sig, 0.5);
        }

        // An active signature above `half` passes through unchanged (allow-list hit):
        // active signatures are never folded even though shrinking is in effect.
        let active = half + 5;
        assert_eq!(s.shrink(active), active);
        assert!(s.shrink_allow_list.is_some());

        // A signature not in the allow-list folds into [0, half).
        let folded = s.shrink(1_000_003);
        assert_eq!(folded, 1_000_003 % half);
        assert!(folded < half);
    }
}
