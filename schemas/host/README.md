# Host contract schemas

This directory contains the checked-in JSON Schema documents for Lash's
host-facing contracts. Each shape has its own directory and one current file
named `v<version>.schema.json`. The document records the Rust version constant
in `x-lash-version-constant` and its numeric value in
`x-lash-schema-version`.

Run `python3 scripts/generate-workflow-schemas.py` after a schema owner changes.
Run the same command with `--check` to detect drift. The check also rejects an
obsolete versioned document left beside the current one.

Each version owner has one generator binary, and the `//:host_schema_documents`
target runs all of them:

| Shape directory | Rust shape | Version owner | Generator |
| --- | --- | --- | --- |
| `workflow-graph` | `WorkflowGraph` | `WORKFLOW_GRAPH_SCHEMA_VERSION` | `crates/lashlang/src/bin/workflow_schema_generator.rs` |
| `workflow-type-facets` | `WorkflowNodeTypeFacets` | `WORKFLOW_TYPE_FACET_SCHEMA_VERSION` | `crates/lashlang/src/bin/workflow_schema_generator.rs` |
| `trace-record` | `TraceRecord` with its `TraceEvent` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `trace-lashlang-graph` | `TraceLashlangGraph` | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/bin/trace_schema_generator.rs` |
| `remote-process-events-request` | `RemoteProcessEventsRequest` | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/bin/remote_schema_generator.rs` |
| `remote-process-events-response` | `RemoteProcessEventsResponse` | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/bin/remote_schema_generator.rs` |
| `remote-process-observation-request` | `RemoteProcessObservationRequest` | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/bin/remote_schema_generator.rs` |
| `remote-process-observation-item` | `RemoteProcessObservationItem` | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/bin/remote_schema_generator.rs` |
| `remote-session-observation-event` | `RemoteSessionObservationEvent` | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/bin/remote_schema_generator.rs` |
| `process-effect-outcome` | `process.effect_outcome` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |
| `process-effect-omissions` | `process.effect_omissions` payload | `PROCESS_EVENT_VOCABULARY_VERSION` | `crates/lash-core-execution/src/bin/process_event_schema_generator.rs` |

A document embedding another owner's shape pins that shape's own version too:
the remote observation item types its snapshot graph and node event record
with the trace definitions and pins their `schema_version` to
`TRACE_SCHEMA_VERSION`. The durable process-event documents are the payload
schemas the runtime registers and validates appends against, so there is no
second description to drift. ADR 0100's compatibility matrix states each
shape's decode rule and names the tests that enforce it.
