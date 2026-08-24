# Hosts register immutable deployments

Lash's durability story on a journaling host rests on an obligation the host
must meet and that nothing, until now, wrote down: **a deployment's code is
immutable for as long as any invocation may replay against it.**

Three independent properties assume it, and each degrades silently if it does
not hold.

## What assumes it

**Whole-envelope hashing.** A journaled effect records the hash of its full
envelope, and replay rejects a mismatch (`validate_recorded_effect_envelope`,
`crates/lash-restate/src/controller/mod.rs`, delegating to
`validate_replayed_effect_envelope`,
`crates/lash-core/src/runtime/effect/validation.rs`). This detects lash's own
nondeterminism bugs, which is why the hash covers content rather than an
identity tuple. It cannot distinguish "the runtime computed a different
envelope" from "the code that computed it changed underneath the invocation".
Mutating a deployment converts a correctness detector into a false alarm, and
worse, trains readers to dismiss it.

**The rejection of version markers.** Temporal-style `patched()` gates are
deliberately absent because they have no job here: an in-flight invocation
completes against the code it started on, and an invocation that cannot drain
is forked forward (below) instead of patched in place. An earlier revision of
this record also called markers *unusable* on positional-correlation grounds;
that argument was wrong and is retired (2026-08-14). Temporal replays safely
past inserted version markers by exempting them from its positional
determinism diff, and DBOS's patch probe consumes no journal position on
pre-patch history. The mechanism is implementable; it is rejected as a second
evolution mechanism whose price — version-conditional branches living in
process code with their own deprecation discipline — buys nothing the fork
does not already provide.

**Content-addressed module pinning.** A lashlang process resolves its pinned
`module_ref` for its whole life (`crates/lash-lashlang-runtime/src/process.rs`).
This gives lash Temporal's build-id behaviour for free — old runs finish on old
artifacts. Pinning the module while mutating the deployment around it pins the
wrong half.

## The obligation

A host must register a new deployment rather than replace the code behind an
existing one. Evolution is pin-and-drain: publish the new deployment, let
in-flight invocations finish against the old one, retire it when drained.

Restate does not enforce this, and an earlier revision of this record claimed
its deployments were immutable by design; that reading was wrong and is
retired (2026-08-14). Restate's default deployment update repoints the address
behind an existing deployment id, its journal-mismatch remedy (RT0016)
sanctions fixing a deployment's code in place, and graceful deployment
deletion is unimplemented — deletion is force-only, with no server-side drain
gate. The obligation is therefore lash discipline that hosts uphold against a
permissive substrate, for example by never redeploying to a fixed identifier
in a container platform that treats the image tag as mutable.

## Invocations that do not drain

Pin-and-drain assumes every deployment eventually empties. A process parked on
a durable wait can pin its deployment indefinitely, and a fix for code it will
run after waking cannot reach it by drain. The accepted evolution primitive
for this case is the journal-prefix fork (ruled 2026-08-14): copy an
invocation's validated journal prefix into a new invocation pinned to the
successor deployment, re-arm its durable waits under the new invocation's
identity, and mark the original forked-from and terminal. The fork point must
precede the divergent code region, and the existing envelope validation is the
checker. Fork is a first-class, predicate-targeted primitive: remediating a
bug across many parked invocations is a bulk fork driven by a drain query, not
a per-invocation operator ritual.

Version markers, a monotone logic-version gate, and mixed-version
co-residency were examined against reference implementations for this case
and rejected: each embeds version-conditional control flow in process code
permanently to serve an occasional event, while fork keeps journals
version-free and evolution at the deployment boundary.

## What is not covered

Restate object state is not part of a replayed journal and does survive a
deployment upgrade. The versioned-metadata miss is the compatibility boundary
there, and it is handled explicitly (`load_durable_wait_index_metadata`,
`crates/lash-restate/src/durable_wait.rs`). That mechanism is unaffected by
this decision; state migration and code immutability are separate concerns.

One change class cannot ride pin-and-drain at all: bumping the durable-wait
index identity epoch invalidates shared index state that draining invocations
still await, so an epoch bump is a stop-the-world cutover — drain, recreate,
then open — never an overlap.

Identity epoch 4 is such a cutover. Durable-wait requests and indexed state now
carry the `AwaitEventKey` preimage so handlers derive addresses locally; epoch-3
state has neither that request shape nor that indexed value. Operators must
drain and recreate both `LashDurableWaitIndex` and `LashDurableWaitWorkflow`
state before opening the epoch-4 deployment. There is no tolerant decoder,
address migration, or dual-deployment window.

Segment handover crosses invocations, so a successor segment is routed to the
latest deployment. Immutability of any single deployment does not make
handover artifacts self-describing, which is why they carry their own format
version rather than leaning on this obligation.

## Consequences

Hosts that mutate deployments will see replay hash mismatches that look like
lash nondeterminism bugs but are not, and will lose the guarantee that a
started invocation completes against consistent code.

Lash cannot detect the violation and does not pretend to: a runtime cannot
distinguish its own nondeterminism from code changing beneath it. Attribution
short of detection is possible and is accepted design (ruled 2026-08-09,
diagnosis only): journaled effect envelopes carry a producer build
fingerprint, so a divergence message names which build wrote the journal and
which is replaying it — same build means a lash bug, different build means
this obligation was violated — as the lashlang program hash already does one
layer down. The stamp never gates replay; tolerance of mismatched journals
stays rejected.

Lash does not decide deployment routing or retirement, but it now exposes the
authoritative `LashCore::drain_status(accepting_new_work)` read. After the host
closes admission, the read counts every retained non-terminal process row,
including waiting/suspended and retrying work, and returns `drained` only when
that count is zero. A host must keep the old deployment registered while the
read is not drained; the read supplies verification, while deployment
discipline and retirement timing remain host policy.

Written after three independent reviews of separate subsystems each discovered
this assumption and none found it stated. Amended 2026-08-14 after a
reference-benchmarking round corrected the Restate characterization and the
marker argument, and recorded the fork clause.
