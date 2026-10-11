# MCP catalog naming and refresh dispatch

This deterministic runbook checks bare cleaned names, symmetric collision
hashes, and durable dispatch through a changing catalog. Read
[`../RULES.md`](../RULES.md) first. The fixtures run real stdio JSON-RPC peers
without provider network calls. Every invocation runs sequentially in the
caller-owned warm fork.

```bash
set -o pipefail
: "${LASH_MCP_NAMING_FORK:?set the caller-owned warm fork name}"
: "${LASH_MCP_NAMING_EVIDENCE_DIR:?set a fresh evidence directory}"
mkdir -p "$LASH_MCP_NAMING_EVIDENCE_DIR"
```

## Phase 1: bounded catalog names

```bash
kiln gate lash "$LASH_MCP_NAMING_FORK" -- \
  kiln test //crates/lash-plugin-mcp:lash-plugin-mcp__unit_test --test_output=all \
  --test_arg=naming::tests --test_arg=--nocapture \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/names.log"
```

Expect exactly three tests and `3 passed; 0 failed`. A durable tool id
length-frames the raw server and native names, so a separator in either cannot
collide two ids. Tool case survives cleanup; each invalid character becomes
`_`. Leading digits get an initial underscore; an empty native name becomes
`tool`. Generated names stay ASCII and within 64 bytes even for long server or
tool inputs.

A generated-suffix collision hashes every member — including a bare occupant
whose raw name already matches the generated form — with `__` plus eight
base32 characters of the durable-id digest; the `search_docs__6rlrgooy` vector
pins the suffix bytes.

## Phase 2: captured raw target across refresh

```bash
kiln gate lash "$LASH_MCP_NAMING_FORK" -- \
  kiln test //crates/lash-plugin-mcp:lash-plugin-mcp__unit_test --test_output=all \
  --test_arg=deferred_call_ --test_arg=--nocapture \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/refresh.log"
```

Expect exactly two tests and `2 passed; 0 failed`. A call accepted by durable
id pauses after resolution. A tools/list_changed notification either adds
`get-user` beside `get_user`, renaming both with hashes, or replaces `get_user`
with `get-user`, which inherits the bare name. In both cases the captured
request still sends raw `get_user` and returns `"underscore"`. A `"hyphen"`
result proves misrouting and fails the test.

## Phase 3: accepted refusal cases

```bash
kiln gate lash "$LASH_MCP_NAMING_FORK" -- \
  kiln test //crates/lash-plugin-mcp:lash-plugin-mcp__unit_test --test_output=all \
  --test_arg=refuses_a_forced_ --test_arg=--nocapture \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/refusals.log"
```

Expect exactly one test and `1 passed; 0 failed`. A forced digest collision
that produces identical final names is refused with a typed
"model-facing name collision" error, and the refusal does not overwrite an
existing member.

Abort any phase if zero cases execute, a test fails, or a recorded call reaches
a different durable id. The cutover changes shapes in place at the 1.0
stored-format reset. It adds no version bump, alias, upcaster or name table.
