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
- [ ] Add `debug_assert!` for runtime-checkable invariants
- [ ] Link specs to test files
- [ ] Include verification matrix
- [ ] Keep subproject specs co-located

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
