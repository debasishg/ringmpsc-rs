Audit atomic memory ordering usage across all crate source files for compliance with the project's minimum-sufficient ordering rules (from CLAUDE.md):

| Pattern | Required ordering |
|---------|------------------|
| Metrics counters (`fetch_add`, etc.) | `Relaxed` |
| Publish data to consumer (`tail.store`) | `Release` |
| Read published data (`tail.load`) | `Acquire` |
| Any pattern | Never `SeqCst` |

Search all `.rs` files under `crates/*/src/` for atomic operations: `.load(`, `.store(`, `.fetch_add(`, `.fetch_sub(`, `.compare_exchange(`, `.compare_exchange_weak(`, `.swap(`.

Flag these violations:
1. **SeqCst anywhere** — always wrong in this codebase
2. **Metrics store/fetch with non-Relaxed** — look for `metrics`, `count`, `span` variable names paired with non-Relaxed ordering
3. **Consumer `load` with non-Acquire** — look for `tail.load` or `head.load` on the consumer side using Relaxed or Release
4. **Producer `store` with non-Release** — look for `tail.store` on the producer side using Relaxed or Acquire

Report each violation as `file:line — found <Ordering>, expected <Ordering> (<reason>)`.

Note: context matters — some loads on cached local copies correctly use Relaxed. Flag for human review rather than auto-correcting.
