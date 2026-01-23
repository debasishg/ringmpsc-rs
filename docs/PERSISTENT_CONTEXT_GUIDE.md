# Persistent Context for Agentic Coding

A guide to structuring project knowledge so AI coding agents can be immediately productive.

## File Hierarchy

```
project-root/
├── .github/
│   └── copilot-instructions.md    # Entry point for agents (workflow-focused)
├── specs/
│   └── *.md                        # Formal invariants for core library
├── examples/
│   └── subproject/
│       └── specs/
│           └── invariants.md       # Domain-specific invariants (co-located)
└── docs/
    └── *.md                        # Design docs, patterns, guides
```

**Principle**: Specs live closest to the code they describe. Subprojects get their own `specs/` folder.

---

## 1. Copilot Instructions (`.github/copilot-instructions.md`)

**Purpose**: Quick-start guide for agents. Workflow-focused, not exhaustive.

**Target**: ~50-100 lines. Agent reads this first.

**Structure**:
```markdown
# Project Name

## Project Overview
One paragraph: what it does, key design decision, performance target.

## Architecture
Table of modules with one-line purposes. Link to source files.

## Development Workflows
```bash
cargo build --release    # Why --release matters
cargo test --features X  # Non-obvious test commands
```

## Coding Patterns
Critical patterns with code snippets. Focus on gotchas.

## Common Gotchas
Numbered list of mistakes agents (and humans) make.

## Specifications
Links to specs/ folder for formal invariants.
```

**Anti-patterns**:
- ❌ Generic advice ("write tests", "handle errors")
- ❌ Duplicating README content
- ❌ Aspirational practices not yet implemented

---

## 2. Invariant Specifications (`specs/*.md`)

**Purpose**: Formal correctness properties that implementations must satisfy.

**Naming Convention**: `INV-{CATEGORY}-{NUMBER}`

| Category | Examples |
|----------|----------|
| `MEM` | Memory layout, alignment |
| `SEQ` | Sequence numbers, ordering |
| `INIT` | Initialization state |
| `SW` | Single-writer guarantees |
| `ORD` | Memory ordering |
| `DOM` | Domain logic |
| `OWN` | Ownership transfer |

**Invariant Template**:
```markdown
### INV-SEQ-01: Bounded Count
```
0 ≤ (tail - head) ≤ capacity
```
The number of items in the ring never exceeds capacity.

**Enforced by**: `debug_assert!` in `commit_internal()`
**Test**: [tests/integration_tests.rs#L77](../tests/integration_tests.rs#L77)
```

**Include**:
- Formal notation where precise (`∀`, `⟺`, `→`)
- Code location links
- Verification method (compile-time, test, runtime assert)

---

## 3. Runtime Enforcement via `debug_assert!`

Add assertions for invariants checkable at runtime:

```rust
pub(crate) fn commit_internal(&self, n: usize) {
    let tail = self.tail.load(Ordering::Relaxed);
    let new_tail = tail.wrapping_add(n as u64);
    let head = self.head.load(Ordering::Relaxed);
    
    // INV-SEQ-01: Bounded Count
    debug_assert!(
        new_tail.wrapping_sub(head) as usize <= self.capacity(),
        "INV-SEQ-01 violated: count {} exceeds capacity {}",
        new_tail.wrapping_sub(head),
        self.capacity()
    );
    
    // INV-SEQ-02: Monotonic Progress
    debug_assert!(
        new_tail >= tail,
        "INV-SEQ-02 violated: tail decreased from {} to {}",
        tail, new_tail
    );
    
    self.tail.store(new_tail, Ordering::Release);
}
```

**Properties**:
- Zero cost in release builds (compiled out)
- Active in `cargo test` (debug profile)
- References invariant ID for traceability

---

## 4. Verification Matrix

End each spec with a verification table:

```markdown
| Invariant | Verification Method |
|-----------|-------------------|
| INV-MEM-01 | Manual inspection (128-byte alignment) |
| INV-SEQ-* | `debug_assert!` + integration tests |
| INV-ORD-* | loom exhaustive interleaving |
| INV-INIT-* | miri UB detection |
| INV-OWN-* | Compile-time (Rust ownership) |
```

---

## 5. Cross-Referencing

**Specs → Code**: Use relative paths with line anchors
```markdown
**Location**: [src/ring.rs#L270-L285](../src/ring.rs#L270-L285)
```

**Code → Specs**: Reference in comments
```rust
// See specs/ring-buffer-invariants.md#INV-SEQ-01
```

**Instructions → Specs**: Link in dedicated section
```markdown
## Specifications
- [specs/ring-buffer-invariants.md](../specs/ring-buffer-invariants.md)
```

---

## 6. Subproject Specs

For self-contained subprojects (examples, plugins):

```
examples/span_collector/
├── specs/
│   └── invariants.md      # Domain invariants (trace_id uniqueness, etc.)
├── docs/
│   └── design-patterns.md # Implementation patterns
└── src/
```

Link back to parent specs:
```markdown
## Related Specifications
- [Ring Buffer Invariants](../../../specs/ring-buffer-invariants.md)
```

---

## 7. Checklist for New Projects

- [ ] Create `.github/copilot-instructions.md` (~50-100 lines)
- [ ] Create `specs/` folder for core invariants
- [ ] Identify 5-10 critical invariants with formal notation
- [ ] **Document SAFETY-XX invariants for all `unsafe` blocks**
- [ ] Add `debug_assert!` for runtime-checkable invariants
- [ ] Link specs to test files
- [ ] Include verification matrix
- [ ] Keep subproject specs co-located
- [ ] **Establish context update triggers (see §10)**
- [ ] **Define pruning schedule for context maintenance**

---

## 8. Invariants under Unsafe

When a project uses `unsafe` for performance (as ringmpsc-rs does for lock-free ring buffers), the persistent context **must** explicitly list safety invariants that agents must never violate—even when "optimizing."

### Safety Invariants Template

Add a dedicated section to your specs:

```markdown
## Safety Invariants (UNSAFE CODE)

These invariants protect memory safety. Violating ANY of these causes undefined behavior.
**AGENTS: Do not modify unsafe blocks without verifying ALL safety invariants.**

### SAFETY-01: Buffer Bounds
```
∀ idx: idx = sequence & mask  →  idx < capacity
```
Index masking MUST use `& (capacity - 1)` with power-of-two capacity.
**Never**: Use modulo (`%`) or unchecked arithmetic.

### SAFETY-02: MaybeUninit Protocol
```
write: MaybeUninit::new(value)  OR  ptr::write()
read:  assume_init_read()       ONLY AFTER write confirmed
```
**Never**: Call `assume_init()` on unwritten slots.

### SAFETY-03: Single-Writer Guarantee
```
UnsafeCell fields (cached_head, cached_tail) accessed by ONE thread only
```
**Never**: Share cached fields across threads or add &mut aliases.

### SAFETY-04: Pointer Validity Lifetime
```
raw pointers derived from &self valid only while &self borrowed
```
**Never**: Store raw pointers beyond the borrow scope.
```

### Code Annotation Pattern

Every `unsafe` block must have a `// SAFETY:` comment referencing invariants:

```rust
unsafe {
    // SAFETY: SAFETY-01 (idx < capacity guaranteed by mask)
    //         SAFETY-02 (slot written by producer before tail advance)
    let value = (*self.buffer.get())[idx].assume_init_read();
}
```

### Agent Rules for Unsafe Code

1. **Read First**: Before modifying any `unsafe` block, read ALL safety invariants
2. **Justify Changes**: Any change to unsafe code requires updating the `// SAFETY:` comment
3. **No "Optimizations"**: Do not remove bounds checks, change orderings, or simplify unsafe code without explicit user approval
4. **Preserve Structure**: If uncertain, leave unsafe code unchanged and ask for clarification

---

## 9. Context Check-in Workflow

**Problem**: Agents may misinterpret context or work from stale understanding.

**Solution**: Before modifying files in `src/`, the agent must perform a "Context Check-in"—a summary of its understanding for user verification.

### Check-in Template

Before touching `src/`, the agent should state:

```markdown
## Context Check-in

**Task**: [One-line description of what I'm about to do]

**Relevant Invariants**:
- INV-SEQ-01: [My understanding of this invariant]
- SAFETY-02: [How my change preserves this]

**Files I'll Modify**:
- src/ring.rs: [What I'll change and why]

**Assumptions**:
- [Any assumptions I'm making about the codebase]

**Proceed?** [Wait for user confirmation on non-trivial changes]
```

### When to Require Check-in

| Change Type | Check-in Required? |
|-------------|-------------------|
| New feature in `src/` | ✅ Yes |
| Modifying `unsafe` code | ✅ Yes (mandatory) |
| Performance optimization | ✅ Yes |
| Bug fix with clear scope | ⚠️ Brief summary |
| Adding tests only | ❌ No |
| Documentation updates | ❌ No |
| Formatting/clippy fixes | ❌ No |

### Example Check-in

```markdown
## Context Check-in

**Task**: Add batch reservation API to Ring<T>

**Relevant Invariants**:
- INV-SEQ-01: Count stays ≤ capacity. My batch reserve will check available space.
- INV-RES-01: Partial reservation allowed. I'll return actual reserved count.
- SAFETY-02: MaybeUninit writes. Reservation returns &mut [MaybeUninit<T>].

**Files I'll Modify**:
- src/ring.rs: Add `reserve_batch(n)` method
- src/reservation.rs: Extend Reservation to handle batch commits

**Assumptions**:
- Contiguous slices only (no wrap-around in single reservation)
- Caller handles partial reservations via loop

**Proceed?**
```

---

## 10. When to Update Context

**The Stale Context Problem**: Code evolves but persistent context files (`copilot-instructions.md`, `specs/*.md`) remain static. The agent hallucinates based on outdated rules, breaking invariants that no longer exist or missing new ones.

### Mandatory Update Triggers

Update persistent context files when:

| Event | Action Required |
|-------|-----------------|
| New `unsafe` block added | Add SAFETY-XX invariant |
| Public API changed | Update copilot-instructions.md |
| New module created | Add to Architecture table |
| Invariant violation fixed | Review if invariant needs clarification |
| Feature stabilized | Prune detailed notes → single invariant line |
| Test strategy changed | Update verification matrix |
| Performance gotcha discovered | Add to Common Gotchas |

### Context Staleness Checks

Agent should watch for staleness signals:

```markdown
## Staleness Signals (Agent Self-Check)

- [ ] Does copilot-instructions.md reference files that don't exist?
- [ ] Are there modules in src/ not listed in Architecture?
- [ ] Do specs reference line numbers that are way off?
- [ ] Are there undocumented unsafe blocks in the codebase?
- [ ] Has Cargo.toml changed features not mentioned in workflows?
```

If any signal fires, **pause and update context before proceeding**.

### Update Protocol

When updating context files:

1. **Atomic Updates**: Update all affected files together (instructions + specs)
2. **Preserve History**: Use `[Updated YYYY-MM-DD]` annotations for significant changes
3. **Validate Links**: Check that file/line references still resolve
4. **Run Tests**: Ensure `cargo test` passes after context-driven changes

### Post-Task Context Review

After completing a significant task:

```markdown
## Post-Task Context Review

- [ ] Did I add new unsafe code? → Update SAFETY invariants
- [ ] Did I add new public API? → Update copilot-instructions.md
- [ ] Did I change module structure? → Update Architecture table
- [ ] Did I discover a gotcha? → Add to Common Gotchas
- [ ] Did I fix an invariant bug? → Clarify the invariant spec
```

---

## 11. Context Pruning

**Problem**: Over time, context files accumulate detailed implementation notes, TODOs, and design explorations that become noise.

**Solution**: When a feature is finished and stabilized, compress detailed "Intent" notes into a single "Invariant" line.

### Pruning Workflow

**Before (during development)**:
```markdown
## Reservation API Design Notes

We explored three approaches for the reservation API:
1. Return Option<&mut [T]> - rejected because MaybeUninit needed
2. Return Option<Reservation<T>> with raw pointer - current approach
3. Use Pin<> for stability - unnecessary complexity

Key insight: Reservation must store raw pointer because...
[50 more lines of design rationale]

TODO: Consider adding reserve_exact() that fails if full count unavailable
```

**After (feature stabilized)**:
```markdown
### INV-RES-03: Pointer Validity
The raw `ring_ptr` in `Reservation` is valid for lifetime `'a` because
the slice borrows from Ring's buffer with `'a`.

**Location**: [src/reservation.rs#L38-L62](../src/reservation.rs#L38-L62)
```

### What to Prune

| Content Type | Action |
|--------------|--------|
| Design alternatives explored | Delete (keep in git history) |
| TODO items completed | Delete |
| Detailed rationale | Compress to invariant + one-line "Rationale:" |
| Temporary workarounds | Delete when fixed, or promote to Gotcha |
| Performance experiments | Keep only final numbers in PERFORMANCE.md |

### What to Keep

| Content Type | Keep Because |
|--------------|--------------|
| Invariants | Core correctness guarantees |
| Gotchas | Prevent recurring mistakes |
| Workflow commands | Daily development needs |
| Architecture overview | Navigation aid |
| Verification matrix | Test strategy reference |

### Pruning Schedule

- **Weekly**: Remove completed TODOs
- **Per-feature**: Compress design notes when feature merges
- **Quarterly**: Full context review, validate all links

### Compression Examples

**Verbose → Invariant**:
```markdown
// Before: 20 lines explaining why u64 sequences prevent ABA
// After:
### INV-SEQ-03: ABA Prevention via Unbounded Sequences
Using u64 sequences instead of wrapped indices prevents ABA problem.
At 10 billion msg/sec, wrap-around takes ~58 years.
```

**Design Doc → Gotcha**:
```markdown
// Before: 2-page doc on why debug builds fail for lock-free code
// After (in copilot-instructions.md):
1. **Debug builds are broken**: Lock-free code requires `--release` optimizations
```

---

## Quick Reference: Invariant Categories

| Prefix | Domain | Example |
|--------|--------|---------|
| `INV-MEM-*` | Memory layout | Cache alignment, buffer sizing |
| `INV-SEQ-*` | Sequencing | Head/tail bounds, monotonicity |
| `INV-INIT-*` | Initialization | MaybeUninit validity ranges |
| `INV-SW-*` | Single-writer | Field ownership tables |
| `INV-ORD-*` | Memory ordering | Acquire/Release protocols |
| `INV-RES-*` | Resource management | Reservation lifecycle |
| `INV-DROP-*` | Cleanup | Drop guarantees, no double-free |
| `INV-DOM-*` | Domain logic | Business rules, data validity |
| `INV-OWN-*` | Ownership | Transfer semantics |
| `INV-BP-*` | Backpressure | Flow control guarantees |
| `INV-MET-*` | Metrics | Counter conservation |
| `INV-ASYNC-*` | Async lifecycle | Task spawning, shutdown |
