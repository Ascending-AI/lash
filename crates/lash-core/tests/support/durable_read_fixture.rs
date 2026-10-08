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
//! | Session graph and checkpoints | `graph_nodes`, `session_head`/`sessions`, `session_meta`, `blobs`, `runtime_turn_commits` | Ordered graph nodes and every payload field; checkpoint turn, usage, tool, plugin, and execution state; current and legacy receipt replay |
//! | Session retention | `session_revisions`, `pins`, `deleted_sessions` | `revisions`, deletion probe, and typed `SessionDeleted` refusal to reopen a retired id |
//! | Attachments | `attachment_referrer_edges`, `attachment_pending_writes`, `attachment_uploads`, SQLite `artifact_refs`, PostgreSQL's artifact table | The committed session's referrer edge plus process-execution-environment reference recovery |
//! | Receiver queue | `queued_work_batches`, `pending_turn_inputs` | Queue/input payloads, deterministic ids, and a cancelled command's tombstone answering its redelivery |
//! | Processes | `processes`, `process_events`, `process_change_clock`, `process_observers`, `process_tombstones` | Process state; every event payload; observers; paginated change feed; typed `ProcessNoLongerRetained` tombstone |
//!
//! The table names above omit PostgreSQL's `lash_` prefix where the logical name is
//! otherwise identical. PostgreSQL's artifact table is named by role rather than
//! spelled out: its literal name carries an integration-protocol infix that the
//! `integration_boundary` lint forbids naming in this crate's `Cargo.toml`, `src/`,
//! and `tests/`.
//!
//! ## Generators
//!
//! The release fixtures are captured at the cut by
//! `python3 scripts/capture_release_fixtures.py --regenerate`, which runs the two
//! ignored generators below and freezes their output under `fixtures/release/`.
//! Generation is deterministic: the generators fix the clock, signing secret,
//! operation ids, and other identity inputs,
//! and normalize the few values a store mints itself, so two runs produce
//! byte-identical artifacts.
//!
//! ```text
//! . ./env.sh
//! LASH_REGENERATE=1 \
//!   cargo test -p lash-internal-sqlite-store --locked \
//!   --test durable_read_fixture regenerate_sqlite_durable_fixture -- --ignored --exact
//! LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
//! LASH_REGENERATE=1 \
//!   cargo test -p lash-internal-postgres-store --locked \
//!   --test durable_read_fixture regenerate_postgres_durable_fixture -- --ignored --exact
//! ```
//!
//! The PostgreSQL generator writes only the dedicated `lash_durable_read_fixture`
//! schema of a caller-owned throwaway database, and uses Docker only for the
//! pinned `postgres:18-alpine` `pg_dump` client.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::{ProcessId, SessionId};
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::runtime::{
    DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkPayload, SessionCommand,
    load_process_execution_env, publish_process_execution_env,
};
use lash_core::{
    ArtifactReferrer, AttachmentId, AttachmentReferrers, AttachmentWrite, Clock, DeploymentStore,
    ExecutionScope, MessageOrigin, MessageRole, OperationId, PartKind, PendingTurnInputDraft,
    PluginNamespaceState, PluginState, ProcessAwaitOutput, ProcessChange, ProcessChangeCursor,
    ProcessCompletionAuthority, ProcessEventLogTestSupport as _, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessIdentity, ProcessInput, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessRegistry, ProcessStatus, ProjectionWatermark, ReferrerClaim, RuntimeCommit,
    RuntimeSessionState, SessionAppendNode, SessionCreationHead, SessionNodePayload, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, StoreError, TokenUsage, TurnInput,
    TurnInputIngress, WaitKind, WaitState,
};
use serde::{Deserialize, Serialize};

pub const SESSION_ID: &str = "durable-read-fixture";
/// The fixture format's declaration, carried in every [`ExpectedFixture`] and
/// checked by [`assert_semantics`], so a captured release fixture names the
/// format it was written in. Move it when [`ExpectedFixture`]'s shape changes.
///
/// version_guard(
///     roots(ExpectedFixture),
/// )
/// version_surface = "migrate"
/// format_outside_manifest = "versions captured test fixtures, not bytes a lash build writes"
/// version_unguarded = "a test fixture's own schema; no production writer or decoder exists to run the laws over"
pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 131;
pub const FIXTURE_WRITE_MS: u64 = 1_700_000_000_000;
pub const FIXTURE_READ_MS: u64 = FIXTURE_WRITE_MS + 1_000;

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

/// The attachment whose pending write carries [`FIXTURE_ATTACHMENT_WRITE_ID`].
pub const FIXTURE_ATTACHMENT_ID: &str = "durable-read-attachment";
/// The seed registers its three processes in this order on a registry
/// minting sequentially (`ProcessIdMint::sequential_for_testing`), so each id
/// is fixed by its registration ordinal (ADR 0107).
fn waiting_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

fn running_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(2)
}

fn tombstone_process_id() -> ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(3)
}

/// The key the waiting process is started under, so read-back can present the
/// same start again and be answered with the retained process.
const WAITING_PROCESS_START_KEY: &str = "durable-read-waiting-process";
const DELETED_SESSION_ID: &str = "durable-read-deleted-session";
const QUEUE_SOURCE_KEY: &str = "durable-read-queued-command";
const CANCELLED_SOURCE_KEY: &str = "durable-read-cancelled-command";

/// A session command filed under `source_key`.
fn fixture_command(source_key: &str, reason: &str) -> QueuedWorkBatchDraft {
    let mut draft = QueuedWorkBatchDraft::new(
        SESSION_ID,
        DeliveryPolicy::EarliestSafeBoundary,
        SessionCommand::RefreshToolCatalog {
            reason: reason.into(),
        },
    );
    draft.source_key = Some(source_key.to_string());
    draft
}

/// The fixture's one queued row.
fn fixture_queued_command() -> QueuedWorkBatchDraft {
    fixture_command(QUEUE_SOURCE_KEY, "durable read queued command")
}

/// The command the seed files and cancels: its tombstone answers a
/// redelivery.
fn fixture_cancelled_command() -> QueuedWorkBatchDraft {
    fixture_command(CANCELLED_SOURCE_KEY, "durable read cancelled command")
}
const INPUT_SOURCE_KEY: &str = "durable-read-input-source";

fn fixture_effect_outcome() -> lash_core::ProcessEffectOccurrence {
    lash_core::ProcessEffectOccurrence::new(
        "durable-read-tool-node",
        1,
        "tool:fixture",
        lash_core::ProcessEffectOutcomeClass::Failure,
        Some(lash_core::FailureCode::provider("durable-read-fixture")),
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
    pub process_envs: Arc<dyn ProcessExecutionEnvStore>,
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
    let lash_core::AttachmentWriteFence::Granted(attachment_permit) = handles
        .store
        .begin_attachment_write(&attachment_write)
        .await
        .expect("begin fixture attachment write")
    else {
        panic!("the fixture digest must grant its writer");
    };
    handles
        .store
        .complete_attachment_write(&attachment_write, attachment_permit)
        .await
        .expect("stamp fixture attachment upload");
    // Keep an independent pending attempt for the fixture generators' token
    // normalization and the durable pending-write shape, alongside the evidence.
    assert!(matches!(
        handles
            .store
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
    let legacy_operation = OperationId::new(
        ExecutionScope::runtime_operation("durable-read-legacy-commit"),
        "commit",
    );
    let mut legacy_commit_retry =
        RuntimeCommit::persisted_state_with_operation_for_testing(&loaded, legacy_operation);
    legacy_commit_retry = legacy_commit_retry.with_committed_attachments([attachment_id.clone()]);
    session
        .commit_runtime_state(legacy_commit_retry.clone())
        .await
        .expect("commit supported NULL-identity legacy-shaped receipt");

    let record_config_state = load_fixture_state(&session).await;
    let mut record_config_retry = RuntimeCommit::persisted_state_with_operation_for_testing(
        &record_config_state,
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
            &SessionId::from(SESSION_ID),
            &lash_core::Target::Revision(committed.head_revision),
        )
        .await
        .expect("pin fixture head revision through the catalog");

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
        .enqueue_queued_work(fixture_queued_command())
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
        .register_process_with_observers(
            registration,
            &[SessionId::fixture(SESSION_ID.to_string())],
        )
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

    lash_core::testing::process_execution_env_fixture(handles.process_envs.as_ref()).await;
    let running_process_id = handles
        .processes
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::json!({"fixture": "running"}),
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register fixture running process")
        .id;
    assert_eq!(running_process_id, self::running_process_id());

    let tombstone_process_id = handles
        .processes
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::json!({"fixture": "tombstone"}),
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
            ProcessCompletionAuthority::workflow_key(&tombstone_process_id),
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

    let cancelled_batch = session
        .enqueue_queued_work(fixture_cancelled_command())
        .await
        .expect("enqueue fixture command to cancel");
    // The fixture's first command stays open and owns the head, so the
    // command lane never reaches this one: its host cancel is its terminal
    // transition, and no head commit follows it.
    session
        .cancel_queued_work_batch(&cancelled_batch.batch_id)
        .await
        .expect("cancel fixture command")
        .expect("fixture command is open");

    let read = load_fixture_window(&session).await;
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
    // The head holds the namespace map, each entry naming its namespace's
    // values body, a component of its own (FIG-5301).
    let map = checkpoint
        .decode_component::<std::collections::BTreeMap<String, serde_json::Value>>(
            lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
        )
        .expect("decode durable fixture plugin namespace map")
        .expect("durable fixture semantic drift: plugin namespace map disappeared");
    let namespaces = map
        .into_iter()
        .map(|(plugin, mut entry)| {
            let values = checkpoint
                .decode_component::<serde_json::Value>(&format!(
                    "{}/{plugin}",
                    lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT
                ))
                .expect("decode durable fixture plugin namespace body")
                .expect("durable fixture semantic drift: plugin namespace body disappeared");
            entry
                .as_object_mut()
                .expect("a namespace entry is an object")
                .insert("values".to_owned(), values);
            (plugin, entry)
        })
        .collect::<serde_json::Map<_, _>>();
    assert_eq!(
        serde_json::Value::Object(namespaces),
        serde_json::to_value(fixture_plugin_state()).expect("encode expected plugin snapshot"),
        "durable fixture semantic drift: plugin snapshot content changed"
    );
    assert_eq!(
        checkpoint.component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        Some(&[0x46, 0x49, 0x47, 0x38, 0x38, 0x37][..]),
        "durable fixture semantic drift: execution-state component changed"
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

    let retained = handles
        .store
        .revisions(&SessionId::from(SESSION_ID))
        .await
        .expect("durable fixture drift: retained-revision read failed");
    assert!(
        retained.last().is_some_and(|revision| revision.head),
        "durable fixture semantic drift: the session's head is not its newest retained revision"
    );
    let pinned = retained
        .iter()
        .filter(|revision| !revision.pinned_by.is_empty())
        .collect::<Vec<_>>();
    let [pinned] = pinned.as_slice() else {
        panic!("durable fixture semantic drift: the one pinned revision is {pinned:?}");
    };
    assert_eq!(
        pinned.leaf_node_id.as_deref(),
        Some(
            expected
                .node_ids_in_read_order
                .last()
                .expect("fixture expected graph has a leaf")
                .as_str()
        )
    );
    assert_eq!(
        pinned.pinned_by,
        vec![lash_core::Target::Revision(pinned.head_revision)],
        "durable fixture semantic drift: the revision's pin changed"
    );
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
        fixture_record_config_operation(),
    );
    changed_retry.config.model = Some(lash_core::testing::test_llm_profile_config(
        "durable-read-changed-model",
        lash_core::testing::test_llm_profile_metadata("durable-read-changed-model"),
    ));
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
    assert_eq!(queued[0].source_key.as_deref(), Some(QUEUE_SOURCE_KEY));
    assert!(
        matches!(
            &queued[0].payload,
            QueuedWorkPayload::SessionCommand { command }
                if matches!(
                    command.as_ref(),
                    SessionCommand::RefreshToolCatalog { reason }
                        if reason == "durable read queued command"
                )
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
    assert!(pending[0].input.state.is_next_turn_input(None));
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
    assert_eq!(process.status(), ProcessStatus::Waiting);
    assert_eq!(process.wait(), Some(&fixture_wait_state()));
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
    assert_eq!(
        process_events[0].fact.event_type(),
        "process.observer_added"
    );
    assert_eq!(
        process_events[0].fact.payload(),
        serde_json::json!({
            "by": {"kind": "host", "operation_id": "registration"},
            "session": SESSION_ID,
        }),
        "durable fixture semantic drift: observer-added event payload changed"
    );
    assert_eq!(process_events[1].sequence, 2);
    assert_eq!(process_events[1].fact.event_type(), "process.first_started");
    assert_eq!(process_events[2].sequence, 3);
    assert_eq!(process_events[2].fact.event_type(), "process.waiting");
    assert_eq!(
        process_events[2].fact.payload(),
        serde_json::json!({"wait": fixture_wait_state()}),
        "durable fixture semantic drift: waiting-process event payload changed"
    );
    assert_eq!(
        process_events[3].fact.event_type(),
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE
    );
    assert_eq!(
        lash_core::ProcessEffectOccurrence::decode(
            process_events[3].fact.payload(),
            lash_core::FleetFormat::current()
        )
        .expect("decode durable fixture effect outcome"),
        fixture_effect_outcome()
    );
    assert_eq!(
        process_events[4].fact.event_type(),
        lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE
    );
    assert_eq!(
        lash_core::ProcessEffectOmissions::decode(
            process_events[4].fact.payload(),
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
            &[SessionId::fixture(SESSION_ID.to_string())],
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
            .get_process(&running_process_id())
            .await
            .expect("durable fixture drift: running process read failed")
            .expect("durable fixture drift: running process disappeared")
            .status(),
        ProcessStatus::Running
    );
    // The cancelled command's tombstone answers its redelivery, and nothing
    // reopens (ADR 0101 §8).
    let redelivery = session
        .enqueue_queued_work_with_outcome(fixture_cancelled_command())
        .await
        .expect("durable fixture drift: a cancelled command's redelivery is refused");
    assert!(
        matches!(
            &redelivery,
            lash_core::runtime::QueuedWorkEnqueueOutcome::Existing(batch) if batch.terminal.is_some()
        ),
        "durable fixture drift: cancelled command was redelivered: {redelivery:?}"
    );

    match handles.processes.get_process(&tombstone_process_id()).await {
        Err(lash_core::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            assert_eq!(terminal_label, lash_core::RetiredProcessStatus::Completed);
            assert_eq!(pruned_at_ms, FIXTURE_WRITE_MS);
        }
        other => panic!(
            "durable fixture drift: process tombstone did not return ProcessNoLongerRetained: {other:?}"
        ),
    }
    assert_process_change_feed(handles.processes.as_ref()).await;
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
        } => {
            assert_eq!(
                frame_key,
                &lash_core::FrameKey::from_caller_material("initial-frame")
                    .expect("non-empty initial frame material")
            );
            assert_eq!(reason.as_str(), "initial");
            assert_eq!(assignment.policy.model, None);
            assert_eq!(assignment.policy.context_window_tokens(), None);
            assert!(!assignment.policy.autonomous);
            assert_eq!(
                assignment.policy.turn_budget,
                lash_core::TurnBudget::Unbounded
            );
            assert_eq!(
                serde_json::to_value(&assignment.plugin_config)
                    .expect("encode frame plugin config"),
                serde_json::json!({})
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

/// `delivered` is the process the fixture occurrence started with its
/// delivery, in the same commit.
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
                assert_eq!(
                    tombstone.terminal_label,
                    lash_core::RetiredProcessStatus::Completed
                );
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
            (running_process_id(), "upsert".to_string()),
        ]),
        "durable fixture semantic drift: ADR-0020 change-feed rows changed"
    );
}

fn fixture_session_request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        config: SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
        .into(),
        head: SessionCreationHead::Config,
    }
}

fn fixture_plugin_state() -> PluginState {
    PluginState {
        plugins: BTreeMap::from([(
            "durable-read-snapshot-plugin".to_string(),
            PluginNamespaceState {
                format_version: lash_core::FormatVersion::ONE,
                generation: 887,
                publication: Default::default(),
                fork: Default::default(),
                values: std::collections::BTreeMap::from([(
                    "state".into(),
                    serde_json::json!({"fixture": "plugin-state", "value": 887}),
                )])
                .into(),
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
        session_id: SessionId::fixture(SESSION_ID.to_string()),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
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
        SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ),
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
        kind: WaitKind::Call {
            call_id: lash_core::ToolCallId::fixture("durable-read-wait-call"),
            tool_id: lash_core::ToolId::from("durable-read-wait-tool"),
        },
        since_ms: 123,
    }
}
