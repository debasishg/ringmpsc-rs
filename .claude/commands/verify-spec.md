Run formal specification verification for the `ringmpsc-rs` crate. Two complementary checks:

**Step 1 — TLA+ model checking**
```
cd crates/ringmpsc/tla && tlc RingSPSC.tla -config RingSPSC.cfg -workers auto
```
TLC checks all reachable states of the ring buffer model. Requires `tlc` on PATH (Java-based TLA+ Tools). Report any invariant violations found or confirm all states checked.

**Step 2 — Quint model-based tests (ITF trace replay)**
```
cargo test -p ringmpsc-rs --features quint-mbt --release
```
Replays ITF traces generated from the Quint spec (`crates/ringmpsc/tla/RingSPSC.qnt`) against the Rust implementation. Each trace is a sequence of state transitions that the Rust code must reproduce exactly.

Report each step's exit status. If TLC finds a violation, show the counterexample trace. If quint-mbt tests fail, show which trace assertion failed and the diff between expected and actual state.
