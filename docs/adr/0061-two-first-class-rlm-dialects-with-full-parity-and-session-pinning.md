# 0061: RLM dialects share one IR and VM

## Status

Partially superseded by [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md).
The parity, permanence, doubled-battery and session-pinning obligations are
superseded. The multi-dialect architecture remains accepted, clarified by
Sam's FIG-4276 ruling on 2026-09-30.

## Retained architecture

A dialect owns its source syntax and semantics and lowers into the shared
Lashlang IR. One linker, compiler, heap VM, continuation format and durable
runtime execute that IR. A dialect does not emulate another language.

TypeScript is the only shipped dialect today. Its language id is `typescript`,
spelled out in prompts, execution telemetry and restored execution state.
There is no current host selector or first-commit language pin. Adding a
production dialect requires explicit selection at the front-end boundary,
with consistent prompt, tool binding and session-state ownership. It does not
require a source-language field on IR artifacts or compiled process bodies.

A new dialect supplies its own lowering and evidence for its accepted
semantics. It has no obligation to reproduce another dialect's examples or
judged scenarios. ADR 0096 describes the retained `Dialect` extension contract
and the remaining registration work.

The workflow-graph lens has a TypeScript canonical printer. Its laws and typed
refusals apply to the IR that printer can spell; another dialect's printer
would need its own evidence.

## Superseded history

The 2026-09-13 decision required two permanent dialects at full parity, a
`lashlang` default, a doubled release battery, first-commit session pins,
reopen refusals and subagent pin inheritance. ADR 0096 retired the authored
Lashlang language and those obligations. Their historical rationale is in
this ADR's revision history; they are not requirements for a new dialect.

The earlier format-bump window is also historical. During the pre-1.0 version
freeze, shapes change in place without bumps or compatibility readers
(FIG-3846). ADR 0115 governs the 1.0 cut.
