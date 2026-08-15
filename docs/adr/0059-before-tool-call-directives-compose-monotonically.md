# Tool-call directives compose monotonically

## Status

Accepted.

## Context

Tool-call hooks are contributed independently by plugins. Lash used to apply
every emitted terminal directive as an unconditional assignment in both hook
folds. A later successful short-circuit could therefore replace an earlier
denial or abort. Before-tool hooks also used to invoke every hook with the
original arguments, so argument replacement could bypass another plugin's
inspection. The same bypass exists after execution when an earlier output
policy approves the original result and a later hook successfully replaces it:
without reinspection, that policy never sees the value that reaches the turn.

Tool Catalog membership remains the availability boundary. This decision is
only about composing directives for a call that already reached the hook chain;
it does not add a general per-call permission system or hook priorities.

## Decision

Both tool-call folds use the same restrictive terminal ordering:

1. `AbortTurn` is most restrictive.
2. A failed or cancelled `ShortCircuitTool` is a denial.
3. A successful `ShortCircuitTool` is least restrictive.

A stronger candidate replaces a weaker one, while a weaker candidate cannot
restore permission. When an abort displaces an earlier structured denial, Lash
retains the denial's typed failure code and message instead of degrading that
evidence to the abort's generic `tool_error`. Non-terminal side effects from
the initial pass continue to run in directive order. This ordering joins plugin
directives only; the tool's original result is not a lattice member. A plugin's
successful replacement may therefore replace an original failed tool result.

### Before-tool arguments

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

Equal-strength before-tool conflicts use plugin ID as a stable tie-breaker;
directives from the same plugin use first-emitted-wins. Thus a hook can reduce
permission but cannot restore it.

`AbortTurn` is the strongest stop directive in this join, but at the
before-tool-call seam it does not abort the enclosing turn. It projects as a
failed tool result. When a later `AbortTurn` displaces an earlier structured
denial, Lash therefore retains the denial's typed failure code and message
instead of degrading that evidence to the abort's generic `tool_error`.

### After-tool results

After-tool hooks run in registration order. A successful `ShortCircuitTool` is
a result-replacement candidate. Lash presents that candidate to every earlier
hook once before advancing the remaining hook chain. If it is the first
successful replacement, it becomes the effective result inspected by later
hooks. Equal-strength replacements remain first-emitted-wins: a later candidate
is reinspected by earlier hooks, then the hook chain continues with the
effective first replacement when that reinspection approves it.

Reinspection is inspection only. Lash re-honors `AbortTurn` and failed or
cancelled `ShortCircuitTool` directives from the repeated invocation, but does
not re-apply side effects. A successful replacement emitted during reinspection
is rejected with typed
`PluginError::AfterToolCallReplacementConflict`; Lash does not seek a fixed
point. Consequently, two unconditional result replacers fail closed, while a
conditional replacer that approves the candidate can compose with it. A hook
may therefore run more than once for one result. The original tool result
remains the result only when no terminal directive is emitted.

### Conflict evidence

Every inter-plugin terminal conflict is emitted through the awaited session
trace seam as a custom `<hook>.directive_conflict` event identifying the
winning and ignored plugin IDs and directive kinds. The hook is
`before_tool_call` or `after_tool_call`. The event is attributed to the actual
later plugin whose directive caused the conflict, including when the later
plugin supplies the stronger winner. A plugin's multiple terminals compose by
the same rules without manufacturing a self-conflict event. If the trace
service rejects the event, Lash logs that emission failure. Lash also attempts
a non-blocking plugin runtime event with the same attribution and payload; that
secondary channel remains best-effort when full or closed.

## Consequences

- Deny-then-allow and allow-then-deny both deny; abort wins against every other
  terminal outcome in both hook folds.
- A conditional argument normalizer that reaches a fixed point is supported:
  earlier and later policy hooks inspect its replacement. Two unconditional
  replacers hard-fail every call with the typed composition rejection. This is
  fail-closed by design, and the bounded rejection is also the termination
  proof: Lash never seeks another fixed point.
- A before-tool hook may run more than once for one call. Its repeated
  invocation is reinspection only: denials and aborts are re-honored, while
  side-effecting directives are not re-applied.
- An after-tool hook has the same invocation contract for a successful result
  replacement: earlier hooks rerun once, only denials and aborts are re-honored,
  and side effects are applied exactly once. A second successful replacement
  during that pass is a typed composition rejection.
- Plugin registration order still orders transformations and side effects, but
  it cannot let a weaker terminal restore permission. Before-tool composition
  is locked by
  `replacement_is_reinspected_by_earlier_policy_in_either_registration_order`,
  `clean_bounded_reinspection_runs_on_replaced_arguments`, and
  `replacement_during_bounded_reinspection_is_a_typed_composition_error`.
  `reinspection_does_not_emit_a_self_conflict` pins conflict evidence,
  `reinspection_rehonors_terminals_without_reapplying_side_effects` pins
  reinspection effects, and
  `two_unconditional_replacers_are_a_typed_composition_error` pins the
  fail-closed termination rule.
- After-tool replacement safety is locked by
  `after_tool_replacement_is_reinspected_by_policy_in_either_registration_order`,
  `after_tool_clean_bounded_reinspection_keeps_the_replaced_result`, and
  `after_tool_replacement_during_reinspection_is_a_typed_composition_error`.
  `after_tool_reinspection_does_not_reapply_side_effects` pins exactly-once
  effects, `after_tool_reinspection_does_not_emit_a_self_conflict` pins bounded
  conflict evidence, and `after_tool_two_unconditional_replacers_fail_closed`
  pins the termination rule. The laws
  `after_tool_deny_wins_in_either_registration_order`,
  `after_tool_abort_wins_in_either_registration_order`, and
  `after_tool_three_plugins_keep_the_most_restrictive_terminal` pin the shared
  strength ordering. `after_tool_equal_strength_result_replacement_is_first_wins`
  pins the result tie-break after clean reinspection. Conflict evidence is
  bounded and identity-bearing, and one plugin's own multiple terminals produce
  no self-conflict.
- A `CreateSession` or `EmitTrace` error contributes a denied terminal without
  stopping the directive fold. Later directives, including side effects, still
  run. An invalid after-tool `ReplaceToolArgs` likewise contributes a denial. If
  another plugin's equal-strength denial displaces its message, the standard
  conflict event still identifies the winning and ignored plugins and directive
  kinds; `displaced_after_tool_replace_args_misuse_emits_conflict_evidence`
  locks that evidence path.
- `PluginDirective` remains a transient in-process value with the same public
  and serde shape; the change needs no persistence or wire migration.

FIG-1399 records the reconsideration scope for replacement semantics once real
multi-plugin compositions provide evidence for a different policy. That scope
now covers bounded reinspection at both the before-tool argument seam and the
after-tool result seam, including after-tool first-emitted replacement
selection; the shared restrictive terminal ordering remains the safety floor.
This ADR does not depend on any particular production tool-hook registrant.
