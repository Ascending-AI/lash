# MCP stable naming and refresh dispatch

This deterministic runbook validates the model-facing MCP naming contract and
the raw dispatch identity retained by an accepted call. Read
[`../RULES.md`](../RULES.md) first. The fixture starts a real MCP peer over
stdio, exchanges initialize and tools/list JSON-RPC messages, publishes a
tools/list_changed notification, and observes the native name sent in the
subsequent tools/call request. It opens no RLM session and makes no provider
network call.

Choose the warm fork and an evidence directory owned by this run. The commands
use `pipefail`, so `tee` cannot hide a failed test process.

```bash
set -o pipefail
: "${LASH_MCP_NAMING_FORK:?set this to the caller-owned warm fork name}"
: "${LASH_MCP_NAMING_EVIDENCE_DIR:?set this to a fresh evidence directory}"
mkdir -p "$LASH_MCP_NAMING_EVIDENCE_DIR"
```

## Phase 1 — stable bounded names

Do:

```bash
orb gate lash "$LASH_MCP_NAMING_FORK" -- bash -lc \
  '. ./env.sh && heavy-slot cargo test --workspace --all-targets --locked \
  naming::tests -- --nocapture' \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/mcp-naming-runbook-names.log"
```

Expect exactly seven matching tests and `7 passed; 0 failed`. The assertions
prove that:

- the complete existing durable ToolId feeds an independently checked 128-bit
  BLAKE3/base32 suffix;
- catalog neighbors and ordering do not affect a surviving tool's name or
  binding;
- normalized Unicode and 64/65-character boundary inputs remain ASCII and at
  most 64 bytes;
- the generated binding exposes no compatibility aliases.

Abort if the filter reports zero tests, any test fails, or the independent
fixed vector changes without an intentional naming-contract change.

## Phase 2 — captured raw target across refresh

Do:

```bash
orb gate lash "$LASH_MCP_NAMING_FORK" -- bash -lc \
  '. ./env.sh && heavy-slot cargo test --workspace --all-targets --locked \
  deferred_call_ -- --nocapture' \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/mcp-naming-runbook-refresh.log"
```

Expect the MCP crate to run exactly two matching tests with
`2 passed; 0 failed`. Other workspace crates may also contain tests matching
the broad filter. The controlled
stdio peer first advertises raw `get_user`. The host accepts a call by durable
ToolId and pauses after resolving it. The peer then publishes
tools/list_changed and introduces raw `get-user` before the call resumes.

Two schedules are required. In the first, `get_user` survives beside
`get-user`, and its model-facing name remains unchanged. In the second,
`get_user` disappears from the refreshed catalog entirely. In both schedules,
the resumed JSON-RPC request must still carry native name `get_user`, producing
the exact result `"underscore"`; `"hyphen"` proves misrouting and fails the
test.

Abort if the call reaches `get-user`, if a saved accepted call is looked up
again by model-facing name, or if the peer exchange does not complete within
the bounded test timeout.

## Phase 3 — collision refusal

Do:

```bash
orb gate lash "$LASH_MCP_NAMING_FORK" -- bash -lc \
  '. ./env.sh && heavy-slot cargo test --workspace --all-targets --locked \
  refuses_a_forced_ -- --nocapture' \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/mcp-naming-runbook-collisions.log"
```

Expect the MCP crate to run exactly two matching tests with
`2 passed; 0 failed`. One injects an
identical final name for two raw tools in a single server catalog. The other
injects the same name across two configured servers and checks the pool-wide
publication view. Both must return a configuration error naming the collision;
the first catalog stays published and the rejected catalog remains invisible.
The check is derived from the current catalogs and never assigns a name.

Abort if either duplicate is overwritten, receives a numeric suffix, or
partially appears in the published tool catalog.

## Scorecard

| Claim | Gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Names are stable, bounded ASCII projections of durable identity | seven naming tests pass | | `mcp-naming-runbook-names.log` |
| A surviving raw tool keeps its name when a normalized neighbor appears | add-neighbor refresh test returns `underscore` | | `mcp-naming-runbook-refresh.log` |
| An accepted call retains its raw target after that target leaves the catalog | remove-original refresh test returns `underscore` | | `mcp-naming-runbook-refresh.log` |
| Duplicate final names are refused atomically across one catalog and the full pool | two forced-collision tests pass | | `mcp-naming-runbook-collisions.log` |
