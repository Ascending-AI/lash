# Concurrent settlement is a durable group at the effect-host seam

## Status

Accepted.

## Context

An aggregate can resume at a deciding child while other children remain
unfinished. Its settlement order must survive a crash and be readable by a
successor invocation. A live completion race cannot supply that durable fact,
and an all-settled batch cannot supply an early return.

The effect host owns execution and replay. Restate implements the production
engine contract; SQLite and PostgreSQL store session and process state.
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
owns tool-child lifetime, protected settlement and incorporation.

### Restate satisfaction: the group virtual object is the rank authority

Restate's `EffectGroupIndex` virtual object, keyed by the group key, owns the
shape fence, lifecycle, final-commit decisions and settlement ranks. Its keyed
serialization makes a child's decision and rank durable. `EffectGroupPayload`
holds the child's payload separately, so the index stores terminal metadata
and rank references rather than tool-output bytes.

A rank read checks recorded state before waiting. A fresh invocation can read
the same rank without possessing the first invocation's futures. The read can
serve a consecutive run of seated ranks and payloads; controller read-ahead
still advances the public cursor one returned settlement at a time.

Deployments bind the index, payload, membership and dispatch handlers used by
the controller. SDK notification order within one invocation is insufficient
for cross-invocation rank reads. A missing handler is an engine routing failure;
a missing executor cannot be converted into a fabricated child terminal.

## Decision

Concurrent settlement is a structured, durable group on
`RuntimeEffectController`. Its three required group methods have no default
bodies. A controller implements them or explicitly refuses all three with
`EffectGroupUnsupported`. Resolver registration is wiring, not a second
durability or capability flag.

### Open, await and close

`open_effect_group(RuntimeEffectGroup)` accepts envelopes, not caller closures.
The registered `GroupExecutors` resolves an envelope to its executor. Dispatch
and recovery use the same resolver. A tool child uses `ToolInvocation` carrying
`ToolChildRequest`; its handler-level driver coordinates retries and waits,
while `ToolAttempt` names an atomic attempt body.

A first open resolves all children before creating group state. A missing
runner refuses the whole open with a shape error naming the child and its
replay key. Repeating the refused open cannot reinterpret a partly recorded
group as accepted. A reopen uses the retained shape and dispatch route. A
deployment that does not carry a recorded child now leaves it accepted: the
miss remains visible and retryable, and it neither invents a settlement nor
denies access to ranks already recorded.

A resolver tells that miss apart from one no retry repairs
(`GroupExecutors::missing_capability`). A deployment that serves the group's lane and
lacks a capability the child needs can never run it: a host that serves the
lane with no resolver registered, or a tool child whose opener lent it no
context on a deployment that installs no tool-child context source.

That answer is the deployment's, so it is drawn only from facts every worker
of the deployment shares: what the child's envelope recorded and what the
deployment registered or wired. Which worker holds a child's live opener is not
one of them. A tool child's request records whether its opener had a context to
lend where it formed the group (`ToolChildOpenerContext`), judged there because
the forming process is the opener's own. A child whose opener lent a context
runs on the worker that holds it; landing on another worker of the same
deployment is a miss of placement, and stays a retry whether or not the
deployment installs a context source.

A handler-driven engine records the answer once, in the child's own journal,
so every replay takes the same branch on whichever worker retries it. A child
recorded as unroutable settles `Failed` with the terminal
`RuntimeEffectGroupChildUnroutable`, whose cause names the missing capability,
and its opener's rank wait resolves. A child recorded as routable keeps the
retry on any miss.

`GroupReopen::RetainedShape` preserves recorded membership;
`GroupReopen::RetainedContent` also checks the offered child content. The shape
fence protects arity, replay keys, wake policy, loser policy and opener.

`await_next_settlement(&mut handle, cancel)` returns the next durable settlement.
`EffectGroupHandle` is the sole consumption cursor. Open starts at
`consumed = 0`; a restored continuation supplies its saved cursor. The host
advances it exactly once for each returned settlement and retains no independent
consumption cursor. Await cancellation changes neither the cursor nor the rank.
Exhaustion is checked locally; advancing beyond the child count refuses.

`close_effect_group(handle, policy)` releases consumer interest and is
idempotent. It does not retire the group or erase ranks and protected work.
A replay may close the same serialized handle again. Reopening establishes new
consumer interest under the retained lifecycle and policy.

### The settlement obligation

Settlement `n` of a group is a durable fact. Every replay observes the same
child at that rank; it never races children again to decide an existing rank.

The contract serves the `(consumed + 1)`-th smallest recorded sequence, not a
literal sequence value. Sequences need not be gapless. Three rules preserve
rank, including the group-atomic rule cited as N3:

1. Sequence allocation is strictly monotonic within the group.
2. The recorded set is append-only below any consumed rank.
3. Retirement is group-atomic. Removing a child below a saved cursor cannot
   shift the remaining ranks.

Restate allocates settlement ranks in the serialized index handler. A child
seats only once; duplicate settlement reads its existing rank. Commit sequence
and settlement rank are distinct facts: cancel-decided children have a rank
without a final-commit sequence.

### Wake policy is journaled identity

`GroupWakePolicy` has `First`, `FirstSuccess` and `All`. `race` consumes the
first deciding settlement; `any` consumes through the first success. `all` and
`allSettled` both use `All`: the caller stops on the first rejection for `all`
and consumes every settlement for `allSettled`. That stopping condition does
not require another host policy. `settlement_order` is a projection of durable
settlements, not an in-memory authority for the answer.

### Normative: ungrouped effects stay hash-identical

`RuntimeEffectEnvelope::group` is optional and omitted when absent. Group
membership carries its key, position, wake rule and loser policy in the
canonical envelope, so those facts participate in the hash fence. An ungrouped
effect carries no membership bytes. Incompatible format changes follow the
version-freeze and 1.0 cut contract of
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

### Normative: a group's copies are made to agree by construction

`RuntimeEffectGroup::try_new` is the checked constructor. It stamps each
unstamped envelope with the group's identity and index, and refuses a foreign
key, wrong position, changed wake rule or changed loser policy. It also refuses
an empty key, empty children and duplicate child replay keys.

A dispatch path unable to honor membership refuses it rather than stripping the
fence. Restate executes timer and durable-wait children as child invocations;
the retained group shape supplies their membership fence.

Zero-operand aggregates do not open groups. `all([])` resolves locally;
`any([])` rejects with an empty `AggregateError`; `race([])` never settles in
ECMA semantics and the host returns a typed unsettled-await failure rather than
parking an unbounded durable group.

### Group identity carries an occurrence discriminator

Language aggregates use the runtime-issued `CommandReplayKey`, whose issue
ordinal belongs to the recorded run. The tool-batch content digest is a content
check on that path, not its durable group address. A compiler instruction
pointer or live-only counter is not the identity authority.

Host tool batches use `{scope_id}:group:{batch_id}`, or
`{scope_id}:group:{parent_effect_id}:{batch_id}` beneath a parent effect. Their
batch id hashes the calls, including call identities and execution grants.
Protocol-standard `batch` contributes members to the turn step's top-level
group beside native calls. Tool bodies do not dispatch nested tool groups;
[ADR 0116](0116-tools-are-opaque.md) owns that tool contract.

### Loser disposition is declared at open

`LoserPolicy` is a durable group fact. Close may narrow `RunToCompletion` to
`Cancel`, but cannot widen `Cancel` to `RunToCompletion`. Recovery applies the
recorded policy rather than guessing what a caller would choose at close.

Promise aggregates use `RunToCompletion` while their opener lives. Selecting a
winner cancels no losing promise. A deadline select can declare `Cancel`.
Normal opener end has ADR 0099's closing protocol; `RunToCompletion` does not
grant an unfinished opaque tool permission to create new work after that end.

## Tool children have an opener lifetime

The opener is an admitted execution identity, including the process incarnation
where applicable. Worker death does not establish opener closure. Durable
closing records the admission boundary before cancellation.

A child's final record and cancellation compete at the serialized group-index
decision. A committed final remains protected through drain and projection;
a winning cancellation refuses a later final. Physical stop follows the
decision and cannot undo already-admitted obligations or known usage.

Committed siblings drain in durable final-commit order. The barrier waits on
the last-committed unseated lower sibling. Its own barrier is transitive, so
that one wake proves every lower committed sibling has seated. Rankability
depends on discharge of the child's obligations, not on opener closure.

Implicit engine cancellation is not the close protocol. The retained child
identity, cooperative cancellation decisions and protected-work recovery
continue to govern cancellation and settlement.

Retained-work admission and controller command budgets apply before dispatch.
The exact opener owns retained-work capacity; controller command headroom is
per executing segment. A segment can hand over outstanding children and saved
consumption. A completed group retires as a whole only when its recovery and
consumer dependencies are discharged and an identity fence prevents reopening.
There is no TTL that makes a saved continuation expire.

## Alternatives rejected

An all-settled batch cannot return a race winner early. Rewriting partial
outcomes in one growing batch payload couples progress and storage to the
slowest child and duplicates the engine's ordering authority.

A second SQL settlement journal makes storage an effect engine and cannot
satisfy a Restate deployment without that database. The index is engine-owned
keyed state.

Caller-owned executor closures cannot reconstruct a child in another handler or
on recovery. The deployment's resolver supplies that code from retained data.

Always cancelling losers changes Promise behavior while the opener is live.
Never cancelling at opener end permits implicit background tool work beyond
its admitted lifetime. The live, closing and settled phases express both rules.

Deleting state at close destroys protected drains and saved ranks. Time-based
deletion destroys continuation without proving severance. Retirement checks the
actual dependencies and retains the identity fence.

## Consequences

Each child is an independently durable execution and contributes to engine
command accounting. Wide groups share a rank authority, but consecutive-rank
reads and a transitive drain barrier avoid a wait or read per lower sibling.
Early aggregate return leaves explicit child obligations; normal opener end
settles those obligations under ADR 0099.

## Executable evidence

- [Checked group construction](../../crates/lash-core-execution/src/runtime/effect/group.rs#L183),
  [cursor](../../crates/lash-core-execution/src/runtime/effect/group.rs#L404),
  [policy narrowing](../../crates/lash-core-execution/src/runtime/effect/group.rs#L632)
  and [controller methods](../../crates/lash-core-execution/src/runtime/effect/executor/control.rs#L528)
  define the portable contract.
- [Restate open preflight](../../crates/lash-restate/src/controller/mod.rs#L737),
  [final commit](../../crates/lash-restate/src/effect_group.rs#L759),
  [rank seating](../../crates/lash-restate/src/effect_group.rs#L917),
  [close](../../crates/lash-restate/src/effect_group.rs#L1119) and retirement in
  that file implement the index authority.
- [Separate payload storage](../../crates/lash-restate/src/effect_group/payload.rs),
  [rank runs](../../crates/lash-restate/src/effect_group/rank_run.rs#L13),
  [transitive barrier](../../crates/lash-restate/src/effect_group/drain_barrier.rs#L58)
  and [child driver](../../crates/lash-restate/src/effect_group/dispatch.rs#L374)
  implement serving and recovery.
- [Runtime command keys](../../crates/lash-lashlang-runtime/src/replay_run.rs#L145),
  [aggregate formation](../../crates/lash-lashlang-runtime/src/aggregate.rs#L35)
  and [host batch keys](../../crates/lash-core-execution/src/session/tool_execution/group.rs#L169)
  define group addressing.
- [Opener reservation](../../crates/lash-core-execution/src/session/opener_groups.rs#L293)
  bounds retained children; [segment budget](../../crates/lash-restate/src/controller/mod.rs#L1057)
  requests a controller boundary separately.
- Shared group laws run against the in-process Restate server double, live
  Restate and lash-sim's in-process effect host. Store laws use SQLite file,
  SQLite memory and PostgreSQL. Upgrade proofs use the synthetic-next tier.
