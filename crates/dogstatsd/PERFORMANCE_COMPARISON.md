# SortedTags Performance Comparison

## Benchmark Results

### Before Optimization
```
SortedTags::parse/empty              23.2 ns
SortedTags::parse/single             60.3 ns
SortedTags::parse/few_tags          170.6 ns
SortedTags::parse/typical           309.6 ns
SortedTags::parse/many_tags         686.0 ns
SortedTags::parse/realistic_lambda  584.6 ns
SortedTags::extend                   86.2 ns
```

### After Optimization (parse)
```
SortedTags::parse/empty               3.5 ns  (84.9% faster)
SortedTags::parse/single             57.7 ns  ( 4.3% faster)
SortedTags::parse/few_tags          148.1 ns  (13.2% faster)
SortedTags::parse/typical           291.2 ns  ( 5.9% faster)
SortedTags::parse/many_tags         627.1 ns  ( 8.6% faster)
SortedTags::parse/realistic_lambda  516.2 ns  (11.7% faster)
SortedTags::extend                   84.8 ns  ( 1.6% faster)
```

### After Optimization (parse_lazy)
```
SortedTags::parse_lazy/typical      230.5 ns  (25.5% faster than optimized parse)
SortedTags::parse_lazy/realistic    388.3 ns  (24.8% faster than optimized parse)
```

## Key Optimizations

1. **Manual byte-level parsing**: Replaced iterator-based `split()` with manual byte iteration
2. **Lazy sorting**: Added `parse_lazy()` that defers sorting until needed
3. **Fixed dedup order**: Moved `dedup()` after `sort_unstable()` for correctness

## Usage Recommendation

For startup performance (like creating MetricsAggregatorService):
```rust
let (aggregator_service, aggregator_handle) = MetricsAggregatorService::new(
    SortedTags::parse_lazy(&tags_provider.get_tags_string()).unwrap_or(EMPTY_TAGS),
    CONTEXTS,
)
```

For runtime parsing where sorted tags are immediately needed:
```rust
let tags = SortedTags::parse(tags_section)?;
```

## Impact on 10ms Startup Issue

The original 10ms issue was likely caused by:
1. Multiple calls to `parse()` during startup
2. Large tag sets being parsed
3. Ustr interning overhead on cold start

With these optimizations:
- Small tag sets (5 tags): ~291ns (34,000 ops/10ms possible)
- Large tag sets (8 tags): ~516ns (19,000 ops/10ms possible)
- Using lazy parsing: Additional 25% speedup

Even if parsing 100 tag sets during startup:
- Before: 100 × 500ns = 50µs (0.05ms)
- After:  100 × 400ns = 40µs (0.04ms)

The 10ms issue is likely from repeated parsing in a loop or from other sources.
Further profiling of the actual startup code is recommended.
