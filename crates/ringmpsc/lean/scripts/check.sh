#!/usr/bin/env bash
# Phase 0 of RINGLEAN_DESIGN.md: regenerate the Lean model of
# crates/ringmpsc/src/window.rs and check every theorem about it.
#
#   1. Charon extracts ringmpsc_rs::window from the crate to LLBC.
#   2. Aeneas translates the LLBC to Lean in RingmpscRs/ (never edit by hand).
#   3. `lake build` checks the proofs in RingLean/ against it.
#   4. Fails if any theorem depends on `sorryAx`.
#   5. Fails if a `[lean: thm]` citation in src/ names no theorem in RingLean/.
#
# Needs the Aeneas release pinned in lakefile.toml, unpacked at $AENEAS_DIR
# (default ~/tools/aeneas; it bundles `aeneas`, `charon` and `charon-driver`),
# and its Rust toolchain: rustup toolchain install nightly-2026-09-17 \
#   -c rustc-dev -c llvm-tools -c rust-src -c miri
#
# With --check-generated, also fails if regeneration changed RingmpscRs/,
# i.e. window.rs changed but the generated Lean wasn't committed (for CI).
set -euo pipefail

AENEAS_DIR="${AENEAS_DIR:-$HOME/tools/aeneas}"
lean_dir="$(cd "$(dirname "$0")/.." && pwd)"
crate_dir="$(cd "$lean_dir/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> charon: extracting ringmpsc_rs::window"
# Separate target dir: Charon's nightly + full-MIR sysroot artifacts must not
# mix with normal builds in target/debug.
(cd "$crate_dir" && PATH="$AENEAS_DIR:$PATH" CARGO_TARGET_DIR="$crate_dir/../../target/charon" \
    charon cargo --preset=aeneas \
    --start-from 'ringmpsc_rs::window' \
    --dest-file "$tmp/ringmpsc_rs.llbc" \
    -- -p ringmpsc-rs --lib --quiet)

echo "==> aeneas: generating RingmpscRs/"
"$AENEAS_DIR/aeneas" -backend lean -split-files -dest "$lean_dir/RingmpscRs" \
    "$tmp/ringmpsc_rs.llbc" > "$tmp/aeneas.log" 2>&1 || { cat "$tmp/aeneas.log"; exit 1; }

if [[ "${1:-}" == "--check-generated" ]]; then
    git -C "$lean_dir" diff --exit-code -- RingmpscRs \
        || { echo "error: RingmpscRs/ is stale; rerun scripts/check.sh and commit it"; exit 1; }
fi

echo "==> lake build"
cd "$lean_dir"
lake build 2>&1 | tee "$tmp/build.log"
if grep -q "declaration uses 'sorry'" "$tmp/build.log"; then
    echo "error: a theorem uses sorry"; exit 1
fi

echo "==> axioms"
# Audit every theorem in RingLean/, generated from the sources so none is missed.
{
    echo "import RingLean"
    for f in RingLean/*.lean; do
        mod="RingLean.$(basename "$f" .lean)"
        echo "import $mod" >> "$tmp/imports"
        ns="$(grep -m1 -E '^namespace ' "$f" | awk '{print $2}')"
        grep -oE '^theorem [A-Za-z_0-9]+' "$f" | awk -v ns="$ns" '{print "#print axioms " ns "." $2}'
    done
} > "$tmp/axioms_body"
{ cat "$tmp/imports"; grep -v '^import' "$tmp/axioms_body"; } > "$tmp/Axioms.lean"
lake env lean "$tmp/Axioms.lean" 2>&1 | tee "$tmp/axioms.log"
if grep -q sorryAx "$tmp/axioms.log"; then
    echo "error: a theorem depends on sorryAx"; exit 1
fi

echo "==> citations"
# Every `[lean: thm, ...]` cited in a // Safety: comment must name a proved theorem.
cited="$(grep -rhoE '\[lean: [^]]+\]' "$crate_dir/src" | sed -E 's/\[lean: (.*)\]/\1/' \
    | tr ',' '\n' | tr -d ' ' | sort -u)"
status=0
for thm in $cited; do
    if ! grep -qE "^theorem ${thm}( |$)" RingLean/*.lean; then
        echo "error: src/ cites [lean: $thm], but RingLean/ has no such theorem"; status=1
    fi
done
[[ $status -eq 0 ]] || exit 1
echo "    $(echo "$cited" | wc -w | tr -d ' ') cited theorems found"
echo "==> all window theorems proved"
