# Security model

OuroborosFS is a single-tenant internal data store. This document
describes the threat model for v1.0: who is trusted, what an attacker
can do, and where the boundaries are.

For the operator's perspective on hardening (auth tokens, systemd
units, network ACLs), see `docs/operations.md`.

## Trust boundary

**Trusted:**
- Every host running an `ouroboros_fs run` process.
- Every host running `ouroboros_fs gateway`.
- Anyone holding the cluster's `OUROBOROS_AUTH_TOKEN`.

**Untrusted:**
- Every other host on the LAN.
- Any process on a trusted host that doesn't hold the token.
- Any HTTP client that doesn't present `Authorization: Bearer <token>`.
- Any network observer (the wire protocol is plaintext; PSK auth
  proves liveness, not encryption — see "Roadmap" below).

Disabled-auth mode (no `--auth-token` and no `OUROBOROS_AUTH_TOKEN`)
expands the trust boundary to *every host that can reach a node
port*. This is documented as development-only and the binary logs a
warning on startup. Don't run disabled-auth in production.

## What an attacker can do

### Without the auth token

- Open TCP connections (caps apply: idle-timeout drops them within
  60 s; max-conns saturates at 1024 by default).
- Send arbitrary first lines. Without `AUTH`, the connection is
  closed within 1 s with `ERR auth required` or `ERR auth timeout`.
- Hit `/health` and `/ready` on the gateway (these intentionally
  bypass bearer auth so orchestrators can probe).
- Hit `OPTIONS` on the gateway (CORS preflight; bypasses auth).

That's the full attack surface: no file content, no metadata, no
cluster topology leaks. The auth check happens before any handler
runs.

### With the auth token

The PSK is a **shared secret**. Anyone with it has *full* access:

- Read every file in the cluster (`FILE LIST`, `FILE PULL`).
- Write any file (`FILE PUSH`).
- Trigger heals (`NODE HEAL`).
- Read per-node metrics (`NODE METRICS` via gateway `/metrics`).
- Spoof internal commands by sending `NETMAP SET`, `TOPOLOGY SET`,
  `FILE TAGS-SET` to any node.

There is **no per-tenant or per-user authorization** in v1.0. The
PSK gates access; once past it, every request is trusted.

### A compromised node

If an attacker takes over one node host:

- They have access to that node's `OUROBOROS_AUTH_TOKEN` (in
  `/etc/default/ouroboros` or systemd `EnvironmentFile=`).
- With the token, they have the same full-cluster access as any
  legitimate participant.
- They can read the local `<storage_root>/<port>/{content,backup}/`
  directly off disk, including files the cluster holds for any
  user.
- They can corrupt their own chunks; the SHA-256 trailer detects
  this on read and triggers fall-through to the predecessor's
  backup, but the attacker can poison the backup too if they
  control the host with predecessor responsibility for the same
  chunks.

There is no compartmentalization or sandboxing between the cluster
and the node host's filesystem.

## DoS surface

| Vector | Mitigation |
|---|---|
| Oversized PUSH | `--file-size` rejects upfront; the body is drained without buffering. |
| Connection flood | `--max-conns` caps in-flight connections. New connections beyond the cap get `ERR server busy` and immediate close. |
| Idle hold | `--idle-timeout` drops connections that don't make progress. AUTH handshake has its own 1 s timeout. |
| Filename traversal | Strict allowlist (`[A-Za-z0-9._-]`, no all-dot names) rejected at parse. The previous `sanitize_filename` rewriter that allowed `..` is gone. |
| HTTP body flood | Gateway rejects `Content-Length` > 50 GB before opening a ring connection. |
| `/metrics` scrape flood | No rate limit; rely on bearer auth to gate scraping. Front a real proxy in production if needed. |

## Wire-format integrity

- Every saved chunk has a SHA-256 trailer. A bit-flip in `content/`
  is detected on read; fall-through to predecessor `backup/` covers
  the recovery.
- A bit-flip in *both* content and backup is detectable but
  unrecoverable; the gateway emits a short body + `\nERR
  truncated …` trailer.
- The wire protocol itself is not authenticated end-to-end (each
  command is a plaintext line). The PSK proves the *connection* is
  authorized, not that individual commands haven't been tampered
  with by an in-path attacker.

## Out of scope for v1.0

- **Data-at-rest encryption.** Chunks live unencrypted on disk.
  Operators relying on confidentiality should encrypt the
  underlying storage volume.
- **Network-layer encryption (TLS).** Listed in the v1.1 roadmap
  (NEXT_STEPS.md §7.3) as the natural successor to PSK auth.
- **Per-namespace ACLs / multi-tenancy.** Single-tenant by design;
  v2.x territory.
- **Audit logging.** Application logs (`tracing`) record events
  but there's no append-only audit log of who did what.
- **Supply-chain integrity** of the binary itself. Standard Rust
  build practices apply.
- **Side-channel resistance** in the auth check. The HMAC compare
  is constant-time; the rest of the request handling isn't.

## Roadmap for narrowing the boundary

The intended trajectory:

1. **v1.0 (now).** PSK + bearer. Internal LAN with operator-managed
   network ACLs.
2. **v1.1.** mTLS (NEXT_STEPS.md §7.3). Every node and the gateway
   gets a cert from an operator-managed CA. The wire protocol
   becomes confidential and tamper-resistant; PSK becomes optional.
3. **v2.x.** Per-tenant namespaces and ACLs (§7.5). A token can be
   scoped to a subset of file names; pull/push are checked against
   the scope.

## Reporting vulnerabilities

If you find a security issue, please don't open a public GitHub
issue. Email the maintainer (or open a private security advisory
on the repository) with:

- A description of the issue and how to reproduce it.
- The commit hash where you observed it.
- An impact assessment (what an attacker can achieve).

Per the v1.0-rc nature of this release, expect discussion to be
public after a patch lands.
