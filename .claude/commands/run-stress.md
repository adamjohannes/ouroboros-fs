---
description: Run cargo test repeatedly with bounded timeouts to detect flakes.
argument-hint: "[count] [extra cargo test args]"
---

# Run-stress: detect test flakes

Run the test suite N times under bounded timeouts and report any flakes. Default N=10.
Optional cargo arguments are passed through (e.g., `/run-stress 5 --test gateway_http`).

## What you're doing

Parse `$ARGUMENTS` to extract:
- The first **integer** token is the run count (default `10`).
- Everything else is passed through to `cargo test` after a `--`.

For each run (1..=count):
1. Background-launch `cargo test [extra args] 2>&1 > /tmp/run_stress_<i>.out`.
2. Wait up to **30 s** (single-test runs) or **60 s** (default suite).
3. If the process is still alive: `kill -9` it and mark the run as `STUCK`.
4. After it exits: scan the output for `FAILED` lines. If any, mark `FAIL` and capture the
   first `^test .* FAILED` line for the summary. Otherwise mark `PASS`.

After all runs:
- Print `summary: P passed, F failed, S stuck` (out of N).
- For each failed/stuck run, print: `run <i> <kind>: <first-failing-test-name>`.
- If all clean, also print the wall-clock per run for visibility.

## Why this exists

Tests in this repo can flake under heavy parallel load (7+ test binaries running
concurrently). The settle-window discipline in `tests/common/mod.rs` papers over the
documented async-relay race, but a 1-in-10 flake is hard to catch in a single run. This
command is the systematic version of "run it 10 times and see what breaks." Used during
PR-T0 and the gateway/server-handlers commits to validate stability before merge.

## Constraints

- Don't use `--test-threads=1`. The flakes are real timing bugs; serializing hides them.
  See `CLAUDE.md` "Don't do these" #6.
- Don't bump settle-window timeouts blindly to make a flake go away — diagnose root cause first.
- The 4 ignored tests (large_file_streaming_100mb, full_heal_*, gateway_tcp_proxy_*) are
  not run here. To stress them: `/run-stress 5 --release -- --ignored full_heal large_file_streaming`.

## Reporting

If you observe ≥1 failure across N runs, propose a diagnosis with the failing test
location, the suspected race window, and a minimal reproducer (e.g., "this test always
fails when X test runs in parallel"). Don't just report counts.
