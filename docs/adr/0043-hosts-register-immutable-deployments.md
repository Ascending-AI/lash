# Hosts register immutable deployments

## What assumes it

A deployment's code remains immutable while any invocation may replay against
it. Replay validates the canonical journaled representation of an effect
against its hash. A mismatch can mean a runtime divergence or code changing
beneath an invocation; hash validation alone cannot distinguish the causes.
Model request content enters that representation by digest, not raw prompt
bytes.

A process also resolves its pinned module artifact for its lifetime. Pinning
the artifact cannot preserve the handler code around it if the deployment is
mutated.

## The obligation

A host registers a new deployment for new code and keeps the old deployment
available while work remains pinned to it. Registering, ordering and retiring
deployments are host policy. Lash supplies generation-qualified endpoint
paths, registration checks and drain reads; it does not decide the operator's
rollout schedule.

Each journal-bearing Restate service binds a stable name and a
build-generation name. Stable routing admits work to the current deployment;
generation routing keeps replay and pinned child work on a compatible build.
An invocation retains the endpoint where it starts. Different builds need
different registered endpoint URIs.

## Invocations that do not drain

A durable wait can retain a deployment indefinitely. ADR 0106 defines
generation handover and format upgrade, and ADR 0115 defines the operator's
upgrade protocol. A host starts drain from the replacing build, observes the
retained generation obligations and retires the old deployment only when the
required work drains. Parked work needs an explicit operator decision.

Version markers and mixed-version branches inside process code are rejected
as a second evolution mechanism with a permanent deprecation burden. The
current upgrade contract keeps evolution at the deployment and versioned-state
boundaries.

## The process handler's journal prefix

The process handler journals generation and admission facts before effects.
A read-only verdict records a nonce; a separate start command writes the
set-if-absent marker. Only the resulting `SegmentStarted` proof permits the
segment to execute (ADR 0045).

`RESTATE_PROCESS_JOURNAL_VERSION` and `JOURNAL_LOGIC_EPOCH` participate in the
build's drain generation. A generation sentinel precedes replay-sensitive
commands. Generation routing and supported successor windows govern handover;
a blanket rejection of all successor segments is not the upgrade policy.

## What is not covered

Restate object state is outside an invocation journal. Its versioned metadata
has its own decoder and upgrade obligations. Immutable deployment code does
not make that state or a handover artifact compatible with a new build.
ADR 0106 owns the per-format migration or drain decision.

## Consequences

Mutating a deployment can produce replay mismatch without enough evidence to
attribute it to a runtime defect. The mismatch remains a refusal, not a
permission to tolerate changed journals. An effect envelope carries no generic
producer-build fingerprint; the deployment and generation mechanisms do not
turn that absence into attribution.

[Generation drain reads](../../crates/lash/src/core.rs) expose retained work,
while retirement remains the host's decision. A host keeps the old deployment
registered until its required obligations drain.

## Implementation

- [Canonical effect validation](../../crates/lash-core-execution/src/runtime/effect/validation.rs).
- [Deployment routes and registration](../../crates/lash-restate/src/engine.rs).
- [Process journal prefix and admission](../../crates/lash-restate/src/process/admission.rs).
