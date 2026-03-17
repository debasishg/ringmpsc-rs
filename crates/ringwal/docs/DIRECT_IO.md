# Direct I/O Support (F_NOCACHE / O_DIRECT)

## Motivation

In fsync-heavy WAL workloads the operating system page cache can become a
bottleneck: every `write()` + `fsync()` cycle pollutes the cache with data the
application will never read again (append-only log). On macOS, setting
`F_NOCACHE` via `fcntl()` tells the kernel **not to cache** the file's pages
after write-through. This reduces cache pollution and often yields more
**predictable** fsync latency (lower variance), especially on M1/M2 Macs
running APFS.

On Linux the equivalent is `O_DIRECT`, which bypasses the page cache entirely.
However, `O_DIRECT` imposes a **block-alignment** constraint on all writes
(typically 4 KiB). Because ringwal's entries are variable-length, supporting
`O_DIRECT` correctly requires either aligned padding or a custom buffering
layer. The initial implementation therefore ships **macOS `F_NOCACHE` only**
and leaves Linux `O_DIRECT` for a follow-up.

## Design

### Config Flag

A new `direct_io: bool` field on `WalConfig` (default `false`) controls the
feature. The builder exposes `.with_direct_io(true)`.

```rust
let config = WalConfig::new("/tmp/wal")
    .with_sync_mode(SyncMode::DataOnly) // fdatasync
    .with_direct_io(true);              // + F_NOCACHE on macOS
```

### Platform Dispatch

A helper in `segment.rs` applies the flag after opening a segment file:

```text
#[cfg(target_os = "macos")]  → fcntl(fd, F_NOCACHE, 1)
#[cfg(target_os = "linux")]  → fcntl(fd, F_SETFL, flags | O_DIRECT)  [future]
#[cfg(other)]                → no-op
```

The flag is stored in `SegmentManager` and propagated through `rotate()` so
newly created segments inherit the setting.

### Write Path — No Alignment Changes Needed (macOS)

`F_NOCACHE` has **no alignment requirement** — the existing `BufWriter<File>`
pipeline works unchanged. The kernel simply marks pages as non-cacheable after
write-back.

### Recovery Path — No Changes

Recovery reads benefit from page cache (cold reads are rare and sequential).
Direct I/O is write-path only.

## Best Combinations

The user-facing recommendation:

| Goal | Config |
|------|--------|
| Lowest fsync variance (macOS) | `DataOnly` + `direct_io` |
| Highest throughput (pipelined) | `PipelinedDataOnly` + `direct_io` |
| Baseline comparison | `Full` vs `Full` + `direct_io` |

Combine with **4 KiB+ payloads** for best alignment with filesystem block
size (the hardware DMA path is happiest with block-aligned writes).

## Benchmarks

A new `wal_direct_io` benchmark group in `wal_throughput.rs` compares:

- `Full` vs `Full+DirectIO`
- `DataOnly` vs `DataOnly+DirectIO`
- `PipelinedDataOnly` vs `PipelinedDataOnly+DirectIO`

Sweep: 1 / 4 / 8 writers, 4 KiB payload.

Run:
```bash
cargo bench -p ringwal -- wal_direct_io
```

## Linux O_DIRECT Follow-up

When adding Linux support:

1. All writes must be **block-aligned** (typically 4 KiB). `BufWriter` does
   not guarantee this.
2. Options:
   - **Aligned BufWriter**: custom writer that pads each flush to a 4 KiB
     boundary. The padding is zeroes; the CRC32 header length field tells
     the reader the true entry size.
   - **Large batch writes**: accumulate a full batch in an aligned buffer,
     write the entire batch in one `write()` call, pad to 4 KiB.
3. The memory buffer itself must also be aligned. Use `std::alloc::Layout`
   with 4 KiB alignment, or a helper crate like `aligned-vec`.

## Dependencies

- `libc` (unix-only) — for `fcntl()` / `F_NOCACHE` / `O_DIRECT` constants.
