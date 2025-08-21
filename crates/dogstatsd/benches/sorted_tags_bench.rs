use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use dogstatsd::metric::SortedTags;

fn bench_parse_small_tags(c: &mut Criterion) {
    let tag_strings = vec![
        "env:prod",
        "env:prod,service:web",
        "env:prod,service:web,version:1.0.0",
    ];
    
    let mut group = c.benchmark_group("parse_small_tags");
    for tags in tag_strings.iter() {
        group.bench_with_input(BenchmarkId::from_parameter(tags.len()), tags, |b, tags| {
            b.iter(|| {
                let _ = SortedTags::parse(black_box(tags));
            });
        });
    }
    group.finish();
}

fn bench_parse_medium_tags(c: &mut Criterion) {
    let tag_strings = vec![
        "env:prod,service:web,version:1.0.0,region:us-east-1,team:backend",
        "env:prod,service:web,version:1.0.0,region:us-east-1,team:backend,tier:gold,customer:acme",
        "env:prod,service:web,version:1.0.0,region:us-east-1,team:backend,tier:gold,customer:acme,feature:search,deployment:blue",
    ];
    
    let mut group = c.benchmark_group("parse_medium_tags");
    for tags in tag_strings.iter() {
        group.bench_with_input(BenchmarkId::from_parameter(tags.len()), tags, |b, tags| {
            b.iter(|| {
                let _ = SortedTags::parse(black_box(tags));
            });
        });
    }
    group.finish();
}

fn bench_parse_large_tags(c: &mut Criterion) {
    let mut large_tags = String::new();
    for i in 0..50 {
        if i > 0 {
            large_tags.push(',');
        }
        large_tags.push_str(&format!("tag{}:value{}", i, i));
    }
    
    c.bench_function("parse_50_tags", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(&large_tags));
        });
    });
}

fn bench_parse_tags_no_values(c: &mut Criterion) {
    let tags_no_values = "tag1,tag2,tag3,tag4,tag5,tag6,tag7,tag8,tag9,tag10";
    
    c.bench_function("parse_tags_no_values", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(tags_no_values));
        });
    });
}

fn bench_parse_mixed_tags(c: &mut Criterion) {
    let mixed_tags = "env:prod,debug,service:web,trace,version:1.0.0,monitoring,region:us-east-1";
    
    c.bench_function("parse_mixed_tags", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(mixed_tags));
        });
    });
}

fn bench_parse_empty_and_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_edge_cases");
    
    group.bench_function("empty", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(""));
        });
    });
    
    group.bench_function("single_with_value", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box("env:prod"));
        });
    });
    
    group.bench_function("single_no_value", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box("debug"));
        });
    });
    
    group.finish();
}

fn bench_parse_duplicates(c: &mut Criterion) {
    let tags_with_dups = "env:prod,service:web,env:prod,version:1.0.0,service:web,region:us-east-1";
    
    c.bench_function("parse_with_duplicates", |b| {
        b.iter(|| {
            let _ = SortedTags::parse(black_box(tags_with_dups));
        });
    });
}

criterion_group!(
    benches,
    bench_parse_small_tags,
    bench_parse_medium_tags,
    bench_parse_large_tags,
    bench_parse_tags_no_values,
    bench_parse_mixed_tags,
    bench_parse_empty_and_single,
    bench_parse_duplicates
);
criterion_main!(benches);