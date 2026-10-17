#!/usr/bin/env bash
# The one check to run before every commit, and what CI runs: formatting, lints, every test (the
# rules and the golden screens included), the release build, and the footprint budgets. Each
# failure says how to fix it.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n▸ %s\n' "$1"; }
fail() { printf '\n✗ %s\n' "$1" >&2; exit 1; }

step "formatting"
cargo fmt --check || fail "Formatting differs. Fix: cargo fmt"

step "lints"
cargo clippy --all-targets --locked -- -D warnings || fail "Clippy found problems above. Fix them; allow one only with a comment saying why."

step "tests (a debug build, so an arithmetic overflow fails too)"
cargo test --locked || fail "A test failed: its message above says what and how to fix it."

step "release build"
cargo build --release --locked
bin=target/release/ccorbit
[ -f "$bin.exe" ] && bin="$bin.exe"

step "binary size"
size=$(wc -c < "$bin" | tr -d ' ')
budget=2000000
echo "$size bytes (budget $budget)"
[ "$size" -le "$budget" ] || fail "The binary grew past $budget bytes. Fix: drop what made it grow (a dependency, an embedded asset), or record why in docs/DECISIONS.md and raise the budget here."

step "frame time of the hologram"
report=$("$bin" --demo --bench-hud 250x70)
echo "$report"
avg=$(echo "$report" | sed -E 's/.*average ([0-9.]+) ms.*/\1/')
awk -v a="$avg" 'BEGIN { exit !(a < 8) }' || fail "A hologram frame takes $avg ms on average, over the 8 ms budget (30 frames a second need room for the terminal too). Fix: find the slow part with --bench-hud; do not lower the frame rate to hide it."

step "the demo runs"
"$bin" --demo --dump > /dev/null || fail "ccorbit --demo --dump failed."

printf '\n✓ all checks passed\n'
