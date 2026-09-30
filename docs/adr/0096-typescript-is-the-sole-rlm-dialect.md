# 0096: One IR and VM, extensible dialects, TypeScript today

Status: Accepted (FIG-3016, 2026-09-13), clarified by Sam's FIG-4276 ruling
on 2026-09-30. Supersedes the parity and session-pinning obligations of
ADR 0061 and ADR 0063, while retaining their multi-dialect architecture.
Amends ADRs 0037, 0055, 0060, 0062 and 0064.

## Decision

Lash has one dialect-neutral IR and VM and supports many possible code-mode
dialects. TypeScript is the only shipped dialect today. Each dialect defines
its own semantics and targets the IR. Dialects need no parity and do not
emulate one another.

`lashlang` names the AST, linker, compiler, bytecode, continuations, heap and
value model, VM, workflow graph and `lash-lashlang-runtime` engine. It has no
authored source language. The retired Lashlang lexer, parser, canonical source
printer and prompt contract remain deleted. A future dialect is a new
front end for the current IR, with its own semantics and acceptance evidence.

## Dialect extension contract

The retained `Dialect` trait in `lash-protocol-rlm/src/dialect.rs` owns:

- A stable language id used for the source execution and its session state.
- Source-only parsing and cell parsing against the live host environment,
  including prior globals, expired functions and process handles.
- Typed refusal classification, source spans and rendered diagnostics.
- Callable host-tool signatures and call-path addressability in its syntax.
- Prompt vocabulary and transport cell tags consumed by shared session code.

`RlmDialectServices` carries session resources rather than source semantics:
artifact storage, deferred tool and trigger resolvers, trace configuration,
execution bounds, the renderer and the session's cell or native-tool channel.
`DialectSession` runs the supplied lowering over the shared IR and VM and
retains that dialect's language id in its execution state. Shared code must
read language syntax through the selected dialect, rather than defaulting to
TypeScript. The native-tool channel has no cell delimiter.

A new production dialect also needs its own prompt and tool adapter,
schema/type spelling, renderer where syntax matters, canonical printer if it
participates in the workflow lens, and acceptance tests. It must be explicitly
selected at the host's front-end/session boundary, consistently with prompt
assembly, tool bindings and restored execution state. The current host serves
TypeScript without a language selector or first-commit session pin.

This is an extension boundary, not a claim that registration alone already
adds a production dialect. `rlm_dialect()` currently selects TypeScript, and
protocol factories, prompt drivers and `processes.create` still use the
concrete `TypescriptDialect` adapter. Generalizing those adapters and defining
the selection contract belong to the addition of a production dialect. The
FIG-4276 report records their code references and recommendations.

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

The journaled clock/random module is `__lashlang_runtime`, its receiver type
is `lashlang.Runtime`, and its host operation is `lashlang.runtime`. Internal
modules in the reserved `__` namespace are hidden from the model. Durable
engine, process and effect identifiers keep their Lashlang spellings.

The compiler emits reference semantics. Durable capture preserves shared
acyclic identity through validated graph encoding and refuses cycles. A new
dialect that needs a new IR operation must justify that shared machine change
explicitly; it does not fork a VM or add language switches to storage.

## Prompt, tools and workflow lens

Each dialect owns its prompt syntax. Shared fragments consume its vocabulary
and tags. Tool descriptions and schema prose render verbatim; hosts own any
syntax-specific prose they supply. FIG-4093 removed the former tool-prose
registration guard and `{{...}}` token mechanism.

The TypeScript canonical workflow printer is implemented. Canonical get-put
and put-get laws and typed refusals govern IR it can spell. The single-shipped-
dialect prompt walker retains explicit IR/VM identifier carve-outs. These
are implemented contracts, not pending printer or retirement work (FIG-4163).

The examples and judged runbooks cover the behavior they name. The
`typescript-host-flows` cells and runbook exercise host results, aggregate
settlement, agent loops and durable process lifecycle. Future dialects own
coverage for their semantics rather than a paired battery.

## Superseded history

ADR 0061 formerly required two permanent dialects at full parity and
first-commit session pinning. The authored Lashlang language was retired by
FIG-3019 through FIG-3024, including its enums, registry, host selectors,
`.lash` sources and doubled runbook/example battery. That retirement did not
remove the architecture for future front ends over one IR and VM.

The original format-bump and drain window is historical. Under the pre-1.0
version freeze, shapes change in place with no bumps, upcasters or compatibility
readers (FIG-3846). ADR 0115 governs the 1.0 cut (FIG-4125).

## Executable evidence

- `no_assembled_prompt_fragment_carries_the_retired_surfaces_words` in
  [the prompt walker](../../crates/lash-protocol-rlm/src/dialect/prompt_walker_tests.rs).
- The extension-session vocabulary and parse-feedback tests in `dialect.rs`.
- `canonical_get_put_and_put_get` in
  [the lens tests](../../crates/lash-typescript/tests/workflow_graph.rs).
- `shared_binding_list_and_record_literals_stay_shared_after_snapshot_round_trip`
  in [the continuation tests](../../crates/lashlang/src/runtime/tests/continuation_wire_cases.rs).
