# Tokio's Performance–Fairness Balance in Scheduling

How Tokio balances performance and fairness in scheduling — contextualized to the RingMPSC-RS workspace, where this tradeoff is directly relevant.

## Why This Matters for RingMPSC-RS

This workspace has **three distinct concurrency layers**, each affected differently by Tokio's scheduling:

| Layer | Pattern | Fairness Concern |
|-------|---------|-----------------|
| **ringmpsc** (core) | Spin-loop producers/consumers on OS threads | Not Tokio-managed |
| **ringmpsc-stream** | `Stream`/`Sink` adapters with async polling | Directly affected |
| **span_collector** | Multi-producer async submission + batch export | Critical path |

## Tokio's Key Scheduling Mechanisms

### 1. Cooperative Scheduling via Budget

Tokio uses a **per-task operation budget** (typically 128 ops) to prevent a single task from monopolizing the runtime. When budget is exhausted, `poll` returns `Pending` even if more work is available.

This directly impacts the `RingReceiver` stream implementation, which uses a **hybrid polling model** (timer + notify):

```rust
// Pattern in ringmpsc-stream: batch draining
// consume_all_up_to(batch_hint) naturally bounds work per poll
```

The `StreamConfig::batch_hint` field is effectively an **application-level budget** — `low_latency()` uses 16, `high_throughput()` uses 256. This mirrors Tokio's internal approach.

### 2. Work-Stealing Scheduler

Tokio's multi-threaded runtime uses work-stealing deques — each worker has a local queue, and idle workers steal from busy ones. The tradeoff:

- **Performance**: Local queue access is lock-free (like our SPSC rings!)
- **Fairness**: Stealing adds latency but prevents starvation

The `AsyncSpanCollector` benefits from this — the consumer task and concurrent export tasks (limited by semaphore) naturally spread across workers.

### 3. `yield_now()` as Explicit Cooperation

The codebase already uses this pattern correctly in `span_generator.rs`:

```rust
// Periodically yield to ensure fair scheduling (every 100 spans)
if sequence.is_multiple_of(100) {
    tokio::task::yield_now().await;
}
```

This is the **manual equivalent** of Tokio's budget exhaustion — surrendering the thread to let other tasks run. The `YieldingRateLimiter` formalizes this:

```rust
impl RateLimiter for YieldingRateLimiter {
    async fn wait(&mut self) {
        tokio::task::yield_now().await;
    }
}
```

### 4. `Notify` Semantics and Fairness

The backpressure-notify-pattern document in the workspace covers this well. Key scheduling implication: `notify_waiters()` wakes **all** waiting producers simultaneously, letting them compete fairly. This is a deliberate fairness choice — vs. `notify_one()` which could starve producers.

## Tension Points in the Architecture

### Hot Spin Loops vs. Async Cooperation

The core ring buffer uses spin loops with `Backoff` (spin → yield → thread::yield_now), which is **outside Tokio's scheduler**. But the async layer wraps this:

```
Producer spin-loop (OS thread)  →  Notify.notified().await (Tokio-managed)
```

The `IntervalRateLimiter` with `MissedTickBehavior::Skip` is a performance-over-fairness choice — if the producer falls behind, it **skips** missed ticks rather than queuing them, avoiding thundering-herd catch-up.

### Batch Size as the Fairness Knob

The `max_consume_per_poll` config (set to 1000 in the demo) bounds how long the consumer task holds the CPU before yielding back to Tokio. Too high = starves export tasks. Too low = underutilizes throughput. This is exactly the budget tradeoff Tokio makes internally.

## Recommendations

1. **`batch_hint` should account for Tokio's budget**: If `batch_hint` > 128, the stream's `poll_next` may hit Tokio's cooperative budget before draining the batch, causing an extra poll cycle. Consider aligning `batch_hint` with Tokio's budget (or documenting the interaction).

2. **Export semaphore fairness**: The `export_semaphore` in `AsyncSpanCollector` uses `Semaphore::new(max_concurrent)` — Tokio's semaphore is FIFO-fair, which is correct for the concurrent export pattern.

3. **The `Interval` tick behavior choice is performance-correct**: `MissedTickBehavior::Skip` in `IntervalRateLimiter` avoids the burst problem. The alternative (`Burst`) would violate the rate-limiting invariant INV-RES-05.

4. **Spin-loop backoff in async context**: When the ring is full and the producer awaits `Notify`, it cooperates perfectly with Tokio. But if the raw `reserve()` retry loop is ever exposed in an async context without yielding, it would **starve the Tokio runtime**. The current architecture correctly separates these concerns.

## Summary

The workspace already makes good scheduling tradeoffs — the ring buffer's spin-loops stay on dedicated OS threads, the async layer uses `Notify`/`yield_now` for cooperation, and batch sizes serve as application-level budgets that mirror Tokio's internal fairness mechanisms.
