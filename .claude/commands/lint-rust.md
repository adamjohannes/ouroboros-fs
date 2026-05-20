---
description: Pre-commit lint sweep — fmt, clippy, rustdoc warnings.
---

# Lint-rust: pre-commit checks

Run the standard Rust lint checks in sequence. Each step prints its status; if any fail,
report the failures clearly and stop (don't run later steps that depend on a clean tree).

## Steps

1. **`cargo fmt --check`**
   Reports formatting drift without modifying files. If it fails, the user runs
   `cargo fmt` to fix.

2. **`cargo clippy --all-targets --all-features -- -D warnings`**
   Runs clippy across lib, bins, tests, and benches. `-D warnings` promotes warnings to
   errors so the run fails on lint regressions. **Note**: this repo doesn't currently
   enforce clippy in CI (NEXT_STEPS.md §5.3 tracks adding it). The local check still
   surfaces drift.

3. **`cargo doc --no-deps 2>&1 | rg -i "warning|error" || echo "rustdoc clean"`**
   Builds rustdoc and surfaces warnings. The repo has 31 pre-existing
   `unclosed HTML tag` warnings from the `<NOUN> <VERB>` notation in protocol.rs's `//!`
   doc comment — those are accepted noise. Anything **above 31** is a regression.

## Reporting

After all 3 steps:
- If all clean: `lint-rust: all checks passed (fmt, clippy, rustdoc).`
- If any fail: list each failure with the offending file:line and a one-line suggestion
  for the fix. **Don't run `cargo fmt`** to auto-fix; let the user decide.
- For rustdoc warnings: report the count delta vs. the 31-line baseline. New warnings
  point at recent edits.

## When to use

Before every commit that touches `src/` or `tests/`. Conventional flow:
1. Make the edit.
2. `/run-stress 5` to confirm tests still green under load.
3. `/lint-rust` to catch fmt/clippy drift.
4. Commit.

Slower but safer than relying on CI to catch issues post-push.
