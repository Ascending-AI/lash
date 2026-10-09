# Host contract schemas

This directory contains the checked-in JSON Schema documents for Lash's
host-facing contracts. Each shape has its own directory and one current file
named `v<version>.schema.json`. The document records the Rust version constant
in `x-lash-version-constant` and its numeric value in
`x-lash-schema-version`.

Schemars-generated documents use JSON Schema Draft 2020-12, with `$defs`
references and `prefixItems` for tuples. The runtime-owned process-event
documents retain Draft 7. Before 1.0, the version freeze permits refreshing
these documents and their generated TypeScript in place without version bumps.

Run `python3 scripts/generate-workflow-schemas.py` after a schema owner changes.
Run the same command with `--check` to detect drift. The check also rejects an
obsolete versioned document left beside the current one.

Each version owner has one generator binary, and the `//:host_schema_documents`
target runs all of them:

| Shape directory | Rust shape | Version owner | Generator |
| --- | --- | --- | --- |
| `workflow-graph` | `WorkflowGraph` | `WORKFLOW_GRAPH_SCHEMA_VERSION` | `crates/lash-vm/src/bin/workflow_schema_generator.rs` |
| `workflow-type-facets` | `WorkflowNodeTypeFacets` | `WORKFLOW_TYPE_FACET_SCHEMA_VERSION` | `crates/lash-vm/src/bin/workflow_schema_generator.rs` |
| `trace-record` | `TraceRecord` with its `TraceEvent` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `workflow-execution-overlay` | `WorkflowExecutionOverlay` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `process-effect-outcome` | `process.effect_outcome` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `process-effect-omissions` | `process.effect_omissions` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `kernel-document` | kernel `Document` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-annotations` | kernel `Annotations` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-function` | kernel `FunctionDefinition` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |

The runtime registers and validates the process-event payload schemas. The
three kernel documents are the stored forms of `lash-kernel-doc`
([kernel semantics](../../docs/kernel/semantics.md), `K-DOC-004`). These
documents describe their owners' stored or projection contracts. Lash
ships no engine wire protocol; hosts own transport DTOs and compatibility
([ADR 0136](../../docs/adr/0136-hosts-own-their-wire-contracts.md)).

## Run-owned tool and operation contracts

Host schemas describe the surviving public facade. Tool-bearing host tasks use
operation Run handles with explicit completion and follow/cancel/result, driven
by the session's existing turn service. Deferred descriptors remain pending;
they are not ToolCompletion values. Source terminals are immutable
`Resolved(ref)` or `Cancelled`, with no core deadline or timeout terminal.
Typed admission, plugin-revision and retained-material causes stay typed through
local APIs; a text message cannot replace their fields.

Definition contraction removes obsolete variants, exhaustive matches, schema
entries and generated TypeScript in the same compiling closure. Refresh the
current shape in place during the freeze. `kiln build //:schema_checks` is the
owning check when a serialized shape changes; documentation alone creates no
schema version or migration. [ADR 0099](../../docs/adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
and the [tool-run contract](../../docs/architecture/tool-run-contract.md) define
ownership, canonical material and source/continuation laws.
