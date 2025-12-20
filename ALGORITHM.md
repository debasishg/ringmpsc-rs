# RingMPSC: A Ring-Decomposed Lock-Free MPSC Channel

## Abstract

We present RingMPSC, a high-performance lock-free Multi-Producer Single-Consumer (MPSC) channel implementation that achieves over 50 billion messages per second on commodity hardware. The key insight is **ring decomposition**: assigning each producer a dedicated Single-Producer Single-Consumer (SPSC) ring buffer, thereby eliminating all producer-producer contention. Combined with cache-aware memory layout, batch consumption APIs, and adaptive backoff, RingMPSC demonstrates near-linear scaling up to 8 producer-consumer pairs.

## 1. Introduction

Inter-thread communication is a fundamental primitive in concurrent systems. Traditional MPSC queues suffer from contention at the tail pointer, where multiple producers compete to enqueue items. This contention manifests as cache-line bouncing and failed Compare-And-Swap (CAS) operations, severely limiting throughput.

RingMPSC takes a different approach: rather than having producers contend for a shared queue, we give each producer its own SPSC ring buffer. The consumer polls all active rings in round-robin fashion. This design trades memory for throughput—an acceptable tradeoff given modern memory capacities.

### 1.1 Contributions

1. A ring-decomposed MPSC architecture eliminating producer contention
2. Cache-optimized memory layout with 128-byte alignment for prefetcher isolation
3. A batch consumption API amortizing atomic operation overhead
4. Empirical evaluation demonstrating 54 billion msg/s on AMD Ryzen 7 5700

## 2. Background

### 2.1 SPSC Ring Buffers

The SPSC ring buffer is the fastest known inter-thread communication primitive. With a single producer and single consumer, we need only two atomic variables:

- **tail**: Written by producer, read by consumer
- **head**: Written by consumer, read by producer

The producer writes to `buffer[tail % capacity]` and increments tail. The consumer reads from `buffer[head % capacity]` and increments head. No CAS operations are required—only atomic loads and stores with appropriate memory ordering.

### 2.2 The Contention Problem

In traditional MPSC queues (e.g., Michael-Scott queue, Vyukov's MPSC), producers must atomically update a shared tail pointer. Under high contention:

1. Multiple cores cache the tail pointer
2. Each CAS attempt invalidates other cores' caches
3. Failed CAS operations must retry, wasting cycles
4. Cache-line bouncing dominates execution time

## 3. Design

### 3.1 Ring Decomposition

RingMPSC assigns each producer a private SPSC ring:

```
Producer 0 ──► [Ring 0] ──┐
Producer 1 ──► [Ring 1] ──┼──► Consumer (polls all rings)
Producer 2 ──► [Ring 2] ──┤
    ...                   │
Producer N ──► [Ring N] ──┘
```

Each producer operates on its dedicated ring with zero contention. The consumer iterates over all active rings, draining each in turn. This transforms an MPSC problem into N independent SPSC problems.

**Trade-off**: Memory usage scales with `O(P × capacity)` where P is the producer count. For 8 producers with 64K-slot rings of 4-byte elements, this is 2MB—negligible on modern systems.

### 3.2 Memory Layout

False sharing occurs when independent variables share a cache line, causing spurious invalidations. Modern Intel and AMD processors use 64-byte cache lines but may prefetch adjacent lines, effectively creating 128-byte "prefetch groups."

RingMPSC separates hot variables into distinct 128-byte regions:

```
┌──────────────────────────────────────────────────────────────────┐
│ Offset 0: Producer Hot (128 bytes)                               │
│   tail: AtomicU64        ← Producer writes, Consumer reads       │
│   cached_head: u64       ← Producer-local (no sharing)           │
│   [padding to 128 bytes]                                         │
├──────────────────────────────────────────────────────────────────┤
│ Offset 128: Consumer Hot (128 bytes)                             │
│   head: AtomicU64        ← Consumer writes, Producer reads       │
│   cached_tail: u64       ← Consumer-local (no sharing)           │
│   [padding to 128 bytes]                                         │
├──────────────────────────────────────────────────────────────────┤
│ Offset 256: Cold State (128 bytes)                               │
│   active: AtomicBool                                             │
│   closed: AtomicBool                                             │
│   metrics: Metrics (optional)                                    │
├──────────────────────────────────────────────────────────────────┤
│ Offset 384+: Data Buffer (64-byte aligned)                       │
│   buffer: [CAPACITY]T                                            │
└──────────────────────────────────────────────────────────────────┘
```

The 128-byte separation ensures that producer and consumer never cause cache invalidations for each other's hot data, even under aggressive prefetching.

### 3.3 Cached Sequence Numbers

Cross-core atomic reads are expensive. RingMPSC minimizes them through cached sequence numbers:

**Producer side**:
```
fn reserve(n: usize) -> ?Reservation {
    tail = self.tail.load(.monotonic)  // Local read (fast)
    
    // Fast path: use cached head
    if CAPACITY - (tail - cached_head) >= n {
        return makeReservation(tail, n)
    }
    
    // Slow path: refresh cache (cross-core read)
    cached_head = self.head.load(.acquire)
    if CAPACITY - (tail - cached_head) >= n {
        return makeReservation(tail, n)
    }
    
    return null  // Ring is full
}
```

The producer only reads the consumer's head when the ring appears full. Under steady-state operation where the consumer keeps up, the producer rarely takes the slow path.

**Consumer side**: Symmetric logic caches the producer's tail.

### 3.4 Batch Consumption

The LMAX Disruptor demonstrated that batch processing dramatically improves throughput by amortizing synchronization costs. RingMPSC's `consumeBatch` API processes all available items with a single head update:

```
fn consumeBatch(handler: Handler) -> usize {
    head = self.head.load(.monotonic)
    tail = self.tail.load(.acquire)  // Single cross-core read
    
    if tail == head { return 0 }
    
    // Process all available items
    while pos != tail {
        handler.process(&buffer[pos & MASK])
        pos += 1
    }
    
    // Single atomic write for entire batch
    self.head.store(tail, .release)
    return count
}
```

For a batch of 32,768 items, this reduces atomic operations from 65,536 (one load + one store per item) to just 3 (one tail load, one head load, one head store).

### 3.5 Adaptive Backoff

When a ring is full (producer) or empty (consumer), busy-waiting wastes CPU cycles and generates memory traffic. RingMPSC implements Crossbeam-style adaptive backoff:

1. **Spin phase** (steps 0-6): Execute 2^step PAUSE instructions
2. **Yield phase** (steps 7-10): Call `sched_yield()` to relinquish CPU
3. **Completion**: Return failure, let caller decide (park, retry later, etc.)

This balances latency (spinning catches quick availability) against efficiency (yielding prevents CPU waste under sustained contention).

## 4. Implementation

RingMPSC is implemented in Zig (0.12+) for several reasons:

1. **Explicit memory layout**: `align(128)` annotations guarantee cache-line placement
2. **Comptime generics**: Zero-cost abstractions without runtime overhead
3. **Inline control**: `inline fn` ensures hot paths are inlined
4. **No hidden allocations**: All memory is stack-allocated or caller-provided

### 4.1 Memory Ordering

We use the weakest sufficient ordering for each operation:

| Operation | Ordering | Rationale |
|-----------|----------|-----------|
| tail load (producer) | monotonic | Reading own variable |
| head load (producer) | acquire | Synchronizes with consumer's release |
| tail store (producer) | release | Makes writes visible to consumer |
| head load (consumer) | monotonic | Reading own variable |
| tail load (consumer) | acquire | Synchronizes with producer's release |
| head store (consumer) | release | Makes read completion visible |

### 4.2 Zero-Copy API

The `reserve`/`commit` pattern enables zero-copy writes:

```zig
if (producer.reserve(1000)) |reservation| {
    // Write directly into ring buffer
    for (reservation.slice) |*slot| {
        slot.* = computeValue();
    }
    producer.commit(reservation.slice.len);
}
```

No intermediate buffers, no memcpy—data goes directly from computation to ring buffer.

## 5. Evaluation

### 5.1 Experimental Setup

- **CPU**: AMD Ryzen 7 5700 (8 cores, 16 threads, 3.7-4.6 GHz)
- **L3 Cache**: 32 MB shared
- **Memory**: DDR4-3200
- **OS**: Linux 6.x
- **Compiler**: Zig 0.13.0, ReleaseFast (-O3 equivalent)

### 5.2 Throughput Results

| Configuration | Throughput | Scaling |
|---------------|------------|---------|
| 1P1C | 7.7 B/s | 1.0x |
| 2P2C | 15.2 B/s | 2.0x |
| 4P4C | 30.0 B/s | 3.9x |
| 6P6C | 44.3 B/s | 5.8x |
| 8P8C | 57.1 B/s | 7.4x |

*B/s = billion messages per second. Each producer sends 500M u32 messages.*

RingMPSC achieves near-linear scaling up to 8P8C, demonstrating the effectiveness of ring decomposition in eliminating contention.

### 5.3 Analysis

The sub-linear scaling (7.4x instead of 8x) at 8P8C is attributed to:

1. **Memory bandwidth saturation**: 57B × 4 bytes = 228 GB/s approaches DDR4 limits
2. **L3 contention**: 8 rings × 256KB = 2MB competing for 32MB L3
3. **Consumer overhead**: Single thread polling 8 rings

### 5.4 Comparison

| Implementation | Language | Reported Throughput |
|----------------|----------|---------------------|
| RingMPSC | Zig | 57 B/s (8P8C) |
| rigtorp/SPSCQueue | C++ | ~1B/s (SPSC) |
| crossbeam-channel | Rust | ~100M/s (MPSC) |
| moodycamel::ConcurrentQueue | C++ | ~500M/s (MPMC) |

*Note: Direct comparison is difficult due to different hardware, message sizes, and measurement methodologies. RingMPSC's high numbers reflect the u32 message size and ring-decomposed architecture.*

## 6. Correctness

RingMPSC guarantees the following properties:

1. **Per-producer FIFO**: Messages from a single producer are received in send order
2. **No data loss**: Every sent message is eventually received (assuming consumer runs)
3. **No data duplication**: Each message is received exactly once
4. **Thread safety**: Concurrent access produces no data races

These properties are verified by the included test suite:

- `test_fifo.zig`: Validates per-producer ordering across 8M messages
- `test_chaos.zig`: Detects races under random scheduling perturbations
- `test_determinism.zig`: Confirms reproducible execution

## 7. Limitations

1. **Memory overhead**: O(P × capacity) memory usage
2. **Consumer complexity**: Must poll all rings; unfair under unbalanced loads
3. **No global ordering**: Only per-producer FIFO, not total order
4. **Fixed producer count**: Maximum producers set at compile time

## 8. Overcoming Limitations for Real-World Use

The benchmark's idealized no-op consumer and large batches (32K) achieve high throughput but may not suit real-world data processing where handlers perform work. To address this:

- Use `consumeAllUpTo(max_items, handler)` instead of `consumeAll(handler)` to limit batch sizes and prevent long processing pauses.
- For low-latency needs, implement custom polling with smaller limits per ring.
- Global ordering can be added by embedding timestamps in messages and sorting during consumption (at cost of throughput).

### Additional Recommendations

#### For Fairness Under Unbalanced Loads
The current round-robin polling can starve rings with fewer items if some producers are slower. Consider:

- **Priority Queue for Ring Selection**: Maintain a priority queue of rings ordered by available items. Consume from the ring with the most pending messages first.
- **Weighted Round-Robin**: Assign weights based on producer activity or use a fair queuing algorithm.

#### For Global Ordering
RingMPSC guarantees only per-producer FIFO. For total order across producers:

- **Add Sequence Numbers**: Embed a global atomic sequence number in each message. Increment atomically on send.
- **Timestamps**: Use high-resolution timestamps (e.g., TSC) for ordering, with sorting at consumption.

Trade-off: Both approaches reduce throughput due to atomic operations or sorting overhead.

#### For Dynamic Producers
The fixed compile-time producer limit simplifies the implementation but restricts flexibility:

- **Runtime Allocation**: Use a dynamic array of rings with atomic resize operations.
- **Linked List**: Maintain a linked list of active rings for unbounded producers.

Trade-off: Increases complexity, potential contention on the list, and memory management overhead.

## 8. Related Work

- **LMAX Disruptor** [Thompson et al., 2011]: Pioneered batch consumption and mechanical sympathy
- **Vyukov MPSC** [Vyukov, 2010]: Lock-free intrusive MPSC queue
- **Lamport's SPSC** [Lamport, 1977]: Original wait-free SPSC formulation
- **Crossbeam** [Tokio team]: Rust concurrent data structures with epoch-based reclamation

## 9. Conclusion

RingMPSC demonstrates that ring decomposition is a viable strategy for high-throughput MPSC channels. By trading memory for contention elimination, we achieve 57 billion messages per second on commodity hardware—approaching memory bandwidth limits rather than synchronization limits.

The key insights are:
1. Decompose MPSC into independent SPSC rings
2. Separate hot variables by 128 bytes (not 64) for prefetcher isolation
3. Cache remote sequence numbers to minimize cross-core traffic
4. Batch operations to amortize atomic overhead

## References

1. Thompson, M., Farley, D., Barker, M., Gee, P., & Stewart, A. (2011). Disruptor: High performance alternative to bounded queues. *LMAX Exchange*.

2. Vyukov, D. (2010). Intrusive MPSC node-based queue. *1024cores.net*.

3. Lamport, L. (1977). Proving the correctness of multiprocess programs. *IEEE TSE*.

4. Michael, M., & Scott, M. (1996). Simple, fast, and practical non-blocking and blocking concurrent queue algorithms. *PODC*.

5. Herlihy, M., & Shavit, N. (2012). *The Art of Multiprocessor Programming*. Morgan Kaufmann.

---

*RingMPSC is released under the MIT License.*
