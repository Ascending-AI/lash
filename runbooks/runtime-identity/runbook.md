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

1. Load `env.sh` from the owned warm workspace before Cargo commands.
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
cargo test --workspace --all-targets scoped_controller_refuses_wrong_scope_before_controller_or_local_execution
cargo test --workspace --all-targets task_proxy_refuses_wrong_scope_before_handoff
cargo test --workspace --all-targets restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution
cargo test --workspace --all-targets same_replay_key_in_distinct_scopes_has_distinct_graph_identity
cargo test --workspace --all-targets effect_header_round_trips_without_universal_subject_or_replay_slots
cargo test --workspace --all-targets legacy_universal_effect_header_is_refused
```

The first three tests must refuse before the wrapped controller, local executor, task handoff,
or Restate `scope_effect_begin`. The graph test must produce distinct addresses for identical
local replay keys in different scopes. The header tests must show a required `address` and no
universal `subject` or optional duplicate `replay` slot, while refusing the old shape.

Run the durable host contracts against fresh storage:

```sh
cargo test --workspace --all-targets sqlite_effect_host_satisfies_scope_conformance
cargo test --workspace --all-targets restate_scope_controller_refuses_wrong_scope_before_index_or_local_execution
```

For an owned PostgreSQL gate, export its fresh database URL and require the test to run rather
than print its `LASH_POSTGRES_DATABASE_URL is not set` skip message:

```sh
cargo test --workspace --all-targets postgres_runtime_effect_controller_satisfies_conformance_when_configured -- --nocapture
```

## Phase 2 — Truthful attribution

```sh
cargo test --workspace --all-targets parentless_effect_envelopes_use_process_originator_not_ambient_session
cargo test --workspace --all-targets process_trace_session_attribution_comes_only_from_a_session_originator
cargo test --workspace --all-targets session_node_identity_is_structural_and_missing_identity_is_refused
```

Require foreground session effects to retain their real session. A session-origin process must
carry the origin session. A host-origin process must remain sessionless in both its effect header
and Lashlang graph identity even when the execution service has an ambient session capability.
A session-node causal fact must carry its own structural session id and refuse the former shape
that omitted it.

## Phase 3 — Cause and shared trace projection

```sh
cargo test --workspace --all-targets remote_cause_validation_preserves_partial_trigger_identity_and_checks_effect_scope
cargo test --workspace --all-targets same_replay_key_in_distinct_scopes_has_distinct_graph_identity
cargo test --workspace --all-targets turn_keeps_causal_parent_when_present
```

Inspect failures as identity failures. Do not accept matching display labels as proof. The
remote round trip must retain every known trigger field, and both core and Restate projections
must use the same scoped causal graph address while retaining unrelated base trace metadata.

## Phase 4 — Remote grammar and cutover

```sh
cargo test --workspace --all-targets remote_owner_scope_validation_matches_each_core_owner_grammar
cargo test --workspace --all-targets journal_identity_v2_bytes_remain_unchanged_for_all_scope_variants
cargo test --workspace --all-targets direct_effect_identity_golden_corpus
cargo test --workspace --all-targets trace_schema_version_is_pinned_at_20
```

Session owner keys use the existing raw nonempty/no-NUL opaque grammar, so a whitespace-only
session key remains valid. Host owner keys use the existing trimmed nonempty/no-NUL check. The
five execution-scope journal encodings remain byte-for-byte version 2.

These versions move together in the FIG-2828 cutover:

| Surface | Previous | Current |
| --- | ---: | ---: |
| Remote protocol | 58 | 59 |
| Trace schema | 19 | 20 |
| Direct-effect identity family | 2 | 3 |
| Runtime effect envelope hash domain | v2 | v3 |
| Process registration family | 4 | 5 |
| Process wake-delivery format | 2 | 3 |
| Append-request identity encoding | 3 | 4 |
| RLM snapshot | 17 | 18 |
| SQLite durable core | 55 | 56 |
| SQLite process registry | 32 | 33 |
| SQLite effect journal | 17 | 18 |
| PostgreSQL component | 84 | 85 |
| Session-head metadata | 7 | 8 |
| Session-node body | 12 | 13 |
| Durable-read fixture | 60 | 61 |

Journal identity remains v2 for all five execution-scope variants, and process-transfer identity
remains v1. Those unchanged byte contracts are separate from the affected formats above.

This release is a fresh-trust-domain redeployment boundary. Do not perform a rolling upgrade or
mix old and new hosts, workers, Restate handlers, or remote peers. Drain in-flight work, stop the
old deployment, and provision the replacement SQLite/PostgreSQL stores and Restate state from
this build together; PostgreSQL component 84 has no migration to 85 and the replacement database
must be created from this build's `schema.sql`. Reset the tombstones, await-event revocation
ledger, effect journal, and Restate state as one operation, then start every producer and consumer
on the same build. Old affected encodings must refuse; there is no compatibility alias or
fabricated default authority. This runbook does not authorize deleting or rewriting a shared
store: production replacement requires the deployment owner's approved drain and provisioning
procedure, while local verification may recreate only stores owned by the current test or Kiln.

## Pass record

Record the exact commit, command, exit status, and bounded terminal log for each phase. A pass
requires scope mismatch before side effects, distinct scoped addresses, truthful optional
attribution, complete known cause, matching core/Restate parent selection, exact owner grammar,
and explicit old-format refusal. Preserve the logs with the delivery evidence; never substitute
source inspection for a failed command.
