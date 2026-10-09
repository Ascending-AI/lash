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

Expect exactly nine tests and `9 passed; 0 failed`. A lone `search-docs`
becomes `docs.search_docs` and `mcp__docs__search_docs`. Tool case survives
cleanup; each invalid character becomes `_`. Leading digits get an initial
underscore; an empty native name becomes `tool`. Server prefixes retain their
lowercase normalization and are bounded independently of the tool catalog.

All members of a cleanup or 64-byte truncation collision receive `__` plus
eight base32 characters of their durable-id digest. Independently computed
vectors pin those suffixes. No member wins the bare name, and removing a
collision restores the survivor's bare name. A raw name matching a generated
suffix joins the hashed group. Random catalogs prove uniqueness and order
independence. Bindings expose no aliases.

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

## Phase 3: cells, replay bindings and process requirements

```bash
kiln gate lash "$LASH_MCP_NAMING_FORK" -- \
  kiln test //crates/lash-plugin-mcp:lash-plugin-mcp__unit_test --test_output=all \
  --test_arg=pool::naming_cell_tests --test_arg=--nocapture \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/cells.log"
```

Expect exactly five tests and `5 passed; 0 failed`. A TypeScript cell and a
shared Lash VM IR cell call `docs.delete` and a hashed collision path through
a real MCP peer. TypeScript still refuses `docs.then`. The recorded bare
binding restores its original durable id after a collision rename or a
foreign occupant. Its required live call yields `LashVmCellBindingDrift`,
which parks the turn. A process artifact requiring the old operation fails
host-requirement admission. The runtime's existing replay and process-admission
laws cover journaled outcomes and the `ProcessHostEnvironmentIncompatible`
wrapper.

## Phase 4: accepted refusal cases

```bash
kiln gate lash "$LASH_MCP_NAMING_FORK" -- \
  kiln test //crates/lash-plugin-mcp:lash-plugin-mcp__unit_test --test_output=all \
  --test_arg=refuses_a_forced_ --test_arg=--nocapture \
  | tee "$LASH_MCP_NAMING_EVIDENCE_DIR/refusals.log"
```

Expect exactly two tests and `2 passed; 0 failed`. A forced 40-bit digest
collision keeps the typed configuration refusal. A forced cross-server final
name collision also refuses publication atomically. Configured server-prefix
collisions remain static configuration errors. Ordinary cleanup or truncation
collisions import successfully.

Abort any phase if zero cases execute, a test fails, or a recorded call reaches
a different durable id. The cutover changes shapes in place at the 1.0
stored-format reset. It adds no version bump, alias, upcaster or name table.
