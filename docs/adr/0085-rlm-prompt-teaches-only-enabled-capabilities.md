# ADR 0085: RLM prompts teach only enabled capabilities

## Context

Prompt instructions must describe operations the configured session can use.
Catalog documentation and host-operation teaching have different inputs, while
cell and native transport need different execution instructions.

## Decision

TypeScript is the RLM authoring dialect under ADR 0096. Render one tool catalog
as typed declarations with descriptions and parameter/return notes. Preserve
the host's distinction between structured results and strings. The host section
contains host operations, constructors and data types.
Catalog operations do not appear there a second time. Reserved internal `__`
modules remain hidden.

Teaching follows the selected catalog, host environment, and prompt features.
Process teaching depends on process catalog membership. Sleep depends on its
host ability. Host event teaching follows the host tools actually enabled under ADR 0136. Image,
decomposition, and continuation instructions follow their configured features
and catalog. The features and abilities a prompt teaches are the ones the
session recorded at creation, not the opening deployment's (FIG-4398, ADR
0126). Empty sections are omitted. Promise aggregates and ordinary
context handling do not require decomposition.

The standard library receives a short description. Unsupported constructs
produce actionable diagnostics, rather than requiring an exhaustive syntax
inventory in every prompt.

History's name, type, and count belong in the iteration tail. Structured history
schema appears when steps or attachments make it useful. Variable previews
explain truncation when this rendering shortens a value; retained outputs keep
their retrieval paths.

Standard mode, cell RLM, and native RLM use the same introduction and Guidance
templates. Execution copy states each transport's action syntax independently.

## Alternatives considered

Teaching all capabilities burdens small hosts with instructions they cannot
execute. Duplicating catalog signatures in the host section gives the same
operation competing descriptions. Deriving native copy by replacing words in
cell instructions risks teaching invalid transport syntax. Configuration-gated
sections and independently authored execution copy avoid those problems.

## Consequences

Prompt contracts track actual capabilities and preserve tool documentation.
Prompt-size, capability, transport, and hint tests supply the executable evidence.
Prompt rendering does not change the catalog's dispatch or durable identity.

## Code references

- `crates/lash-protocol-rlm/src/dialect/typescript.rs:324-373,553-625` renders catalog/host sections and gated execution copy.
- `crates/lash-protocol-rlm/src/protocol/prompt.rs:31-145` hides internal modules and derives host inventory.
- `crates/lash-protocol-rlm/src/driver/history.rs` and `crates/lash-protocol-rlm/src/native/history.rs` render iteration tails.
- `crates/lash-protocol-rlm/src/rlm_support.rs:320` explains actual variable truncation.
- `crates/lash-protocol-rlm/src/prompt_contract_tests.rs` pins capability and transport teaching.

[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
