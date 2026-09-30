# 0104: Restate is the only effect engine; SQL stores are storage

## Status

Accepted.

## Context

Durable execution needs replay, timers, keyed waits, group settlement,
cancellation and recovery. Implementing those semantics in each SQL store
also makes SQL tests certify an engine different from the production engine.
Lash delegates execution to Restate and keeps its SQL stores responsible for
storage. The kernel expresses the obligations as engine-neutral contracts.

## Decision

### 1. One effect engine, and SQL stores hold storage only

`RestateEngine` is the shipping implementation of `EffectEngine`. A backend
combines it with a SQLite or PostgreSQL `StoreSet`. SQLite memory is storage,
with the lifetime defined by [ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md)
D3, and supplies no local durable engine.

The store set holds session commits and history, attachment manifests and byte
storage, module artifacts, process environments, definitions, records and
continuations, triggers, ingress and queued-work rows, parked work, and
store-to-engine delivery obligations. PostgreSQL takes an attachment-byte
backend at construction. Session drive fences and head compare-and-set checks
protect store mutations under
[ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md). They are
not scheduling or replay engines.

Restate owns session execution through `LashSession` and `LashTurn`, process
segments through `LashProcessWorkflow`, and delivery through the obligation
relay of [ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md).
SQL stores implement the data transitions those handlers require.

Evidence: `crates/lash-core-execution/src/backend.rs:248`,
`crates/lash-restate/src/engine.rs:113`,
`crates/lash-restate/src/engine.rs:418`,
`crates/lash-restate/src/session_driver.rs:1`,
`crates/lash-postgres-store/src/postgres/backend.rs:44`.

### 2. The effect interface is engine-neutral

The B2 construction is one engine over one store set:

```rust
RestateEngine::new(stores: Arc<dyn StoreSet>, config: RestateConfig) -> RestateEngine;
Backend::new(engine: Arc<dyn EffectEngine>) -> Backend;
EffectEngine::stores(&self) -> Arc<dyn StoreSet>;
EffectEngine::process_work(&self) -> ProcessWorkWiring;
EffectEngine::session_work(&self) -> Arc<dyn SessionWorkEngine>;
StoreSet::binding_identity(&self) -> &StoreBindingId;
StoreSet::module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;
```

`Backend` has one private engine field, and every port derives from that
binding. The engine holds its stores rather than taking another store set on
an operation. `RestateConfig` carries ingress and admin connections, effect
authority, build generation and namespace. A serving application calls
`endpoint_builder` with its process worker; a submitting application does not
bind handlers. Construction itself returns the engine directly.

Storage identity and effect authority are distinct. The engine fixes them
together, and the runtime does not compare storage identity with effect
authority as an admission check. Module artifacts belong to `StoreSet`; the
byte-level port lives in `lash-core-execution`, and the language integration
owns its codec. A plugin factory bound to another backend is refused.

The drive uses `RuntimeEffectController` through a scoped controller. The
engine records effects, admission, waits and cancel races. Commit and park are
fenced, idempotent store writes with receipt validation, under ADR 0105 §9.
A future engine implements these contracts and runs their conformance laws.

Engine-specific contexts, invocation identities, errors and formats belong to
the engine crate. Kernel contracts remain engine-neutral. The facade re-exports
an optional engine integration so the host can select it. The engine declares
its durable formats, and the facade includes those declarations in its format
manifest. `scripts/check-substrate-boundary.sh` checks the kernel boundary.

Evidence: `crates/lash-core-execution/src/backend.rs:43`,
`crates/lash-core-execution/src/backend.rs:89`,
`crates/lash-restate/src/engine.rs:26`,
`crates/lash-restate/src/engine.rs:199`,
`crates/lash-core-execution/src/module_artifacts.rs:1`,
`crates/lash/src/plugin_binding.rs:1`,
`crates/lash/src/formats.rs:580`,
`scripts/check-substrate-boundary.sh:241`.

### 3. Engine obligations are contracts

| Obligation | Required outcome | Restate implementation |
|---|---|---|
| Per-session serialized execution, O1 | One authorized logical drive; stale store mutations refuse. External operations retain stable idempotency identities because a fence cannot retract a request already sent. | Session virtual object, root workflow and sealed drive fence. |
| Durable acceptance and scheduling, O2 | Persist acceptance and delivery intent, acknowledge submission separately, and reconcile unacknowledged obligations. Status reads do not own recovery. | SQL acceptance and obligation ledger, with the engine relay. |
| Durable step result | Replay returns the recorded envelope's outcome without fresh body dispatch. | Named `ctx.run` entries and canonical-envelope validation. |
| Durable timer | Retain the deadline and resume the wait through replay. | Journaled timers and durable-wait deadlines. |
| Durable keyed promise | A neutral key names a first-writer-wins resolution that survives resolve-before-wait and supports revocation. | Durable-wait workflow and `LashDurableWaitIndex` service. |
| Effect groups | Retain membership, ranked settlement, protected drain and cancellation admission fences under ADRs 0065 and 0099. | `EffectGroupIndex`, `EffectGroupPayload` and `EffectGroupDispatch` services. |
| Retry and park, O3 | Preserve replay history, retry live faults under engine policy, and prevent fresh semantic admission while a root is parked. | Invocation retry policy, durable parks and reconciliation through owner mappings. |
| Redrive, cancellation and recovery, O4 | Redrive preserves outcomes; a durable cancel cooperates with Lash closure. Engine kill alone proves no Lash terminal. Reset does not substitute for redrive. | Turn gate promises, process cancellation and group-child cancellation. |
| Replay validation | Same-generation replay preserves recorded commands and keys; a mismatch refuses fresh dispatch. | Positional SDK replay, envelope checks and generation sentinels. |
| Process identity across runs, O5 | Carry logical identity, continuation and obligations across bounded segments. | Process workflow, segment handovers and journal budget. |
| Admission before the first effect, O6 | Preserve the journaled start proof and refuse a fresh execution whose retained start history is lost. | Root start nonce and drive seal; process admission and start marker. |
| Process execution and terminal wait | Run submitted segments, recover from retained history, and await the process terminal through `ProcessWorkSubstrate`. | Process workflow and process-attach workflow. |

These are observable outcomes. The table's last column describes Restate's
mechanisms rather than adding them to the kernel contract. Engine-neutral laws
live in `lash-conformance`; protocol and invoker-specific tests live in
`lash-restate`. Parking and segment boundaries are not logical terminal
evidence, and cannot close a lifetime scope by themselves. Tool draining and
scope closure remain Lash obligations.

Evidence: `crates/lash-core-execution/src/backend.rs:43`,
`crates/lash-core-execution/src/engine/control.rs:122`,
`crates/lash-restate/src/controller/journaled_effect.rs:314`,
`crates/lash-restate/src/controller/context.rs:1`,
`crates/lash-restate/src/durable_wait.rs:1`,
`crates/lash-restate/src/effect_group/dispatch.rs:1`,
`crates/lash-restate/src/process/admission.rs:1`,
`crates/lash-core-store/src/store/drive_fence.rs:185`,
`crates/lash-conformance/src/macros.rs:1`.

### 4. Zero-infra is a local Restate server

A durable local application runs `restate-server` beside a SQLite file store
set. The examples' local runner starts the server, constructs the engine and
registers its endpoint. The in-process `lash-restate-test` server double is
for tests and supplies no shipping restart durability. Lash offers no second
embedded durable engine.

One server can serve several deployment namespaces. Each engine binds and
calls its namespace's names. Registration refuses another authority's claimed
names and an endpoint URI serving another generation. These checks are
separate admin calls, so registration is not an atomic exclusion protocol.
The host owns deployment ordering and retirement under ADR 0111.

Evidence: `examples/shared/local_restate.rs:1`,
`crates/lash-restate-test/src/lib.rs:1`,
`crates/lash-restate/src/engine.rs:263`,
`crates/lash-restate/src/services.rs:18`.

### 5. Testing model

Storage laws run against SQLite file, SQLite memory and PostgreSQL. Execution
hosts are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. `lash-restate-test` drives the real endpoint and SDK
VM, retains invocation journals, injects crashes and supports virtual time
and always-replay. Its backend constructor wires the real engine over a store
set. Lash-sim's `SimEngine` uses that backend over SQLite memory storage.

Live-server suites cover real invoker, network, retry, suspension and restart
behavior. A controller law uses the backend and controller ports; a test that
inspects the double's journal or clock is a test of that implementation.
Random chaos and virtual time exercise durability. Deterministic Tokio
scheduling is not a requirement. Upgrade proofs use the synthetic-next tier
under ADR 0106 §6.

Evidence: `crates/lash-restate-test/src/lib.rs:1`,
`crates/lash-restate-test/src/backend.rs:82`,
`crates/lash-sim/src/backend.rs:35`,
`crates/lash-restate/src/tests/conformance_and_poison.rs:1`,
`crates/lash-upgrade-harness/tests/phase_a/main.rs:1`.

## Rejected alternatives

- A Lash-owned SQLite engine or one SQL engine behind dialect hooks still
  duplicates the production engine's durable execution obligations.
- A shipping host around the shared-core VM must also implement journal
  persistence, timers, dispatch, serialization and recovery. The VM alone
  does not supply those server responsibilities.
- An embedded Restate node adds another packaging and lifecycle contract.
  The current integration reaches Restate through its connections and endpoint.
- Splitting an engine into independently assembled persistence ports weakens
  the backend binding of ADR 0102 D2.

## Consequences

The SQL stores concentrate on storage, and the production engine owns replay
and scheduling. A durable local application runs a server. A second engine
needs its own implementation of the neutral contracts and their laws. The
server double can differ from the live invoker, so live-server coverage remains
necessary even when a controller law passes on the double.
