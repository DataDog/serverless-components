use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkGroup};
use dogstatsd::metric::SortedTags;

mod optimized {
    include!("../src/metric_optimized.rs");
}

use optimized::SortedTagsOptimized;

fn compare_implementations(c: &mut Criterion) {
    let test_cases = vec![
        ("small", "env:prod,service:web,version:1.0.0"),
        ("medium", "env:prod,service:web,version:1.0.0,region:us-east-1,team:backend,tier:gold,customer:acme"),
        ("large", &(0..50).map(|i| format!("tag{}:value{}", i, i)).collect::<Vec<_>>().join(",")),
        ("no_values", "tag1,tag2,tag3,tag4,tag5,tag6,tag7,tag8,tag9,tag10"),
        ("mixed", "env:prod,debug,service:web,trace,version:1.0.0,monitoring,region:us-east-1"),
        ("duplicates", "env:prod,service:web,env:prod,version:1.0.0,service:web,region:us-east-1"),
    ];

    for (name, tags) in test_cases {
        let mut group = c.benchmark_group(format!("parse_{}", name));
        
        group.bench_function("original", |b| {
            b.iter(|| {
                let _ = SortedTags::parse(black_box(tags));
            });
        });
        
        group.bench_function("single_pass", |b| {
            b.iter(|| {
                let _ = SortedTagsOptimized::parse_single_pass(black_box(tags));
            });
        });
        
        group.bench_function("hash_dedup", |b| {
            b.iter(|| {
                let _ = SortedTagsOptimized::parse_with_hash_dedup(black_box(tags));
            });
        });
        
        #[cfg(target_arch = "x86_64")]
        group.bench_function("simd_x86", |b| {
            b.iter(|| {
                let _ = SortedTagsOptimized::parse_simd(black_box(tags));
            });
        });
        
        #[cfg(target_arch = "aarch64")]
        group.bench_function("neon_arm", |b| {
            b.iter(|| {
                let _ = SortedTagsOptimized::parse_neon(black_box(tags));
            });
        });
        
        group.finish();
    }
}

fn bench_pathological_cases(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathological");
    
    let many_colons = "a:b:c:d,e:f:g:h,i:j:k:l";
    group.bench_function("many_colons_original", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(many_colons));
        });
    });
    
    group.bench_function("many_colons_optimized", |b| {
        b.iter(|| {
            let _ = SortedTagsOptimized::parse_single_pass(black_box(many_colons));
        });
    });
    
    let all_duplicates = "tag:val,tag:val,tag:val,tag:val,tag:val";
    group.bench_function("all_dups_original", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(all_duplicates));
        });
    });
    
    group.bench_function("all_dups_hash", |b| {
        b.iter(|| {
            let _ = SortedTagsOptimized::parse_with_hash_dedup(black_box(all_duplicates));
        });
    });
    
    group.finish();
}

criterion_group!(benches, compare_implementations, bench_pathological_cases);
criterion_main!(benches);