# E2E Scenario: Host-Provisioned PostgreSQL Schema

> **Read [../RULES.md](../RULES.md) first.** This runbook documents a deterministic,
> operator-only rehearsal — the counterpart to `version-bump-recreation`'s companion,
> not a judged browser journey. Its executable half is the
> `host_provisioned_rollout` integration test in `crates/lash-postgres-store`, run
> against a disposable PostgreSQL by `scripts/ci/with-service.sh`; the same leg CI's
> `postgres-store` job uses for every other store suite.

**Purpose.** Give an operator or CI job a supported way to prepare and check the Lash
PostgreSQL schema **outside** runtime startup, so a host that owns its migrations —
Figments running Goose against PostgreSQL while runtime connections go through
PgBouncer — never needs DDL privileges, or DDL at all, on the application pool.

**Deterministic companion.**

```sh
bash scripts/ci/with-service.sh pg16 -- \
  cargo test -p lash-internal-postgres-store --locked --test host_provisioned_rollout
```

`with-service.sh` owns the disposable instance: `postgres:16-alpine` on an ephemeral
loopback port, removed on exit, with `LASH_POSTGRES_DATABASE_URL` and
`LASH_REQUIRE_POSTGRES=1` exported. Without the wrapper the test skips itself; with
`LASH_REQUIRE_POSTGRES=1` and no URL it fails rather than silently passing.

**Released API and revision to adopt.** The surface shipped by
[FIG-888](https://linear.app/ascending-ai/issue/FIG-888) (PR #225, commit `d7a49fefd`),
first released on the `0.1.0-alpha` channel at **`v0.1.0-alpha.114`** (any tag whose
history contains `d7a49fefd`; `git describe` reports `v0.1.0-alpha.113-87-gd7a49fefd`
for the landing commit, so alpha.113 predates it). Figments' pinned revision
(`fa443796`) predates it and still runs DDL at open. Adopt, from
`lash-internal-postgres-store` (lib `lash_postgres_store`):

- `PostgresStorage::schema_ddl()` / the committed `crates/lash-postgres-store/schema.sql`
  — the byte-exact artifact host migration tooling applies. Copy it; never transcribe.
- `PostgresStorage::verify_schema_for(&PgPool) -> SchemaReport` — the read-only CI gate;
  `SchemaReport::is_conformant()` is the verdict, `Display` renders the per-object diff.
- `PostgresStoreConfig { schema_provisioning: SchemaProvisioning::HostProvisioned,
  schema_check: SchemaCheck::Enforce, .. }` + `PostgresStorage::from_pool_with` — the
  runtime open: no DDL, hard failure on drift or version mismatch.
- `PostgresStorage::schema_advisory_lock_key()` — the `(namespace, key)` for
  `pg_advisory_lock` host tooling takes around its own schema operations.

## Ownership and ordering

- **DDL is host-owned.** `schema.sql` is the artifact the host's migration tooling
  (Goose, in Figments) applies with a migration-privileged role. Lash never writes
  schema under `HostProvisioned`.
- **Seed data ships in the same artifact.** `schema.sql` ends with the required
  seeds: the `lash_schema_versions` component stamp, the `lash_process_change_clock`
  and `lash_turn_park_clock` singletons, and the `lash_catalog_identity` row. A
  schema that skipped them is *provisioned but incomplete*: open refuses naming
  `lash_catalog_identity` and `schema.sql`. Apply the artifact whole.
- **Ordering: migrate → verify → deploy.** Run the host migration to completion,
  gate on `verify_schema_for` conforming against the release being deployed, *then*
  roll the runtime. Verification is read-only and takes the published advisory lock
  in shared mode.
- **Concurrent schema operations.** Lash's advisory lock serializes only lash's own
  opens and verifications. Host migrations must either take
  `schema_advisory_lock_key()` themselves or simply never overlap a runtime open —
  migrate before ingress, not beside it.
- **Runtime role.** `CONNECT` on the database, `USAGE` on the lash schema,
  `SELECT/INSERT/UPDATE/DELETE` on its tables, `USAGE, SELECT` on its sequences. No
  `CREATE`, no ownership — under PgBouncer this is the only role the pool uses.

## Failure semantics — no auto-repair

Every refusal is terminal: lash reports and exits, it never recreates, patches, or
seeds what the host did not.

| Condition | Behavior |
|---|---|
| Version stamp ≠ this build's `SCHEMA_VERSION` | Open refuses, naming found and expected. Fatal under every `SchemaCheck`. |
| Structural drift (missing/extra/diverged objects) | `SchemaCheck::Enforce` refuses with a per-object diff naming the drifted objects. `WarnOnly` logs and opens — for diagnosis, never production. |
| Seed row missing (`lash_catalog_identity`) | `verify_schema_for` reports a `SEED ROWS` finding; open refuses naming the table and `schema.sql`. No `SchemaCheck` relaxes it. |
| Upgrading across a reject-and-recreate bump | Drop the schema lash owns (`DROP SCHEMA ... CASCADE`) or recreate the database, then re-apply this build's `schema.sql`. This build's `teardown.sql` names only this build's tables; an older catalog can hold tables it no longer declares (component 132 retired the effect engine's eight tables), and teardown leaves those behind. |
| `verify_schema_for` non-conformant | CI gate fails pre-deploy; the runtime never starts. |

After any refusal the database is exactly as the host left it — the companion asserts
the dropped table stays dropped and the missing seed stays missing.

## Scenario-specific golden rules

1. **The artifact is the contract.** Preparation is `schema.sql` applied verbatim
   through host tooling, end to end, seeds included.
2. **Verification precedes rollout** and uses the same structural check the open runs.
3. **The runtime role cannot run DDL** — not "is configured not to": the companion
   proves the privilege is absent before it opens.
4. **Refusal is the correct outcome** for missing, incomplete, or incompatible
   schemas, and it is non-destructive by construction.
5. **No second provisioning path.** Lash-managed open (`SchemaProvisioning::LashManaged`)
   remains for development; a host-provisioned deployment never mixes modes.

## Scorecard

| Item | Objective gate | Evidence |
|---|---|---|
| Prepare + verify | `verify_schema_for` reports conformant after applying `schema.sql` | `a_host_provisioned_schema_opens_under_a_role_without_ddl_privileges` |
| Runtime open, no DDL | open succeeds under a role whose `CREATE TABLE` is refused | same test |
| Incomplete schema refused | missing seed row refused by name; still absent after | `a_schema_missing_its_seed_row_is_refused_without_repair` |
| Incompatible schema refused | dropped `lash_processes` refused by name; still dropped after | `a_drifted_schema_is_refused_without_repair` |

**Aggregate:** would an operator following only this runbook get a conformant
database to production without ever granting the runtime DDL, and recognize a failed
gate as stop-and-fix rather than repair-and-retry?
