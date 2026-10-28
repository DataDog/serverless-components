// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Hot-path benchmarks for DogStatsD: parse, packet, insert, flush, compress
//! and the `ustr` interner.
//!
//! Run with `cargo bench -p dogstatsd`, or filter with
//! `cargo bench -p dogstatsd -- dogstatsd/parse`.

use std::hint::black_box;
use std::io::Write;
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

use criterion::{BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group};
use ddsketch_agent::DDSketch;
use dogstatsd::aggregator::Aggregator;
use dogstatsd::constants::MAX_CONTEXTS;
use dogstatsd::datadog::Series;
use dogstatsd::metric::{self, Metric, MetricValue, SortedTags, parse};
use protobuf::Message;
use ustr::Ustr;
use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer};

const ENRICH_TAGS: &str = "function_arn:arn:aws:lambda:us-east-1:123456789012:function:my-checkout-fn,region:us-east-1,account_id:123456789012,aws_account:123456789012,functionname:my-checkout-fn,resource:my-checkout-fn,executedversion:$LATEST,memorysize:128,architecture:arm64,runtime:nodejs22.x,_dd.runtime_family:nodejs,env:prod,service:checkout,version:1.2.3,dd_extension_version:90,_dd.compute_stats:1";
const TAGS_8: &str = "env:prod,service:checkout,version:1.2.3,region:us-east-1,team:payments,endpoint:/api/v1/pay,status:200,method:POST";
const COUNT_2_TAGS: &str = "my.metric:1|c|#tag1:a,tag2:b";
const DIST_2_TAGS: &str = "my.metric:1.5|d|#tag1:a,tag2:b";
const DIST_8_TAGS: &str = "app.checkout.latency:12.5|d|#env:prod,service:checkout,version:1.2.3,region:us-east-1,team:payments,endpoint:/api/v1/pay,status:200,method:POST";
const CONTEXT_COUNTS: [usize; 3] = [20, 1_000, 10_000];
const INSERT_BATCH: usize = 1_000;
// The flusher's default level: `metrics_config_compression_level` in
// the `logs_config_compression_level` default in datadog-agent-config.
const ZSTD_LEVEL: i32 = 3;

// The `ustr` interner is process-global, so its first use is measured in a
// child process: the bench binary re-executes itself with this variable set.
const CHILD_ENV: &str = "DOGSTATSD_BENCH_CHILD";
const FIRST_PARSE_LINE: &str = "warm:1|c|#a:b";

fn enrich_tags() -> SortedTags {
    SortedTags::parse(ENRICH_TAGS).expect("enrich tags parse")
}

fn lcg(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *seed >> 33
}

/// `n` values in `1..=distinct`, in pseudo-random order.
fn sketch_values(n: usize, distinct: u64) -> Vec<f64> {
    let mut seed = 42;
    (0..n)
        .map(|_| (lcg(&mut seed) % distinct + 1) as f64)
        .collect()
}

fn unique_line(i: usize) -> String {
    format!("my.metric:1|c|#env:prod,service:api,request_id:{i:08x}-0000-4000-8000-{i:012x}")
}

fn build_aggregator(contexts: usize, distribution: bool) -> Aggregator {
    let mut aggregator = Aggregator::new(enrich_tags(), MAX_CONTEXTS).expect("aggregator");
    for i in 0..contexts {
        let line = if distribution {
            format!(
                "app.request.latency{}:{}|d|#endpoint:/api/v{i},status:200,method:GET",
                i % 50,
                (i % 900) + 1
            )
        } else {
            format!(
                "app.request.count{}:1|c|#endpoint:/api/v{i},status:200,method:GET",
                i % 50
            )
        };
        aggregator
            .insert(parse(&line).expect("parse"))
            .expect("insert");
    }
    aggregator
}

fn series_bodies(series: &[Series]) -> Vec<Vec<u8>> {
    series
        .iter()
        .map(|s| serde_json::to_vec(s).expect("serialize series"))
        .collect()
}

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("dogstatsd/parse");
    group.throughput(Throughput::Elements(1));
    for (id, line) in [
        ("count_2_tags", COUNT_2_TAGS),
        ("distribution_2_tags", DIST_2_TAGS),
        ("distribution_8_tags", DIST_8_TAGS),
    ] {
        parse(line).expect("parse");
        group.bench_function(id, |b| b.iter(|| parse(black_box(line))));
    }
    group.bench_function("sorted_tags_8", |b| {
        b.iter(|| SortedTags::parse(black_box(TAGS_8)))
    });
    group.bench_function("sorted_tags_16_enrich", |b| {
        b.iter(|| SortedTags::parse(black_box(ENRICH_TAGS)))
    });
    let name = Ustr::from("app.checkout.latency");
    let tags = Some(SortedTags::parse(TAGS_8).expect("tags"));
    group.bench_function("metric_id_8_tags", |b| {
        b.iter(|| metric::id(black_box(name), black_box(&tags), black_box(1_700_000_000)))
    });
    // The single-point sketch every distribution line builds.
    group.bench_function("distribution_value", |b| {
        b.iter(|| MetricValue::distribution(black_box(12.5)))
    });
    group.finish();
}

/// Mirrors the per-datagram path up to the aggregator send, with no namespace:
/// `process_packet` and `insert_metrics` in src/dogstatsd.rs. Log calls are left out.
fn packet_today(buf: &[u8]) -> Vec<Metric> {
    let Ok(msgs) = std::str::from_utf8(buf) else {
        return Vec::new();
    };
    let statsd_metric_strings: Vec<&str> = msgs.split('\n').collect();
    let metric_count_in_packet = statsd_metric_strings
        .iter()
        .filter(|m| !m.is_empty())
        .count();
    black_box(metric_count_in_packet);
    statsd_metric_strings
        .into_iter()
        .filter(|m| {
            !m.is_empty()
                && !m.starts_with("_sc|")
                && !m.starts_with("_e{")
                && !m.starts_with("aws.lambda.enhanced.invocations")
        })
        .map(|m| m.replace('\n', ""))
        .filter_map(|m| parse(m.as_str()).ok())
        .collect()
}

fn bench_packet(c: &mut Criterion) {
    let lines: Vec<String> = (0..20)
        .map(|i| format!("my.metric{i}:1|c|#tag1:a,tag2:b"))
        .collect();
    let packet = lines.join("\n");
    assert_eq!(packet_today(packet.as_bytes()).len(), 20);

    let mut group = c.benchmark_group("dogstatsd/packet");
    group.throughput(Throughput::Elements(20));
    group.bench_function("20_lines", |b| {
        b.iter(|| packet_today(black_box(packet.as_bytes())))
    });
    group.finish();
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("dogstatsd/insert");

    // Occupied path: every metric lands on one of 100 existing contexts.
    let lines: Vec<String> = (0..100)
        // `|T` pins the bucket, so the batch and the base never straddle a 10 s boundary.
        .map(|i| format!("my.metric{i}:1|c|#tag1:a,tag2:b|T1700000000"))
        .collect();
    let mut base = Aggregator::new(enrich_tags(), MAX_CONTEXTS).expect("aggregator");
    for line in &lines {
        base.insert(parse(line).expect("parse")).expect("insert");
    }
    let batch: Vec<Metric> = (0..INSERT_BATCH)
        .map(|i| parse(&lines[i % lines.len()]).expect("parse"))
        .collect();
    group.throughput(Throughput::Elements(INSERT_BATCH as u64));
    group.bench_function("count_100_contexts", |b| {
        b.iter_batched(
            || (base.clone(), batch.clone()),
            |(mut aggregator, batch)| {
                for m in batch {
                    let _ = aggregator.insert(m);
                }
                aggregator
            },
            BatchSize::LargeInput,
        )
    });

    // Vacant path: 10k new contexts, including table growth and rehash.
    let unique: Vec<Metric> = (0..10_000)
        .map(|i| parse(&unique_line(i)).expect("parse"))
        .collect();
    group.throughput(Throughput::Elements(unique.len() as u64));
    group.bench_function("count_vacant_10000_contexts", |b| {
        b.iter_batched(
            || {
                (
                    Aggregator::new(enrich_tags(), MAX_CONTEXTS).expect("aggregator"),
                    unique.clone(),
                )
            },
            |(mut aggregator, batch)| {
                for m in batch {
                    let _ = aggregator.insert(m);
                }
                aggregator
            },
            BatchSize::LargeInput,
        )
    });

    // Merge path: each metric carries a single-point sketch merged into one of
    // 10 contexts. The base is warmed up so its sketches are at full size.
    let lines: Vec<String> = (0..10)
        .map(|i| format!("my.dist{i}:1|d|#tag1:a,tag2:b|T1700000000"))
        .collect();
    let dist_metrics = |values: &[f64]| -> Vec<Metric> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut m = parse(&lines[i % lines.len()]).expect("parse");
                m.value = MetricValue::distribution(*v);
                m
            })
            .collect()
    };
    let mut base = Aggregator::new(enrich_tags(), MAX_CONTEXTS).expect("aggregator");
    for m in dist_metrics(&sketch_values(20_000, 2_000)) {
        base.insert(m).expect("insert");
    }
    let batch = dist_metrics(&sketch_values(INSERT_BATCH, 2_000));
    group.throughput(Throughput::Elements(INSERT_BATCH as u64));
    group.bench_function("distribution_10_contexts", |b| {
        b.iter_batched(
            || (base.clone(), batch.clone()),
            |(mut aggregator, batch)| {
                for m in batch {
                    let _ = aggregator.insert(m);
                }
                aggregator
            },
            BatchSize::LargeInput,
        )
    });

    // What the merge path costs per value against a plain insert. Each batch
    // starts from a fresh copy of the warm sketch: a full bin spills into a new
    // bin, so a sketch fed for the whole run would keep growing.
    group.throughput(Throughput::Elements(INSERT_BATCH as u64));
    for distinct in [1, 64, 2_000] {
        let mut warm = DDSketch::default();
        for v in sketch_values(20_000, distinct) {
            warm.insert(v);
        }
        let values = sketch_values(INSERT_BATCH, distinct);
        let variant = format!("{distinct}_distinct");

        group.bench_function(
            BenchmarkId::new("sketch_merge_single_point", &variant),
            |b| {
                b.iter_batched_ref(
                    || warm.clone(),
                    |sketch| {
                        for v in &values {
                            let mut one = DDSketch::default();
                            one.insert(*v);
                            sketch.merge(black_box(&one));
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
        group.bench_function(BenchmarkId::new("sketch_insert", &variant), |b| {
            b.iter_batched_ref(
                || warm.clone(),
                |sketch| {
                    for v in &values {
                        sketch.insert(black_box(*v));
                    }
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn bench_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("dogstatsd/flush");
    // Iterations take up to tens of milliseconds; linear sampling would need
    // thousands of them.
    group.sampling_mode(SamplingMode::Flat);
    for contexts in CONTEXT_COUNTS {
        group.throughput(Throughput::Elements(contexts as u64));

        let counts = build_aggregator(contexts, false);
        group.bench_function(BenchmarkId::new("consume_metrics", contexts), |b| {
            b.iter_batched(
                || counts.clone(),
                |mut aggregator| aggregator.consume_metrics(),
                BatchSize::LargeInput,
            )
        });
        let series = counts.clone().consume_metrics();
        // The flusher deep-copies both batches to return them on error:
        // `Flusher::flush` in src/flusher.rs.
        // Clone and drop are both timed; production drops its copy too.
        group.bench_function(BenchmarkId::new("series_clone", contexts), |b| {
            b.iter(|| series.clone())
        });
        // Series serialization in src/datadog/shipping.rs.
        group.bench_function(BenchmarkId::new("series_to_json", contexts), |b| {
            b.iter(|| series_bodies(black_box(&series)))
        });

        let distributions = build_aggregator(contexts, true);
        group.bench_function(BenchmarkId::new("consume_distributions", contexts), |b| {
            b.iter_batched(
                || distributions.clone(),
                |mut aggregator| aggregator.consume_distributions(),
                BatchSize::LargeInput,
            )
        });
        let sketches = distributions.clone().consume_distributions();
        // Clone and drop are both timed; production drops its copy too.
        group.bench_function(BenchmarkId::new("sketch_payload_clone", contexts), |b| {
            b.iter(|| sketches.clone())
        });
        // Sketch serialization in src/datadog/shipping.rs.
        group.bench_function(BenchmarkId::new("sketch_to_protobuf", contexts), |b| {
            b.iter(|| {
                sketches
                    .iter()
                    .map(|p| p.write_to_bytes().expect("serialize sketches"))
                    .collect::<Vec<_>>()
            })
        });
    }
    group.finish();
}

/// Size of a level-3 `CCtx` after compressing `src` in one of the ways
/// `zstd` offers: streaming with or without a pledged size, or one shot.
fn cctx_size(src: &[u8], mode: &str) -> usize {
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
        .expect("level");
    let mut dst = vec![0u8; zstd::zstd_safe::compress_bound(src.len())];
    if mode == "one_shot" {
        cctx.compress2(&mut dst[..], src).expect("compress2");
        return cctx.sizeof();
    }
    if mode == "stream_pledged" {
        cctx.set_pledged_src_size(Some(src.len() as u64))
            .expect("pledged size");
    }
    let mut input = InBuffer::around(src);
    let mut output = OutBuffer::around(&mut dst[..]);
    while input.pos() < src.len() {
        cctx.compress_stream2(&mut output, &mut input, ZSTD_EndDirective::ZSTD_e_continue)
            .expect("compress_stream2");
    }
    let mut empty = InBuffer::around(&[]);
    while cctx
        .compress_stream2(&mut output, &mut empty, ZSTD_EndDirective::ZSTD_e_end)
        .expect("compress_stream2 end")
        != 0
    {}
    cctx.sizeof()
}

fn print_zstd_memory(bodies_by_contexts: &[(usize, Vec<Vec<u8>>)]) {
    for (contexts, bodies) in bodies_by_contexts {
        let largest = bodies.iter().max_by_key(|b| b.len()).expect("bodies");
        let compressed: usize = bodies
            .iter()
            .map(|b| zstd::bulk::compress(b, ZSTD_LEVEL).expect("compress").len())
            .sum();
        let raw: usize = bodies.iter().map(Vec::len).sum();
        println!(
            "zstd memory ({contexts} contexts, {} payloads, {raw} B raw, {compressed} B compressed, largest payload {} B): \
             CCtx stream={} B, stream_pledged={} B, one_shot={} B",
            bodies.len(),
            largest.len(),
            cctx_size(largest, "stream"),
            cctx_size(largest, "stream_pledged"),
            cctx_size(largest, "one_shot"),
        );
    }
}

/// The three paths must give the same bytes back and sizes within 1 % of each other.
fn check_compress_outputs(bodies_by_contexts: &[(usize, Vec<Vec<u8>>)]) {
    let mut reused = zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("compressor");
    for (_, bodies) in bodies_by_contexts {
        for body in bodies {
            let mut encoder =
                zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL).expect("encoder");
            encoder.write_all(body).expect("write");
            let stream = encoder.finish().expect("finish");
            assert_eq!(
                zstd::decode_all(stream.as_slice()).expect("decode"),
                *body,
                "stream output must decompress to the input"
            );
            let bulk = zstd::bulk::compress(body, ZSTD_LEVEL).expect("compress");
            let reused_out = reused.compress(body).expect("compress");
            assert_eq!(bulk, reused_out, "bulk and reused compressor must match");
            let diff = stream.len().abs_diff(bulk.len());
            assert!(
                diff * 100 <= stream.len(),
                "stream {} B and bulk {} B differ by more than 1 %",
                stream.len(),
                bulk.len()
            );
        }
    }
}

fn bench_compress(c: &mut Criterion) {
    let bodies_by_contexts: Vec<(usize, Vec<Vec<u8>>)> = [1_000, 10_000]
        .into_iter()
        .map(|contexts| {
            let series = build_aggregator(contexts, false).consume_metrics();
            (contexts, series_bodies(&series))
        })
        .collect();
    let print_memory = Once::new();

    let mut group = c.benchmark_group("dogstatsd/compress");
    group.sampling_mode(SamplingMode::Flat);
    for (contexts, bodies) in &bodies_by_contexts {
        let raw: usize = bodies.iter().map(Vec::len).sum();
        group.throughput(Throughput::Bytes(raw as u64));

        // What the shipper does today: the stream `Encoder` in src/datadog/shipping.rs.
        group.bench_function(BenchmarkId::new("stream_encoder_level3", contexts), |b| {
            print_memory.call_once(|| {
                check_compress_outputs(&bodies_by_contexts);
                print_zstd_memory(&bodies_by_contexts);
            });
            b.iter(|| {
                for body in bodies {
                    let mut encoder =
                        zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL).expect("encoder");
                    encoder.write_all(body).expect("write");
                    black_box(encoder.finish().expect("finish"));
                }
            })
        });
        group.bench_function(BenchmarkId::new("bulk_compress_level3", contexts), |b| {
            b.iter(|| {
                for body in bodies {
                    black_box(zstd::bulk::compress(body, ZSTD_LEVEL).expect("compress"));
                }
            })
        });
        let mut compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("compressor");
        group.bench_function(
            BenchmarkId::new("bulk_compressor_reused_level3", contexts),
            |b| {
                b.iter(|| {
                    for body in bodies {
                        black_box(compressor.compress(body).expect("compress"));
                    }
                })
            },
        );
    }
    group.finish();
}

fn rss_kib() -> i64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(-1)
}

/// Entry point of the child process. Nothing touches `ustr` before this.
fn child(mode: &str) {
    match mode {
        "first_parse" => {
            let start = Instant::now();
            black_box(parse(black_box(FIRST_PARSE_LINE)).expect("parse"));
            println!("{}", start.elapsed().as_nanos());
        }
        "report" => {
            let rss_before = rss_kib();
            let start = Instant::now();
            black_box(parse(black_box(FIRST_PARSE_LINE)).expect("parse"));
            let first_ns = start.elapsed().as_nanos();
            let rss_first = rss_kib();
            println!(
                "ustr first parse: {first_ns} ns, RSS +{} KiB, arena allocated {} B, capacity {} B",
                rss_first - rss_before,
                ustr::total_allocated(),
                ustr::total_capacity(),
            );
            let mut done = 0;
            for target in [1_000, 10_000, 100_000] {
                let lines: Vec<String> = (done..target).map(unique_line).collect();
                let start = Instant::now();
                for line in &lines {
                    black_box(parse(line).expect("parse"));
                }
                let ns_per_parse = start.elapsed().as_nanos() / lines.len() as u128;
                drop(lines);
                done = target;
                println!(
                    "ustr after {target} unique tag values: {ns_per_parse} ns/parse, RSS +{} KiB since first parse, \
                     arena allocated {} B, capacity {} B",
                    rss_kib() - rss_first,
                    ustr::total_allocated(),
                    ustr::total_capacity(),
                );
            }
        }
        other => panic!("unknown {CHILD_ENV} mode {other}"),
    }
}

fn run_child(mode: &str) -> String {
    let exe = std::env::current_exe().expect("current exe");
    let out = Command::new(exe)
        .env(CHILD_ENV, mode)
        .output()
        .expect("spawn child");
    assert!(out.status.success(), "child {mode} failed: {out:?}");
    String::from_utf8(out.stdout).expect("utf8 child output")
}

fn bench_ustr(c: &mut Criterion) {
    let mut group = c.benchmark_group("dogstatsd/ustr");

    let parts: Vec<&str> = TAGS_8.split([',', ':']).collect();
    for p in &parts {
        Ustr::from(p);
    }
    group.throughput(Throughput::Elements(parts.len() as u64));
    group.bench_function("from_existing_16", |b| {
        b.iter(|| {
            for p in &parts {
                black_box(Ustr::from(black_box(p)));
            }
        })
    });

    // Each iteration is one child process; only the child-reported parse time
    // counts. It includes every first-call cost of `parse`, the interner's being
    // the largest. Criterion sizes the run from the wall time, spawn included.
    let print_report = Once::new();
    group
        .throughput(Throughput::Elements(1))
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1));
    group.bench_function("first_parse", |b| {
        print_report.call_once(|| print!("{}", run_child("report")));
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    let ns = run_child("first_parse")
                        .trim()
                        .parse()
                        .expect("child nanoseconds");
                    Duration::from_nanos(ns)
                })
                .sum()
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_parse,
    bench_packet,
    bench_insert,
    bench_flush,
    bench_compress,
    bench_ustr
);

fn main() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        child(&mode);
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
