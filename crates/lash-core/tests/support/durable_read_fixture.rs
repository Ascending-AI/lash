//! Durable read fixture: seed, round trip and generators.
//!
//! [`seed`] writes one session's worth of durable state through every store
//! surface the fixture covers, and returns the [`ExpectedFixture`] it wrote.
//! [`assert_semantics`] reads that state back through the supported read and
//! replay surfaces and requires the same public meaning.
//!
//! Each backend holds one round-trip law over these: seed a fresh store, reopen
//! its handles at [`FIXTURE_READ_MS`], and assert the semantics of what was just
//! written. Before 1.0 no durable data survives an upgrade, so there is no
//! committed artifact to read back; the fixture proves that what this build
//! writes, it can read.
//!
//! ## Coverage
//!
//! Every application-owned table has at least one row after [`seed`].
//! Assertions use supported read/replay surfaces, not row counts, for semantic
//! coverage.
//!
//! | Durable area | Populated tables | Supported read or refusal asserted |
//! | --- | --- | --- |
//! | Session graph and checkpoints | `graph_nodes`, `session_head`/`sessions`, `session_meta`, `blobs`, `usage_deltas`, `runtime_turn_commits` | Ordered graph nodes and every payload field; checkpoint turn, usage, tool, plugin, and execution state; current and legacy receipt replay |
//! | Session retention | `node_anchors`, `deleted_sessions` | `fork_points`, deletion probe, and typed `SessionDeleted` refusal to reopen a retired id |
//! | Attachments | `attachment_manifest`, SQLite `artifact_refs`, PostgreSQL's artifact table | Manifest listing plus process-execution-environment reference recovery |
//! | Receiver queue | `queued_work_batches`, `queued_work_items`, `pending_turn_inputs`, `wake_redelivery_fences` | Queue/input payloads, deterministic ids, and typed wake-rewind refusal |
//! | Processes | `processes`, `process_events`, `process_change_clock`, `process_observers`, `process_segment_handovers`, `process_tombstones`, `process_wake_deliveries`, `wake_allocation_floors` | Process state; every event payload; observers; continuation; wake delivery/floor; paginated change feed; typed `ProcessNoLongerRetained` tombstone |
//! | Triggers | `trigger_subscriptions`, `trigger_occurrences`, `trigger_deliveries`, `trigger_mutation_receipts` | List/filter, delivery reservation, deterministic receipt replay, and `Unchanged` re-registration |
//!
//! The table names above omit PostgreSQL's `lash_` prefix where the logical name is
//! otherwise identical. PostgreSQL's artifact table is named by role rather than
//! spelled out: its literal name carries an integration-protocol infix that the
//! `integration_boundary` lint forbids naming in this crate's `Cargo.toml`, `src/`,
//! and `tests/`.
//!
//! The intentionally expired session lease is a raw durable generation fact.
//! Reading it proves decoding and identity continuity; it does not grant live
//! execution authority.
//!
//! ## Generators
//!
//! The release fixtures are captured at the cut by
//! `python3 scripts/capture_release_fixtures.py --regenerate`, which runs the two
//! ignored generators below and freezes their output under `fixtures/release/`.
//! Generation is deterministic: the generators fix the clock, signing secret,
//! lease nonces, trigger incarnation, operation ids, and other identity inputs,
//! and normalize the few values a store mints itself, so two runs produce
//! byte-identical artifacts.
//!
//! ```text
//! LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
//!   kiln run //crates/lash-sqlite-store:durable_read_fixture__test -- \
//!   regenerate_sqlite_durable_fixture --ignored --exact
//! LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
//! LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
//!   kiln run //crates/lash-postgres-store:durable_read_fixture__test -- \
//!   regenerate_postgres_durable_fixture --ignored --exact
//! ```
//!
//! The PostgreSQL generator writes only the dedicated `lash_durable_read_fixture`
//! schema of a caller-owned throwaway database, and uses Docker only for the
//! pinned `postgres:16-alpine` `pg_dump` client.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::{ProcessId, SessionId};
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::runtime::{
    QueuedWorkPayload, load_process_execution_env, process_wake_batch_draft,
    publish_process_execution_env,
};
use lash_core::{
    ArtifactReferrer, AttachmentId, AttachmentReferrers, AttachmentWrite, BoundaryReason, Clock,
    DeploymentStore, ExecutionScope, LashSchema, MessageOrigin, MessageRole, OperationId, PartKind,
    PendingTurnInputDraft, PersistedSegmentHandover, PluginNamespaceState, PluginState,
    ProcessAwaitOutput, ProcessChange, ProcessChangeCursor, ProcessCompletionAuthority,
    ProcessContinuationStore, ProcessEventAppendRequest, ProcessEventLogTestSupport as _,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessExecutionWriteAuthority, ProcessIdentity, ProcessInput,
    ProcessOriginator, ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessRegistry,
    ProcessStatus, ProcessValueSelector, ProcessWakeDelivery, ProcessWakeSpec, ProjectionWatermark,
    ProtocolTurnOptions, ReferrerClaim, RuntimeCommit, RuntimeSessionState, SegmentHandover,
    SessionAppendNode, SessionCreationHead, SessionNodePayload, SessionPolicy, SessionRelation,
    SessionScope, SessionStoreCreateRequest, StoreError, TokenLedgerEntry, TokenUsage,
    TriggerCommand, TriggerCommandOutcome, TriggerDeliveryReservation,
    TriggerDeliveryReservationOutcome, TriggerInputBinding, TriggerMutationOutcome,
    TriggerOccurrenceFilter, TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft, TriggerSubscriptionFilter, TurnInput, TurnInputIngress, WaitKind,
    WaitState,
};
use serde::{Deserialize, Serialize};

pub const SESSION_ID: &str = "durable-read-fixture";
/// The fixture format's declaration, carried in every [`ExpectedFixture`] and
/// checked by [`assert_semantics`], so a captured release fixture names the
/// format it was written in. Move it when [`ExpectedFixture`]'s shape changes.
pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 131;
pub const FIXTURE_WRITE_MS: u64 = 1_700_000_000_000;
pub const FIXTURE_READ_MS: u64 = FIXTURE_WRITE_MS + 1_000;
pub const FIXTURE_PARENT_END_OBLIGATION_ID: &str = "parent_end:00000000000040008000000000000887";

/// Fixed stand-in for the await-event signing secret each store mints from
/// system randomness when it first creates its schema.
///
/// The fixture also pins a minted attachment write token and parent-end
/// obligation id. Left alone, they make regeneration nondeterministic and
/// defeat the no-diff double-regeneration proof. The generators replace only
/// their fixture copies; production continues minting fresh values.
#[allow(
    dead_code,
    reason = "only a store whose engine journals await-event promises pins their signing secret"
)]
pub const FIXTURE_AWAIT_EVENT_SIGNING_SECRET: [u8; 32] = [0x88; 32];

/// Fixed stand-in for the attachment write token minted by
/// `begin_attachment_write` while seeding the fixture.
///
/// Spelled as the 32-character lowercase hexadecimal encoding the durable
/// stores persist, and shaped like the v4 UUID the production token is drawn
/// from so a reader cannot mistake the column's domain. See
/// [`FIXTURE_AWAIT_EVENT_SIGNING_SECRET`] for why it is pinned.
pub const FIXTURE_ATTACHMENT_WRITE_ID: &str = "88888888888848888888888888888888";

/// The attachment whose manifest row carries [`FIXTURE_ATTACHMENT_WRITE_ID`].
pub const FIXTURE_ATTACHMENT_ID: &str = "durable-read-attachment";
/// The seed registers its three processes in this order on a registry
/// minting sequentially (`ProcessIdMint::sequential_for_testing`), so each id
/// is fixed by its registration ordinal (ADR 0107).
fn waiting_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

fn wake_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(2)
}

fn tombstone_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(3)
}

/// The key the waiting process is started under, so read-back can present the
/// same start again and be answered with the retained process.
const WAITING_PROCESS_START_KEY: &str = "durable-read-waiting-process";
const DELETED_SESSION_ID: &str = "durable-read-deleted-session";
const TRIGGER_KEY: &str = "durable-read-trigger";
const TRIGGER_REGISTER_OPERATION: &str = "durable-read-trigger-register";
const QUEUE_WAKE_PROCESS: &str = "durable-read-queue-process";

/// The fixture's queued wake names this process.
fn queue_wake_process() -> lash_core::runtime::ProcessId {
    lash_core::runtime::ProcessId::fixture(QUEUE_WAKE_PROCESS)
}

/// The fixture's one queued row: a process wake, the one turn-work payload.
fn fixture_wake() -> lash_core::runtime::ProcessWakeDelivery {
    let process_id = queue_wake_process();
    lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "durable-read-queue-wake".to_string(),
        target_session_id: SessionId::from(SESSION_ID),
        process_id: process_id.clone(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::runtime::RuntimeInvocation {
            attribution: lash_core::runtime::RuntimeAttribution::for_session(SESSION_ID),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id,
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: "durable read queued task".to_string(),
        created_at_ms: FIXTURE_WRITE_MS,
    }
}
const INPUT_SOURCE_KEY: &str = "durable-read-input-source";

fn fixture_effect_outcome() -> lash_core::ProcessEffectOccurrence {
    lash_core::ProcessEffectOccurrence::new(
        "durable-read-tool-node",
        1,
        "tool:fixture",
        lash_core::ProcessEffectOutcomeClass::Failure,
        Some(
            lash_core::TriggerOperationError::Invalid {
                message: "fixture".to_string(),
            }
            .failure_code(),
        ),
        "durable-read-tool-effect:1",
        lash_core::FleetFormat::current(),
    )
}

const FIXTURE_EFFECT_OMISSIONS_KEY: &str = "durable-read-effect-omissions";

fn fixture_effect_omissions() -> lash_core::ProcessEffectOmissions {
    lash_core::ProcessEffectOmissions::new(
        std::collections::BTreeMap::from([(
            "durable-read-tool-node".to_string(),
            lash_core::ProcessEffectOmittedCounts {
                success: 3,
                failure: 1,
                cancelled: 0,
            },
        )]),
        lash_core::FleetFormat::current(),
    )
}

pub struct FixtureHandles {
    pub clock: Arc<dyn Clock>,
    /// The deployment's catalog store; the fixture session is a view of it.
    pub store: Arc<dyn DeploymentStore>,
    pub processes: Arc<dyn lash_core::ConformanceProcessRegistry>,
    pub continuations: Arc<dyn ProcessContinuationStore>,
    pub process_envs: Arc<dyn ProcessExecutionEnvStore>,
    pub triggers: Arc<dyn TriggerStore>,
}

impl FixtureHandles {
    /// The fixture session's view of the catalog.
    fn session(&self) -> lash_core::store::SessionStore {
        let runtime: Arc<dyn lash_core::store::RuntimeStore> = self.store.clone();
        lash_core::store::SessionStore::new(runtime, SessionId::from(SESSION_ID))
            .expect("the fixture session id is valid")
    }
}

/// The fixture session's durable state at its current window.
async fn load_fixture_state(session: &lash_core::store::SessionStore) -> RuntimeSessionState {
    lash_core::store::load_session_window_state(session, lash_core::store::WindowSelector::Current)
        .await
        .expect("load fixture session state")
        .expect("fixture session exists")
        .state
}

/// The fixture session's current window.
async fn load_fixture_window(
    session: &lash_core::store::SessionStore,
) -> lash_core::store::SessionWindowRead {
    session
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await
        .expect("durable fixture drift: public session read failed")
        .expect("durable fixture drift: session disappeared")
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
    pub wake_delivery: ProcessWakeDelivery,
    /// The trigger subscription, occurrence and delivery payloads as this build
    /// writes them (FIG-1485). One field covers all three tables:
    /// [`TriggerDeliveryReservation`] carries the occurrence and subscription
    /// records whole. The read-back assertions on them are deliberately shallow
    /// — subscription key, enabled flag, reservation status, occurrence payload
    /// — so an additive field anywhere else in those payloads used to land
    /// unflagged, which is FIG-1377's class of change in a different store.
    pub trigger_delivery: TriggerDeliveryReservation,
    /// The projected registration payload of the waiting process (FIG-1485).
    /// A re-registration is answered by its start key and compares no content,
    /// so pinning the whole record is what makes the payload's shape visible.
    pub waiting_process: ProcessRecord,
}

fn assert_fixture_schema_version(found: u32) {
    assert_eq!(
        found, DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        "durable fixture schema version changed without regeneration"
    );
}

pub async fn seed(handles: &FixtureHandles) -> ExpectedFixture {
    handles
        .store
        .admit_session(&fixture_session_request(&SessionId::from(SESSION_ID)))
        .await
        .expect("admit the fixture session");
    let session = handles.session();
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
    session
        .commit_runtime_state(current_append_retry.clone())
        .await
        .expect("commit identity-bearing fixture append");

    let attachment_id = AttachmentId::parse(FIXTURE_ATTACHMENT_ID).expect("valid attachment id");
    let attachment_write = AttachmentWrite {
        attachment_id: attachment_id.clone(),
        claim: ReferrerClaim::unguarded(ArtifactReferrer::Session(SessionId::from(SESSION_ID)))
            .expect("fixture session attachment claim"),
    };
    let lash_core::AttachmentWriteFence::Granted(attachment_permit) = session
        .begin_attachment_write(&attachment_write)
        .await
        .expect("begin fixture attachment write")
    else {
        panic!("the fixture digest must grant its writer");
    };
    session
        .complete_attachment_write(&attachment_write, attachment_permit)
        .await
        .expect("stamp fixture attachment upload");
    // Keep an independent pending attempt for the fixture generators' token
    // normalization and the durable pending-write shape, alongside the evidence.
    assert!(matches!(
        session
            .begin_attachment_write(&attachment_write)
            .await
            .expect("begin fixture pending attachment write"),
        lash_core::AttachmentWriteFence::Granted(_)
    ));

    let mut loaded = load_fixture_state(&session).await;
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
    loaded.set_execution_state_snapshot(Some(vec![0x46, 0x49, 0x47, 0x38, 0x38, 0x37].into()));
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
    session
        .commit_runtime_state(legacy_commit_retry.clone())
        .await
        .expect("commit supported NULL-identity legacy-shaped receipt");

    let record_config_state = load_fixture_state(&session).await;
    let mut record_config_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &record_config_state,
        &[],
        fixture_record_config_operation(),
    );
    record_config_retry
        .stamp_semantic_boundary()
        .expect("stamp fixture semantic-boundary identity");
    session
        .commit_runtime_state(record_config_retry.clone())
        .await
        .expect("commit fixture semantic-boundary receipt");

    let committed = load_fixture_window(&session).await;
    handles
        .store
        .pin(
            committed
                .window
                .leaf_node_id
                .as_ref()
                .expect("fixture graph has a leaf"),
        )
        .await
        .expect("pin fixture leaf through the catalog");

    let deleted_request = fixture_session_request(&SessionId::from(DELETED_SESSION_ID));
    handles
        .store
        .admit_session(&deleted_request)
        .await
        .expect("admit fixture session that will be retired");
    handles
        .store
        .delete_session(&SessionId::from(DELETED_SESSION_ID))
        .await
        .expect("retire fixture session through the catalog");

    let queued = session
        .enqueue_queued_work(lash_core::runtime::process_wake_batch_draft(fixture_wake()))
        .await
        .expect("enqueue fixture queued work");
    let pending = session
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
    let host_claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("fixture host pin claim");
    let process_env_ref =
        publish_process_execution_env(handles.process_envs.as_ref(), &host_claim, &process_env)
            .await
            .expect("persist fixture process execution environment");
    let registration = waiting_process_registration(process_env_ref.clone());
    let waiting = handles
        .processes
        .register_process_with_observers(registration, &[SessionId::from(SESSION_ID.to_string())])
        .await
        .expect("register waiting fixture process");
    assert_eq!(
        waiting.id,
        waiting_process_id(),
        "the seed's registry mints sequentially"
    );
    let authority =
        ProcessExecutionWriteAuthority::invocation(waiting_process_id(), "durable-read-fixture")
            .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("bound fixture invocation has a started fact");
    handles
        .processes
        .record_first_started_with_authority(&waiting_process_id(), started, &authority)
        .await
        .expect("record fixture invocation start");
    handles
        .processes
        .set_process_wait_with_authority(
            &waiting_process_id(),
            fixture_wait_state(),
            Vec::new(),
            &authority,
        )
        .await
        .expect("persist fixture process wait state");
    handles
        .processes
        .append_event_with_authority(
            &waiting_process_id(),
            fixture_effect_outcome().append_request(),
            &authority,
        )
        .await
        .expect("persist fixture effect outcome");
    handles
        .processes
        .append_event_with_authority(
            &waiting_process_id(),
            fixture_effect_omissions().append_request(FIXTURE_EFFECT_OMISSIONS_KEY),
            &authority,
        )
        .await
        .expect("persist fixture effect omissions");
    handles
        .continuations
        .put_segment_handover(&waiting_process_id(), fixture_handover())
        .await
        .expect("persist fixture continuation");

    let wake_process_id = handles
        .processes
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::json!({"fixture": "wake"}),
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
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
        .expect("register fixture wake process")
        .id;
    assert_eq!(wake_process_id, self::wake_process_id());
    let wake_append = handles
        .processes
        .append_event(
            &wake_process_id,
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

    let tombstone_process_id = handles
        .processes
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::json!({"fixture": "tombstone"}),
            },
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register fixture process to prune")
        .id;
    assert_eq!(tombstone_process_id, self::tombstone_process_id());
    handles
        .processes
        .complete_process(
            &tombstone_process_id,
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

    let wake_batch = session
        .enqueue_queued_work(process_wake_batch_draft(wake_delivery.clone()))
        .await
        .expect("enqueue fixture process wake at receiver");
    let queue_admission = lash_core::store::AdmissionId::new("durable-read-queue-admission");
    let queue_epoch = session
        .drive_epoch()
        .await
        .expect("read fixture drive epoch");
    match session
        .seal_drive_epoch(
            &queue_admission,
            queue_epoch.epoch,
            &lash_core::store::RootStartNonce::new(queue_admission.as_str()),
        )
        .await
        .expect("seal fixture queue drive")
    {
        lash_core::store::DriveEpochSeal::Sealed(_) => {}
        other => panic!("fixture queue drive did not seal: {other:?}"),
    }
    // The receiver wake sits behind the fixture's queued work, which stays
    // pending, so the turn lane never reaches it: its host cancel is the
    // terminal transition that persists the redelivery fence (FIG-3545).
    session
        .cancel_queued_work_batch(&wake_batch.batch_id)
        .await
        .expect("cancel fixture receiver wake")
        .expect("fixture receiver wake is open");
    let wake_state = load_fixture_state(&session).await;
    let wake_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-wake-settlement"),
        "commit",
    );
    let wake_commit =
        RuntimeCommit::persisted_state_with_operation_for_testing(&wake_state, &[], wake_operation);
    session
        .commit_runtime_state(wake_commit)
        .await
        .expect("commit the fixture head after the receiver wake's cancel");
    let retained_admission = lash_core::store::AdmissionId::new("durable-read-retained-admission");
    let retained_epoch = session
        .drive_epoch()
        .await
        .expect("read fixture drive epoch");
    assert!(matches!(
        session
            .seal_drive_epoch(
                &retained_admission,
                retained_epoch.epoch,
                &lash_core::store::RootStartNonce::new(retained_admission.as_str()),
            )
            .await
            .expect("seal retained fixture drive"),
        lash_core::store::DriveEpochSeal::Sealed(_)
    ));

    let read = load_fixture_window(&session).await;
    let seeded_occurrences = handles
        .triggers
        .list_occurrences(TriggerOccurrenceFilter::default())
        .await
        .expect("read seeded fixture trigger occurrence");
    let [seeded_occurrence] = seeded_occurrences.as_slice() else {
        panic!("fixture seeds exactly one trigger occurrence");
    };
    let seeded_deliveries = handles
        .triggers
        .list_deliveries_by_occurrence_id(&seeded_occurrence.occurrence_id)
        .await
        .expect("read seeded fixture trigger delivery");
    let [trigger_delivery] = seeded_deliveries.as_slice() else {
        panic!("fixture seeds exactly one trigger delivery");
    };
    let waiting_process = handles
        .processes
        .get_process(&waiting_process_id())
        .await
        .expect("read seeded fixture waiting process")
        .expect("fixture waiting process exists after seeding");
    ExpectedFixture {
        fixture_schema_version: DURABLE_READ_FIXTURE_SCHEMA_VERSION,
        head_revision: read.head_revision,
        node_ids_in_read_order: read
            .window
            .nodes
            .iter()
            .map(|node| node.node_id.to_string())
            .collect(),
        current_append_retry,
        legacy_commit_retry,
        record_config_retry,
        queue_batch_id: queued.batch_id.to_string(),
        pending_input_id: pending.input_id.to_string(),
        process_env_ref,
        wake_delivery,
        trigger_delivery: trigger_delivery.clone(),
        waiting_process,
    }
}

pub async fn assert_semantics(handles: &FixtureHandles, expected: &ExpectedFixture) {
    assert_fixture_schema_version(expected.fixture_schema_version);
    let session = handles.session();
    let read = load_fixture_window(&session).await;
    assert_eq!(
        read.head_revision, expected.head_revision,
        "durable fixture semantic drift: head revision changed"
    );
    let node_ids = read
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        node_ids, expected.node_ids_in_read_order,
        "durable fixture semantic drift: graph node ids or order changed"
    );
    assert_graph_payloads(&read.window.nodes);
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
    let usage_rows = session
        .load_usage_ledger_page(None, std::num::NonZeroU32::new(10).expect("a nonzero page"))
        .await
        .expect("durable fixture drift: usage ledger read failed");
    assert_eq!(usage_rows.rows.len(), 1);
    assert!(usage_rows.next.is_none());
    assert_eq!(
        usage_rows.rows[0].entry.usage,
        TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
        "durable fixture semantic drift: usage ledger totals changed"
    );
    assert_eq!(
        AttachmentReferrers::attachment_referrers(
            handles.store.as_ref(),
            &AttachmentId::parse(FIXTURE_ATTACHMENT_ID).expect("valid attachment id"),
        )
        .await
        .expect("read fixture attachment referrers"),
        vec![ArtifactReferrer::Session(SessionId::from(SESSION_ID))],
        "durable fixture semantic drift: committed session attachment edge disappeared"
    );

    let pinned = handles
        .store
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
        matches!(
            handles
                .store
                .lookup_session(&SessionId::from(DELETED_SESSION_ID))
                .await
                .expect("durable fixture drift: deleted-session probe failed"),
            lash_core::store::SessionLookup::Deleted
        ),
        "durable fixture semantic drift: session tombstone disappeared"
    );
    match handles
        .store
        .admit_session(&fixture_session_request(&SessionId::from(
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

    let stored_epoch = session
        .drive_epoch()
        .await
        .expect("durable fixture drive epoch read");
    assert_eq!(stored_epoch.epoch, 2);
    assert_eq!(
        stored_epoch.admission.as_ref().map(|id| id.as_str()),
        Some("durable-read-retained-admission")
    );

    let current_replay = session
        .commit_runtime_state(expected.current_append_retry.clone())
        .await
        .expect("durable fixture identity drift: current append receipt no longer replays");
    assert!(
        current_replay.receipt_replayed,
        "durable fixture identity drift: current append receipt was applied instead of replayed"
    );
    let legacy_replay = session
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
    let semantic_replay = session
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
    let rebuilt_state = load_fixture_state(&session).await;
    let mut rebuilt_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &rebuilt_state,
        &[],
        fixture_record_config_operation(),
    );
    rebuilt_retry
        .stamp_semantic_boundary()
        .expect("stamp rebuilt fixture semantic-boundary identity");
    let rebuilt_replay = session.commit_runtime_state(rebuilt_retry).await.expect(
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
    let refused = session.commit_runtime_state(changed_retry).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::SemanticBoundaryIdentityConflict { .. })
        ),
        "durable fixture identity drift: differing semantic-boundary content was not refused: \
         {refused:?}"
    );

    let queued = session
        .list_queued_work()
        .await
        .expect("durable fixture drift: queued-work read failed");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, expected.queue_batch_id);
    assert_eq!(
        queued[0].source_key,
        Some(lash_core::runtime::process_wake_source_key(
            &queue_wake_process(),
            1
        ))
    );
    assert_eq!(queued[0].items.len(), 1);
    assert!(
        matches!(
            &queued[0].items[0].payload,
            QueuedWorkPayload::ProcessWake { wake }
                if wake.process_id == queue_wake_process()
                    && wake.input == "durable read queued task"
        ),
        "durable fixture semantic drift: queued-work payload changed"
    );
    let pending = session
        .list_pending_turn_inputs()
        .await
        .expect("durable fixture drift: pending-input read failed");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input.input_id, expected.pending_input_id);
    assert_eq!(
        pending[0].input.source_key.as_deref(),
        Some(INPUT_SOURCE_KEY)
    );
    assert!(pending[0].input.state.is_next_turn_pending());
    assert_eq!(
        serde_json::to_value(&pending[0].input.input).expect("encode fixture pending input"),
        serde_json::to_value(TurnInput::text("durable read pending input"))
            .expect("encode expected pending input"),
        "durable fixture semantic drift: pending-input payload changed"
    );

    let process = handles
        .processes
        .get_process(&waiting_process_id())
        .await
        .expect("durable fixture drift: process read failed")
        .expect("durable fixture drift: process disappeared");
    assert_eq!(process.status, ProcessStatus::Waiting);
    assert_eq!(process.wait.as_ref(), Some(&fixture_wait_state()));
    assert_eq!(process.env_ref.as_ref(), Some(&expected.process_env_ref));
    assert_eq!(
        process, expected.waiting_process,
        "durable fixture drift: the projected registration payload recovered from the committed \
         rows is not the one the expectations carry"
    );
    let process_events = handles
        .processes
        .full_event_window(&waiting_process_id(), 0)
        .await
        .expect("durable fixture drift: waiting-process event read failed");
    assert_eq!(process_events.len(), 5);
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
    assert_eq!(process_events[1].event_type, "process.first_started");
    assert_eq!(process_events[2].sequence, 3);
    assert_eq!(process_events[2].event_type, "process.waiting");
    assert_eq!(
        process_events[2].payload,
        serde_json::json!({"wait": fixture_wait_state()}),
        "durable fixture semantic drift: waiting-process event payload changed"
    );
    assert_eq!(
        process_events[3].event_type,
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
    );
    assert_eq!(
        lash_core::ProcessEffectOccurrence::decode(
            process_events[3].payload.clone(),
            lash_core::FleetFormat::current()
        )
        .expect("decode durable fixture effect outcome"),
        fixture_effect_outcome()
    );
    assert_eq!(
        process_events[4].event_type,
        lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE
    );
    assert_eq!(
        lash_core::ProcessEffectOmissions::decode(
            process_events[4].payload.clone(),
            lash_core::FleetFormat::current()
        )
        .expect("decode durable fixture effect omissions"),
        fixture_effect_omissions()
    );
    assert_eq!(
        handles
            .processes
            .observers_for_process(&waiting_process_id())
            .await
            .expect("durable fixture drift: process-observer read failed"),
        vec![SESSION_ID.to_string()],
        "durable fixture semantic drift: process-observer edge changed"
    );
    assert_eq!(
        handles
            .continuations
            .latest_segment_handover(&waiting_process_id())
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
        .expect("durable fixture identity drift: a start under a retained key failed");
    assert_eq!(
        reregistered.id, process.id,
        "durable fixture identity drift: a start under a retained key answers the retained process"
    );
    assert_eq!(
        handles
            .processes
            .get_process(&wake_process_id())
            .await
            .expect("durable fixture drift: wake process read failed")
            .expect("durable fixture drift: wake process disappeared")
            .status,
        ProcessStatus::Running
    );
    let wake_events = handles
        .processes
        .full_event_window(&wake_process_id(), 0)
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
            .any(|delivery| delivery.wake.process_id == wake_process_id()),
        "durable fixture semantic drift: process wake delivery disappeared"
    );
    assert_eq!(
        handles
            .processes
            .wake_allocation_floor_for_testing(&SessionId::from(SESSION_ID), &wake_process_id())
            .await
            .expect("durable fixture drift: wake-allocation-floor read failed"),
        Some(1),
        "durable fixture semantic drift: sender wake allocation floor changed"
    );
    let redelivery = session
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

    match handles.processes.get_process(&tombstone_process_id()).await {
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
    assert!(subscriptions[0].lifecycle.enabled());
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
    assert!(deliveries[0].subscription.lifecycle.enabled());
    assert_eq!(
        deliveries[0].reservation_status,
        TriggerDeliveryReservationOutcome::AlreadyReserved
    );
    assert_eq!(
        deliveries[0].occurrence.payload,
        serde_json::json!({"value": 42})
    );
    assert_eq!(
        deliveries[0], expected.trigger_delivery,
        "durable fixture drift: the trigger subscription, occurrence or delivery payload recovered \
         from the committed rows is not the one the expectations carry"
    );
    assert_eq!(
        subscriptions[0], expected.trigger_delivery.subscription,
        "durable fixture drift: the subscription read directly disagrees with the one the \
         delivery row projects"
    );
    assert_eq!(
        occurrences[0], expected.trigger_delivery.occurrence,
        "durable fixture drift: the occurrence read directly disagrees with the one the delivery \
         row projects"
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
}

fn assert_graph_payloads(nodes: &[std::sync::Arc<lash_core::SessionNodeRecord>]) {
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
            assert_eq!(message.parts[0].kind(), PartKind::Text);
            assert_eq!(message.parts[0].content(), "durable read user message");
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
                &serde_json::json!({"fixture": true, "order": 2, "output": fixture_tool_output()}),
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
            (waiting_process_id(), "upsert".to_string()),
            (tombstone_process_id(), "deleted".to_string()),
            (wake_process_id(), "upsert".to_string()),
        ]),
        "durable fixture semantic drift: ADR-0020 change-feed rows changed"
    );
}

fn fixture_session_request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        config: SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
        head: SessionCreationHead::CommittedByCreator,
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
            serde_json::json!({"fixture": true, "order": 2, "output": fixture_tool_output()}),
        ),
    ]
}

fn fixture_tool_output() -> lash_core::ToolCallOutput {
    lash_core::ToolCallOutput::success(serde_json::json!({"fixture": "raw"})).with_view(
        lash_core::ToolView {
            blocks: vec![lash_core::ToolViewBlock::Text {
                text: "durable read authored view".to_string(),
                meta: lash_core::ToolViewMeta::default(),
            }],
        },
    )
}

fn fixture_process_env() -> ProcessExecutionEnvSpec {
    let mut env = ProcessExecutionEnvSpec::new(
        Default::default(),
        SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    );
    env.render = Some(lash_core::RecordedRender {
        renderer_id: "standard".to_string(),
        params: serde_json::json!({"max_chars": 120}),
    });
    env
}

fn waiting_process_registration(env_ref: ProcessExecutionEnvRef) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "durable-read-engine".to_string(),
            payload: serde_json::json!({"fixture": "process"}),
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(
        WAITING_PROCESS_START_KEY,
    )))
    .with_execution_env_ref(Some(env_ref))
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(
                "durable-read-engine",
                serde_json::json!({"fixture": "process"}),
            ),
            Some("Durable read fixture".to_string()),
        ),
    ))
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
        writer: String::new(),
        segment_ordinal: 1,
        written_generation: Some(lash_core::engine::BuildGeneration::for_test("t0")),
        route: "LashProcessWorkflow".to_string(),
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
            source_capture: lash_core::TriggerSourceCapture::provider(
                ["fixture", "event"],
                LashSchema::new(serde_json::json!({
                    "type": "object",
                    "properties": {"fixture": {"type": "string"}},
                    "additionalProperties": false
                })),
                "fixture-provider",
                serde_json::json!({"account": "fixture"}),
            ),
            target: ProcessInput::Engine {
                kind: "durable-read-trigger-target".to_string(),
                payload: serde_json::json!({"fixture": "trigger"}),
            },
            target_identity: ProcessIdentity::for_definition(
                lash_core::ProcessDefinitionRef::unclaimed(
                    "durable-read-trigger-target",
                    serde_json::json!({"fixture": "trigger"}),
                ),
                Some("Durable read trigger target".to_string()),
            ),
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
