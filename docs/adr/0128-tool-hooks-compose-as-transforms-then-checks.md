# 0128: Tool hooks compose as transforms, then checks

## Status

Accepted (FIG-1399).

## Context

Tool hooks come from independent plugins. A hook that may both replace a
value and decide whether the call proceeds creates an ordering hazard: a
policy that approved one value can be bypassed by a later plugin that
replaces it. Inspecting every replacement again with every earlier hook
repairs that hazard only at quadratic cost, drops cache hits discovered on
the second pass, and makes the selected failure depend on registration
order.

Lash separates the two roles structurally. Transforms derive values and
cannot decide. Checks decide and cannot replace values. All transforms
finish before any check starts, so every check inspects the value that
executes or is returned.

Every other hook seam has its own fixed composition rule, stated in one
table below, so a plugin author never has to reason about how a seam merges
replies.

## Decision

### 1. Tool call sequence

For a call that the Tool Catalog admitted, with its call identity fixed:

1. Argument transforms (`ToolCallRegistrations::transform_args`) chain once
   in recorded registration order. Each receives the model's original
   arguments and the preceding candidate and returns the next candidate.
2. The arguments are validated against the tool's input schema.
3. The bound provider prepares the call. Its identity checks still apply,
   and arguments it changed are validated again
   (`invalid_prepared_tool_args`).
4. Every before-check (`check_args`) inspects the same immutable
   `PreparedCallReadView` together with the original arguments. All checks
   run, in recorded order, even after a terminal reply, so diagnostics are
   complete. No argument changes after the checks.
5. The selected decision executes the body, supplies a cached success, or
   produces the call's terminal output.
6. On a `Done` result, result transforms (`transform_result`) chain once in
   recorded order over the immutable original result and the preceding
   candidate.
7. Every after-check (`check_result`) inspects the one final candidate.

A deferred (parked) result is not final and runs no result hooks; its
completion runs them when it settles. Result hooks receive an explicit
`ToolHookOccurrence`: an attempt ordinal, a deferred completion of an
attempt, or a cached success, so a retried attempt and a completion are
never confused.

### 2. Check reduction

Before-check replies reduce with the ranks
**AbortRun > Deny = Cancel > Cached > Allow** (`CheckRecord::reduce` in
`lash_core_store::tool_run::tool_hooks`). Ties break by ascending UTF-8
plugin id, then ascending callback key. Registration and arrival order never
select the winner, so fixed replies reduce to the same control, result and
evidence in every order. One callback returns one verdict; several
callbacks of one plugin are distinct participants.

A cached success supplies data only. It passes through the result
transforms and after-checks as a `Cached` occurrence, and the body never
runs. Deny, Cancel and AbortRun always outrank a cache.

After-checks choose only **Allow, Deny, Cancel or AbortRun** and reduce by
the same ranks and ties. They cannot replace a successful result; recovery
of a result belongs in a result transform.

When a reduction displaces terminal replies, the winning plugin publishes a
`{phase}.conflict` plugin event and the trace event
`plugin.<winner>.{phase}.conflict` naming the winner and every displaced
terminal, each with its plugin id, callback key and verdict.

### 3. AbortRun is Run control

A check's AbortRun stops the owning logical Run. The call's output is a
plugin-sourced execution failure carrying the namespaced failure code, and
its `ToolControl::AbortRun` control is separate from that result. The turn
machine records every result of the batch, then finishes with
`TurnStop::PluginAbort` and a plugin-kind turn issue. A plugin that wants
only a failed call returns Deny.

### 4. Failures

A transform failure stops its chain and fails the call
(`tool_args_transform_failed`, `tool_result_transform_failed`): later
transforms have no valid input. A check failure is a restrictive reply
(`tool_args_check_failed`, `tool_result_check_failed`); it never becomes
Allow, and the other checks still run on the unchanged candidate.

### 5. Keyed registrations

Every composable callback is registered under a `HookKey`: 1 to 64 bytes of
`[A-Za-z0-9_.-]`, validated at registration, or at compile time through
`hook_key!`. The callback identity is the seam's slot and key, for example
`tool_args_check:policy`. A duplicate key within one plugin and seam is a
registration error. Context hooks use their trait's `id()` as the key.

### 6. The composition table

| Seam | Composition |
| --- | --- |
| `tool_calls().transform_args` | Chain once in recorded order. |
| `tool_calls().check_args` | Every check on one prepared call; reduce per §2. |
| `tool_calls().transform_result` | Chain once in recorded order over original and current. |
| `tool_calls().check_result` | Every check on one final result; Allow, Deny, Cancel or AbortRun; reduce per §2. |
| `turn().before`, `turn().after`, `turn().checkpoint` | Ordered observers returning declared contributions (events, tool membership and graph appends; after-turn also records). No abort or veto. A callback error fails the phase. |
| `output().stream` | Ordered chunk transforms; a stop request is sticky. |
| `output().stream_finished` | Ordered collection of end-of-stream state; a response reads the state of the finished callback it names. |
| `output().response` | Ordered full-response transforms. `stream_state_from` names one finished callback of the same plugin, validated at registration. |
| `tool_results().presentation_step` | Presenter first, then ordered steps, with the existing retry and fallback rule. Changes representation, never the result or Run control. |
| `tool_results().presenter` | One exclusive renderer. |
| `session().on_event` | Read-only observers delivered sequentially in registration order. A failure cannot retract a commit. |
| `context().attachment_omissions` | Every policy by descending priority; core omits the union. |
| `context().compact` | First substantive decision by descending priority, ties by recorded order. |
| `context().pressure` | Record contributions accumulate; the first OpenFrame stops. |
| `tool_catalog().contribute` | Union of removals; no priorities or terminal decision. |

### 7. Recording

The decision values are S01's typed records in
`lash_core_store::tool_run::tool_hooks`: `ToolHookPhase`, `ToolHookOccurrence`,
`CheckVerdict` and `CheckRecord`. Durable recording of the admission and
result decisions belongs to call admission and the resolved result decision
(FIG-4877). Namespace state commands proposed by turn, checkpoint and
result-check callbacks belong to the durable command publisher (FIG-4878).
Resume invokes no completed hook or reducer. Turn and checkpoint
contexts expose only `SessionReadService`. Their `SessionContributions`
record tool membership changes by tool identity and graph appends with their
operation identities and ancestor requirements. The runtime applies them
after the owning phase commits, on the live pass and on resume. A graph
append joins the turn draft and lands with its commit.

A route without a recorded result decision refuses command-bearing result
checks as `tool_result_check_state_unrecorded`, with the proposing plugin in
`ToolFailureCause::PluginStateUnrecorded`. It records no separate state-only
step. Cached and Deferred results publish commands through the Run
coordinator's D record.

## Consequences

- A check always sees the arguments that execute and the result that is
  returned. No hook runs twice for one call to repair ordering.
- A transform cannot see a check's decision, and a cache or denial cannot
  skip transforms. Transforms must stay cheap and free of ambient effects.
- Two cache hits with different values select one by callback identity;
  this is conflict evidence, not consensus.
- Turn observers cannot stop a turn. Domain rejection moves to a tool check,
  host admission, or a Run decision.
- Presentation remains trusted: the check guarantee covers semantic tool
  arguments and results, not the final model-facing bytes.

## Rejected alternatives

- **Directives with reinspection.** One reply type for replacement, policy
  and effects needs a second inspection pass whose cost grows with every
  replacement and which cannot honour a cache found on that pass.
- **A plain ordered chain.** An early inspector approves a value that a
  later callback changes; schema validation is not policy.
- **After-checks that replace results.** That reintroduces the bypass among
  checks; result replacement belongs to the transform phase.
