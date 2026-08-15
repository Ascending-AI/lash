# Before-tool-call directives compose monotonically

## Status

Accepted.

## Context

Before-tool-call hooks are contributed independently by plugins, but Lash used
to invoke every hook with the original arguments and then apply every emitted
directive as an unconditional assignment. A later successful short-circuit
could therefore replace an earlier denial, a later directive could replace an
abort, and an argument replacement could bypass every later plugin's inspection.
The result depended on registration order in ways that could restore permission.

Tool Catalog membership remains the availability boundary. This decision is
only about composing directives for a call that already reached the hook chain;
it does not add a general per-call permission system or hook priorities.

## Decision

Before-tool-call hooks run in registration order, but each
`ReplaceToolArgs` takes effect before the next hook is invoked. The remaining
hook chain therefore inspects the current arguments, not a stale initial
snapshot. Re-running already-completed hooks would require fixed-point and
cycle semantics for transformations without adding a stronger safety property,
so Lash deliberately advances once through the remaining chain.

Terminal directives form a restrictive join:

1. `AbortTurn` is most restrictive.
2. A failed or cancelled `ShortCircuitTool` is a denial.
3. A successful `ShortCircuitTool` is least restrictive.

The most restrictive terminal directive wins regardless of registration order.
Equal-strength conflicts use plugin ID as a stable tie-breaker; directives from
the same plugin retain authored order. Thus a hook can reduce permission but
cannot restore it. Non-terminal side effects continue to run in directive order.

Every terminal conflict emits a `lash::plugin_composition` warning identifying
the winning and ignored plugin IDs and directive kinds. Lash also attempts a
non-blocking plugin runtime event named `before_tool_call.directive_conflict`;
the structured warning remains the guaranteed evidence if that best-effort
channel is full or closed. A conflict is never silently dropped.

## Consequences

- Deny-then-allow and allow-then-deny both deny; abort wins against every other
  terminal outcome.
- Argument normalizers remain supported, and later policy hooks inspect their
  normalized arguments.
- Plugin registration order still orders transformations and side effects, but
  it cannot change terminal permission.
- `PluginDirective` remains a transient in-process value with the same public
  and serde shape; the change needs no persistence or wire migration.
