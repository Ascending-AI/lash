//! The prose lives here, beside the constant it describes, rather than in a
//! README under `fixtures/durable-read/`, so that tree holds only generated
//! artifacts. FIG-2808 registers a byte-exact whole-file guard over it, and a
//! prose edit inside a guarded tree would demand a fixture-version bump that has
//! no honest way to be made: the constant is asserted equal to the
//! `fixture_schema_version` embedded in each `expected.json`, so moving it
//! without regenerating fails read-back. Documentation does not belong inside a
//! guard over generated artifacts.
//!
//! Durable read fixture v1.
//!
//! This fixture detects silent durable-format drift: current code must recover the
//! same public meaning from rows written by the previous committed artifact. Lash
//! does not migrate these stores across a declared store-schema change; a schema
//! mismatch is a forward-only reject-and-recreate boundary and therefore fails this
//! test instead of skipping it.
//!
//! The fixture format has its own declaration,
//! `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, the constant below.
//! `scripts/versioned-surfaces.toml` registers that constant with a whole-file
//! guard over `fixtures/durable-read/*`, so any change to a file in that tree makes
//! `scripts/check_version_bumps.py` require the declaration to be strictly greater
//! than its merge-base value in the same diff. That check runs in CI only, not
//! pre-commit, because it compares against a merge base. Store schema versions
//! remain the authority for whether an old store may be opened.
//!
//! ## Two laws: read-back and write shape
//!
//! Read-back (`*_durable_fixture_reads_with_identical_semantics`) decodes the
//! committed artifact and asserts the meaning recovered from it. It is blind by
//! construction to a change in what this build *writes*: a payload field that is
//! defaulted on read and skipped when absent lets the committed bytes decode,
//! re-encode, and re-hash exactly as the previous writer wrote them, so the receipt
//! still replays and every semantic assertion still holds.
//!
//! Write shape (`*_durable_fixture_expectations_match_what_this_build_writes`)
//! closes that gap. It re-seeds a throwaway store with the current code and requires
//! the committed `expected.json` to equal what the seed produces, naming the drifted
//! JSON paths on failure. Content-addressed identities — process-env refs, node ids,
//! turn-commit hashes — move with the payload shape, so this law catches shape
//! changes the read-back cannot see.
//!
//! The schema-declaration gate is a third, weaker thing: it fires only once a
//! fixture artifact is already in the diff, so it cannot see a shape change that
//! leaves `fixtures/` untouched. Order of use: the write-shape law says drift
//! happened, the decision procedure below decides whether to revert the change or
//! regenerate for it, and the declaration gate then forces the version bump onto the
//! regeneration.
//!
//! ### What the write-shape law does not cover
//!
//! The law compares one artifact: the committed `expected.json`, whose shape is
//! `ExpectedFixture`. It therefore covers exactly the payloads that struct carries —
//! runtime commits (session config, graph node payloads, receipt identity), queue
//! and pending-input identity, process-execution-env specs, await-event keys, and
//! wake deliveries — plus everything their content-addressed hashes depend on.
//!
//! It does not cover payloads absent from `ExpectedFixture`. Trigger subscription,
//! occurrence, and delivery payloads are the largest gap: the read-back assertions
//! for them are deliberately shallow (subscription key, enabled flag, reservation
//! status, occurrence payload), so an additive trigger-payload field — FIG-1377's
//! class of change in a different store — still lands unflagged. Process
//! registrations are the same shape of gap: `registration_fingerprint` is only
//! compared against a re-registration by the same build, so it agrees with itself
//! whatever the payload became.
//!
//! It also does not cover a new field whose fixture value is skipped during
//! serialization (e.g. a `None` that serde skips): such a field is invisible to the
//! law unless a fixture scenario populates it. The `prompt` field was caught only
//! because it serialized as `Some(empty)`.
//!
//! Closing those gaps means extending `ExpectedFixture`, which necessarily
//! regenerates `expected.json` and moves the fixture declaration, so it is follow-up
//! work on its own ticket rather than something to bundle into an unrelated change.
//!
//! ### Drift these laws missed before the write-shape law existed (FIG-1433)
//!
//! Both landed with the schema-declaration gate green because neither pull request
//! touched a file under `fixtures/durable-read/`, and both survived read-back for
//! the reason above. They surfaced only when FIG-1259 regenerated for an unrelated
//! attachment-GC schema bump, which is why that regeneration moved the fixture
//! declaration by two generations (16 to 18) instead of one.
//!
//! | Commit | Shape change | How it appeared later |
//! | --- | --- | --- |
//! | `122e7b348` — Reject drifted process wake delivery payloads (#399, FIG-1377) | `ProcessWakeDelivery` gained the stamped `version` field | `wake_delivery.version: 1` appeared in the SQLite expectations |
//! | `771e875f2` — Persist session prompts and trace composition changes (#411, FIG-1376) | `PersistedSessionConfig` gained `prompt`, written as an explicit empty layer | `prompt: {}` appeared twice in both backends' expectations, and the `runtime_turn_commits` payload hashes were rewritten |
//! | `8a23dca6a` — Guard the durable graph-node body with a versioned surface (#486) | `SessionNodeBody` gained a stamped `schema_version`, defaulted on read for unstamped rows | `"schema_version":1` appeared on every `graph_nodes` payload when FIG-1536's PostgreSQL generation bump forced a regeneration |
//!
//! ### A fixture cannot hold a legacy shape
//!
//! That last row also shows what this fixture is *not* for. Before the regeneration
//! its `graph_nodes` rows happened to be unstamped, which incidentally exercised the
//! defaulted-on-read path; regenerating re-stamped them and the exercise vanished.
//! That was never coverage worth relying on: a fixture is written by the current
//! writer, so every regeneration converts every row to the current shape, and any
//! legacy shape sitting here is one bump away from disappearing without a failing
//! test.
//!
//! A shape older than what this build writes therefore belongs in a frozen byte
//! literal beside the decoder that must keep reading it —
//! `session_graph_tests.rs::unstamped_conversation_bodies_keep_loading` and
//! `::unstamped_stored_bodies_keep_loading` are that, for the node body — not in a
//! generated artifact. Read this fixture as "the previous committed writer's
//! output", which is the drift it exists to catch, and put "some writer, once, long
//! ago" somewhere regeneration cannot reach.
//!
//! ## Coverage
//!
//! Every application-owned table in both artifacts has at least one row. Assertions
//! use supported read/replay surfaces, not row counts, for semantic coverage.
//!
//! | Durable area | Populated tables | Supported read or refusal asserted |
//! | --- | --- | --- |
//! | Session graph and checkpoints | `graph_nodes`, `session_head`/`sessions`, `session_meta`, `blobs`, `usage_deltas`, `runtime_turn_commits` | Ordered graph nodes and every payload field; checkpoint turn, usage, tool, plugin, and execution state; current and legacy receipt replay |
//! | Session retention | `node_anchors`, `deleted_sessions` | `fork_points`, deletion probe, and typed `SessionDeleted` refusal to reopen a retired id |
//! | Attachments | `attachment_manifest`, SQLite `artifact_refs`, PostgreSQL's artifact table | Manifest listing plus process-execution-environment reference recovery |
//! | Receiver queue | `queued_work_batches`, `queued_work_items`, `pending_turn_inputs`, `wake_redelivery_fences`, `session_execution_leases` | Queue/input payloads, deterministic ids, typed wake-rewind refusal, and the raw expired lease generation |
//! | Processes | `processes`, `process_events`, `process_change_clock`, `process_leases`, `process_observers`, `process_segment_handovers`, `process_tombstones`, `process_wake_deliveries`, `wake_allocation_floors` | Process state; every event payload; observers; continuation; wake delivery/floor; expired raw lease; paginated change feed; typed `ProcessNoLongerRetained` tombstone |
//! | Triggers | `trigger_subscriptions`, `trigger_occurrences`, `trigger_deliveries`, `trigger_mutation_receipts` | List/filter, delivery reservation, deterministic receipt replay, and `Unchanged` re-registration |
//! | Effects and awaits | `runtime_effect_replay`, `await_event_meta`, `await_event_waits`, `await_event_revoked_sessions` | Completed effect replay without a local executor, signed await key resolution, and typed late-resolution/revocation behavior |
//! | Backend metadata | PostgreSQL `lash_schema_versions`; SQLite `user_version` | Exact component/store schema-version comparison before read-back |
//!
//! The table names above omit PostgreSQL's `lash_` prefix where the logical name is
//! otherwise identical. PostgreSQL's artifact table is named by role rather than
//! spelled out: its literal name carries an integration-protocol infix that the
//! `integration_boundary` lint forbids naming in this crate's `Cargo.toml`, `src/`,
//! and `tests/`. Both backends' literal table names are in
//! `fixtures/durable-read/v1/postgres/fixture.sql` and
//! `fixtures/durable-read/v1/sqlite/durable-core.db`.
//!
//! The intentionally expired process and session leases are raw durable generation
//! facts. Reading them proves decoding and identity continuity; it does not grant
//! live execution authority. Transient WAL contents, PostgreSQL advisory locks,
//! database indexes, and database-engine bookkeeping are outside this semantic-read
//! contract. SQLite WAL files are checkpointed with `TRUNCATE`, required to report
//! `busy = 0`, and required to be absent before artifact copying.
//!
//! PostgreSQL generation uses only the dedicated `lash_durable_read_fixture` schema
//! and never drops or mutates `public`. PostgreSQL owns lease time and effect-row
//! audit time, so the generator normalizes those volatile timestamps to the fixed
//! fixture epoch after populating them through supported APIs. Assertions still
//! read the resulting records through supported surfaces.
//!
//! ## Regeneration policy
//!
//! Regeneration is deterministic: the generator fixes its clock, signing secret,
//! lease nonces, trigger incarnation, operation ids, and other identity inputs.
//! Determinism is now verified cross-environment (Postgres 14/16/18 and across runner
//! env/TZ/locale), not just two-consecutive-run on one machine: regenerations must
//! produce byte-identical artifacts.
//!
//! An index-only catalog addition is not a reason to regenerate *when it moves no
//! store schema version*. Indexes are outside the semantic-read contract above, and
//! the SQLite catalog is created with `CREATE INDEX IF NOT EXISTS`, so on that tier
//! such an addition leaves the declared version alone: the committed artifact keeps
//! opening and adopts the new index in the copy under test, and regenerating would
//! only replace working old-file evidence with a file the current binary just wrote.
//!
//! PostgreSQL is the exception, and FIG-1536 is the case that proved it. That tier
//! has explicit migrations rather than a reject-and-recreate boundary, so an index
//! addition is a new component generation with a creation-only migration into it —
//! which moves `PostgresStorage::schema_version()`, which the read-back law compares
//! against `postgres/version.json`. Once a store schema version moves, step 2 of the
//! decision procedure below applies in full: bump `DURABLE_READ_FIXTURE_SCHEMA_VERSION`
//! and regenerate *both* backends, because the declaration is embedded in each
//! `expected.json` and asserted on read-back. A postgres-only bump cannot be
//! regenerated alone.
//!
//! When a read-back test fails, use this decision procedure:
//!
//! 1. If no intentional durable-format change and store-schema bump exists, treat
//!    the failure as silent drift. Repair decoding/identity compatibility; do not
//!    regenerate the evidence away.
//! 2. If the on-disk contract intentionally changed, bump every affected store
//!    schema version and `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, state the
//!    reject-and-recreate policy, regenerate both backends, and review the semantic
//!    and artifact diffs.
//!
//! When a write-shape law fails, the failure is about the code in your diff, not
//! about decoding the old artifact, so it gets its own branch:
//!
//! 1. Read the drifted paths the failure names and decide whether writing that shape
//!    is intended. Most of the time it is not: an accidentally serialized field, a
//!    flipped skip condition, or a default that stopped being the default. Revert the
//!    shape change. Regenerating instead absorbs the drift into the committed
//!    surface, which is exactly the failure mode this law exists to prevent.
//! 2. Only for an intended write-shape change, continue with step 2 of the read-back
//!    procedure above: bump the affected store schema versions and
//!    `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, regenerate both backends, and review the
//!    semantic and artifact diffs — the shape change is now a reviewed surface.
//! 3. Run the two destructive drift proofs, both normal read-back tests, both
//!    write-shape laws, and the no-diff double-regeneration proof before
//!    committing.
//!
//! Generate SQLite:
//!
//! ```text
//! LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
//!   cargo test -p lash-internal-sqlite-store --test durable_read_fixture \
//!   regenerate_sqlite_durable_fixture -- --ignored --exact
//! ```
//!
//! Generate PostgreSQL against a caller-owned throwaway database (Docker is used
//! only for the pinned `postgres:16-alpine` `pg_dump` client):
//!
//! ```text
//! LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
//! LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
//!   cargo test -p lash-internal-postgres-store --test durable_read_fixture \
//!   regenerate_postgres_durable_fixture -- --ignored --exact
//! ```
//!
//! Read back without regenerating:
//!
//! ```text
//! cargo test -p lash-internal-sqlite-store --test durable_read_fixture
//! LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
//! LASH_REQUIRE_POSTGRES=1 \
//!   cargo test -p lash-internal-postgres-store --test durable_read_fixture
//! ```
//!
//! For the no-diff proof, hash every file under `fixtures/durable-read/v1/`, run
//! both generation commands twice, and require the hash set to remain unchanged
//! after each pass. Released-pin reproducibility starts with the next published alpha;
//! until then, the committed generators at HEAD are the source of truth.

use lash_sansio::{ProcessId, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::runtime::{
    QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkPayload, load_process_execution_env,
    persist_process_execution_env, process_wake_batch_draft,
};
use lash_core::{
    AttachmentId, AttachmentIntent, AttachmentManifest, AwaitEventKey, AwaitEventWaitIdentity,
    BoundaryReason, Clock, DeliveryPolicy, EffectHost, ExecResponse, ExecutionScope, LashSchema,
    LeaseClaimNonce, LeaseOwnerIdentity, MessageOrigin, MessageRole, OperationId, PartKind,
    PendingTurnInputDraft, PersistedSegmentHandover, PluginNamespaceState, PluginState,
    ProcessAwaitOutput, ProcessChange, ProcessChangeCursor, ProcessCompletionAuthority,
    ProcessContinuationStore, ProcessEventAppendRequest, ProcessEventSemanticsSpec,
    ProcessEventType, ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessIdentity, ProcessInput, ProcessOriginator,
    ProcessProvenance, ProcessRegistration, ProcessRegistry, ProcessStatus, ProcessValueSelector,
    ProcessWakeDelivery, ProcessWakeSpec, ProjectionWatermark, ProtocolTurnOptions,
    RecoveryContract, Resolution, ResolveOutcome, RuntimeCommit, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeInvocation, RuntimePersistence, RuntimeScope, RuntimeSessionState, SegmentHandover,
    SessionAppendNode, SessionNodePayload, SessionPolicy, SessionRelation, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, TextProjectionMetadata,
    TokenLedgerEntry, TokenUsage, TriggerCommand, TriggerCommandOutcome,
    TriggerDeliveryReservationOutcome, TriggerInputBinding, TriggerMutationOutcome,
    TriggerOccurrenceFilter, TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft, TriggerSubscriptionFilter, TurnInput, TurnInputIngress, WaitKind,
    WaitState,
};
use serde::{Deserialize, Serialize};

pub const SESSION_ID: &str = "durable-read-fixture";
pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 59;
pub const FIXTURE_WRITE_MS: u64 = 1_700_000_000_000;
pub const FIXTURE_READ_MS: u64 = FIXTURE_WRITE_MS + 1_000;
const PROCESS_ID: &str = "durable-read-waiting-process";
const WAKE_PROCESS_ID: &str = "durable-read-wake-process";
const TOMBSTONE_PROCESS_ID: &str = "durable-read-retired-process";
const DELETED_SESSION_ID: &str = "durable-read-deleted-session";
const REVOKED_SESSION_ID: &str = "durable-read-revoked-session";
const TRIGGER_KEY: &str = "durable-read-trigger";
const TRIGGER_REGISTER_OPERATION: &str = "durable-read-trigger-register";
const QUEUE_SOURCE_KEY: &str = "durable-read-queue-source";
const INPUT_SOURCE_KEY: &str = "durable-read-input-source";

#[allow(dead_code)]
pub async fn assert_prior_component_encoding_is_refused(store: &dyn RuntimePersistence) {
    let error = store
        .load_session()
        .await
        .expect_err("component encoding version 1 must be refused during hydration");
    assert_eq!(
        error.to_string(),
        "checkpoint component `execution_state` uses encoding version 1, but this build requires version 2; remedy: drain affected sessions and recreate the store with this Lash version"
    );
}

pub struct FixtureHandles {
    pub clock: Arc<dyn Clock>,
    pub runtime: Arc<dyn RuntimePersistence>,
    pub session_factory: Arc<dyn SessionStoreFactory>,
    pub processes: Arc<dyn lash_core::ConformanceProcessRegistry>,
    pub continuations: Arc<dyn ProcessContinuationStore>,
    pub process_envs: Arc<dyn ProcessExecutionEnvStore>,
    pub triggers: Arc<dyn TriggerStore>,
    pub effects: Arc<dyn EffectHost>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpectedFixture {
    pub fixture_schema_version: u32,
    pub head_revision: u64,
    pub node_ids_in_read_order: Vec<String>,
    pub current_append_retry: RuntimeCommit,
    pub legacy_commit_retry: RuntimeCommit,
    pub record_config_retry: RuntimeCommit,
    pub queue_batch_id: String,
    pub pending_input_id: String,
    pub process_env_ref: ProcessExecutionEnvRef,
    pub await_event_key: AwaitEventKey,
    pub revoked_await_event_key: AwaitEventKey,
    pub wake_delivery: ProcessWakeDelivery,
}

fn assert_fixture_schema_version(found: u32) {
    assert_eq!(
        found, DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        "durable fixture schema version changed without regeneration"
    );
}

#[test]
fn immediate_predecessor_fixture_schema_is_adjacent_and_refused() {
    const PREDECESSOR: u32 = 58;
    assert_eq!(
        PREDECESSOR + 1,
        DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        "durable-read fixture adjacency pin"
    );
    assert!(
        std::panic::catch_unwind(|| assert_fixture_schema_version(PREDECESSOR)).is_err(),
        "the immediate predecessor fixture schema must be refused"
    );
}

pub async fn seed(handles: &FixtureHandles) -> ExpectedFixture {
    let mut state = fixture_state();
    let append_nodes = fixture_append_nodes();
    let current_append_retry = lash_core::store::append_request_commit_with_clock_for_testing(
        &mut state,
        "durable-read-current-append",
        &append_nodes,
        None,
        handles.clock.as_ref(),
    )
    .expect("build identity-bearing fixture append");
    handles
        .runtime
        .commit_runtime_state(current_append_retry.clone())
        .await
        .expect("commit identity-bearing fixture append");

    let attachment_id =
        AttachmentId::parse("durable-read-attachment").expect("valid attachment id");
    handles
        .runtime
        .record_intent(AttachmentIntent {
            attachment_id: attachment_id.clone(),
            session_id: SessionId::from(SESSION_ID.to_string()),
            canonical_uri: "session:durable-read-fixture:sha256:durable-read-attachment"
                .to_string(),
            intent_at_epoch_ms: 100,
            owner_kind: None,
            owner_id: None,
        })
        .expect("record fixture attachment intent");

    let mut loaded = lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
        .await
        .expect("load fixture state before legacy commit")
        .expect("fixture session exists before legacy commit");
    loaded.turn_index = 7;
    loaded.token_usage = TokenUsage {
        input_tokens: 13,
        output_tokens: 8,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 3,
        reasoning_output_tokens: 2,
    };
    loaded.set_tool_state_snapshot(Some(
        serde_json::from_value(serde_json::json!({"generation": 887, "tools": {}}))
            .expect("build distinctive fixture tool state"),
    ));
    loaded.set_plugin_state(Some(fixture_plugin_state()));
    loaded.set_execution_state_snapshot(Some(vec![0x46, 0x49, 0x47, 0x38, 0x38, 0x37]));
    let usage = TokenLedgerEntry {
        source: "durable-read-turn".to_string(),
        model: "durable-read-model".to_string(),
        usage: TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        usage_disposition: Default::default(),
    };
    let legacy_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-legacy-commit"),
        "commit",
    );
    let mut legacy_commit_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &loaded,
        &[usage],
        legacy_operation,
    );
    legacy_commit_retry = legacy_commit_retry.with_committed_attachments([attachment_id.clone()]);
    handles
        .runtime
        .commit_runtime_state(legacy_commit_retry.clone())
        .await
        .expect("commit supported NULL-identity legacy-shaped receipt");

    let record_config_state =
        lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
            .await
            .expect("load fixture state before semantic-boundary commit")
            .expect("fixture session exists before semantic-boundary commit");
    let mut record_config_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &record_config_state,
        &[],
        fixture_record_config_operation(),
    );
    record_config_retry
        .stamp_semantic_boundary()
        .expect("stamp fixture semantic-boundary identity");
    handles
        .runtime
        .commit_runtime_state(record_config_retry.clone())
        .await
        .expect("commit fixture semantic-boundary receipt");

    let committed = handles
        .runtime
        .load_session()
        .await
        .expect("load fixture before pin")
        .expect("fixture exists before pin");
    handles
        .session_factory
        .pin(
            committed
                .graph
                .leaf_node_id
                .as_deref()
                .expect("fixture graph has a leaf"),
        )
        .await
        .expect("pin fixture leaf through session factory");

    let deleted_request = fixture_session_request(&SessionId::from(DELETED_SESSION_ID));
    handles
        .session_factory
        .create_store(&deleted_request)
        .await
        .expect("create fixture session that will be retired");
    handles
        .session_factory
        .delete_session(&SessionId::from(DELETED_SESSION_ID))
        .await
        .expect("retire fixture session through session factory");

    let queued = handles
        .runtime
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                SESSION_ID,
                DeliveryPolicy::EarliestSafeBoundary,
                lash_core::runtime::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(SESSION_ID),
                        "durable-read-frame",
                    ),
                    "durable read queued task",
                    None,
                ),
            )
            .with_source_key(QUEUE_SOURCE_KEY),
        )
        .await
        .expect("enqueue fixture queued work");
    let pending = handles
        .runtime
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                SESSION_ID,
                TurnInputIngress::NextTurn,
                TurnInput::text("durable read pending input"),
            )
            .with_input_id("durable-read-pending-input")
            .with_source_key(INPUT_SOURCE_KEY),
        )
        .await
        .expect("enqueue fixture pending turn input");

    let process_env = fixture_process_env();
    let process_env_ref =
        persist_process_execution_env(handles.process_envs.as_ref(), &process_env)
            .await
            .expect("persist fixture process execution environment");
    let registration = waiting_process_registration(process_env_ref.clone());
    handles
        .processes
        .register_process_with_observers(registration, &[SessionId::from(SESSION_ID.to_string())])
        .await
        .expect("register waiting fixture process");
    let lease = handles
        .processes
        .claim_process_lease(
            &ProcessId::from(PROCESS_ID),
            &LeaseOwnerIdentity::opaque("durable-read-owner", "durable-read-incarnation"),
            100,
        )
        .await
        .expect("claim fixture process lease")
        .acquired()
        .expect("fixture process lease acquired");
    handles
        .processes
        .set_process_wait_with_authority(
            &ProcessId::from(PROCESS_ID),
            fixture_wait_state(),
            &ProcessExecutionWriteAuthority::lease(lease),
        )
        .await
        .expect("persist fixture process wait state");
    handles
        .continuations
        .put_segment_handover(&ProcessId::from(PROCESS_ID), fixture_handover())
        .await
        .expect("persist fixture continuation");

    handles
        .processes
        .register_process(
            ProcessRegistration::new(
                WAKE_PROCESS_ID,
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "wake"}),
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            )
            .with_extra_event_types([ProcessEventType {
                name: "fixture.wake".to_string(),
                payload_schema: LashSchema::any(),
                semantics: ProcessEventSemanticsSpec {
                    wake: Some(ProcessWakeSpec {
                        when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some(SessionId::from(SESSION_ID.to_string()))),
        )
        .await
        .expect("register fixture wake process");
    let wake_append = handles
        .processes
        .append_event(
            &ProcessId::from(WAKE_PROCESS_ID),
            ProcessEventAppendRequest::new(
                "fixture.wake",
                serde_json::json!({"wake_input": "durable read wake"}),
            ),
        )
        .await
        .expect("append fixture wake event");
    let wake_delivery = wake_append
        .wake_delivery
        .expect("wake-semantic fixture event emits a delivery");

    handles
        .processes
        .register_process(ProcessRegistration::new(
            TOMBSTONE_PROCESS_ID,
            ProcessInput::External {
                metadata: serde_json::json!({"fixture": "tombstone"}),
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register fixture process to prune");
    handles
        .processes
        .complete_process(
            &ProcessId::from(TOMBSTONE_PROCESS_ID),
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({ "fixture": "retired" }),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete fixture process to prune");
    let (_, terminal_cursor) = handles
        .processes
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect("project fixture terminal process before prune");
    let prune = handles
        .processes
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .expect("prune fixture terminal process to a tombstone");
    assert_eq!(prune.pruned_processes, 1);

    let register_command = fixture_register_command(process_env_ref.clone());
    let receipt = trigger_receipt(
        handles.triggers.as_ref(),
        TRIGGER_REGISTER_OPERATION,
        register_command,
    )
    .await;
    assert_eq!(receipt.disposition, TriggerMutationOutcome::Created);
    handles
        .triggers
        .ingest_occurrence(TriggerOccurrenceRequest::new(
            "fixture.event",
            "fixture-source",
            serde_json::json!({"value": 42}),
            "durable-read-occurrence",
        ))
        .await
        .expect("ingest fixture occurrence");

    let scope = ExecutionScope::turn(SESSION_ID, "durable-read-turn");
    let await_event_key = handles
        .effects
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("durable-read-tool-call"),
        )
        .await
        .expect("mint fixture await-event key");
    assert_eq!(
        handles
            .effects
            .resolve_await_event(
                &await_event_key,
                Resolution::Ok(serde_json::json!({"fixture": "resolved"})),
            )
            .await
            .expect("resolve fixture await-event"),
        ResolveOutcome::Accepted
    );

    let revoked_await_event_key = handles
        .effects
        .await_event_key(
            &ExecutionScope::turn(REVOKED_SESSION_ID, "durable-read-revoked-turn"),
            AwaitEventWaitIdentity::tool_completion("durable-read-revoked-tool-call"),
        )
        .await
        .expect("mint fixture await-event key before session revocation");
    handles
        .effects
        .revoke_await_events_for_session(&SessionId::from(REVOKED_SESSION_ID))
        .await
        .expect("persist fixture await-event session revocation");

    let effect_envelope = fixture_effect_envelope();
    handles
        .effects
        .scoped(ExecutionScope::turn(SESSION_ID, "durable-read-effect-turn"))
        .expect("scope fixture effect journal")
        .controller()
        .execute_effect(
            effect_envelope,
            RuntimeEffectLocalExecutor::testing(|envelope| async move {
                assert!(matches!(
                    envelope.command,
                    RuntimeEffectCommand::ExecCode { ref language, ref code }
                        if language == "fixture" && code == "return 887"
                ));
                Ok(RuntimeEffectOutcome::ExecCode {
                    result: Box::new(Ok(ExecResponse {
                        observations: vec![lash_core::Observation {
                            text: "durable read effect".to_string(),
                            projection: TextProjectionMetadata {
                                truncated: false,
                                original_chars: 19,
                                projected_chars: 19,
                                original_lines: 1,
                                projected_lines: 1,
                                limit: 50 * 1024,
                                limit_mode: "bytes".to_string(),
                                max_lines: 2_000,
                            },
                        }],
                        calls: Vec::new(),
                        printed_images: Vec::new(),
                        error: None,
                        duration_ms: 887,
                        degraded_bindings: Vec::new(),
                        terminal_finish: Some(serde_json::json!({"fixture": 887})),
                    })),
                })
            }),
        )
        .await
        .expect("persist fixture runtime-effect replay row");

    let wake_batch = handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(wake_delivery.clone()))
        .await
        .expect("enqueue fixture process wake at receiver");
    let queue_owner = LeaseOwnerIdentity::opaque(
        "durable-read-session-owner",
        "durable-read-session-incarnation",
    );
    let queue_lease = handles
        .runtime
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(SESSION_ID),
            &queue_owner,
            "durable-read-queue-executor",
            &LeaseClaimNonce::for_testing("durable-read-queue-claim-nonce"),
            100,
        )
        .await
        .expect("claim fixture session lane for wake consumption")
        .acquired()
        .expect("fixture session lane is available");
    let wake_claim = handles
        .runtime
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from(SESSION_ID),
            &queue_lease.fence(),
            &queue_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&wake_batch.batch_id),
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim fixture receiver wake")
        .expect("fixture receiver wake is claimable");
    let wake_state = lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
        .await
        .expect("load fixture state before wake settlement")
        .expect("fixture exists before wake settlement");
    let wake_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-wake-settlement"),
        "commit",
    );
    let wake_commit =
        RuntimeCommit::persisted_state_with_operation_for_testing(&wake_state, &[], wake_operation)
            .completing_queue_claim(wake_claim.completion())
            .releasing_session_execution_lease(queue_lease.completion());
    handles
        .runtime
        .commit_runtime_state(wake_commit)
        .await
        .expect("settle fixture receiver wake and persist redelivery fence");
    handles
        .runtime
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(SESSION_ID),
            &queue_owner,
            "durable-read-retained-executor",
            &LeaseClaimNonce::for_testing("durable-read-retained-session-lease"),
            100,
        )
        .await
        .expect("persist fixture retained session lease")
        .acquired()
        .expect("fixture retained session lease is available");

    let read = handles
        .runtime
        .load_session()
        .await
        .expect("load seeded fixture session")
        .expect("seeded fixture session exists");
    ExpectedFixture {
        fixture_schema_version: DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        head_revision: read.head_revision,
        node_ids_in_read_order: read
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect(),
        current_append_retry,
        legacy_commit_retry,
        record_config_retry,
        queue_batch_id: queued.batch_id,
        pending_input_id: pending.input_id,
        process_env_ref,
        await_event_key,
        revoked_await_event_key,
        wake_delivery,
    }
}

pub async fn assert_semantics(handles: &FixtureHandles, expected: &ExpectedFixture) {
    assert_fixture_schema_version(expected.fixture_schema_version);
    let read = handles
        .runtime
        .load_session()
        .await
        .expect("durable fixture drift: public session read failed")
        .expect("durable fixture drift: session disappeared");
    assert_eq!(
        read.head_revision, expected.head_revision,
        "durable fixture semantic drift: head revision changed"
    );
    let node_ids = read
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        node_ids, expected.node_ids_in_read_order,
        "durable fixture semantic drift: graph node ids or order changed"
    );
    assert_graph_payloads(&read.graph.nodes);
    let checkpoint = read
        .checkpoint
        .expect("durable fixture semantic drift: checkpoint disappeared");
    assert_eq!(
        checkpoint.turn_state.turn_index, 7,
        "durable fixture semantic drift: checkpoint turn_index changed"
    );
    assert_eq!(
        checkpoint.turn_state.token_usage,
        TokenUsage {
            input_tokens: 13,
            output_tokens: 8,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        "durable fixture semantic drift: checkpoint token usage changed"
    );
    assert_eq!(
        serde_json::to_value(
            checkpoint
                .decode_component::<lash_core::ToolState>(
                    lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT,
                )
                .expect("decode durable fixture tool state")
                .as_ref()
                .expect("durable fixture semantic drift: tool-state component disappeared")
        )
        .expect("encode fixture tool state"),
        serde_json::json!({"generation": 887, "tools": {}}),
        "durable fixture semantic drift: tool-state content changed"
    );
    assert_eq!(
        serde_json::to_value(
            checkpoint
                .decode_component::<PluginState>(
                    lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
                )
                .expect("decode durable fixture plugin snapshot")
                .as_ref()
                .expect("durable fixture semantic drift: plugin snapshot disappeared")
        )
        .expect("encode fixture plugin snapshot"),
        serde_json::to_value(fixture_plugin_state()).expect("encode expected plugin snapshot"),
        "durable fixture semantic drift: plugin snapshot content changed"
    );
    assert_eq!(
        checkpoint.component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        Some(&[0x46, 0x49, 0x47, 0x38, 0x38, 0x37][..]),
        "durable fixture semantic drift: execution-state component changed"
    );
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(
        read.token_ledger[0].usage,
        TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        "durable fixture semantic drift: usage ledger totals changed"
    );
    assert!(
        AttachmentManifest::list_all_refs(handles.runtime.as_ref())
            .expect("read fixture attachment manifest")
            .contains(
                &AttachmentId::parse("durable-read-attachment").expect("valid attachment id")
            ),
        "durable fixture semantic drift: committed attachment disappeared"
    );

    let pinned = handles
        .session_factory
        .fork_points()
        .await
        .expect("durable fixture drift: node-anchor read failed");
    assert_eq!(pinned.len(), 1);
    assert_eq!(
        pinned[0].node_id,
        *expected
            .node_ids_in_read_order
            .last()
            .expect("fixture expected graph has a leaf")
    );
    assert_eq!(pinned[0].source_session_id, SESSION_ID);
    assert!(pinned[0].pinned);
    assert!(
        handles
            .session_factory
            .session_was_deleted(&SessionId::from(DELETED_SESSION_ID))
            .await
            .expect("durable fixture drift: deleted-session probe failed"),
        "durable fixture semantic drift: session tombstone disappeared"
    );
    match handles
        .session_factory
        .create_store(&fixture_session_request(&SessionId::from(
            DELETED_SESSION_ID,
        )))
        .await
    {
        Err(StoreError::SessionDeleted { session_id }) => {
            assert_eq!(session_id, DELETED_SESSION_ID)
        }
        Ok(_) => panic!("durable fixture drift: retired session id was reopened"),
        Err(error) => panic!(
            "durable fixture drift: retired session open returned wrong typed error: {error}"
        ),
    }

    let session_lease = handles
        .runtime
        .get_session_execution_lease(&SessionId::from(SESSION_ID))
        .await
        .expect("durable fixture drift: session lease read failed")
        .lease
        .expect("durable fixture drift: retained session lease disappeared");
    assert_eq!(
        session_lease.owner,
        LeaseOwnerIdentity::opaque(
            "durable-read-session-owner",
            "durable-read-session-incarnation"
        )
    );
    assert_eq!(
        session_lease.lease_token,
        "durable-read-retained-session-lease"
    );
    assert_eq!(session_lease.fencing_token, 2);
    assert_eq!(session_lease.claimed_at_epoch_ms, FIXTURE_WRITE_MS);
    assert_eq!(session_lease.lease_term_ms, 100);
    assert_eq!(session_lease.expires_at_epoch_ms, FIXTURE_WRITE_MS + 100);
    assert!(
        session_lease.expires_at_epoch_ms <= FIXTURE_READ_MS,
        "fixture session lease must deliberately read as an expired raw generation fact"
    );

    let current_replay = handles
        .runtime
        .commit_runtime_state(expected.current_append_retry.clone())
        .await
        .expect("durable fixture identity drift: current append receipt no longer replays");
    assert!(
        current_replay.receipt_replayed,
        "durable fixture identity drift: current append receipt was applied instead of replayed"
    );
    let legacy_replay = handles
        .runtime
        .commit_runtime_state(expected.legacy_commit_retry.clone())
        .await
        .expect("durable fixture identity drift: NULL-identity legacy receipt no longer replays");
    assert!(
        legacy_replay.receipt_replayed,
        "durable fixture identity drift: legacy receipt was applied instead of replayed"
    );
    assert_eq!(
        legacy_replay.committed_usage_delta_identities,
        vec![
            expected.legacy_commit_retry.usage_deltas[0]
                .identity
                .clone()
        ],
        "durable fixture identity drift: usage receipt identity changed"
    );
    let semantic_replay = handles
        .runtime
        .commit_runtime_state(expected.record_config_retry.clone())
        .await
        .expect("durable fixture identity drift: semantic-boundary receipt no longer replays");
    assert!(
        semantic_replay.receipt_replayed,
        "durable fixture identity drift: semantic-boundary receipt was applied instead of replayed"
    );
    // FIG-2480: a same-request retry REBUILT at today's (advanced) head must be
    // answered from the durable receipt evidence, not refused for its moved
    // whole-commit hash.
    let rebuilt_state = lash_core::store::load_persisted_session_state(handles.runtime.as_ref())
        .await
        .expect("durable fixture drift: reload for semantic-boundary rebuild failed")
        .expect("durable fixture drift: session disappeared before semantic-boundary rebuild");
    let mut rebuilt_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &rebuilt_state,
        &[],
        fixture_record_config_operation(),
    );
    rebuilt_retry
        .stamp_semantic_boundary()
        .expect("stamp rebuilt fixture semantic-boundary identity");
    let rebuilt_replay = handles
        .runtime
        .commit_runtime_state(rebuilt_retry)
        .await
        .expect(
            "durable fixture identity drift: rebuilt semantic-boundary retry no longer replays",
        );
    assert!(
        rebuilt_replay.receipt_replayed,
        "durable fixture identity drift: rebuilt semantic-boundary retry was applied instead of \
         replayed"
    );
    let mut changed_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &rebuilt_state,
        &[],
        fixture_record_config_operation(),
    );
    changed_retry.config.provider_id = "durable-read-changed-provider".to_string();
    changed_retry
        .stamp_semantic_boundary()
        .expect("stamp changed fixture semantic-boundary identity");
    let refused = handles.runtime.commit_runtime_state(changed_retry).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::SemanticBoundaryIdentityConflict { .. })
        ),
        "durable fixture identity drift: differing semantic-boundary content was not refused: \
         {refused:?}"
    );

    let queued = handles
        .runtime
        .list_queued_work(&SessionId::from(SESSION_ID))
        .await
        .expect("durable fixture drift: queued-work read failed");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, expected.queue_batch_id);
    assert_eq!(queued[0].source_key.as_deref(), Some(QUEUE_SOURCE_KEY));
    assert_eq!(queued[0].items.len(), 1);
    assert!(
        matches!(
            &queued[0].items[0].payload,
            QueuedWorkPayload::AgentFrameTask { frame_id, task, .. }
                if frame_id == &lash_core::facade_support::frame_node_id(&SessionId::from(SESSION_ID), "durable-read-frame")
                    && task == "durable read queued task"
        ),
        "durable fixture semantic drift: queued-work payload changed"
    );
    let pending = handles
        .runtime
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
        .await
        .expect("durable fixture drift: pending-input read failed");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input_id, expected.pending_input_id);
    assert_eq!(pending[0].source_key.as_deref(), Some(INPUT_SOURCE_KEY));
    assert!(pending[0].state.is_next_turn_pending());
    assert_eq!(
        serde_json::to_value(&pending[0].input).expect("encode fixture pending input"),
        serde_json::to_value(TurnInput::text("durable read pending input"))
            .expect("encode expected pending input"),
        "durable fixture semantic drift: pending-input payload changed"
    );

    let process = handles
        .processes
        .get_process(&ProcessId::from(PROCESS_ID))
        .await
        .expect("durable fixture drift: process read failed")
        .expect("durable fixture drift: process disappeared");
    assert_eq!(process.status, ProcessStatus::Waiting);
    assert_eq!(process.wait.as_ref(), Some(&fixture_wait_state()));
    assert_eq!(process.env_ref.as_ref(), Some(&expected.process_env_ref));
    let process_events = handles
        .processes
        .events_after(&ProcessId::from(PROCESS_ID), 0)
        .await
        .expect("durable fixture drift: waiting-process event read failed");
    assert_eq!(process_events.len(), 2);
    assert_eq!(process_events[0].sequence, 1);
    assert_eq!(process_events[0].event_type, "process.observer_added");
    assert_eq!(
        process_events[0].payload,
        serde_json::json!({
            "by": {"kind": "host", "operation_id": "registration"},
            "session": SESSION_ID,
        }),
        "durable fixture semantic drift: observer-added event payload changed"
    );
    assert_eq!(process_events[1].sequence, 2);
    assert_eq!(process_events[1].event_type, "process.waiting");
    assert_eq!(
        process_events[1].payload,
        serde_json::json!({"wait": fixture_wait_state()}),
        "durable fixture semantic drift: waiting-process event payload changed"
    );
    assert_eq!(
        handles
            .processes
            .observers_for_process(&ProcessId::from(PROCESS_ID))
            .await
            .expect("durable fixture drift: process-observer read failed"),
        vec![SESSION_ID.to_string()],
        "durable fixture semantic drift: process-observer edge changed"
    );
    let process_lease = handles
        .processes
        .get_process_lease(&ProcessId::from(PROCESS_ID))
        .await
        .expect("durable fixture drift: process-lease read failed")
        .expect("durable fixture drift: process lease disappeared");
    let expected_process_lease = expected_process_lease();
    assert_eq!(
        process_lease.schema_version,
        expected_process_lease.schema_version
    );
    assert_eq!(process_lease.process_id, expected_process_lease.process_id);
    assert_eq!(process_lease.owner, expected_process_lease.owner);
    assert_eq!(
        process_lease.lease_token,
        expected_process_lease.lease_token
    );
    assert_eq!(
        process_lease.fencing_token,
        expected_process_lease.fencing_token
    );
    assert_eq!(
        process_lease.claimed_at_epoch_ms,
        expected_process_lease.claimed_at_epoch_ms
    );
    assert_eq!(
        process_lease.expires_at_epoch_ms,
        expected_process_lease.expires_at_epoch_ms
    );
    assert!(
        process_lease.expires_at_epoch_ms <= FIXTURE_READ_MS,
        "fixture process lease is intentionally expired; get_process_lease must expose the raw row without treating it as live authority"
    );
    assert_eq!(
        handles
            .continuations
            .latest_segment_handover(&ProcessId::from(PROCESS_ID))
            .await
            .expect("durable fixture drift: continuation read failed"),
        Some(fixture_handover())
    );
    let loaded_env =
        load_process_execution_env(handles.process_envs.as_ref(), &expected.process_env_ref)
            .await
            .expect("durable fixture identity drift: process env ref no longer resolves");
    assert_eq!(
        serde_json::to_value(loaded_env).expect("encode loaded fixture env"),
        serde_json::to_value(fixture_process_env()).expect("encode expected fixture env")
    );
    let reregistered = handles
        .processes
        .register_process_with_observers(
            waiting_process_registration(expected.process_env_ref.clone()),
            &[SessionId::from(SESSION_ID.to_string())],
        )
        .await
        .expect("durable fixture identity drift: identical process re-registration conflicted");
    assert_eq!(
        reregistered.registration_fingerprint,
        process.registration_fingerprint
    );
    assert_eq!(
        handles
            .processes
            .get_process(&ProcessId::from(WAKE_PROCESS_ID))
            .await
            .expect("durable fixture drift: wake process read failed")
            .expect("durable fixture drift: wake process disappeared")
            .status,
        ProcessStatus::Running
    );
    let wake_events = handles
        .processes
        .events_after(&ProcessId::from(WAKE_PROCESS_ID), 0)
        .await
        .expect("durable fixture drift: wake-process event read failed");
    assert_eq!(wake_events.len(), 1);
    assert_eq!(wake_events[0].sequence, 1);
    assert_eq!(wake_events[0].event_type, "fixture.wake");
    assert_eq!(
        wake_events[0].payload,
        serde_json::json!({"wake_input": "durable read wake"}),
        "durable fixture semantic drift: wake-process event payload changed"
    );
    assert!(
        handles
            .processes
            .list_wake_deliveries(None)
            .await
            .expect("durable fixture drift: wake-delivery read failed")
            .iter()
            .any(|delivery| delivery.wake.process_id == WAKE_PROCESS_ID),
        "durable fixture semantic drift: process wake delivery disappeared"
    );
    assert_eq!(
        handles
            .processes
            .wake_allocation_floor_for_testing(
                &SessionId::from(SESSION_ID),
                &ProcessId::from(WAKE_PROCESS_ID)
            )
            .await
            .expect("durable fixture drift: wake-allocation-floor read failed"),
        Some(1),
        "durable fixture semantic drift: sender wake allocation floor changed"
    );
    let redelivery = handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(expected.wake_delivery.clone()))
        .await
        .expect_err("durable fixture drift: settled process wake was redelivered");
    assert!(
        matches!(
            redelivery,
            StoreError::ProcessWakeSequenceRewound {
                sequence: 1,
                allocation_floor: 1,
                ..
            }
        ),
        "durable fixture drift: receiver wake-redelivery fence returned {redelivery}"
    );

    match handles
        .processes
        .get_process(&ProcessId::from(TOMBSTONE_PROCESS_ID))
        .await
    {
        Err(lash_core::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            assert_eq!(terminal_label, "completed");
            assert_eq!(pruned_at_ms, FIXTURE_WRITE_MS);
        }
        other => panic!(
            "durable fixture drift: process tombstone did not return ProcessNoLongerRetained: {other:?}"
        ),
    }
    assert_process_change_feed(handles.processes.as_ref()).await;

    let subscriptions = handles
        .triggers
        .list_subscriptions(TriggerSubscriptionFilter::for_session(SESSION_ID))
        .await
        .expect("durable fixture drift: trigger subscription read failed");
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0].subscription_key, TRIGGER_KEY);
    assert!(subscriptions[0].enabled);
    let occurrences = handles
        .triggers
        .list_occurrences(TriggerOccurrenceFilter::default())
        .await
        .expect("durable fixture drift: trigger occurrence read failed");
    assert_eq!(occurrences.len(), 1);
    let deliveries = handles
        .triggers
        .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
        .await
        .expect("durable fixture drift: trigger delivery read failed");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].subscription.subscription_key, TRIGGER_KEY);
    assert!(deliveries[0].subscription.enabled);
    assert_eq!(
        deliveries[0].reservation_status,
        TriggerDeliveryReservationOutcome::AlreadyReserved
    );
    assert_eq!(
        deliveries[0].occurrence.payload,
        serde_json::json!({"value": 42})
    );
    let replayed_receipt = trigger_receipt(
        handles.triggers.as_ref(),
        TRIGGER_REGISTER_OPERATION,
        fixture_register_command(expected.process_env_ref.clone()),
    )
    .await;
    assert_eq!(
        replayed_receipt.subscription_id,
        subscriptions[0].subscription_id
    );
    let unchanged = trigger_receipt(
        handles.triggers.as_ref(),
        "durable-read-trigger-reregister",
        fixture_register_command(expected.process_env_ref.clone()),
    )
    .await;
    assert_eq!(
        unchanged.disposition,
        TriggerMutationOutcome::Unchanged,
        "durable fixture identity drift: identical trigger re-registration changed meaning"
    );

    let reminted = handles
        .effects
        .await_event_key(
            &ExecutionScope::turn(SESSION_ID, "durable-read-turn"),
            AwaitEventWaitIdentity::tool_completion("durable-read-tool-call"),
        )
        .await
        .expect("durable fixture identity drift: promise key cannot be reminted");
    assert_eq!(
        reminted, expected.await_event_key,
        "durable fixture identity drift: promise key bytes changed"
    );
    assert_eq!(
        reminted.promise_key(),
        expected.await_event_key.promise_key()
    );
    assert_eq!(
        handles
            .effects
            .peek_await_event(&reminted)
            .await
            .expect("durable fixture drift: await-event peek failed"),
        Some(Resolution::Ok(serde_json::json!({"fixture": "resolved"}))),
        "durable fixture semantic drift: await-event resolution changed"
    );

    assert_eq!(
        handles
            .effects
            .resolve_await_event(
                &expected.revoked_await_event_key,
                Resolution::Ok(serde_json::json!({"fixture": "late"})),
            )
            .await
            .expect("durable fixture drift: revoked await-event resolve failed"),
        ResolveOutcome::UnknownOrRevoked,
        "durable fixture semantic drift: await-event session revocation disappeared"
    );
    let revoked_error = handles
        .effects
        .await_await_event(
            &expected.revoked_await_event_key,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .expect_err("durable fixture drift: revoked await-event unexpectedly remained open");
    assert_eq!(
        revoked_error.code.as_str(),
        "await_event_unknown_or_revoked"
    );

    let replayed_effect = handles
        .effects
        .scoped(ExecutionScope::turn(SESSION_ID, "durable-read-effect-turn"))
        .expect("durable fixture drift: scope replayed effect journal")
        .controller()
        .execute_effect(
            fixture_effect_envelope(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("durable fixture drift: completed runtime effect did not replay");
    let RuntimeEffectOutcome::ExecCode { result } = replayed_effect else {
        panic!("durable fixture semantic drift: runtime-effect replay outcome kind changed");
    };
    let response = result.expect("durable fixture semantic drift: exec effect became an error");
    assert_eq!(response.observations.len(), 1);
    assert!(response.calls.is_empty());
    assert_eq!(response.observations[0].text, "durable read effect");
    assert_eq!(
        response.observations[0].projection,
        TextProjectionMetadata {
            truncated: false,
            original_chars: 19,
            projected_chars: 19,
            original_lines: 1,
            projected_lines: 1,
            limit: 50 * 1024,
            limit_mode: "bytes".to_string(),
            max_lines: 2_000,
        },
        "durable fixture semantic drift: observation projection changed"
    );
    assert_eq!(response.duration_ms, 887);
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!({"fixture": 887})),
        "durable fixture semantic drift: runtime-effect replay payload changed"
    );
}

/// Requires the committed expectations to equal what this build writes today.
///
/// [`assert_semantics`] is a read-back law: it decodes the previous artifact and
/// asserts the meaning recovered from it. A write-path payload-shape change is
/// invisible to it, because the committed bytes keep round-tripping through the
/// new types — an added field that is defaulted on read and skipped when absent
/// decodes, re-encodes, and re-hashes exactly as the old writer wrote it. The
/// schema-declaration gate cannot see it either: that gate only fires once a
/// fixture artifact is already in the diff.
///
/// This is the converse law (FIG-1433). The caller re-seeds a throwaway store
/// with the current code and hands the serialized expectations here, so a shape
/// change fails in the diff that introduces it instead of being absorbed by the
/// next unrelated regeneration.
///
/// Its reach is exactly [`ExpectedFixture`]: payloads that struct does not carry
/// — trigger subscription/occurrence/delivery rows and process registrations —
/// can still gain a field unflagged. The fixture README records that bound.
pub fn assert_committed_expectations_match_current_writes(committed: &[u8], written_now: &[u8]) {
    if committed == written_now {
        return;
    }
    panic!(
        "durable fixture write-shape drift: this build writes durable payloads the committed \
         expectations do not carry.{}\nDecide first whether the new write shape is intended. If \
         it is not, revert the shape change: regenerating here would absorb the drift into the \
         committed surface, which is the failure FIG-1433 closed. Drift that appears or \
         disappears between runs (without a code change) means nondeterminism in the fixture \
         inputs — e.g. a non-empty HashMap reaching serialization, or a tie in the `ORDER BY \
         generation` read — and must be fixed at the source, NOT by regenerating the fixture. \
         Only once the change is intended, bump DURABLE_READ_FIXTURE_SCHEMA_VERSION and \
         regenerate both backends:\n  {REGENERATION_COMMANDS}",
        rendered_expectation_drift(committed, written_now)
    );
}

const REGENERATION_COMMANDS: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p \
     lash-sqlite-store --test durable_read_fixture regenerate_sqlite_durable_fixture -- \
     --ignored --exact\n  LASH_POSTGRES_DATABASE_URL=<throwaway> \
     LASH_REGENERATE_DURABLE_READ_FIXTURES=1 cargo test -p lash-postgres-store --test \
     durable_read_fixture regenerate_postgres_durable_fixture -- --ignored --exact";

fn rendered_expectation_drift(committed: &[u8], written_now: &[u8]) -> String {
    let (Ok(committed), Ok(written_now)) = (
        serde_json::from_slice::<serde_json::Value>(committed),
        serde_json::from_slice::<serde_json::Value>(written_now),
    ) else {
        return String::new();
    };
    let mut drift = Vec::new();
    collect_expectation_drift("", &committed, &written_now, &mut drift);
    if drift.is_empty() {
        return String::new();
    }
    drift.truncate(20);
    format!("\n  - {}", drift.join("\n  - "))
}

fn collect_expectation_drift(
    path: &str,
    committed: &serde_json::Value,
    written_now: &serde_json::Value,
    drift: &mut Vec<String>,
) {
    match (committed, written_now) {
        (serde_json::Value::Object(committed), serde_json::Value::Object(written_now)) => {
            let keys = committed
                .keys()
                .chain(written_now.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let child = format!("{path}/{key}");
                match (committed.get(key), written_now.get(key)) {
                    (Some(committed), Some(written_now)) => {
                        collect_expectation_drift(&child, committed, written_now, drift)
                    }
                    (Some(committed), None) => drift.push(format!(
                        "{child}: committed only ({})",
                        rendered_drift_value(committed)
                    )),
                    (None, written_now) => drift.push(format!(
                        "{child}: written by this build only ({})",
                        written_now.map_or_else(String::new, rendered_drift_value)
                    )),
                }
            }
        }
        (serde_json::Value::Array(committed), serde_json::Value::Array(written_now))
            if committed.len() == written_now.len() =>
        {
            for (index, (committed, written_now)) in
                committed.iter().zip(written_now.iter()).enumerate()
            {
                collect_expectation_drift(
                    &format!("{path}/{index}"),
                    committed,
                    written_now,
                    drift,
                );
            }
        }
        (committed, written_now) if committed != written_now => drift.push(format!(
            "{path}: committed {} but this build writes {}",
            rendered_drift_value(committed),
            rendered_drift_value(written_now)
        )),
        _ => {}
    }
}

fn rendered_drift_value(value: &serde_json::Value) -> String {
    let mut rendered = value.to_string();
    if rendered.chars().count() > 80 {
        rendered = rendered.chars().take(77).collect::<String>() + "...";
    }
    rendered
}

fn assert_graph_payloads(nodes: &[lash_core::SessionNodeRecord]) {
    assert_eq!(
        nodes.len(),
        3,
        "durable fixture semantic drift: graph node count changed"
    );
    match &nodes[0].payload {
        SessionNodePayload::FrameOpen {
            frame_key,
            reason,
            assignment,
            protocol_turn_options,
        } => {
            assert_eq!(
                frame_key,
                &lash_core::FrameKey::from_caller_material("initial-frame")
                    .expect("non-empty initial frame material")
            );
            assert_eq!(reason.as_str(), "initial");
            assert_eq!(assignment.policy.model.id, "");
            assert_eq!(assignment.policy.recorded_provider_id(), "");
            assert_eq!(assignment.policy.context_window_tokens(), 1);
            assert_eq!(assignment.policy.session_id, None);
            assert!(!assignment.policy.autonomous);
            assert_eq!(
                assignment.policy.turn_budget,
                lash_core::TurnBudget::Unbounded
            );
            assert_eq!(assignment.usage_source, None);
            assert_eq!(
                serde_json::to_value(&assignment.plugin_options)
                    .expect("encode frame plugin options"),
                serde_json::json!({})
            );
            assert_eq!(
                serde_json::to_value(protocol_turn_options)
                    .expect("encode frame protocol-turn options"),
                serde_json::to_value(ProtocolTurnOptions::default())
                    .expect("encode expected protocol-turn options")
            );
        }
        other => {
            panic!("durable fixture semantic drift: first graph node is not FrameOpen: {other:?}")
        }
    }
    match &nodes[1].payload {
        SessionNodePayload::Event {
            event: lash_core::SessionHistoryRecord::Conversation(message),
        } => {
            assert_eq!(message.role, MessageRole::User);
            assert_eq!(message.parts.len(), 1);
            assert_eq!(message.parts[0].kind, PartKind::Text);
            assert_eq!(message.parts[0].content, "durable read user message");
            assert!(matches!(
                message.origin.as_ref(),
                Some(MessageOrigin::Plugin { plugin_id, transient: false })
                    if plugin_id == "plugin"
            ));
        }
        other => panic!(
            "durable fixture semantic drift: second graph node is not the fixture message: {other:?}"
        ),
    }
    match &nodes[2].payload {
        SessionNodePayload::Plugin { plugin_type, body } => {
            assert_eq!(plugin_type, "durable-read-plugin");
            assert_eq!(
                body.as_ref(),
                &serde_json::json!({"fixture": true, "order": 2}),
                "durable fixture semantic drift: plugin node body changed"
            );
        }
        other => panic!(
            "durable fixture semantic drift: third graph node is not the fixture plugin: {other:?}"
        ),
    }
}

async fn assert_process_change_feed(processes: &dyn ProcessRegistry) {
    let (first, first_cursor) = processes
        .processes_changed_since(ProcessChangeCursor::initial(), 2)
        .await
        .expect("durable fixture drift: first process-change page failed");
    assert_eq!(first.len(), 2);
    assert!(first_cursor.store_sequence() > 0);
    let (second, final_cursor) = processes
        .processes_changed_since(first_cursor, 10)
        .await
        .expect("durable fixture drift: second process-change page failed");
    assert_eq!(second.len(), 1);
    assert!(final_cursor.store_sequence() > first_cursor.store_sequence());
    let (empty, stable_cursor) = processes
        .processes_changed_since(final_cursor, 10)
        .await
        .expect("durable fixture drift: terminal process-change page failed");
    assert!(empty.is_empty());
    assert_eq!(stable_cursor, final_cursor);

    let mut observed = BTreeMap::new();
    for change in first.into_iter().chain(second) {
        match change {
            ProcessChange::Upsert { record } => {
                observed.insert(record.id.clone(), "upsert".to_string());
            }
            ProcessChange::Deleted { tombstone } => {
                assert_eq!(tombstone.terminal_label, "completed");
                assert_eq!(tombstone.pruned_at_ms, FIXTURE_WRITE_MS);
                observed.insert(tombstone.process_id, "deleted".to_string());
            }
        }
    }
    assert_eq!(
        observed,
        BTreeMap::from([
            (ProcessId::from(PROCESS_ID), "upsert".to_string()),
            (ProcessId::from(TOMBSTONE_PROCESS_ID), "deleted".to_string()),
            (ProcessId::from(WAKE_PROCESS_ID), "upsert".to_string()),
        ]),
        "durable fixture semantic drift: ADR-0020 change-feed rows changed"
    );
}

fn fixture_session_request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

fn fixture_plugin_state() -> PluginState {
    PluginState {
        plugins: BTreeMap::from([(
            "durable-read-snapshot-plugin".to_string(),
            PluginNamespaceState {
                generation: 887,
                values: std::collections::BTreeMap::from([(
                    "state".into(),
                    serde_json::json!({"fixture": "plugin-state", "value": 887}),
                )]),
            },
        )]),
    }
}

fn fixture_effect_envelope() -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn(SESSION_ID, "durable-read-effect-turn", 7, 0),
            "durable-read-exec-effect",
            RuntimeEffectKind::ExecCode,
            "durable-read-exec-replay",
        ),
        RuntimeEffectCommand::ExecCode {
            language: "fixture".to_string(),
            code: "return 887".to_string(),
        },
    )
}

fn fixture_record_config_operation() -> OperationId {
    OperationId::new(
        ExecutionScope::runtime_operation(format!(
            "session:{SESSION_ID}:boundary:protocol-materialization"
        )),
        "record-config",
    )
}

fn fixture_state() -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: SessionId::from(SESSION_ID.to_string()),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}

fn fixture_append_nodes() -> Vec<SessionAppendNode> {
    vec![
        SessionAppendNode::message(lash_core::PluginMessage::text(
            lash_core::MessageRole::User,
            "durable read user message",
        )),
        SessionAppendNode::plugin(
            "durable-read-plugin",
            serde_json::json!({"fixture": true, "order": 2}),
        ),
    ]
}

fn fixture_process_env() -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        Default::default(),
        SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    )
}

pub fn expected_process_lease() -> lash_core::ProcessLease {
    lash_core::facade_support::registry_transitions::acquired_process_lease(
        &ProcessId::from(PROCESS_ID),
        &LeaseOwnerIdentity::opaque("durable-read-owner", "durable-read-incarnation"),
        1,
        FIXTURE_WRITE_MS,
        100,
    )
}

fn waiting_process_registration(env_ref: ProcessExecutionEnvRef) -> ProcessRegistration {
    ProcessRegistration::new(
        PROCESS_ID,
        ProcessInput::Engine {
            kind: "durable-read-engine".to_string(),
            payload: serde_json::json!({"fixture": "process"}),
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
    )
    .with_execution_env_ref(Some(env_ref))
    .with_identity(
        ProcessIdentity::new("durable-read-engine")
            .with_label(Some("Durable read fixture".to_string()))
            .with_definition(Some(serde_json::json!({"fixture": "process"}))),
    )
}

fn fixture_wait_state() -> WaitState {
    WaitState {
        kind: WaitKind::Signal {
            name: "fixture-ready".to_string(),
            event_type: "process.signal.fixture-ready".to_string(),
            key: "durable-read-wait-key".to_string(),
            ordinal: 1,
        },
        since_ms: 123,
    }
}

fn fixture_handover() -> PersistedSegmentHandover {
    PersistedSegmentHandover {
        segment_ordinal: 1,
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: "durable-read-program-v1".to_string(),
            engine_state: vec![8, 8, 7],
        },
    }
}

fn fixture_register_command(env_ref: ProcessExecutionEnvRef) -> TriggerCommand {
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), TriggerInputBinding::Event);
    TriggerCommand::Register {
        owner_scope: TriggerOwnerScope::session(SESSION_ID),
        actor: ProcessOriginator::session(SessionScope::new(SESSION_ID)),
        draft: TriggerSubscriptionDraft {
            subscription_key: TRIGGER_KEY.to_string(),
            env_ref,
            wake_target: Some(SessionScope::new(SESSION_ID)),
            name: Some("Durable read trigger".to_string()),
            source_type: "fixture.event".to_string(),
            source_key: "fixture-source".to_string(),
            source: serde_json::json!({"fixture": "source"}),
            payload_schema: LashSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"],
                "additionalProperties": false
            })),
            target: ProcessInput::Engine {
                kind: "durable-read-trigger-target".to_string(),
                payload: serde_json::json!({"fixture": "trigger"}),
            },
            target_identity: ProcessIdentity::new("durable-read-trigger-target")
                .with_label(Some("Durable read trigger target".to_string()))
                .with_definition(Some(serde_json::json!({"fixture": "trigger"}))),
            event_types: Vec::new(),
            input_template,
            target_label: Some("Durable read trigger target".to_string()),
        },
    }
}

async fn trigger_receipt(
    store: &dyn TriggerStore,
    operation_id: &str,
    command: TriggerCommand,
) -> lash_core::TriggerMutationReceipt {
    let outcome = store
        .execute_command(operation_id, command)
        .await
        .expect("execute fixture trigger command")
        .expect("fixture trigger command domain outcome");
    match outcome {
        TriggerCommandOutcome::Mutation { receipt } => *receipt,
        TriggerCommandOutcome::List { .. } | TriggerCommandOutcome::Prune { .. } => {
            panic!("fixture trigger command must return a mutation receipt")
        }
    }
}
