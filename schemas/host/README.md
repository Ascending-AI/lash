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

Every document here ships through `lash::schema::host`: directory names become
snake_case module names, each with `JSON_SCHEMA` and, where shipped,
`TYPESCRIPT_DECLARATION`. For example,
`lash::schema::host::kernel_migration_refusal::JSON_SCHEMA` and
`lash::schema::host::kernel_parked_refusal::TYPESCRIPT_DECLARATION` are available
without optional runtime features or a host-side Schemars dependency. The
facade's drift law compares every constant with its document here.

Run `python3 scripts/generate-workflow-schemas.py` after a schema owner changes.
Run the same command with `--check` to detect drift. The check also rejects an
obsolete versioned document left beside the current one.

Each version owner has one generator binary, and the `//:host_schema_documents`
target runs all of them:

| Shape directory | Rust shape | Version owner | Generator |
| --- | --- | --- | --- |
| `trace-record` | `TraceRecord` with its `TraceEvent` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `workflow-execution-overlay` | `WorkflowExecutionOverlay` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `kernel-migration-refusal` | `KernelMigrationRefusal`, including document and parked causes | `KERNEL_VERSION` | `crates/lash-core/src/bin/process_workflow_schema_generator.rs` |
| `kernel-parked-refusal` | `ParkedRefusal` | `KERNEL_VERSION` | `crates/lash-core/src/bin/process_workflow_schema_generator.rs` |
| `process-workflow-overlay` | `ProcessWorkflowOverlay` | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core/src/bin/process_workflow_schema_generator.rs` |
| `process-effect-outcome` | `process.effect_outcome` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `process-effect-omissions` | `process.effect_omissions` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `process-ran` | `process.ran` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `kernel-document` | kernel `Document` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-function-declarations` | kernel `FunctionDeclarations` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-function` | kernel `FunctionDefinition` | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-authoring-operation` | kernel edit `authoring::Operation` | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-entry-extraction` | atomic extracted document and effects | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-edit-refusal` | typed edit and extraction refusal | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-edit-transaction` | kernel edit `Transaction` | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-correspondence` | kernel edit `Correspondence` | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-workflow-view` | kernel edit `view::WorkflowView` | `KERNEL_VERSION` | `crates/lash-kernel-edit/src/bin/kernel_edit_schema_generator.rs` |
| `kernel-captions` | kernel `Captions`, a host-held sidecar, never a checkpoint kind | `KERNEL_VERSION` | `crates/lash-kernel-doc/src/bin/kernel_schema_generator.rs` |
| `kernel-caption-view` | `view::CaptionView`, captions joined by view location | `KERNEL_VERSION` | `crates/lash-kernel-view/src/bin/kernel_view_schema_generator.rs` |

The runtime registers and validates the process-event payload schemas. The
three kernel documents are the typed readable/editable JSON forms of `lash-kernel-doc`
([kernel semantics](../../docs/kernel/semantics.md), `K-DOC-004`), and the
kernel canvas and extraction documents are the data of `lash-kernel-edit` (`CANVAS-CLOSED`, `EXTRACT-REACHABLE`);
the authoring operation is Lash-owned request data embedded in host envelopes,
and the workflow view is the read a host edits against (`lash::workflow::view`). A parked
run has no JSON document: it is the persistent heap's checkpoint pages
([kernel design](../../docs/kernel/design.md), section 6). These
documents describe their owners' readable or projection contracts.
The default runnable host value is opaque `DocumentBytes`, tagged with Lash's
token grammar version and its Lash-issued id; JSON and generated TypeScript
are only for reading and editing. See the [host blob format note](../../docs/kernel/document-bytes.md). Lash
ships no engine wire protocol; hosts own transport DTOs and compatibility
([ADR 0136](../../docs/adr/0136-hosts-own-their-wire-contracts.md)).

Kernel migration and parked refusals also publish `v1.d.ts`, derived from
those schemas. Hosts enabling `codemode` can use `lash::vm::KernelMigrationRefusal`
and `lash::vm::KernelParkedRefusal` directly with Schemars. As with other
published enums, the schemas and TypeScript name this build's known variants;
Rust consumers retain a wildcard arm for the `#[non_exhaustive]` enums.

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
