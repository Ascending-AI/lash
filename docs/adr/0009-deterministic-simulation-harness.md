# Deterministic Simulation Harness

## Status

accepted

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: lash-sim's SQL worlds and their
production-backed lease, fencing and reopen contention artifacts; lash-sim moves
onto the `lash-restate-test` runtime. Those passages stay as written until the
PR that deletes the code (FIG-3667, FIG-3668, or FIG-3600 for the session lease)
rewrites them.

## Decision

Lash's Deterministic Simulation Harness will be an unpublished `lash-sim`
workspace crate that composes the existing Runtime, Standard Protocol, RLM
Protocol, Agent, provider conformance, persistence conformance, and Durable
Fault Matrix contracts under a boundary-event scheduler. Provider simulation
will exercise real LLM Provider crates through a production-visible
provider-agnostic HTTP-ish transport seam in `lash-llm-transport`, using
Provider Wire Scripts instead of live LLM calls or a primary mock provider.
The v1 Simulated Worker Topology is bounded to in-process worker identities,
lease owners, incarnations, crash/restart/failover, and lease contention; it
models Lash durable effect boundaries, not Restate internals or physical
deployment behavior.

The accepted architecture is a true DST, not merely a deterministic
conformance/replay harness. The implementation is not done until the scheduler
drives at least two sessions' turns concurrently and resolves scripted provider
chunks plus final completions in seeded order; tool and durable-effect
completion paths pass through a real suspended turn; graph acyclicity, single
active Agent Frame, usage monotonicity, and Final Value semantics are executable
oracles; scenario-contract oracles have per-contract evidence or actual named
scenario runs; at least one real discovered regression has been minimized and
promoted to `crates/lash-sim/replays/`; and the generator's fast randomness,
provider mutations, queued-ingress modes, and worker failover are real.

## Why

The surprising part is that simulation belongs below the facade but above
provider/backend mocks: the harness must be deterministic and cheap enough for
randomized search, yet still use real provider serialization/parsing and real
Lash lease/effect semantics. A custom async executor, live provider tests, or a
new fifth scenario family would all create parallel contracts that drift from
the existing architecture.

## Substrate fault classes: what the harness models, and what it does not

The harness injects substrate faults at the real store transaction seam, behind
each backend crate's `testing` feature, through one neutral vocabulary
(`BackendFaultKind`, `BackendFaultPoint`, `BackendFaultArm`,
`BackendFaultObservation` in `crates/lash-sim/src/backend_fault.rs`), so one
scenario plan drives the SQLite injector
(`lash_sqlite_store::testing::SqliteFaultInjector`) and the PostgreSQL injector
(`lash_postgres_store::testing::PostgresFaultInjector`) alike. Two fault classes
from lash's own bug history are deliberately not modeled in the simulator; both
are covered by live behavioral tests instead, and this section is the written
decision.

**Clock skew is not modeled in the simulator.** The harness has exactly one
virtual clock (`crates/lash-sim/src/clock.rs`), while skew is by definition a
disagreement between two processes' clocks, so a simulated skew would only
re-time the harness, never the authority that decides a lease. The property that
matters — that lease and claim validity is decided by the store's own clock and
persisted row, never by a caller's supplied expiry — is covered directly against
a real database by `crates/lash-postgres-store/tests/postgres_clock_contract.rs`
(a client clock skewed ten years into the future: claims stay claimable and live
claims stay uncancellable, with a lint that keeps those paths off the client wall
clock) and by `crates/lash-postgres-store/tests/postgres_lease_multiconnection.rs`
lines 296-318, where a fenced host forges a decade-future expiry on its stale
lease and is still refused. Adding a second simulated clock would duplicate that
contract in a place that cannot fail for the real reason.

**Torn writes are modeled only at the commit boundary.** `CommitIo` is exactly
that fault: the substrate fails at the moment of commit, and the durable-prefix
and no-duplicate-effect oracles judge what survived. Sub-transaction tearing —
half of one transaction's rows reaching the disk — is not modeled, because both
supported backends are transactional and lash writes nothing durable outside a
transaction: every session commit, lease decision, claim, and checkpoint body
runs inside one `BEGIN`/`COMMIT`. A simulated partial-transaction write would
therefore assert against a state the storage engines forbid rather than against
a state lash can reach. If lash ever gains a non-transactional durable writer,
that writer brings this decision back.

**Slow-but-alive is modeled** (`crates/lash-sim/src/slow_alive.rs`): a
store-operation delay injector on the virtual clock advances time past the lease
TTL between a live worker's claim and its commit, and two oracles judge the
result — the lease-loss refusal fires
(`StoreError::SessionExecutionLeaseExpired`), and no partial write survives, the
durable prefix being exactly what a reopen reads while the refused operation
still publishes exactly once under fresh authority.

## Consequences

- `lash-sim` owns generation, scheduling, replay, model-store simulation,
  worker topology, and sim artifacts, but it does not become part of the
  publishable SDK surface.
- `lash-llm-transport` grows a production-visible injectable transport seam;
  provider crates continue to own vendor schemas and parsing.
- Existing scenarios and conformance suites remain authoritative. Shared
  simulation oracles are extracted narrowly instead of importing test modules
  wholesale.
- Confidence lanes extend `scripts/confidence-gate.sh`: fast aggregates
  first-class shards for scenario/property/fault-matrix evidence, a small
  generated simulation/provider corpus, minimizer fixtures, and performance
  guards; default adds local conformance, backend contention, coverage,
  targeted mutation, and a search-mode seed lane; broad adds bounded
  full-profile simulation, a deeper search lane, and static model replay
  evidence without claiming full confidence; full means broad semantics plus
  full critical-crate mutation, a sharded high-volume search fleet, and the
  true DST interleaving/effect/oracle/replay/generator criteria above.
- Broad confidence is honest bounded evidence. Full confidence is reserved for
  lanes that exercise the true DST criteria, including replayed promoted
  regressions rather than only generated coverage packages or negative fixtures.
