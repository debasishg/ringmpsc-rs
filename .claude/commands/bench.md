Run criterion benchmarks. Optional target is provided as `$ARGUMENTS`. Valid values: `stack-ring`, `ringwal`, `allocator`, or empty (runs all).

**All benchmarks (default, no argument):**
```
cargo bench -p ringmpsc-rs
cargo bench -p ringmpsc-rs --features stack-ring --bench stack_vs_heap
cargo bench -p ringwal
```

**`stack-ring` argument:**
```
cargo bench -p ringmpsc-rs --features stack-ring --bench stack_vs_heap
```

**`ringwal` argument:**
```
cargo bench -p ringwal
```

**`allocator` argument:**
```
cargo bench -p ringmpsc-rs --bench allocator
```

After each run, extract and print the criterion summary table (ns/iter or Melem/s). Also print the path to the generated HTML report under `target/criterion/`.

Reference numbers from the project README for comparison:
| Benchmark | Expected (Apple M2) |
|-----------|-------------------|
| SPSC heap | ~2.97 B msg/s |
| SPSC stack | ~5.94 B msg/s |
| MPSC 4P heap | ~2.73 B msg/s |
| MPSC 4P stack | ~6.71 B msg/s |

Flag any result more than 20% below the reference as a potential regression.
