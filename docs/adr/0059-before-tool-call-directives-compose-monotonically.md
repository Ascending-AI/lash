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

Before-tool-call hooks run in registration order. Each `ReplaceToolArgs` takes
effect immediately, then Lash re-runs every earlier hook once with the replaced
arguments before advancing through the remaining hook chain. Directives from
that bounded reinspection participate in the same fold. If the reinspection
produces another replacement, Lash rejects composition with the typed
`PluginError::BeforeToolCallReplacementConflict`; it does not seek a fixed
point. This makes a single replacement visible to every argument-inspecting
hook regardless of whether the inspector was registered before or after the
replacer. Reinspection is inspection only: Lash honors denials and aborts from
the repeated hook invocation, but does not re-apply side effects already
applied from its initial invocation.

Terminal directives form a restrictive join:

1. `AbortTurn` is most restrictive.
2. A failed or cancelled `ShortCircuitTool` is a denial.
3. A successful `ShortCircuitTool` is least restrictive.

The most restrictive terminal directive wins regardless of registration order.
Equal-strength conflicts use plugin ID as a stable tie-breaker; directives from
the same plugin use first-emitted-wins. Thus a hook can reduce permission but
cannot restore it. Non-terminal side effects from the initial pass continue to
run in directive order.

`AbortTurn` is the strongest stop directive in this join, but at the
before-tool-call seam it does not abort the enclosing turn. It projects as a
failed tool result. When a later `AbortTurn` displaces an earlier structured
denial, Lash therefore retains the denial's typed failure code and message
instead of degrading that evidence to the abort's generic `tool_error`.

Every inter-plugin terminal conflict is emitted through the awaited session trace seam as a
custom `before_tool_call.directive_conflict` event identifying the winning and
ignored plugin IDs and directive kinds. The event is attributed to the actual
later plugin whose directive caused the conflict. If the trace service rejects
the event, Lash logs that emission failure. Lash also attempts a non-blocking
plugin runtime event with the same attribution and payload; that secondary
channel remains best-effort when full or closed.

## Consequences

- Deny-then-allow and allow-then-deny both deny; abort wins against every other
  terminal outcome.
- A conditional argument normalizer that reaches a fixed point is supported:
  earlier and later policy hooks inspect its replacement. Two unconditional
  replacers hard-fail every call with the typed composition rejection. This is
  fail-closed by design, and the bounded rejection is also the termination
  proof: Lash never seeks another fixed point.
- A before-tool hook may run more than once for one call. Its repeated
  invocation is reinspection only: denials and aborts are re-honored, while
  side-effecting directives are not re-applied.
- Plugin registration order still orders transformations and side effects, but
  it cannot change terminal permission. This is locked by
  `replacement_is_reinspected_by_earlier_policy_in_either_registration_order`,
  `clean_bounded_reinspection_runs_on_replaced_arguments`, and
  `replacement_during_bounded_reinspection_is_a_typed_composition_error`.
  `reinspection_does_not_emit_a_self_conflict` pins conflict evidence,
  `reinspection_rehonors_terminals_without_reapplying_side_effects` pins
  reinspection effects, and
  `two_unconditional_replacers_are_a_typed_composition_error` pins the
  fail-closed termination rule.
- `PluginDirective` remains a transient in-process value with the same public
  and serde shape; the change needs no persistence or wire migration.

FIG-1399 records the option to reconsider replacement semantics once real
multi-plugin compositions provide evidence for a different policy. This ADR
does not depend on any particular production before-tool hook registrant.
The separate `after_tool_call` directive fold is unchanged and remains tracked
by FIG-1400.
