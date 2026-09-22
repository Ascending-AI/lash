# Runtime Identity Verification

> **Read [../RULES.md](../RULES.md) first.** This procedure is deterministic and must run in
> an isolated warm workspace. It does not authorize editing or resetting an existing store.

**Purpose.** Verify that admitted effects use their actual execution scope and replay key,
that session and turn attribution contains only known runtime facts, and that complete causal
identity survives native, SQLite, PostgreSQL, Restate, and remote projections.

## Contract

An admitted effect is identified by one `EffectAddress`:

```text
EffectAddress { execution_scope, replay_key }
```

`effect_id` is a descriptive label. Changing it cannot change deferred-link equality or the
journal address. Session, turn, turn index, and protocol iteration are attribution; they do not
own an effect and may be absent independently. In particular, a host binding, trigger owner
namespace, process id, or ambient current-session capability must never be copied into a session
attribution field.

The trace-parent order is:

1. an explicit parent already present in the base trace context;
2. the invocation's full causal reference;
3. a fallback derived from a real attributed turn;
4. no parent when none of those facts exists.

A known trigger cause includes occurrence id, subscription id, subscription incarnation, and
subscription revision. Partial causes remain partial; the verifier must not fabricate missing
fields.

## Safety and stop conditions

1. Work in the owned Kiln fork. Use the exact Bazel label for each deterministic
   test. `kiln test` executes these on the shared pool; the PostgreSQL gate below
   keeps its service-owned Cargo recipe.
1b. **Every named test filter in this runbook must be scored on its executed count, never on
   its exit code.** A filter that matches nothing does not prove the named test ran. Require a
   non-zero passed count for each filter — for example by guarding on
   `test result: ok. <n> passed` — and treat `0 passed` as a failure of the runbook, not a
   pass of the code.
2. Use temporary SQLite files. For PostgreSQL, use only a database created for the current Kiln
   gate and named by `LASH_POSTGRES_DATABASE_URL`.
3. Do not run schema probes against a user or shared database. Do not change schema stamps to
   make an incompatible store open.
4. Stop on any local/controller/store call after a wrong-scope refusal, any invented session id,
   any missing known trigger field, or any difference between the core and Restate trace parent.

## Phase 1 — Representation and admission

From the repository root:

```sh
. ./env.sh
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=scoped_controller_refuses_wrong_scope_before_controller_or_local_execution
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=task_proxy_refuses_wrong_scope_before_handoff
kiln test --test_output=all //crates/lash-restate:lash-restate__unit_test --test_arg=restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution
kiln test --test_output=all //crates/lash-sansio:lash-sansio__unit_test --test_arg=same_replay_key_in_distinct_scopes_has_distinct_graph_identity
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=effect_header_round_trips_without_universal_subject_or_replay_slots
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=legacy_universal_effect_header_is_refused
```

The first three tests must refuse before the wrapped controller, local executor, task handoff,
or Restate `scope_effect_begin`. The graph test must produce distinct addresses for identical
local replay keys in different scopes. The header tests must show a required `address` and no
universal `subject` or optional duplicate `replay` slot, while refusing the old shape.

Run the durable host contracts against fresh storage. The SQLite effect-host scope
conformance is the shared `effect_controller_` conformance family instantiated for the SQLite
backend (`crates/lash-sqlite-store/tests/conformance.rs`), not a test of its own:

```sh
kiln test --test_output=all //crates/lash-sqlite-store:conformance__test --test_arg=effect_controller_ --test_arg=--nocapture
```

(The former filter `sqlite_effect_host_satisfies_scope_conformance` names no test and matched
nothing; it passed vacuously.) The Restate refusal
(`restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution`) is already
run above in this phase — running it twice adds no coverage and inflates the phase count.

For an owned PostgreSQL gate, let the repository's service owner provide the database rather
than hand-rolling a container: `scripts/ci/with-service.sh pg16 -- <cmd>` binds an ephemeral
loopback port, exports `LASH_POSTGRES_DATABASE_URL` into the command, and labels the container
so the gate's leftover-refusal can see it. Require the test to run rather than print its
`LASH_POSTGRES_DATABASE_URL is not set` skip message:

```sh
scripts/ci/with-service.sh pg16 -- \
  cargo test -p lash-internal-postgres-store --locked --test conformance effect_controller_ -- --nocapture
```

## Phase 2 — Truthful attribution

```sh
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=parentless_effect_envelopes_use_process_originator_not_ambient_session
kiln test --test_output=all //crates/lash-lashlang-runtime:lash-lashlang-runtime__unit_test --test_arg=process_trace_session_attribution_comes_only_from_a_session_originator
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=session_node_identity_is_structural_and_missing_identity_is_refused
```

Require foreground session effects to retain their real session. A session-origin process must
carry the origin session. A host-origin process must remain sessionless in both its effect header
and Lashlang graph identity even when the execution service has an ambient session capability.
A session-node causal fact must carry its own structural session id and refuse the former shape
that omitted it.

## Phase 3 — Cause and shared trace projection

```sh
kiln test --test_output=all //crates/lash-remote-protocol:lash-remote-protocol__unit_test --test_arg=remote_cause_validation_preserves_partial_trigger_identity_and_checks_effect_scope
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=turn_keeps_causal_parent_when_present
```

(`same_replay_key_in_distinct_scopes_has_distinct_graph_identity` belongs to Phase 1 and is
run there; repeating it here under a second rationale inflates the phase count without adding
a witness.)

Inspect failures as identity failures. Do not accept matching display labels as proof. The
remote round trip must retain every known trigger field, and both core and Restate projections
must use the same scoped causal graph address while retaining unrelated base trace metadata.

## Phase 4 — Remote grammar and cutover

```sh
kiln test --test_output=all //crates/lash-remote-protocol:lash-remote-protocol__unit_test --test_arg=remote_owner_scope_validation_matches_each_core_owner_grammar
kiln test --test_output=all //crates/lash-sansio:lash-sansio__unit_test --test_arg=journal_identity_v2_bytes_remain_unchanged_for_all_scope_variants
kiln test --test_output=all //crates/lash-core-execution:lash-core-execution__unit_test --test_arg=direct_effect_identity_golden_corpus
kiln test --test_output=all //crates/lash-trace:schema__test --test_arg=trace_schema_version_is_pinned_at_
```

The pin test is named for the version it pins, so it is renamed at every bump. Filter on the
stable prefix above and score it on its executed count; the fully-spelled
`trace_schema_version_is_pinned_at_20` was left behind by a bump and matched nothing.

Session owner keys use the existing raw nonempty/no-NUL opaque grammar, so a whitespace-only
session key remains valid. Host owner keys use the existing trimmed nonempty/no-NUL check. The
five execution-scope journal encodings remain byte-for-byte version 2.

These surfaces move together at a cutover. **This runbook does not quote their values**: it
is re-run every drive while the constants bump independently, and a table of numbers here is
stale the moment one of them moves. An operator who provisions from a frozen table provisions
wrong. Read the value from the constant:

| Surface | Constant | Read it at |
| --- | --- | --- |
| Trace schema | `TRACE_SCHEMA_VERSION` | `crates/lash-trace/src/lib.rs` |
| Remote protocol | `REMOTE_PROTOCOL_VERSION` | `crates/lash-remote-protocol/src/lib.rs` |
| PostgreSQL component | `SCHEMA_VERSION` | `crates/lash-postgres-store/src/lib.rs` |
| SQLite durable core | `SCHEMA_VERSION` | `crates/lash-sqlite-store/src/schema.rs` |
| RLM snapshot | `RLM_SNAPSHOT_VERSION` | `crates/lash-protocol-rlm/src/executor/snapshot.rs` |
| Process wake-delivery format | `PROCESS_WAKE_DELIVERY_FORMAT_VERSION` | `crates/lash-core/src/runtime/process/events.rs` |
| Process registration family | `PROCESS_REGISTRATION_FAMILY_VERSION` | `crates/lash-core/src/runtime/process/validation.rs` |
| Append-request identity encoding | `APPEND_REQUEST_IDENTITY_ENCODING_VERSION` | `crates/lash-core-store/src/store/commit_identity.rs` |
| Session-node body | `SESSION_NODE_BODY_SCHEMA_VERSION` | `crates/lash-core-store/src/session_graph.rs` |
| Durable-read fixture | `DURABLE_READ_FIXTURE_SCHEMA_VERSION` | `crates/lash-core/tests/support/durable_read_fixture.rs` |

`crates/lash/src/formats.rs` is the live registry that binds each durable format to its owning
crate and constant; read it rather than any list in prose when you need the full inventory.

Journal identity remains v2 for all five execution-scope variants, and process-transfer identity
remains v1. Those unchanged byte contracts are separate from the versioned formats above.

**A version cutover is a fresh-trust-domain redeployment boundary.** This is a standing
property of any bump on the surfaces above, not a statement about one particular release: when
the build you are deploying moves any of them past the store's stamp, do not perform a rolling
upgrade or mix old and new hosts, workers, Restate handlers, or remote peers. Drain in-flight
work, stop the old deployment, and provision the replacement SQLite/PostgreSQL stores and
Restate state from this build together; the PostgreSQL component schema has no migration across
a cutover, so the replacement database must be created from this build's `schema.sql`. Reset the
tombstones, await-event revocation ledger, effect journal, and Restate state as one operation,
then start every producer and consumer on the same build. Old affected encodings must refuse; there is no compatibility alias or
fabricated default authority. This runbook does not authorize deleting or rewriting a shared
store: production replacement requires the deployment owner's approved drain and provisioning
procedure, while local verification may recreate only stores owned by the current test or Kiln.

## Pass record

Record the exact commit, command, exit status, and bounded terminal log for each phase. A pass
requires scope mismatch before side effects, distinct scoped addresses, truthful optional
attribution, complete known cause, matching core/Restate parent selection, exact owner grammar,
and explicit old-format refusal. Preserve the logs with the delivery evidence; never substitute
source inspection for a failed command.
