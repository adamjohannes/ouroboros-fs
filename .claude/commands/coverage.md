---
description: Generate cargo-llvm-cov report and print per-file summary.
argument-hint: "[--release | --html | --lcov]"
---

# Coverage: local llvm-cov report

Run `cargo llvm-cov` and report results. Default mode runs the **default test suite**
(matching CI's `coverage` job).

## Usage

- `/coverage` — runs `cargo llvm-cov` and prints the per-file table.
- `/coverage --html` — also writes the HTML report to `target/llvm-cov/html/index.html`
  and prints the URL.
- `/coverage --release` — runs in release mode (slower build, more accurate inlining
  picture; almost never needed).
- `/coverage --lcov` — writes `lcov.info` for IDE integration.

Pass-through: any other `$ARGUMENTS` go to `cargo llvm-cov` after a `--`.

## Steps

1. Check for the toolchain mismatch. On this user's machine, `rustc` is installed via
   Homebrew but `cargo-llvm-cov` looks in the rustup-managed toolchain. If `which rustc`
   resolves to `/opt/homebrew/...` instead of `~/.rustup/toolchains/...`, surface a
   one-line warning and recommend:

   ```bash
   rustup default stable
   rustup component add llvm-tools-preview
   ```

   Don't run those for the user; just report the mismatch and proceed if possible.
   (CI doesn't have this problem.)

2. Run `cargo llvm-cov $ARGUMENTS` (with `--workspace` always added). Capture stdout.

3. Print:
   - The headline metrics (line %, function %, region %).
   - A per-file table sorted by line coverage ascending (worst first), so gaps surface
     immediately.
   - The 3 **documented uncovered surfaces** from `NEXT_STEPS.md` for context:
     - `src/server.rs::handle_node_death` lines 1610-1685 (binary respawn, only hit by
       the `#[ignore]`d heal_subprocess test).
     - `src/gateway.rs::trigger_node_kill` success branch (Unix lsof, in-process suite
       can't trigger).
     - `src/gateway.rs::handle_tcp_proxy` (deadlock pinned by ignored tests).

## Why this exists

CI uploads `lcov.info` as a build artifact but doesn't yet enforce a floor (NEXT_STEPS.md
§3 commit notes mention the floor is deferred until baseline). Local coverage is the
primary signal during development; this command makes it a one-step operation instead of
the multi-step toolchain-fight it would otherwise be on this machine.

## When to use

- Before opening a PR that adds new tests, to confirm they actually hit the intended code.
- When investigating "is this code path tested?" — the per-file table + the documented
  uncovered surfaces give a complete picture.
- When deciding whether to write more tests vs. ship: if a PR moves coverage by < 0.5 %,
  it's not worth the test overhead; spend time elsewhere.
