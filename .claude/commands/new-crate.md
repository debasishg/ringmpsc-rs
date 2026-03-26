Scaffold a new workspace crate. The crate name is provided as `$ARGUMENTS` (use snake_case, e.g. `my_crate`).

Create the following files:

**`crates/$ARGUMENTS/Cargo.toml`**
```toml
[package]
name = "$ARGUMENTS"
version.workspace = true
edition = "2021"
license.workspace = true
repository.workspace = true

[dependencies]
# Add workspace deps with: dep.workspace = true

[dev-dependencies]
```

**`crates/$ARGUMENTS/src/lib.rs`**
```rust
//! $ARGUMENTS — <one-line description>
```

**`crates/$ARGUMENTS/spec.md`**
```markdown
# $ARGUMENTS Specification

## Invariants

### INV-$UPPER-01: <name>
<description>

### INV-$UPPER-02: <name>
<description>
```
(Replace `$UPPER` with the crate name in SCREAMING_SNAKE_CASE.)

**`crates/$ARGUMENTS/src/invariants.rs`**
```rust
//! Debug assertions for spec.md invariants.
//! Each macro corresponds to one INV-* ID.

/// INV-$UPPER-01: <name>
macro_rules! debug_assert_inv_01 {
    ($expr:expr) => {
        debug_assert!($expr, "INV-$UPPER-01 violated")
    };
}
pub(crate) use debug_assert_inv_01;
```

**`crates/$ARGUMENTS/SKILLS.md`**
```markdown
# Skills — $ARGUMENTS

## Available Skills

| Skill | Command | What it does |
|-------|---------|-------------|
| Test | `/test-crate $ARGUMENTS` | Run all tests for this crate |
| Invariant sync | `/verify-invariants` | Cross-check spec.md INV-* IDs vs invariants.rs |
| Spec sync | `/spec-sync` | Diff INV-* IDs between spec.md and invariants.rs |
```

After creating the files, print the line to add to `[workspace.members]` in the root `Cargo.toml`:
```
"crates/$ARGUMENTS",
```

Do not modify `Cargo.toml` automatically — let the user add the member line themselves.
