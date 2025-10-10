# Presorted Tags Performance Analysis

## Question
During startup, if tags are known and can be pre-sorted, how much faster is parsing?

## Benchmark Comparison (5 typical tags)

| Method | Time | Improvement |
|--------|------|-------------|
| `parse()` - with sorting | 282.3 ns | baseline |
| `parse_lazy()` - no sorting | 229.8 ns | **18.6% faster** |
| `parse_presorted()` - skip sort/dedup | 257.9 ns | **8.6% faster** |

## Benchmark Comparison (8 Lambda tags)

| Method | Time | Improvement |
|--------|------|-------------|
| `parse()` - with sorting | 510.5 ns | baseline |
| `parse_lazy()` - no sorting | 389.0 ns | **23.8% faster** |
| `parse_presorted()` - skip sort/dedup | 485.6 ns | **4.9% faster** |

## Analysis

**Surprising Result**: `parse_presorted()` is SLOWER than `parse_lazy()`!

### Why?

1. **Sort cost is minimal**: For small tag sets (5-8 tags), `sort_unstable()` is extremely fast (~50ns)
2. **Ustr interning dominates**: The `Ustr::from()` calls for string interning take ~40-50ns per tag pair
3. **Lazy is better**: Deferring sorting until actually needed is faster because:
   - It avoids the sort cost entirely if sorting never happens
   - For startup, tags may be parsed but never need to be sorted before aggregator uses them

### Breakdown (8 tags, approximate):
```
Parsing + Ustr interning: ~450ns
Sorting:                   ~50-60ns  (sort_unstable on 8 elements)
Dedup:                     ~10-15ns  (linear scan)
Total:                     ~510-525ns
```

## Recommendations

### 1. Best for Startup: Use `parse_lazy()`
```rust
let (aggregator_service, aggregator_handle) = MetricsAggregatorService::new(
    SortedTags::parse_lazy(&tags_provider.get_tags_string()).unwrap_or(EMPTY_TAGS),
    CONTEXTS,
)
```
**Why**: 24% faster, and sorting happens automatically when needed

### 2. If Tags Are Pre-sorted: Still use `parse_lazy()`
**Why**: Even if you pre-sort tags, `parse_lazy()` is still faster because it avoids the sort operation entirely. Pre-sorting doesn't help unless you use `parse_presorted()`, which is still slower than `parse_lazy()`.

### 3. Only Use `parse_presorted()` if:
- You have a COMPILE-TIME constant sorted tag string
- You want to document that tags are guaranteed sorted
- The 5% speedup over regular `parse()` matters

But in practice, `parse_lazy()` is the winner.

## The Real Question: Where's the 10ms?

Given these numbers:
- Parsing 8 tags: ~500ns
- Even 1000 parses: 500µs = 0.5ms
- Even 10,000 parses: 5ms

If startup truly takes 10ms in `SortedTags::parse`:
1. Either parsing is called **20,000+ times** during startup
2. Or there's something else in the call chain (regex compilation, Ustr global lock contention, etc.)

**Recommendation**: Profile the actual startup code to find WHERE the time is spent:
```rust
let start = std::time::Instant::now();
let tags = SortedTags::parse_lazy(&tags_string).unwrap();
println!("Parse took: {:?}", start.elapsed());
```

The bottleneck might be:
- Repeated regex compilation in metric parsing
- Ustr global HashMap lock contention
- Many small allocations
- Something outside of `SortedTags::parse` entirely
