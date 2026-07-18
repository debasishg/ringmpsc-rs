Run formal specification verification for the `ringmpsc-rs` crate. Two complementary checks:

**Step 1 — Exhaustive model checking (safety + liveness)**
```
cd crates/ringmpsc/tla && quint verify RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --backend=tlc
cd crates/ringmpsc/tla && quint verify RingSPSC.qnt --main=RingSPSC --invariant=noDeadlock --backend=tlc
cd crates/ringmpsc/tla && quint verify RingSPSCLiveness.qnt --main=RingSPSCLiveness --temporal=eventuallyConsumed --backend=tlc
```
TLC checks all reachable states of the ring buffer model (955 states at default parameters). Requires Quint >= 0.32.0 and JDK 21+ (Quint manages the Apalache/TLC distribution itself). Report any invariant violations found or confirm all states checked.

**Step 2 — Quint model-based tests (ITF trace replay)**
```
cargo test -p ringmpsc-rs --features quint-mbt --release
```
Replays ITF traces generated from the Quint spec (`crates/ringmpsc/tla/RingSPSC.qnt`) against the Rust implementation. Each trace is a sequence of state transitions that the Rust code must reproduce exactly.

Report each step's exit status. If TLC finds a violation, show the counterexample trace. If quint-mbt tests fail, show which trace assertion failed and the diff between expected and actual state.
