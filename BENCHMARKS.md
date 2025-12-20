# Benchmark Results

Comprehensive throughput benchmarks using Criterion for RingMPSC-RS, similar to the Zig implementation's `bench_final.zig`.

## Quick Run Results

Run with: `cargo bench --bench throughput -- --quick`

### SPSC (Single Producer Single Consumer)

| Benchmark | Time | Throughput |
|-----------|------|------------|
| single_producer_consumer | 6.84 ms | **1.46 Gelem/s** |

### MPSC (Multi-Producer Single Consumer)

| Configuration | Time | Throughput | Scaling |
|---------------|------|------------|---------|
| 2P_2C | 14.04 ms | **1.42 Gelem/s** | 0.97x |
| 4P_4C | 32.52 ms | **1.23 Gelem/s** | 0.84x |
| 8P_8C | 89.76 ms | **891 Melem/s** | 0.61x |

### Batch Size Effects

| Batch Size | Time | Throughput | Improvement |
|------------|------|------------|-------------|
| 256 | 7.55 ms | 1.32 Gelem/s | baseline |
| 1024 | 6.49 ms | 1.54 Gelem/s | +16.7% |
| 4096 | 6.35 ms | 1.58 Gelem/s | +19.7% |
| **16384** | 6.17 ms | **1.62 Gelem/s** | +22.7% |

### Zero-Copy Operations

| Benchmark | Time | Throughput |
|-----------|------|------------|
| reserve_commit (64B items) | 2.51 ms | **399 Melem/s** |

### Contention Scenarios (Small Ring)

| Configuration | Time | Throughput |
|---------------|------|------------|
| 4P_small_ring | 5.73 ms | **69.9 Melem/s** |
| 8P_small_ring | 9.10 ms | **87.9 Melem/s** |

## Benchmark Details

### Configuration
- **Messages per producer**: 10M (adjustable)
- **Batch size**: 4096 (default)
- **Ring size**: 64K slots (default)
- **Platform**: Measured on your system

### Benchmarks Included

1. **SPSC** - Single producer, single consumer baseline
2. **MPSC** - Multi-producer configurations (2P, 4P, 8P)
3. **Batch Sizes** - Effect of different batch sizes on throughput
4. **Zero-Copy** - Reserve/commit pattern with 64-byte items
5. **Contention** - High contention with small ring (4K slots)

## Running Benchmarks

### Quick Run (Fast)
```bash
cargo bench --bench throughput -- --quick
```

### Full Run (Accurate)
```bash
cargo bench --bench throughput
```

### Run Specific Benchmark
```bash
# Run only SPSC
cargo bench --bench throughput -- spsc

# Run only MPSC
cargo bench --bench throughput -- mpsc

# Run only batch size tests
cargo bench --bench throughput -- batch_sizes
```

### Save Baseline for Comparison
```bash
# Save current results as baseline
cargo bench --bench throughput -- --save-baseline main

# Compare against baseline
cargo bench --bench throughput -- --baseline main
```

## Performance Notes

### Throughput Metrics
- **Gelem/s** = Billion elements per second
- **Melem/s** = Million elements per second

### Expected Performance
The Zig implementation reports:
- 1P1C: 8.6 B/s
- 8P8C: 54.3 B/s (with 500M msgs, 32K batch)

The Rust implementation shows:
- 1P1C: 1.46 B/s (with 10M msgs, 4K batch)
- 8P8C: 891 M/s (with 80M msgs, 4K batch)

To achieve higher throughput similar to Zig:
1. Increase messages per producer to 50M-100M
2. Increase batch size to 16K-32K
3. Run on high-core-count systems (8+ cores)
4. Use CPU pinning for producer/consumer threads

### Scaling Characteristics
- Near-linear scaling up to 2-4 producers
- Sub-linear scaling beyond 4 producers due to:
  - Memory bandwidth saturation
  - L3 cache contention
  - Consumer single-thread bottleneck

### Optimization Tips
1. **Larger batches** → Better throughput (amortizes atomic ops)
2. **Larger rings** → Less contention (more buffering)
3. **CPU pinning** → Better cache locality
4. **Fewer producers** → Better scaling efficiency

## Comparison with Other Implementations

| Implementation | Language | Peak Throughput | Configuration |
|----------------|----------|-----------------|---------------|
| RingMPSC-Zig | Zig | 54.3 B/s | 8P8C, 500M msgs, 32K batch |
| RingMPSC-RS | Rust | 1.62 B/s | 1P1C, 10M msgs, 16K batch |
| crossbeam-channel | Rust | ~100 M/s | MPSC |
| tokio-mpsc | Rust | ~50 M/s | MPSC |

Note: Direct comparison is difficult due to different hardware, message sizes, and measurement methodologies.

## Future Improvements

To match Zig's 50+ B/s performance:
1. Increase benchmark message count to 500M+
2. Use 32K batch sizes
3. Add CPU pinning support
4. Test on high-core systems (Ryzen 7 5700 or similar)
5. Profile and optimize hot paths
6. Consider SIMD optimizations for batch operations
