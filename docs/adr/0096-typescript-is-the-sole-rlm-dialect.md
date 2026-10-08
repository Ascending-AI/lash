# 0096: One IR and VM, extensible dialects, TypeScript today

Status: Accepted. Amends ADRs 0037, 0055, 0060, 0061, 0062, 0063 and 0064.

## Decision

Lash has one dialect-neutral IR and VM and supports many possible code-mode
dialects. TypeScript is the only shipped dialect. Each dialect defines its own
semantics and targets the IR. Dialects need no parity and do not emulate one
another.

`lashlang` names the AST, linker, compiler, bytecode, continuations, heap and
value model, VM, workflow graph and `lash-lashlang-runtime` engine. It has no
authored source language. A dialect is a front end for the current IR, with
its own semantics and acceptance evidence.

## Dialect selection contract

A dialect is a value of the public `lash_protocol_rlm::Dialect` trait. A host
selects exactly one per RLM protocol by passing it to
`RlmProtocolPluginFactory::new(config, dialect, backend)`; a prompt-only host
passes it to `RlmDriver::new` or `RlmProjectorConfig::new`. There is no default
and no registry: TypeScript is selected by naming `TypescriptDialect`
(`Arc::new(TypescriptDialect)`), and a new dialect is added by implementing the
trait and passing it at the same place.

Every adapter a session uses comes from that one value: the worker frontend
for cells, `processes.create` and module source parsing, tool call-path spelling and
signatures, authored-example rendering, prompt vocabulary and cell tags, the
notation of inferred value shapes, the history item definition, the execution
section, and the stream event and diagnostic names derived from its language
id. The concrete `TypescriptDialect` type appears only inside its adapter
(`lash-protocol-rlm/src/dialect/typescript.rs`) and where a host names it.

The session records the selection twice and resumes only under it:

- Its language id under `dialect` in the RLM protocol-turn options, beside the
  `channel`, written when the session materializes. Every later build compares
  it with the host's selection: a different id is the typed
  `PluginError::RecordedSessionConfigConflict { field: "dialect" }`, and a
  rematerialized session with no recorded id is
  `PluginError::MissingRecordedSessionConfig { field: "dialect" }`.
- The execution-state snapshot's `engine`, which a restore under another
  dialect refuses.

A session's create options carry no dialect: the create contract has no such
field and refuses one.

### What a dialect implements

- `language_id`: the stable id the session records.
- `worker_service`: the compiled worker entry and bounds for the source
  frontend. That entry implements `WorkerFrontend::parse`: source to
  `lashlang::Program`, with the live host environment for cells, including
  prior globals, expired functions and process handles. It returns typed
  `WorkerFrontendRefusal` outcomes with the diagnostic and policy class.
  `worker_entry_with_frontend` is entered before host initialization. Parent
  `parse` and `parse_cell` callbacks are removed under ADR 0123: the host
  selects the frontend but all model-source lowering runs in its child.
- `render_parse_diagnostic`: host-side presentation of the worker's typed
  refusal; its default preserves the worker's rendered diagnostic.
- `tool_call_path`: the call path a cell writes for a front-end-neutral
  `ResolvedToolBinding`, or a typed `DialectRefusal` when no cell can address
  it. Registration refuses a catalog member the selected dialect cannot call.
- `tool_signature`, `schema_type` and `render_tool_example`: catalog tool docs
  in its syntax; an authored example it cannot spell is left out. A dialect
  is handed `SchemaShape`s, the contract layer's one reading of a tool's JSON
  Schemas (`lash-sansio`), and only spells them. It reads no JSON Schema
  itself, so an open object's fields, nested shapes, enums, unions and
  constraints reach every surface from the same import. Shared code builds the
  per-field rows and the required-output block from the same shapes through
  `schema_type`. Inferred runtime values construct the same shape directly.
- `schema_definition`: a named record, with its fields spelled through
  `schema_type`. Shared code defines the history item this way, from the shape
  `lash-rlm-types` reads off the item's own serialized form, so a dialect
  never declares the item by hand.
- `prompt_vocabulary`: language name, execution title, `CellTags`, cell noun,
  history type and history item name, inspect and finish forms, continue-as
  forms, the field-miss rule of its runtime.
- `render_execution_section`: the whole execution section, given the
  transport, the rendered tool docs, the catalog, the host environment and the
  discovery operation.

Shared code does language-neutral work only: shape inference and naming,
manifest binding resolution to a typed `ResolvedToolBinding` under the manifest
key `lash.tool`, transport, finish and retry copy assembled from the
vocabulary and cell tags, and the `code` stream message that carries a cell's
source. `RlmDialectServices` carries session resources rather than source
semantics: artifact storage, deferred tool resolvers, trace
configuration, the worker service, execution bounds, the renderer and the
session's cell or native-tool channel. The native-tool channel has no cell delimiter.

`scripts/check-dialect-boundary.py` fails when the TypeScript adapter's
concrete type is used outside the adapter as more than a value a host names,
when a retired TypeScript binding name returns, when TypeScript prompt text
appears in shared code-mode production sources, or when the lash integration
tests' fixture dialect or frontend is mentioned outside them. Its compiled
frontend is registered only by its test worker binary, never by the shipped
worker or a default registry.

## Dialect-neutral guarantees

A module's identity is its linked IR, not the front end that produced it.
`ModuleArtifact::ir` retains that program verbatim, including binder names,
binding visibility and hidden process arguments. There is no renamed copy.
Alpha-variant cells have distinct refs and immutable artifacts (law L9).
`0` and `-0` are distinct, NaNs share one representation, and non-finite
values store losslessly (FIG-3571).

The module ref and source identity use the atom `lashlang-ir`. Identical
linked programs share a module ref regardless of their source dialect
(FIG-4020). Neither `ExecRequest` nor the exec-code effect command carries a
language string. Compiled artifacts, durable processes, stores and wire
execution commands consume IR and shared values, not source syntax.
`processes.create` lowers its source through the session's dialect and hands
the process engine IR.

The clock/random module is `__lashlang_runtime`, its receiver type
is `lashlang.Runtime`, and its host operation is `lashlang.runtime`. Internal
modules in the reserved `__` namespace are hidden from the model. Durable
engine, process and effect identifiers keep their Lashlang spellings.

The compiler emits reference semantics. Durable capture preserves shared
acyclic identity through validated graph encoding and refuses cycles. A new
dialect that needs a new IR operation must justify that shared machine change
explicitly; it does not fork a VM or add language switches to storage.

## Prompt, tools and workflow lens

Each dialect owns its prompt syntax. Shared fragments consume its vocabulary,
tags and notation. Tool descriptions and schema prose render verbatim; hosts
own any syntax-specific prose they supply.

The TypeScript canonical workflow printer is implemented. Canonical get-put
and put-get laws and typed refusals govern IR it can spell. The prompt walker
retains explicit IR/VM identifier carve-outs.

The examples and judged runbooks cover the behavior they name. The
`typescript-host-flows` cells and runbook exercise host results, aggregate
settlement, agent loops and durable process lifecycle. Each dialect owns
coverage for its semantics rather than a paired battery.

Shapes change in place under the pre-1.0 version freeze, with no bumps,
upcasters or compatibility readers (FIG-3846). ADR 0115 governs the 1.0 cut.

## Executable evidence

- `a_seam_proof_dialect_runs_a_real_turn_through_the_host` and the
  `a_suspended_session_keeps_its_selected_dialect_*` law in
  [the seam proof](../../crates/lash/tests/seam_proof_dialect.rs).
- `recorded_dialect_refuses_substitution_and_missing_pin` in
  [the session pins](../../crates/lash-protocol-rlm/src/plugin/channel.rs).
- [`scripts/check-dialect-boundary.py`](../../scripts/check-dialect-boundary.py)
  and its test.
- `no_assembled_prompt_fragment_carries_the_retired_surfaces_words` in
  [the prompt walker](../../crates/lash-protocol-rlm/src/dialect/prompt_walker_tests.rs).
- The extension-session vocabulary and parse-feedback tests in `dialect.rs`.
- `canonical_get_put_and_put_get` in
  [the lens tests](../../crates/lash-typescript/tests/workflow_graph.rs).
- `shared_binding_list_and_record_literals_stay_shared_after_snapshot_round_trip`
  in [the continuation tests](../../crates/lashlang/src/runtime/tests/continuation_wire_cases.rs).

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
