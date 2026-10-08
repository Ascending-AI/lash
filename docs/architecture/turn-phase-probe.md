# Turn-phase instrumentation

`RuntimeTurnPhaseProbe` is an explicitly unstable internal instrumentation
seam. It is available across workspace crate boundaries for measurement,
simulation pauses and fault injection. Its types, methods, phase names and
callback locations carry no stability or SemVer promise, including at 1.0.
They may change with the turn loop. The definitions, facade re-exports and
runtime setters are `doc(hidden)`; visibility exists for these consumers,
not as a supported host lifecycle API.

## Consumers

- `crates/lash-perf/src/runtime_perf/measurement/phase_probe.rs` measures phase
  duration, allocation and process memory. Its typed phases map to snake-case
  report keys, and each key can have several simultaneous open spans.
- `crates/lash-conformance/src/conformance/frame_switch_redrive.rs` is the
  probe-based crash proof: `PanicAfterSwitchCommit` injects a crash at
  `PostCommitDelivery` and proves redrive from the recorded admission.

These are instrumentation consumers. Hosts submit through `send()`; the engine
owns executing, admission and recovery under ADRs 0101, 0109 and 0110. A probe
cannot settle work, change admission or grant a host turn-executing authority.

## Fixed phases

The durable driver brackets preparation, the live effect loop, head-commit
assembly through acknowledgement, and post-commit publication with spans that
close on drop. Store-call measurements decorate the durable port before the
backend is built, so the node's transactions use the same measurement sink as
the catalog.

The current typed vocabulary is `BeforeTurnHooks`, `PromptBuild`,
`EffectLoop`, `PreparedTurn`, `CommittedTurn` and `PostCommitDelivery`. The definition is in
`crates/lash-core-llm/src/turn_vocabulary.rs`. These are observation points in
physical turn execution, not a persisted state machine. Cancellation, early
returns and failure may skip phases or leave a begun phase without an end.
They do not promise one pair per logical run or committed turn.

## Named phases

Named callbacks carry the exact name emitted by the caller. There are two
current naming forms:

- Runtime operations use dotted owner and operation names, with snake-case
  components, such as `turn_cancel.start_gate`,
  `commit_admission.product_attempt`, `rlm_process.await_handle` and
  `rlm_lashlang.cell_tool_bindings`. `RuntimeNamedPhase::begin` accepts a
  static string; dropping its guard emits the corresponding end, including
  when an async future is cancelled or unwinds.
- Plugin dispatch uses `plugin_hook.{hook_kind}.{plugin_id}`. The current
  kinds are `before_turn`, `after_turn`, `checkpoint`, `context_pressure`,
  `turn_finalized`, `session_restored` and `session_config_changed`. The plugin id is appended verbatim, without
  escaping, case conversion or normalization. It may contain dots; parsers
  must treat everything after the hook-kind separator as the id. For example,
  plugin `probe.fixture` produces `plugin_hook.before_turn.probe.fixture`.

Plugin spans surround the awaited hook invocation. An ordinary `Err` closes
its span before the error propagates or is aggregated. Unlike the guard above,
plugin spans are explicit callbacks: dropping the dispatch future or a panic
in the hook may leave them open. No global balanced-span guarantee exists.
The interface performs no name validation; consumers must tolerate unfamiliar
names rather than interpret them as durable protocol vocabulary. These forms
record current production call sites, not a closed list of future names.

## Registration and dispatch

`RuntimeTurnPhaseProbeSlot` shares a scope-keyed registry. A frame-specific
registration overrides the session registration; an unregistered frame falls
back to its session. Registration replaces the probe for that scope. The host config
shares the slot with the served node. Registering a session's probe updates that slot; every runtime the node opens resolves the session's
registration, and process tool steps resolve their recorded session capability.
Workers carry the resulting `Arc` through execution.
The slot is not consulted at every hook boundary.

The runtime binds `PluginSession::dispatch` to that resolved probe. Its borrowed
`PluginDispatchContext` carries the session and probe through `prepare_turn`,
`before_turn`, `after_turn` and `emit_runtime_event`. The latter
clones the probe only for concurrent lifecycle hooks. The context borrows the
session rather than cloning its contributions, performs no registry lookup,
and adds no shared mutable instrumentation state to plugin sessions. Binding
`None` preserves uninstrumented dispatch. Pressure hooks retain their own
phase inputs.

## Callback obligations

Callbacks run synchronously on the execution thread. Lifecycle hooks run
concurrently and process activity can overlap, so probes must synchronize their
own state and tolerate overlapping spans with equal names. There is no total
order across such work and no span identity beyond its name or typed phase.
A missing probe emits nothing; named methods default to no-ops.

Production measurement callbacks should be short and must not panic, block on
runtime work or invoke executing. Test-only pause and crash probes deliberately
block or panic under a controlled harness. Callback overhead belongs to the
measurement; durations can overlap and are not disjoint wall-clock partitions.
The seam is not journaled, serialized or a recovery contract.
