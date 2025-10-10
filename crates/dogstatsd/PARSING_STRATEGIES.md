# SortedTags Parsing Strategies - Complete Guide

## Available Methods

### 1. `parse()` - Standard Parsing
```rust
let tags = SortedTags::parse("env:prod,service:api,version:1.0.0")?;
```
- Parses, sorts, and deduplicates tags
- Returns immediately sorted tags
- **Use when**: You need sorted tags immediately for hashing/comparison

### 2. `parse_lazy()` - Deferred Sorting
```rust
let tags = SortedTags::parse_lazy("env:prod,service:api,version:1.0.0")?;
```
- Parses tags but defers sorting
- Sorting happens automatically when needed (via `ensure_sorted()`)
- **Use when**: Startup code where sorting may not be immediately needed
- **Performance**: 19-24% faster than `parse()`

### 3. `parse_presorted()` - Skip Sort/Dedup
```rust
// Tags MUST be alphabetically sorted
let tags = SortedTags::parse_presorted("env:prod,service:api,version:1.0.0")?;
```
- Assumes input is already sorted
- Skips sort and dedup operations
- **Use when**: You have compile-time sorted constants
- **Performance**: 5-9% faster than `parse()` but SLOWER than `parse_lazy()`

## Performance Comparison

### Typical Tags (5 tags: env, service, version, host, region)
| Method | Time | vs parse() | vs parse_lazy() |
|--------|------|-----------|-----------------|
| `parse()` | 282 ns | - | +22.6% |
| `parse_lazy()` | 230 ns | **-18.6%** | - |
| `parse_presorted()` | 258 ns | -8.6% | +12.2% |

### Lambda Tags (8 tags)
| Method | Time | vs parse() | vs parse_lazy() |
|--------|------|-----------|-----------------|
| `parse()` | 511 ns | - | +31.4% |
| `parse_lazy()` | 389 ns | **-23.8%** | - |
| `parse_presorted()` | 486 ns | -4.9% | +24.9% |

## Why parse_lazy() Wins

For small tag sets (5-15 tags):
- **Parsing + Ustr interning**: ~450ns (dominates cost)
- **Sorting**: ~50-60ns (minimal)
- **Dedup**: ~10-15ns (minimal)

By deferring sorting, `parse_lazy()` saves 60-75ns, which is significant relative to the total cost.

Pre-sorting doesn't help much because:
1. The sort operation is already very fast for small inputs
2. You still pay the ~450ns parsing/interning cost
3. Lazy evaluation is better than skipping a cheap operation

## Recommended Usage

### For Startup (Aggregator Creation)
```rust
let (aggregator_service, aggregator_handle) = MetricsAggregatorService::new(
    SortedTags::parse_lazy(&tags_provider.get_tags_string()).unwrap_or(EMPTY_TAGS),
    CONTEXTS,
)
```

### For Runtime Metric Parsing
```rust
// In the metric parser, tags are needed immediately
let tags = Some(SortedTags::parse(tags_section.as_str())?);
```

### For Compile-Time Constants (Optional)
```rust
// If you want to document that tags are pre-sorted
const SORTED_TAGS: &str = "env:prod,region:us-east-1,service:api";

lazy_static! {
    static ref BASE_TAGS: SortedTags = 
        SortedTags::parse_presorted(SORTED_TAGS).unwrap();
}
```

## Investigating the 10ms Startup Issue

If `SortedTags::parse` is truly taking 10ms during startup, the cause must be:

### Hypothesis 1: Many Parse Calls
- At 500ns/parse, you'd need ~20,000 calls to reach 10ms
- **Action**: Add logging to count parse invocations

### Hypothesis 2: Ustr Lock Contention
- Ustr uses a global HashMap for string interning
- Multiple threads calling `parse()` simultaneously could cause contention
- **Action**: Check if parsing happens across multiple threads

### Hypothesis 3: Cold Start Effects
- First Ustr interning initializes global state
- Regex compilation in metric parser
- **Action**: Profile with `cargo flamegraph` or `perf`

### Hypothesis 4: Not Actually SortedTags::parse
- The 10ms might be in a different part of the call chain
- **Action**: Add precise timing:

```rust
let start = std::time::Instant::now();
let tags = SortedTags::parse_lazy(&tags_provider.get_tags_string())
    .unwrap_or(EMPTY_TAGS);
let elapsed = start.elapsed();
if elapsed.as_millis() > 1 {
    eprintln!("WARNING: parse_lazy took {:?}", elapsed);
}
```

## Debugging Tips

### 1. Measure Actual Startup Time
```rust
use std::time::Instant;

let start = Instant::now();
let (service, handle) = MetricsAggregatorService::new(
    SortedTags::parse_lazy(&tags).unwrap_or(EMPTY_TAGS),
    CONTEXTS,
)?;
println!("MetricsAggregatorService::new took: {:?}", start.elapsed());
```

### 2. Check Tag String Size
```rust
let tags_string = tags_provider.get_tags_string();
println!("Tag string length: {} bytes", tags_string.len());
println!("Tag string: {}", tags_string);
```

### 3. Profile with Criterion
```bash
cargo bench --bench sorted_tags -- --verbose
```

## Summary

**Answer to "How much faster with pre-sorted tags?"**
- Pre-sorting gives **5-9% improvement** over regular parsing
- But `parse_lazy()` gives **19-24% improvement** without pre-sorting
- **Conclusion**: Use `parse_lazy()` regardless of whether tags are pre-sorted

**For your startup code:**
```rust
// Fastest option - use this
let tags = SortedTags::parse_lazy(&tags_string).unwrap_or(EMPTY_TAGS);
```

**If still seeing 10ms:**
- It's likely not from a single `parse()` call
- Profile to find where the time is actually spent
- Check for repeated parsing in loops or thread contention
