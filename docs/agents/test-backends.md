# Test backends and hosts

This is the current matrix for writing a law or its acceptance criteria.
Storage laws and engine laws certify different contracts. A law runs where its
registration macro is invoked, rather than on every tier by implication.
The registration bodies in `crates/lash-conformance/src/macros.rs`,
`effect_host_macros.rs` and `macros/` define each family. The concrete suite
files below select its laws and supply its fixtures. Check those invocations
when naming the gates for a new law.

`python3 scripts/check_conformance_mounts.py` follows Cargo targets, Rust
modules, test bodies and invoked registration macros to find public
conformance functions with no executable consumer. Dormant macros and imports
do not count as mounts. Its mutation witnesses run alongside it in CI's
repository gates. Performance helpers consumed by executable targets remain
reachable without claiming a backend law ran.

## Store laws

| Store | Registration and target | Gate or recipe |
| --- | --- | --- |
| SQLite file | `crates/lash-sqlite-store/tests/conformance.rs` loads `conformance/suite.rs`; `//crates/lash-sqlite-store:conformance__test` | `kiln test //crates/lash-sqlite-store:conformance__test`. The cacheable CI workspace partition includes it. File-only schema, WAL and cross-process witnesses stay in this binary. |
| SQLite memory | `crates/lash-sqlite-store/tests/conformance_memory.rs` loads the same `conformance/suite.rs`; `//crates/lash-sqlite-store:conformance_memory__test` | `kiln test //crates/lash-sqlite-store:conformance_memory__test`. The cacheable CI workspace partition includes it. This is a named SQLite memdb store set, with SQL transactions and constraints. |
| PostgreSQL | `crates/lash-postgres-store/tests/conformance.rs`; `//crates/lash-postgres-store:conformance__test` and its service shards | CI's `postgres-store` job runs `scripts/ci/with-service.sh pg -- scripts/ci/store-tests.sh pg-store`. It executes the package's service binaries, including integration, schema and atomicity suites, with an explicit PostgreSQL URL and ignored-test selection. Locally run `kiln gate lash <fork> -- env BAZEL_TRUSTED=true scripts/ci/with-service.sh pg -- scripts/ci/store-tests.sh pg-store`. |

PostgreSQL 17 and 18 are supported for lash 1.0; 18 is primary. Development
and CI run 18 alone: the hermetic server (`native//:postgres`), the `pg`
service of `with-service.sh` and every CI pin name it. The release gate also
runs on 17: the release workflow's `release-postgres` job and
`scripts/release-rehearsal.sh` run `store-tests.sh pg-release` (the store's
unit, conformance, schema-shape and version-stamp suites, the durable crash
proofs, the crash matrix, the failover cases and the cross-backend
differential) under both `with-service.sh pg` and `with-service.sh pg17`.
`schema-shape.txt` is rendered on 18, and 17 must match it. Nothing older than
17 is supported. The commands and test selection live in
[store-tests.sh](../../scripts/ci/store-tests.sh) and the
[CI workflow](../../.github/workflows/ci.yml). Service tests need their require
flags and a live service; a skipped service test is not law execution evidence.

An explicitly selected PostgreSQL variant must fail without a non-empty
`LASH_POSTGRES_DATABASE_URL`. Fixtures use
`lash_postgres_store::testing::required_database_url()` before opening the
service. Mark service variants `#[ignore]` so ordinary runs report them as
ignored, and select them with `--include-ignored` inside the service gate.
`pg-store` also executes the facade's `llm_profiles` laws, including their
PostgreSQL and always-replay variants. Use `pg-model-keys` for that focused
suite with the same wrapper.

The facade's remaining PostgreSQL-only laws -- ignored tests spread across
`//crates/lash:lash__unit_test` and the integration binaries -- run in the
same job as `pg-facade-laws`, which selects them by name: a law a `pg`
container alone satisfies carries `postgres` in its libtest path. A law that
also needs a managed service is named and skipped there;
its own suite owns it.
`scripts/check_postgres_gate_coverage.py` fails if a PostgreSQL-gated ignored
test lands in a binary no Postgres suite runs, or is named so the filter
misses it.

`just store-contract-soak` increases the property-case budgets on SQLite
memory, SQLite file and PostgreSQL. Its PostgreSQL leg still requires a
service. The storage differential is
`//crates/lash-sim:cross_backend_store_differential__test`;
`just cross-backend-store-soak` runs its generated law with PostgreSQL required
(`scripts/ci/with-service.sh pg -- just cross-backend-store-soak`). The
generated law compares SQLite file and PostgreSQL storage rows, not engine
journal semantics. These are targeted soak recipes, not extra default gates.

## Engines and effect hosts

| Host | Code and targets | Gate or recipe |
| --- | --- | --- |
| lash-sim in-process recording effect host | The storage differential uses `lash_conformance::recording_backend_over` and `RecordingEffectHost` for lifecycle authority over its SQL stores. This host records fixture outcomes; it does not certify a production durable engine. Full simulated turns use `SimEngine` in `crates/lash-sim/src/backend.rs`, which builds lash's durable engine over SQLite memory. | `kiln test //crates/lash-sim:cross_backend_store_differential__test` for the non-service differential laws and the cacheable lash-sim test targets for simulated histories. |

`scripts/confidence-gate.sh` composes these existing targets and
service recipes into optional confidence lanes; its lanes do not create
another backend.

## Synthetic-next upgrade tier

`synthetic-next` builds N and N+1 from the same source with different advertised
read/write ranges. It is an upgrade configuration over SQLite and PostgreSQL,
not a new store. Normal feature-lane checks prove compile/test availability;
they do not replace a live upgrade proof.

## Retired implementations and mechanisms

Each row names the deletion commit, not merely the ADR that proposed it.

| Retired item | Retirement evidence and replacement |
| --- | --- |
| In-memory store conformance legs | Retired in `dd80a5e7cc`, which removed their registrations and moved unique engine laws to the engine's test host. The Rust stores were then retired in `60e0e86b2a`. Store laws now register on SQLite file/memory and PostgreSQL. |
| Rust in-memory stores, `lash-core-memory`, local process registry, native effect host and store-delegated turn control | Retired in `60e0e86b2a`. The store set is SQL storage; the engine owns turn execution and replay. Process observation buffers and recording fixtures still exist, but do not constitute that retired persistence tier. |
| In-memory Lash VM artifact store on the facade | Retired in `7f11e493a7` from the facade in favor of the backend's artifact store. Remaining Rust stores were retired in `60e0e86b2a`. |
| PostgreSQL effect engine | Retired in `4f03596847`. PostgreSQL keeps storage, with no engine journal tables or await-event engine. |
| SQLite effect engine and store-journal turn host | Retired in `476264fbea`. SQLite file and memory remain storage. |
| SQL session-execution leases | Retired in `1ea8fcff75`. Writes use the sealed shift fence and session-head compare-and-set. |
| Process leases and native recovery sweeps | Retired in `33e7ebc44d`. The engine owns execution and lost-run reconciliation; the process registry retains lifecycle and events; product notice delivery belongs to the host under ADR 0137. |
| Turn-input and queued-work claim tokens, `queued_runs` ledger | Retired in `671a616419`. Admission binds inputs and work; run identity keys settlement. |
| Await-event signing material | Retired in `7ab7707a8f`. Wait keys are identities; a host owns any authorization policy. |
| Runtime-owned tool-intent admission | Retired in `0417b7f48b`. The engine realizes intents and the submission ledger retains their binding and outcome. |
| The external journaling engine, its server double and its live suites | Deleted in `5ad9641f9e` (FIG-5190). Lash's durable engine replaces it ([ADR 0132](../adr/0132-durability-is-state-first-over-the-lash-store.md)); each test law that went with it has a row in `docs/testing/substrate-port-ledger.toml`. |
| Local rewind after worker loss | Retired in `9c1bbd2189`. The parent settles the admitted operation and fails retryable so the execution substrate redrives from the checkpoint. Parking a live VM is a different, surviving operation. |
| Runtime-operation effect journals outside turn/session scope | Retired in `caa1f7efe3`. Engine-owned workflows/objects carry durable waits and operation scopes. |
| Orchestrating tools and their registry lane | Retired in `501f323f61`. Tools are opaque providers; a Pending result can declare a child start. |
| Durable stopped partial and turn capture store | Retired in `dddace81f0`, closing the deletion arc through `65102e4944`, `1ca6b375f2`, `a9aa9071ee` and `54fa765274`. A stopped turn's uncommitted tail exists only on the live stream. |
| Universal exclusively-owned copies at durable stores | Retired in `c51e616528`. ECMA stores preserve shared references; durable capture uses forest encoding where possible and otherwise validated shared-graph encoding. Runtime roots remain authoritative and host views remain projections. |
| Retryable failed intent state on control intents | Retired in `02ffbca4fd`. `ControlIntentState` is `Pending`, `Acknowledged`, `Superseded` or `Refused { cause }`; a retryable failure lives on the obligation alone and writes nothing on the intent. |

## Keeping this document current

`python3 scripts/check_retired_terms.py` checks tracked and new documentation
and source comments, plus backend and host identifiers. `python3 scripts/test_check_retired_terms.py` proves that a
planted current statement fails, that its corrected matrix passes, and that
historical markers cannot leak into current sections or exempt ADR text. Both commands belong to
CI's repository-gates heredoc, which
`scripts/ci/repository-gates.sh` also extracts for local runs.

ADRs describe the current design in present tense; their history lives in git.
They have no historical paragraph or section exemption.

For a historical paragraph outside `docs/adr`, write `Retired in <commit>` using the matching
commit in `scripts/check_retired_terms.py`. For an entire historical section,
put `Historical` and the same marker in its heading. The exception ends at the
next heading of equal or lower depth. A status elsewhere in the file
is insufficient. SQLite memory references must name SQLite explicitly.
Historical filenames can remain. Code identifiers must name the current backend
or host, even when a historical marker appears nearby. The identifier gate
allows genuine memory-only replay buffers, store-double ledgers, artifact
reference models, upstream memory APIs and the retirement checks themselves.
