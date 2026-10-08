# Plugin state is a lash-mediated per-plugin store

## Context

Durable plugin state needs observable changes and a runtime-owned checkpoint
boundary. A plugin's private freshness assertion cannot prove that the state
captured at a boundary includes every accepted change, and a change made
inside work that is never recorded must not reach work that is.

## Decision

Lash owns a JSON key-value namespace for each runtime owner and plugin id.
For a session plugin the owner is its session. `PluginStateView` binds that
identity once; reads do not accept another plugin id. A namespace carries the
plugin's declared nonzero `format_version`. State and recorded config share
that stamp and the factory's pure format codecs.

### 1. The surface

The cloneable view exposes `owner`, `plugin_id`, `generation`, `get`, `get_as`
and `keys` over the published namespace. It has no writer, and no handle a
plugin retains writes. Reads are synchronous map operations under a shared
mutex and return owned JSON values.

A plugin changes its namespace only by returning `StateCommands` with a
recorded result: from a tool body (`ToolOutcome::Done` carries them), or from a
before-turn, after-turn, checkpoint or after-tool result-check callback. Every
other callback slot is decision-only; its answer type has no command field.
Commands are `set`, `remove` and `apply`, which names a pure reducer the
plugin registered with `PluginRegistrar::state_reducer`. A read-modify-write
goes through a reducer; it never reads a view and writes back.

### 2. Keys, values, refusals and the error type

Keys are 1 through 128 bytes of ASCII letters, digits, dot, underscore, or
hyphen. Namespaces are flat. A JSON value is capped at 32 KiB of compact JSON;
the namespace's values map is capped at 128 KiB; a batch holds at most 64
commands. Limits are runtime constants.

A batch publishes all of its commands or none. An invalid key, an oversized
value or namespace, too many commands, an unknown reducer, a reducer's typed
refusal or panic, a batch for another plugin's revision, a stored writer
format the plugin cannot write, and commands from a decision-only slot each
refuse the whole batch with a typed `StateCommandRefusal`. The refusal is the
batch's recorded resolution: it publishes no value and the namespace's
generation advances once.

`PluginStateError` carries what a plugin can still fail on directly: a codec
failure in `get_as` or `set_as`, and a pure initial or converted namespace over
the limits. Its `Into<PluginError>` conversion returns `PluginError::State`
with the typed variant and fields. `PluginStateError` and `KeyRejection` are
cloneable and serializable; plugin JSON and recorded process outcomes retain
their variants and fields.

### 3. One coordinator, sequenced per namespace

One coordinator per runtime owner reduces every batch privately. A recorded
body's batches are held until the body returns its result; a failed result
publishes nothing. The coordinator then reserves each namespace a batch names,
waiting while another unreturned publication holds it, reduces the batch
against the published namespace, and attaches each `StateResolution` to the
body's recorded outcome. Bodies of other namespaces, and bodies that return no
commands, never wait.

A resolution names its plugin revision, origin (tool attempt or callback
occurrence), owner activation, ordinal and predecessor. The ordinal is the
namespace's publication position; the predecessor is the last publication it
was reduced against. Its publisher's effect address identifies the logical Run
and recorded phase. Namespace generation tracks value freshness, including
format conversion, separately from the publication frontier.

### 4. Publication and resume

A resolution commits in the transaction that records the outcome that carries
it ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5), so a
published change is durable with its result. An outcome that did not commit
published nothing.

Resume installs committed resolutions without running the body, the hook, the
reducer or a format converter. A delivery ahead of its predecessor waits for
it; one at or below the namespace's frontier applies nothing. The frontier
keeps the receipt digests of the publications its owner's current run applied:
a durable run settles the frontier of the head it begins from, so a head holds
its settled ordinal and the receipts of the run that wrote it, never the whole
history (FIG-5393). A delivery in that window applies nothing only when its
complete receipt digest matches, and a different receipt is refused as
`FrontierRefusal::ReceiptMismatch`. A delivery at or below the settled ordinal
is one the run's base head already holds: the only records that deliver one
are the after-turn state of the commit that wrote that head, which cannot
differ from it, so it applies nothing without a receipt. After
ownership moves to a later activation, an earlier activation's unapplied
resolution is refused with a typed `FrontierRefusal`.

Physical-turn preparation adopts the admitted session turn index as publication
ownership before any hook runs. A process adopts its admitted activation
before capability construction. A callback retains the activation it started
under across awaits, so a new owner's claim cannot relabel a stale callback.

Before-turn and after-turn callbacks run in the turn-boundary phase they belong
to, and their decisions commit with it. A phase that did not commit runs them
again from committed state, so hooks are repeat-safe (ADR 0132 §4). After-tool
result checks of a tool attempt are recorded with the attempt; a cached or
deferred result's checks commit with their own phase.

### 5. Where the view is exposed

`PluginRegistrar::state()` supplies the bound view during registration.
`SessionReadyContext.state` supplies it at readiness. Hook closures retain the
registration view.

The engine records pure initialization and conversion as one complete
`PluginTransitionRecord` before constructing capabilities. The request names
its admitted identity, runtime owner, retained session head or captured process
state, and target plugin admission. A refused namespace or config keeps the
whole candidate unpublished. Inactive namespaces retain their stamp and values.

For a session, one fenced `RuntimeCommit` publishes the namespace checkpoint,
recorded admission and native view with the existing operation receipt. Resume
loads the recorded candidate, and acknowledgement loss reuses that receipt.
Factory build, registration and readiness reconstruct capabilities over the
published native view. The plugin view of the host cannot export other
namespaces.

### 6. The checkpoint component

The `plugin_state` keyed component is the session's namespace map: for each
plugin, its format version, generation, publication frontier (its settled
ordinal and the receipt digests of the run that wrote the head), fork policy
and the content address of its values. The frontier counts in the session's
plugin-state budget with the values. Each namespace's
values are one immutable body of their own, the `plugin_state/<plugin>`
component, by content address (FIG-5301). A commit writes a body only for a
namespace whose values changed; an unchanged namespace keeps its reference
and costs nothing, however large. Retention is by reachability from a
retained head.

A durable run pins the head it starts from and records the namespaces it
changes as rows beside its turn (`turn_namespaces`), each body written once
per value. Phase checkpoints carry no plugin state. A restore reinstalls the
pinned state with the run's rows over it; `turn.commit` promotes them into the
head, and a cancelled turn drops them with the turn, leaving the head as it
was. A code cell's quiet point writes the rows before it prunes a settled
call's records, so pruning never removes the only copy of a value. Resident
namespaces are immutable and shared, never deep-copied into a prompt cut.

A publication is refused with `StateCommandRefusal::SessionTooLarge` when it
would take the session's encoded total past 6 MiB, below the 8 MiB capture
bound; past 4 MiB it applies and is reported
(`plugin_state.session_budget_warn`).

The `plugin_admission` opaque checkpoint component carries the transition
request and current native namespace/config view. It uses the existing
checkpoint blob and operation receipt machinery, with no second log or
transition table. Resident adoption reads this native view and performs no
conversion. The separately encoded `plugin_state` component retains the
admission's writer formats.

Per-key generations add no useful invalidation boundary because capture writes
a changed namespace whole. Resident hydration adopts the recorded native
namespaces and their frontiers.

### 7. Fork

A fork is bound to the cut its source made visible at capture. Each plugin
declares at registration what a fork does with its namespace
(`PluginDeclaration::state_fork`): `copy`, the default, keeps the parent's
namespace as of the fork point, its values body shared by content address;
`reset` omits it from the child's map, so the child starts from the plugin's
initial state. The policy is recorded with the namespace. A fork preserves
non-resident namespaces as well as resident ones.
Parent and child publications are independent; there is no merge.
Fork creation resets publication ownership to the child's initial activation
and retains the inherited frontier. Content-addressed
storage can deduplicate unchanged bodies. Retention policy governs how long
the session's checkpoint contents remain available.

A catalog fork removes the parent's `plugin_admission` from its checkpoint
manifest while retaining the namespace components. The admission names the
parent's runtime owner and cannot authorize the child. A cold child open
defers capability construction until the engine records and publishes the
child's own transition. Adopting another owner's native view remains refused.

## Alternatives considered

Snapshot callbacks and plugin-owned revision counters delegate freshness to
the mutating plugin. Mediation makes freshness a runtime fact. A writable
handle with speculative capture and rollback lets an unrecorded write reach a
sibling that becomes durable; per-edit base and postimage receipts and
generation guards only detect that after the fact. Returned commands with one
reducing coordinator keep every published change behind a recorded result.

A global namespace has no session lifecycle owner. Append-only plugin logs add
another write algebra and retention contract; capped JSON arrays already serve
bounded list state. Per-key dependency tracking adds bookkeeping to a namespace
that is captured whole. Unconditional capture of every namespace remains
correct but rewrites unchanged state at every commit. File-store escape hatches move plugin durability
outside the runtime-owned boundary; larger state belongs in host storage.

## Consequences

The plugin owns its JSON schema and its pure migration and writer encoders.
Encoding takes the caller's explicit writer version. The host never selects it
from a live fleet read. Config stamps travel with options, recorded config and
process environments; state stamps travel with every captured `SessionPluginInit`.
Lash owns publication order, serialization, and checkpoint capture. Derived
caches can be rebuilt from the store. A plugin sees its own commands only after
their result returns; reducers must be pure, since a recorded resolution is
installed without them.

## Code references

- `crates/lash-core-store/src/tool_run/state_command.rs` defines commands, slot authority, refusals, reduction and the frontier.
- `crates/lash-core-execution/src/plugin/state.rs` implements the read-only view, bounds and native hydration.
- `crates/lash-core-execution/src/plugin/state/publication.rs` is the coordinator: private reduction, recorded resolutions and publication.
- `crates/lash-core-execution/src/plugin/recorded_callbacks.rs` records before-turn and deferred result-check callbacks.
- `crates/lash-core/src/runtime/turn_driver/after_turn.rs` runs a finished turn's after-turn callbacks; their decisions and staged state commit in its `turn.commit` (FIG-5283).
- `crates/lash-core-execution/src/plugin/transition.rs` defines complete transitions and checkpoint native views.
- `crates/lash-core/src/runtime/plugin_transition.rs` prepares and publishes the session transition.
- `crates/lash-core/src/runtime/process_runtime.rs` adopts a process's recorded transition.
- `crates/lash-core-execution/src/plugin/runtime_impl.rs` reconstructs read-only capabilities.
- `crates/lash-core-store/src/session_state.rs` captures the checkpoint components.
