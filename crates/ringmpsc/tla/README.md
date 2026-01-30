# TLA+ Formal Specification

This directory contains TLA+ specifications for formal verification of the ringmpsc lock-free protocols.

## Files

| File | Description |
|------|-------------|
| [RingSPSC.tla](RingSPSC.tla) | SPSC ring buffer specification |
| [RingSPSC.cfg](RingSPSC.cfg) | Model checker configuration |

## Design Decisions

### Unbounded Naturals (not u64 wrap-around)

The Rust implementation uses `u64` sequence numbers to prevent ABA problems. At 10 billion messages/second, wrap-around takes ~58 years.

For TLA+ model checking, we use unbounded `Nat` instead of modeling wrap-around arithmetic because:

1. **Invariants don't depend on overflow** - Bounded count and monotonicity hold regardless of number representation
2. **Finite state space** - TLC explores states bounded by `MaxItems` anyway
3. **Separation of concerns** - Wrap-around correctness is tested empirically via Loom

### Refinement Mapping

| TLA+ Action | Rust Function | Spec Invariant |
|-------------|---------------|----------------|
| `ProducerReserveFast` | `ring.rs: reserve()` fast path | INV-ORD-01 |
| `ProducerRefreshCache` | `ring.rs: reserve()` slow path | INV-SW-01 |
| `ProducerWrite` | `ring.rs: commit_internal()` | INV-SEQ-01, INV-ORD-01 |
| `ConsumerRefreshCache` | `ring.rs: consume_batch()` slow path | INV-SW-02 |
| `ConsumerAdvance` | `ring.rs: advance()`, `consume_batch()` | INV-SEQ-01, INV-ORD-02 |

## Running TLC Model Checker

### Prerequisites

```bash
# macOS (Homebrew) - installs TLA+ Toolbox GUI + command-line tools
brew install --cask tla+-toolbox

# After installation, TLC is at:
# /Applications/TLA+\ Toolbox.app/Contents/Eclipse/tla2tools.jar

# Or download JAR directly from https://github.com/tlaplus/tlaplus/releases
# Get tla2tools.jar and place it somewhere convenient
```

### Run Model Checker

```bash
cd crates/ringmpsc/tla

# Using JAR from TLA+ Toolbox installation (macOS)
java -jar "/Applications/TLA+ Toolbox.app/Contents/Eclipse/tla2tools.jar" \
    RingSPSC.tla -config RingSPSC.cfg -workers auto

# Or if you downloaded tla2tools.jar separately
java -jar /path/to/tla2tools.jar RingSPSC.tla -config RingSPSC.cfg -workers auto

# Verbose output (shows state count)
java -jar "/Applications/TLA+ Toolbox.app/Contents/Eclipse/tla2tools.jar" \
    RingSPSC.tla -config RingSPSC.cfg -workers auto -coverage 1
```

### Convenience Alias (optional)

Add to your `~/.zshrc`:
```bash
alias tlc='java -jar "/Applications/TLA+ Toolbox.app/Contents/Eclipse/tla2tools.jar"'
```

Then run simply:
```bash
tlc RingSPSC.tla -config RingSPSC.cfg -workers auto
```

### Expected Output

**Success:**
```
Model checking completed. No error has been found.
  Estimates of the probability that TLC did not check all reachable states...
```

**Invariant Violation:**
```
Error: Invariant BoundedCount is violated.
Error: The behavior up to this point is:
State 1: <Initial predicate>
  head = 0
  tail = 0
  ...
State 2: <ProducerWrite>
  ...
```

The counterexample trace shows the sequence of actions leading to the violation.

## Adjusting Model Parameters

Edit [RingSPSC.cfg](RingSPSC.cfg) to change:

| Parameter | Default | Effect |
|-----------|---------|--------|
| `Capacity` | 4 | Ring buffer size |
| `MaxItems` | 8 | Total items to produce (bounds state space) |

**Tradeoffs:**
- Larger values → more thorough checking, exponentially more states
- `Capacity=4, MaxItems=8` checks ~1000 states in seconds
- `Capacity=8, MaxItems=16` checks ~100K states in minutes

## Quint Integration

[Quint](https://quint-lang.org/) is a modern specification language with TypeScript-like syntax, built on the same TLA+ foundations.

### Files

| File | Description |
|------|-------------|
| [RingSPSC.qnt](RingSPSC.qnt) | Quint spec (translated from TLA+) |
| [../tests/quint_mbt.rs](../tests/quint_mbt.rs) | Model-based test driver |

### Installing Quint

```bash
# Via npm (recommended)
npm install -g @informalsystems/quint

# Verify installation
quint --version
```

### Running Quint

```bash
cd crates/ringmpsc/tla

# Typecheck the spec
quint typecheck RingSPSC.qnt

# Run simulation (random traces)
quint run RingSPSC.qnt --main=RingSPSC --max-steps=100

# Run the embedded tests
quint test RingSPSC.qnt --main=RingSPSC

# Verify invariants via Apalache backend (optional)
quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant
```

### Model-Based Testing

The [quint_mbt.rs](../tests/quint_mbt.rs) driver executes traces against the real `Ring<T>`:

```bash
# Run model-based tests
cargo test -p ringmpsc-rs --test quint_mbt --features quint-mbt --release
```

The driver:
1. Defines action sequences matching Quint transitions
2. Executes each action on the real `Ring<T>` implementation
3. Verifies invariants hold after each step

### Quint ↔ TLA+ Mapping

| TLA+ | Quint | Notes |
|------|-------|-------|
| `VARIABLE x` | `var x: int` | Quint requires types |
| `x' = expr` | `x' = expr` | Same syntax |
| `\/ A \/ B` | `any { A, B }` | Nondeterministic choice |
| `/\ A /\ B` | `all { A, B }` | Conjunction |
| `[Next]_vars` | `step` action | Stuttering in `run` |
| `~>` (leads-to) | Not yet supported | Use simulation |

## Future Work

- [ ] Add TLC to CI (`.github/workflows/tla.yml`) when CI/CD is set up
- [ ] Add MPSC channel specification (`RingMPSC.tla`) modeling multiple producers
- [ ] Add liveness checking with fairness constraints
- [x] ~~Translate `RingSPSC.tla` to `RingSPSC.qnt` for Quint tooling~~
- [x] ~~Implement `quint-connect` driver for model-based testing~~
- [ ] Loom trace export → Quint/TLA+ verification
- [ ] ITF trace parsing for automated test generation
