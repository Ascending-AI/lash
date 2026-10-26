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
| PostgreSQL | `crates/lash-postgres-store/tests/conformance.rs`; `//crates/lash-postgres-store:conformance__test` and its service shards | CI's `postgres-store` job runs `scripts/ci/with-service.sh pg -- scripts/ci/store-tests.sh pg-store`. It executes the package's service binaries, including integration, schema and atomicity suites, with an explicit PostgreSQL URL. Locally run `kiln gate lash <fork> -- env BUCK2_TRUSTED=true scripts/ci/with-service.sh pg -- scripts/ci/store-tests.sh pg-store`. Logic services disable flushes. The `pg-pool-wait` suite exercises performance entry points and requires `with-service.sh --durable-postgres pg`, which enables fsync, synchronous WAL commit and full-page writes; it refuses weaker settings before test execution. |

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

`examples/integrator-contract/tests/conformance.rs` demonstrates an unpublished
external `StoreSet` implementation importing the complete persistence and
process-registry catalogues, catalog/read/append/retention laws, and attachment
reopen suite. Its `//examples/integrator-contract:conformance__test` target uses
SQLite memory. See the example's [certification instructions](../../examples/integrator-contract/README.md)
for fresh-fixture, reopen and test-hook requirements. This certifies storage
ports; it adds no engine or backend tier.

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
