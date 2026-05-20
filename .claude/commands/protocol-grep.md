---
description: Locate every layer of a wire-protocol command (variant, parser, dispatch, handler, tests).
argument-hint: "<COMMAND-NAME>  e.g. \"FILE PUSH-CHUNK\" or \"NETMAP SET\""
---

# Protocol-grep: find every layer of a command

Given a wire-protocol command name (e.g., `FILE PUSH-CHUNK`, `NETMAP SET`, `NODE PING`),
locate every layer where it appears in the codebase. Useful when:

- Understanding how a command is parsed → dispatched → handled.
- Refactoring a command (need to change all 5 layers in lockstep).
- Onboarding to the wire protocol — the layer breakdown is more revealing than reading
  files top-to-bottom.

## Argument

`$ARGUMENTS` is the command name. Treat it case-insensitively. For multi-word commands
the canonical form is `NOUN VERB[-MODIFIER]`. Examples:
- `FILE PUSH` → just the entry point, not PUSH-CHUNK or PUSH-OK
- `FILE PUSH-CHUNK` → the fan-out chunk-receive path
- `NETMAP SET` → the netmap broadcast receive

The `noun` and `verb-modifier` form a pair Claude grep can match on.

## What to surface

For the given command, find and report each layer:

### 1. Enum variant (in `src/protocol.rs`)

The `Command` enum variant matching the noun+verb. For `FILE PUSH-CHUNK` that's
`FilePushChunk { name, chunk_size, file_size, parts, index, start_port }`.

Print: file:line range with the variant + its field signature.

### 2. Parser branch (in `src/protocol.rs`)

The `parse_*_cmd` function and the specific `if let Some(rest) = rest.strip_prefix(...)`
block that produces this variant.

Print: file:line of the `strip_prefix` line + the field-extraction logic.

### 3. Dispatch arm (in `src/server.rs::handle_client`)

The `match` arm for `protocol::Command::FilePushChunk { ... }` that routes to a handler.

Print: file:line of the match arm.

### 4. Handler (in `src/server.rs`)

The `async fn handle_*` function that implements the command. Shape varies (W, RW, etc.)

Print: file:line of the function signature + first comment paragraph.

### 5. Wire emitter (sender side, anywhere)

Where in the codebase is this command **sent** (not received)? Look for
`format!("FILE PUSH-CHUNK ...` or `s.write_all(b"FILE PUSH-CHUNK ...`. The fan-out
PUSH-CHUNK is sent from `handle_file_push`, BACKUP-PUSH from `push_to_predecessor`, etc.

Print: each emitter file:line.

### 6. Tests

Find every test file that exercises this command. Look for the literal command string in
`tests/*.rs` and `src/*.rs` `#[cfg(test)]` blocks.

Print: the test name(s) and file:line.

### 7. Documentation

Where is this command documented? Search `README.md` §4.2, `docs/06_command_protocol_.md`,
the doc-comment block at the top of `src/protocol.rs`.

Print: file:line of each doc reference.

## Output format

Use a single markdown report with one section per layer. If a layer is missing (e.g., a
deleted command leaves only the doc reference), say so explicitly — that's diagnostic.

## When to use

- Refactoring a wire-protocol command (rename, field change, deletion). Use this first to
  generate a checklist of changes.
- "Is this command actually exercised end-to-end?" If the test count for a command is 0,
  flag it as a coverage gap.
- Investigating a parsing bug. The parser-branch + dispatch-arm pair tells you whether
  the bug is in parsing or in handling.

## Caveats

- `parse_line` splits on a literal space. A command name with multi-space or tab
  separators won't be found by this command (and won't be found by the parser either —
  see CLAUDE.md "Don't do these" #2).
- Don't run this for commands that no longer exist (e.g., `RELAY-STREAM`, deleted in
  PR7). The expected output is "every layer is missing"; if any matches show up, that's
  the cleanup signal.
