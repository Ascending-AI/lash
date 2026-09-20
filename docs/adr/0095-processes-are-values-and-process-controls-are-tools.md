# 0095: Processes are values, process controls are tools, one handle kind

Status: Accepted (FIG-2990, 2026-09-13). Supersedes
[ADR 0087](0087-typescript-runtime-promise-arrays.md). Amends
[ADR 0011](0011-self-contained-processes.md),
[ADR 0037](0037-lashlang-workflows-use-a-code-graph-code-lens.md),
[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md),
[ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md) and
[ADR 0090](0090-named-process-signatures-are-authoritative.md).

## Context

A process was three things at once and none of them completely. `defineProcess`
and `start` were static special forms the lowerer had to recognise, so a process
could only be written where the compiler expected one. `processes.list` took the
definition as an untyped object, because a tool contract had no way to say
`Process`. The VM carried two handle encodings with two settlement phases, and
four readers that each decided independently what a handle was. A third-party
tool could neither receive a process nor start one.

The cost was paid per feature: every addition to the process surface was a
language change in two dialects, a lowering change, and a new entry in a
hand-written catalogue that existed only because the type vocabulary was too
weak to describe the values already flowing through it. The same weakness
produced defects rather than just friction — `processes.list` could never match
a cell-supplied definition (FIG-2989) because the two encoders of a definition
value disagreed, and the mixed process/tool aggregate rule in ADR 0087 needed a
second settlement phase purely because a process handle was not the same kind of
thing as a tool handle.

The design was reviewed in one round (adversarial critique, schema review,
complexity review) against the alternative of keeping the special forms; the
spec attached to FIG-2990 records the verdicts. This ADR records the settled
decisions. Implementation detail belongs to the children: FIG-2992 (core
reference and codec), FIG-2993 (`x-lash`), FIG-2994 (intents), FIG-2995
(registry), FIG-2996 (one handle kind), FIG-2997 (literal lift), FIG-2998
(captures and inferred signals), FIG-2999 (dialect deletions), FIG-3000
(plugin tools), FIG-3001 (examples, runbook and fixtures).

## Decision

### A process definition is a value

A process definition is a value of type `Process<(params), out>`, carried by
tool contracts like any other value. Every process control — start, await,
emit, signal, cancel, list, register, trigger register, create — is an ordinary
leaf plugin tool that takes and returns those values. The only constructs that
remain in the language are the ones that read or write the running VM's own
control state: `waitSignal`, `sleep`, `finish`.

`defineProcess`, Lashlang `process` declarations, the `start` keyword, `wake`
in both arities, `registerTrigger` and the `signals` configuration block are
deleted from both dialects. There is no compatibility reader for any of them.

### Contracts say `Process` through one tagged keyword

Tool-contract schemas gain a single serde-tagged `x-lash` extension keyword with
three inhabitants: `{kind: "process", signature}`, `{kind: "process_unknown"}`
and `{kind: "handle"}`. A malformed keyword is refused rather than ignored.
`x-lash` stays an extension keyword on JSON Schema; Lash does not grow a private
contract vocabulary. The hand-written trigger catalogue exists only because the
vocabulary was missing, and is deleted with it.

A signature that arrives on a value is a **claim**, never authority. Every
intent that would create a durable row calls the engine's `resolve` on the
reference first and refuses a mismatch before the row exists, so a fabricated
signature type-checks a cell and still starts no process.

### One definition reference, one codec

`ProcessDefinitionRef` — engine kind, engine-owned definition value, claimed
signature, with a fingerprint derived over engine kind and definition —
replaces the optional untyped definition on process identity. One encoder and
one decoder own the definition value; the second, divergent encoder is deleted.
Process identity becomes a derivation from its input and the engine registry
rather than a mutable record with setters.

### No new intent-identity mechanism

Start does not mint identity. The existing attempt identity
(`AttemptContext::intent_identity`) and the existing call-site-derived tool call
ids are sufficient and already replay-stable: a leaf start derives its process
id from its attempt identity, and the executor derives the same id from the
committed attempt, so a crash-redrive of the same attempt returns the same id.
The three copies of that derivation collapse to one. A tool call id is a
call-site identity and is never an ordinal; anything that treated it as a
position was reading a coincidence.

### Names are tool input only

A named-definition registry (`process_definitions`) is modelled on
`trigger_subscriptions`: owner scope, name, revision, definition fingerprint,
tombstone, change sequence, unique on owner scope and name, written by a
register intent under revision-and-fingerprint CAS. Under
[ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md),
session-scoped names follow the [ADR 0049](0049-session-ids-are-used-once.md)
frontier and host- or platform-scoped tombstones are never collected.

A name never crosses a durable boundary. The target type a tool accepts is
either a reference or a name, and it is tool input only; every durable record
and every draft pins a `ProcessDefinitionRef`. A trigger delivery therefore
fires the definition pinned at registration and never re-resolves a name at
delivery time.

### Literals lift syntactically, and are accepted type-directed

Both dialects gain a process-literal expression (a Lashlang `process (params)
{ }` expression, a TypeScript async arrow in argument position). Discovery is a
syntactic pre-pass in the lowerer; **acceptance is type-directed in the linker**
against the catalogue type, with no marker of any kind at the call site. A
literal whose expected type contains `Process` is hoisted to a declaration named
from its canonical body source and AST path; any other expected type is a type
error. The `triggers.register` special case is served by that one rule and
deleted.

Cell locals read by a lifted body become hidden parameters carrying their values
into the start arguments. Only immutable, durably representable locals lift; the
rest are refused with a diagnostic naming the variable and the rewrite, which
keeps [ADR 0011](0011-self-contained-processes.md)'s prohibition on capturing a
mutable name into an environment.

Signals are inferred from `waitSignal` sites — the set from the sites, the
payload type from the await-site expected type, disagreeing sites refused — and
the inferred set feeds exactly the same event-type registration, trigger-draft
seeding and reference hash that declared signals fed.

### One handle kind, and await is a Durable Wait

The VM has exactly one handle encoding, `{__handle__: "lash", id}`, with one
mint/parse pair shared by the language and core. The execution nonce folds into
the handle id; incarnation lives on the record; the four divergent handle
readers become one parse. Pending requests are keyed by handle id.

`processes.await` is a leaf tool that returns pending on a **Durable Wait**
resolved by the process terminal through the work-driver seam
([ADR 0016](0016-process-waits-live-on-the-work-driver-seam.md); Restate resolves
it by attaching to the process). It is never a batch child. Consequently
`Promise.all` and Lashlang aggregates are one resource-operation batch whose
recorded settlement order is authoritative, and
[ADR 0087](0087-typescript-runtime-promise-arrays.md)'s two-phase
tool-then-process rule is deleted along with the process-leaf settlement walk.
Cancellation of a wait carries a 50 ms grace before the wait is abandoned.

Every law ADR 0087 asserted has a replacement asserted under the single batch
order before the old test file is removed.

### Formats bump together

Bytecode, continuation, snapshot, VM ABI, semantic hash and the workflow graph
schema (8 → 9, and the graph schema joins the manifest) advance as one set
through `durable_formats()`. There are no compatibility readers for the old
handle, continuation or process-row shapes; stored artifacts are recompiled and
republished.

### The workflow graph sees calls, not a start effect

Starting a process is a call node, not a dedicated effect kind, and a process
literal in an argument projects as a process container node. The
[ADR 0037](0037-lashlang-workflows-use-a-code-graph-code-lens.md) lens laws are
unchanged: PutGet and the fixpoint hold for a cell containing a process literal.

## Alternatives considered

**Keep a marker at the lift site** (an explicit `process(...)` wrapper, or a
naming convention, telling the lowerer that an inline arrow is a process body).
Rejected: the catalogue already carries the expected type, so the marker states
a fact the compiler can check and can therefore disagree with it. A marker the
author can get wrong on a value the linker already types is a second source of
truth for the same fact, and its only load-bearing use — `triggers.register` —
is the case the type-directed rule serves directly.

**Keep `t.event` (and the aliased trigger surface) as sugar over the tool.**
Rejected: the point of the arc is that there is one way to reach a process
control. An alias keeps the hand-written catalogue entry and the dialect-side
lowering alive for a surface that the rendered tool catalogue already teaches,
so every later process feature is still paid for twice.

**Await a process as a batch child**, which would have kept ADR 0087's phase
structure and needed no Durable Wait. Rejected: a batch child must settle within
the batch's resource operation, and a process terminal can be days away. That
forced the two-phase rule, the separate process-leaf walk and the second handle
kind — the whole complexity ADR 0087 paid for existed to work around a process
not being a durable wait. On the work-driver seam, a mixed aggregate has exactly
one recorded settlement order and no phases.

## Consequences

- A third-party tool can declare `definition: Process`, receive a resolvable
  reference, and start a child from a leaf attempt with a replay-stable id.
  Process capability is no longer reserved to code Lash ships.
- New process features are catalogue and plugin changes, not language changes.
  The dialect prompt renders from the catalogue plus a few lines of teaching
  rather than a hand-written process block.
- Signature claims are checked exactly once, at the boundary where a durable row
  would appear, which is the only place a forged claim could do damage.
- [ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md)'s agent
  surface changes shape: durable work is no longer a static definition object,
  and static extractability is replaced by an engine-resolvable reference. The
  `return`/`throw`/`finally` contract for a process body is unchanged.
- [ADR 0090](0090-named-process-signatures-are-authoritative.md)'s authoritative
  signature is unchanged as a rule and moves site: it is what `resolve` returns,
  and the signature travelling on a value is checked against it.
- Pre-existing stored continuations, handles and process rows do not load. This
  is a pre-1.0 break taken deliberately, consistent with ADR 0011.

## Amendment: "never a batch child" becomes the Durable Wait child contract (FIG-3392)

**Not yet implemented.** FIG-3397 lands it; the contract is
[docs/design/effect-group-tool-children.md](../design/effect-group-tool-children.md).

"One handle kind, and await is a Durable Wait" above says `processes.await` "is
never a batch child". That sentence was written against the **atomic** batch,
whose children had to settle inside one resource operation — which is exactly why
a process terminal days away could not be one, and why the alternative
"await a process as a batch child" was rejected for forcing ADR 0087's two-phase
structure.

Under [ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md)'s
effect groups a child is an independently durable unit with no such bound, so
the sentence changes meaning rather than being deleted:

> **`processes.await(h)` is a resumable child of the group, on the existing
> Durable Wait protocol.** It is retained across segment boundaries, it takes its
> place in the recorded settlement order at the moment its completion arrives
> rather than at the position it was launched in, and it is not subject to the
> cancel grace a running tool attempt is, because there is no attempt body to
> interrupt.

Nothing about the routing moves. The wait is still resolved by the process
terminal through the work-driver seam
([ADR 0016](0016-process-waits-live-on-the-work-driver-seam.md)); on Restate it
is still resolved by attaching to the process;
`crates/lash-core-execution/src/runtime/effect/executor/process_local.rs` still
holds the equivalence it documents today, that "`await processes.await({ handle
})` answers exactly what `await handle`".

**Cancelling the wait releases the wait and never the process.** A losing
`processes.await` inside a `race` is cancelled at opener close like any other
unfinished arm, and the process it observed keeps running under its
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
lifecycle, with its own captured environment, journal, cancel protocol and
terminal delivery. This is the ruled answer to "I want work that outlives the
turn": name a process, and race the wait rather than the work.

The refusal of a **raw** handle at an element position is unchanged —
`crates/lashlang/src/runtime/vm/pending_tools.rs` keeps the repair "a process
handle cannot be awaited directly; call `processes.await(handle)` and await that
call, so the durable wait settles with the rest of the batch" — and that repair
is now literally what the group does.
