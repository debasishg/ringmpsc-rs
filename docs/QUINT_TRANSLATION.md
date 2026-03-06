# TLA+ → Quint Translation Guide

> **Last updated**: 2026-03-01 | **Quint**: ≥ 0.31.0

This document explains how to manually translate TLA+ specifications to Quint. There's **no automated translator** - it's a manual process, but the mapping is straightforward since both languages express the same TLA+ logic.

## Quick Reference

| TLA+ | Quint | Example |
|------|-------|---------|
| `VARIABLE x` | `var x: int` | State variable (Quint requires types) |
| `CONSTANT C` | `val C = 4` | Module-level constants (Quint uses `val`, see `RingSPSC.qnt`) |
| `x' = expr` | `x' = expr` | Next-state value (same syntax) |
| `UNCHANGED x` | `x' = x` | Explicit assignment |
| `UNCHANGED <<x,y>>` | `x' = x, y' = y` | Multiple unchanged |
| `/\ A /\ B` | `all { A, B }` | Conjunction |
| `\/ A \/ B` | `any { A, B }` | Disjunction/choice |
| `~P` | `not(P)` | Negation |
| `P => Q` | `P implies Q` | Implication |
| `\in` | `S.contains(x)` (membership) or `\E x \in S` → `S.exists(x => ...)` (quantification) | Set membership and existential quantification map differently |
| `\E x \in S : P(x)` | `S.exists(x => P(x))` | Existential quantification |
| `\A x \in S : P(x)` | `S.forall(x => P(x))` | Universal quantification |
| `Nat` | `int` | Quint's `int` includes negatives; add an explicit `x >= 0` guard if the spec relies on non-negativity |
| `IF c THEN a ELSE b` | `if (c) a else b` | Conditional |

## Concrete Examples from RingSPSC

> **Reserved name caveat**: Quint (≥ 0.30.0) reserves `head` and `tail` as built-in list
> operation names. When translating `RingSPSC.tla`, these variables must be renamed. The
> actual spec uses `hd` (for `head`) and `tl` (for `tail`). The examples below reflect the
> real spec names.

### 1. Variables

```tla
\* TLA+
VARIABLES head, tail, cached_head, cached_tail, items_produced
```

```quint
// Quint — note: hd/tl instead of head/tail (reserved names in Quint ≥ 0.30.0)
var hd: int
var tl: int
var cached_head: int
var cached_tail: int
var items_produced: int
```

### 2. Constants

```tla
\* TLA+
CONSTANTS Capacity, MaxItems
```

```quint
// Quint — uses `val` for module-level constants (not `const`)
val CAPACITY = 4
val MAX_ITEMS = 8
```

### 3. Invariants

```tla
\* TLA+
BoundedCount == 
    /\ tail >= head
    /\ (tail - head) <= Capacity

HappensBefore == head <= tail
```

```quint
// Quint
val boundedCount: bool =
    tl >= hd and (tl - hd) <= CAPACITY

val happensBefore: bool = hd <= tl
```

### 4. Actions

```tla
\* TLA+
ProducerWrite ==
    /\ (tail - head) < Capacity
    /\ items_produced < MaxItems
    /\ tail' = tail + 1
    /\ items_produced' = items_produced + 1
    /\ UNCHANGED <<head, cached_head, cached_tail>>
```

```quint
// Quint
action producerWrite = all {
    (tl - hd) < CAPACITY,
    items_produced < MAX_ITEMS,
    tl' = tl + 1,
    items_produced' = items_produced + 1,
    // UNCHANGED becomes explicit assignments
    hd' = hd,
    cached_head' = cached_head,
    cached_tail' = cached_tail,
}
```

### 5. Initial State

```tla
\* TLA+
Init ==
    /\ head = 0
    /\ tail = 0
    /\ cached_head = 0
    /\ cached_tail = 0
    /\ items_produced = 0
```

```quint
// Quint
action init = all {
    hd' = 0,
    tl' = 0,
    cached_head' = 0,
    cached_tail' = 0,
    items_produced' = 0,
}
```

### 6. Next State (Nondeterministic Choice)

```tla
\* TLA+
Next ==
    \/ ProducerReserveFast
    \/ ProducerRefreshCache
    \/ ProducerWrite
    \/ ConsumerRefreshCache
    \/ ConsumerAdvance
```

```quint
// Quint
action step = any {
    producerReserveFast,
    producerRefreshCache,
    producerWrite,
    consumerRefreshCache,
    consumerAdvance,
}
```

### 7. Pure Functions (Helpers)

```tla
\* TLA+
ProducerHasSpace == (tail - cached_head) < Capacity
```

```quint
// Quint - can be pure function with parameters
pure def producerHasSpace(t: int, ch: int): bool = 
    (t - ch) < CAPACITY
```

## Key Differences

| Aspect | TLA+ | Quint |
|--------|------|-------|
| **Types** | Implicit | Explicit (`int`, `bool`, `Set[int]`) |
| **Naming** | `PascalCase` or any | `camelCase` convention |
| **UNCHANGED** | `UNCHANGED <<x,y>>` | Must write `x' = x, y' = y` |
| **Comments** | `\* comment` | `// comment` |
| **Operators** | `/\`, `\/`, `~` | `and`, `or`, `not()` |
| **Module** | `MODULE Name` | `module Name { }` |
| **Sequences** | `Seq(T)` | `List[T]` |
| **Records** | `[field |-> value]` | `{ field: value }` |
| **Sets** | `{1, 2, 3}` | `Set(1, 2, 3)` |

### Set Operations

Sets are common in TLA+ specs and translate straightforwardly to Quint. The `RingSPSC.qnt` spec
uses `Set[int]` to track initialized buffer slots (`initialized` state variable).

```tla
\* TLA+ — Set operations
{1, 2, 3}                          \* Set literal
x \in S                            \* Membership
S \union T                         \* Union
S \intersect T                     \* Intersection
S \ T                              \* Difference
{x \in S : P(x)}                   \* Filter (set comprehension)
{f(x) : x \in S}                   \* Map
SUBSET S                           \* Power set
Cardinality(S)                     \* Size (from FiniteSets)
```

```quint
// Quint — Set operations
Set(1, 2, 3)                              // Set literal
S.contains(x)                             // Membership
S.union(T)                                // Union
S.intersect(T)                            // Intersection
S.exclude(T)                              // Difference
S.filter(x => P(x))                       // Filter
S.map(x => f(x))                          // Map
S.powerset()                              // Power set
S.size()                                  // Size
```

**Example from `RingSPSC.qnt`** — the `initialized` set uses set comprehension to compute expected slot indices:

```quint
// State variable
var initialized: Set[int]

// Set comprehension — expected initialized range
val expected = 0.to(count - 1).map(k => (hd + k) % CAPACITY).toSet()

// Invariant comparing actual set to expected
val initializedRange: bool =
    initialized == expected
```

## Translation Workflow

```
1. Create module structure
   MODULE Foo → module Foo { }

2. Translate CONSTANTS → val declarations (Quint uses `val`, not `const`)

3. Translate VARIABLES → var declarations (add types!)

4. Translate helper definitions → pure def or val

5. Translate invariants → val definitions (type: bool)

6. Translate Init → action init

7. Translate each action → action name = all { ... }
   - Replace /\ with commas inside all { }
   - Replace UNCHANGED with explicit x' = x

8. Translate Next → action step = any { ... }

9. Add run blocks for tests (Quint-specific)
```

## Complete Module Structure

### TLA+ Structure

```tla
---------------------------- MODULE RingSPSC ----------------------------
EXTENDS Naturals, Sequences

CONSTANTS Capacity, MaxItems

VARIABLES head, tail, cached_head, cached_tail, items_produced

vars == <<head, tail, cached_head, cached_tail, items_produced>>

TypeOK == ...

BoundedCount == ...

Init == ...

ProducerWrite == ...

ConsumerAdvance == ...

Next == \/ ProducerWrite \/ ConsumerAdvance

Spec == Init /\ [][Next]_vars

=============================================================================
```

### Quint Structure

```quint
module RingSPSC {
    // Constants — Quint uses `val` for module-level constants
    val CAPACITY = 4
    val MAX_ITEMS = 8

    // State variables — hd/tl instead of head/tail (reserved names in Quint ≥ 0.30.0)
    var hd: int
    var tl: int
    var cached_head: int
    var cached_tail: int
    var items_produced: int

    // Invariants
    val boundedCount: bool = ...

    // Pure helpers
    pure def producerHasSpace(t: int, ch: int): bool = ...

    // Initial state
    action init = all { ... }

    // Actions
    action producerWrite = all { ... }
    action consumerAdvance = all { ... }

    // Next state
    action step = any {
        producerWrite,
        consumerAdvance,
    }

    // Tests (Quint-specific)
    run testProduceConsume = {
        init
        .then(producerWrite)
        .then(consumerAdvance)
        .then(all { assert(boundedCount) })
    }
}
```

## Quint-Specific Features

### Run Blocks (Tests)

Quint supports inline tests that TLA+ doesn't have:

```quint
/// Test: Fill ring to capacity
run fillToCapacity = {
    init
    .then(producerWrite)
    .then(producerWrite)
    .then(producerWrite)
    .then(producerWrite)
    .then(all {
        assert(boundedCount),
        assert(tail - head == CAPACITY),
    })
}
```

### Nondeterministic Values

```quint
// Pick any value from a set
action writeAny = all {
    val item = oneOf(Set(1, 2, 3, 4, 5))
    buffer' = buffer.append(item)
    ...
}
```

### Type Aliases

```quint
type State = {
    head: int,
    tail: int,
}
```

## Verifying the Translation

```bash
# Check syntax and types
quint typecheck RingSPSC.qnt

# Run embedded tests
quint test RingSPSC.qnt --main=RingSPSC

# Simulate random traces
quint run RingSPSC.qnt --main=RingSPSC --max-steps=50

# Exhaustive model checking via TLC backend (requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=boundedCount --backend=tlc

# Symbolic model checking via Apalache backend (default, requires JDK 21+)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=boundedCount --backend=apalache
```

## Common Pitfalls

### 1. Forgetting to Assign All Variables

TLA+ `UNCHANGED` is easy to miss in translation:

```tla
\* TLA+ - easy to write
/\ UNCHANGED <<head, cached_head, cached_tail>>
```

```quint
// Quint - must list each one (using hd/tl since head/tail are reserved)
hd' = hd,
cached_head' = cached_head,
cached_tail' = cached_tail,
```

### 2. Type Mismatches

Quint is typed, so you may need casts:

```quint
// If mixing int operations
val count: int = tail - head  // OK
val ratio: int = count / 2    // Integer division
```

### 3. Set vs List

```tla
\* TLA+ - Sequences
buffer \in Seq(Nat)
Append(buffer, x)
```

```quint
// Quint - Lists
var buffer: List[int]
buffer.append(x)
```

## Resources

- [Quint Language Reference](https://quint-lang.org/docs/lang)
- [Quint Cheatsheet](https://quint-lang.org/docs/quint-cheatsheet)
- [TLA+ to Quint Migration](https://quint-lang.org/docs/tla-to-quint)
- [Apalache Model Checker](https://apalache.informal.systems/)
