//! Version-bump recreation harness (`runbooks/version-bump-recreation`).
//!
//! Four phases, one process each, driven by
//! `scripts/version-bump-recreation-e2e.sh`:
//!
//! * `seed` — create the store this binary expects, put live sessions, a live
//!   background process with a pending wake, and a fired trigger delivery in it,
//!   then rewind only `lash_schema_versions`. The result deliberately diverges:
//!   its live catalog has current artifacts while its ledger names the one
//!   explicitly migratable predecessor.
//! * `refuse` — prove that divergence gets its own typed refusal before DDL,
//!   prove a genuinely older version remains a reject-and-recreate boundary,
//!   and prove a store stamped one version *ahead* is refused just as hard. The
//!   last direction is the forward-only claim: an old binary meeting a recreated
//!   store cannot boot. Each direction names the refusal *kind* it exists to
//!   prove and fails on any other, so a fixture that drifts off its generation
//!   cannot pass on someone else's refusal.
//! * `recreate` — perform the recreation bump (drop every `lash_*` object, then
//!   open), and record that nothing seeded survived it.
//! * `health` — verify the three durable surfaces on the recreated store, reusing
//!   the pre-bump session ids: a session opens and a turn commits, a background
//!   process takes a wake through the queued-work rail and reaches a terminal,
//!   and a trigger fires and starts its target process.
//!
//! Every phase prints one JSON `checkpoint` line; the shell runner asserts on
//! those lines and keeps them as artifacts.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use lash::persistence::QueuedWorkStore as _;
use lash::preflight::PreflightOptions;
use lash::process::{WakeDeliveryDriver, process_wake_source_key};
use lash_core::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEventAppendRequest,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvStore as _, ProcessIdentity,
    ProcessInput, ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessStatus,
    ProcessValueSelector, ProcessWakeSpec, RecoveryContract, SessionRelation, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory as _, TriggerCommand, TriggerOccurrenceRequest,
    TriggerOwnerScope, TriggerSubscriptionDraft,
};
use lash_postgres_store::{PostgresStorage, PostgresStorePreflight};
use serde_json::json;
use sqlx::PgPool;

const SCHEMA_COMPONENT: &str = "lash-postgres-store";
/// The oldest component version this build has an explicit migration from
/// (`lash_postgres_store::postgres::schema::SCHEMA_MIGRATIONS`). Anything below
/// it is the ordinary reject-and-recreate boundary, which is what the
/// older-store refusal exists to prove — so the fixture stamps a version under
/// this floor, never one the build would happily migrate.
///
/// This constant and the four artifact lists below are pinned to the newest
/// component generation. `scripts/check_version_bump_fixtures.py` derives every
/// one of them from `SCHEMA_MIGRATIONS` and fails when a bump moves the
/// component without moving them, so they are never discovered stale by a live
/// run.
const MIGRATION_FLOOR_VERSION: i32 = 50;
/// Tables the component generations *above* the floor introduced, newest first
/// (60 adds no table; 59: turn-cancel requests; 58 adds no table; 57: checkpoint edges; 56 and
/// 55 add no table; 54: the effect-group journal;
/// 52: attachment GC fence; 51: parent-end plans and tool-intent submissions).
/// Dropping them leaves the published floor
/// catalog: the set is exactly the floor migration's `source_missing_tables`.
const POST_FLOOR_TABLES: [&str; 6] = [
    "lash_turn_cancel_requests",
    "lash_checkpoint_blob_refs",
    "lash_runtime_effect_group",
    "lash_attachment_condemnations",
    "lash_tool_intent_submissions",
    "lash_process_parent_end_plans",
];
/// Indexes those generations added to tables the floor catalog already had, so
/// dropping the post-floor tables does not take them with it (60: the session
/// state inventory index; 57: the two root
/// indexes; 56: trigger reclaim eligibility; 55: the drain's
/// unsettled-children index; 54: the settlement uniqueness guard, both on the
/// effect-replay table; 53: the ingress-family ordering pair). This list is the
/// post-floor `introduced_relations` that are not
/// themselves post-floor tables and do not belong to one, which is what
/// `scripts/check_version_bump_fixtures.py` proves.
const POST_FLOOR_INDEXES: [&str; 9] = [
    "idx_lash_session_meta_state_version",
    "idx_lash_session_meta_catalog",
    "idx_lash_sessions_checkpoint_ref",
    "idx_lash_node_anchors_checkpoint_ref",
    "idx_lash_queued_work_session_command_order",
    "idx_lash_pending_turn_input_order",
    "uq_lash_runtime_effect_replay_group_seq",
    "idx_lash_runtime_effect_replay_group_unsettled",
    "idx_lash_trigger_occurrences_reclaimable",
];
/// Columns those generations added to tables the floor catalog already had, so
/// dropping the post-floor tables does not take them with it either (54: the two
/// effect-group columns on the effect-replay table; 56: the occurrence
/// eligibility arm; 58: the session-enumeration metadata). Without this axis the
/// "published floor catalog" the fixture reconstructs is a floor catalog with
/// current-generation columns bolted on, and the refusal it proves is not the
/// one a genuinely older store gets. The set is exactly the floor migration's
/// `source_missing_columns`, which `scripts/check_version_bump_fixtures.py`
/// proves.
const POST_FLOOR_COLUMNS: [(&str, &str); 11] = [
    ("lash_session_meta", "session_state_version"),
    ("lash_runtime_effect_replay", "group_key"),
    ("lash_runtime_effect_replay", "settlement_seq"),
    ("lash_trigger_occurrences", "reclaimable_at_ms"),
    ("lash_session_meta", "created_at_ms"),
    ("lash_session_meta", "last_commit_at_ms"),
    ("lash_deleted_sessions", "created_at_ms"),
    ("lash_deleted_sessions", "last_commit_at_ms"),
    ("lash_deleted_sessions", "head_revision"),
    ("lash_deleted_sessions", "relation_kind"),
    ("lash_deleted_sessions", "parent_session_id"),
];
/// Every post-floor relation, for proving the fixture retained none of them: the
/// floor migration's `introduced_relations`.
const POST_FLOOR_ARTIFACTS: [&str; 19] = [
    "lash_turn_cancel_requests",
    "idx_lash_session_meta_state_version",
    "idx_lash_session_meta_catalog",
    "lash_checkpoint_blob_refs",
    "idx_lash_checkpoint_blob_refs_blob_ref",
    "idx_lash_sessions_checkpoint_ref",
    "idx_lash_node_anchors_checkpoint_ref",
    "idx_lash_pending_turn_input_order",
    "idx_lash_queued_work_session_command_order",
    "idx_lash_runtime_effect_group_scope",
    "idx_lash_runtime_effect_group_session",
    "idx_lash_runtime_effect_replay_group_unsettled",
    "idx_lash_tool_intent_submissions_scope",
    "idx_lash_trigger_occurrences_reclaimable",
    "lash_attachment_condemnations",
    "lash_process_parent_end_plans",
    "lash_runtime_effect_group",
    "lash_tool_intent_submissions",
    "uq_lash_runtime_effect_replay_group_seq",
];
/// What the newest generation alone introduced — the `introduced_relations` of
/// the migration out of the immediate predecessor version. The divergent fixture
/// records that predecessor over the *current* catalog, so these are exactly the
/// artifacts its refusal must enumerate.
const DIVERGENT_ARTIFACTS: [&str; 1] = ["lash_session_meta_pending_observer_intents"];
/// Sessions a live pre-bump deployment owned. `health` reopens the same ids on
/// the recreated store: identifiers are host-chosen and must survive a bump even
/// though their rows do not.
const SESSION_IDS: [&str; 2] = ["version-bump-live-alpha", "version-bump-live-beta"];
const PROCESS_ID: &str = "version-bump-live-process";
const WAKE_EVENT_TYPE: &str = "runbook.wake";
const TRIGGER_SOURCE_TYPE: &str = "runbook.button.pressed";
const TURN_PROMPT: &str = "commit one turn";

/// Prose that only the divergence refusal carries
/// (`schema_migration_divergence_error`).
const DIVERGENT_ARTIFACTS_MARKER: &str = "schema artifacts newer than the recorded version";
/// Prose that only the migration-source-shape refusal carries
/// (`schema_migration_source_mismatch_error`).
const SOURCE_MISMATCH_MARKER: &str = "does not match the published component-";
/// Prose that only the plain exact-match refusal carries
/// (`version_mismatch_error`).
const NO_APPLICABLE_MIGRATION_MARKER: &str = "has no applicable migration";

/// Which typed refusal the schema gate produced.
///
/// Every refusal phase exists to prove one specific gate, and the kinds are not
/// interchangeable: a fixture that drifts off its intended generation can be
/// refused for a *different* reason and still look like a pass. Each phase names
/// the kind it exists to prove and fails on any other, so a refusal can never be
/// counted as evidence for a claim it does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefusalKind {
    /// The recorded version has a migration, but the live catalog already holds
    /// the relations that migration would create.
    DivergentArtifacts,
    /// The recorded version has a migration and the catalog does not diverge,
    /// but the live shape is not the published source shape.
    MigrationSourceMismatch,
    /// No migration applies to the recorded version at all: the ordinary
    /// reject-and-recreate boundary, in either direction.
    NoApplicableMigration,
}

impl RefusalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::DivergentArtifacts => "divergent_artifacts",
            Self::MigrationSourceMismatch => "migration_source_mismatch",
            Self::NoApplicableMigration => "no_applicable_migration",
        }
    }

    /// Classify a refusal by the marker only its own error carries. Zero or more
    /// than one match is itself a failure: the harness never guesses which claim
    /// a refusal supports.
    fn classify(message: &str) -> Result<Self> {
        let matched: Vec<Self> = [
            (DIVERGENT_ARTIFACTS_MARKER, Self::DivergentArtifacts),
            (SOURCE_MISMATCH_MARKER, Self::MigrationSourceMismatch),
            (NO_APPLICABLE_MIGRATION_MARKER, Self::NoApplicableMigration),
        ]
        .into_iter()
        .filter(|(marker, _)| message.contains(marker))
        .map(|(_, kind)| kind)
        .collect();
        match matched.as_slice() {
            [kind] => Ok(*kind),
            _ => bail!(
                "refusal matched {} known refusal kinds, not exactly one: {message}",
                matched.len()
            ),
        }
    }
}

/// Assert a refusal is the exact gate the calling phase exists to prove.
fn refusal_kind(phase: &str, expected: RefusalKind, message: &str) -> Result<RefusalKind> {
    let found = RefusalKind::classify(message)?;
    anyhow::ensure!(
        found == expected,
        "the {phase} refusal was {}, not the {} refusal this phase exists to prove: {message}",
        found.as_str(),
        expected.as_str()
    );
    Ok(found)
}

#[tokio::main]
async fn main() -> Result<()> {
    let phase = std::env::args()
        .nth(1)
        .context("usage: lash-e2e-version-bump seed|refuse|recreate|health")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;

    match phase.as_str() {
        "seed" => seed(&database_url).await,
        "refuse" => refuse(&database_url).await,
        "recreate" => recreate(&database_url).await,
        "health" => health(&database_url).await,
        other => bail!("unknown version-bump phase `{other}`"),
    }
}

/// The store-readability probe's answer for this deployment, serialized.
///
/// Read-only and pre-open by construction: the handle is built from the same
/// connection string rather than from a wired store, so asking costs nothing
/// the refusal it predicts would have to undo. Every phase below emits the
/// probe beside the refusal the store then delivers, which is what makes
/// "safe to start" evidence rather than a claim: the two answers are produced
/// by different code paths against the same bytes, and the artifacts show them
/// agreeing.
async fn probe(database_url: &str, options: PreflightOptions) -> Result<serde_json::Value> {
    let handle = PostgresStorePreflight::for_database_url(database_url)
        .context("build the read-only preflight handle")?;
    let report = lash::preflight::probe_store(&handle, options)
        .await
        .context("probe the store's durable readability")?;
    handle.close().await;
    serde_json::to_value(&report).context("serialize the preflight report")
}

fn emit(checkpoint: serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string(&checkpoint).expect("serialize checkpoint")
    );
}

async fn admin_pool(database_url: &str) -> Result<PgPool> {
    PgPool::connect(database_url)
        .await
        .context("connect version-bump admin pool")
}

async fn recorded_version(pool: &PgPool) -> Result<i32> {
    sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
        .bind(SCHEMA_COMPONENT)
        .fetch_one(pool)
        .await
        .context("read recorded component schema version")
}

async fn stamp_version(pool: &PgPool, version: i32) -> Result<()> {
    sqlx::query(
        "INSERT INTO lash_schema_versions (component, version) VALUES ($1, $2)
         ON CONFLICT (component) DO UPDATE SET version = EXCLUDED.version",
    )
    .bind(SCHEMA_COMPONENT)
    .bind(version)
    .execute(pool)
    .await
    .context("stamp component schema version")?;
    Ok(())
}

/// The exact-match gate, as an operator meets it: try to open and keep the
/// refusal text verbatim.
async fn open_attempt(database_url: &str) -> (bool, String) {
    match PostgresStorage::connect(database_url).await {
        Ok(_) => (true, String::new()),
        Err(err) => (false, err.to_string()),
    }
}

/// `expected N` in the refusal is the only in-band statement of the version this
/// binary requires; the harness never hardcodes the integer.
fn expected_version_from_refusal(message: &str) -> Result<i32> {
    let tail = message
        .rsplit_once("expected ")
        .map(|(_, tail)| tail)
        .with_context(|| format!("refusal did not name an expected version: {message}"))?;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits
        .parse()
        .with_context(|| format!("refusal named an unparseable expected version: {message}"))
}

fn wake_event_type() -> ProcessEventType {
    ProcessEventType {
        name: WAKE_EVENT_TYPE.to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: None,
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }
}

fn wake_registration(process_id: &str, wake_session_id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        process_id,
        ProcessInput::External {
            metadata: json!({"runbook": "version-bump-recreation"}),
        },
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
    )
    .with_identity(
        ProcessIdentity::new("version-bump")
            .with_label(Some(process_id.to_string()))
            .with_definition(Some(json!({"scenario": "version-bump-recreation"}))),
    )
    .with_extra_event_types([wake_event_type()])
    .with_wake_session_id(Some(wake_session_id.to_string()))
}

async fn create_sessions(storage: &PostgresStorage) -> Result<()> {
    let factory = storage.session_store_factory_with_shared_process_registry();
    for session_id in SESSION_IDS {
        factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .with_context(|| format!("create session store `{session_id}`"))?;
    }
    Ok(())
}

/// One real turn against the store under test: the provider is deterministic, the
/// commit is not. Returns the turn id whose commit row proves the turn landed.
async fn commit_one_turn(storage: &PostgresStorage, session_id: &str, tag: &str) -> Result<String> {
    let attachments = tempfile::tempdir().context("attachment dir for version-bump turn")?;
    // The row's dialect decides the scripted cell *and* the session's pin: a
    // TypeScript row that commits a Lashlang cell cannot execute it, and the
    // turn never reaches a terminal state.
    let dialect = lash_restate_postgres_workers_e2e::runbook_rlm_dialect()?;
    let scripted = lash_restate_postgres_workers_e2e::scripted_finish_cell(dialect, "\"ok\"");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("version-bump-recreation")
        .complete(move |_request| {
            let text = scripted.clone();
            async move {
                Ok(lash::provider::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..lash::provider::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(storage.lashlang_artifact_store()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("version-bump-mock")
                .context_window_tokens(200_000)
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .store_factory(Arc::new(
            storage.session_store_factory_with_shared_process_registry(),
        ))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path().to_path_buf(),
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(storage.process_env_store()))
        .process_registry(Arc::new(storage.process_registry()))
        .trigger_store(Arc::new(storage.trigger_store()))
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        // A boot UUID, not the PID: the contract is that the incarnation changes
        // on every process boot, and PIDs are reused.
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "version-bump-worker",
            uuid::Uuid::new_v4().to_string(),
        ))
        .context("build version-bump core")?;

    let session = {
        core.session(session_id)
            .plugin_option(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    dialect: Some(dialect),
                    ..lash::rlm::RlmCreateExtras::default()
                },
            )
            .context("state the row's dialect")?
            .open()
            .await
            .with_context(|| format!("open session `{session_id}`"))?
    };
    let turn_id = format!("version-bump-{tag}-{session_id}");
    let output = session
        .turn(lash::TurnInput::text(TURN_PROMPT))
        .turn_id(turn_id.clone())
        .run()
        .await
        .with_context(|| format!("run turn on session `{session_id}`"))?;
    anyhow::ensure!(
        output.final_value() == Some(&json!("ok")),
        "turn on `{session_id}` did not finish with the scripted value: {:?}",
        output.final_value()
    );
    Ok(turn_id)
}

/// Durable evidence that a turn landed: the session head advanced past its
/// created revision and the committed graph carries nodes.
async fn committed_session_facts(pool: &PgPool, session_id: &str) -> Result<(i64, i64)> {
    let head_revision: i64 =
        sqlx::query_scalar("SELECT head_revision FROM lash_sessions WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .context("read committed session head revision")?;
    let nodes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lash_graph_nodes WHERE session_id = $1 AND NOT tombstoned",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .context("count committed session graph nodes")?;
    Ok((head_revision, nodes))
}

/// One fired trigger, end to end at the store surface: register a subscription,
/// emit an occurrence against it, then register and finish the process the
/// reserved delivery names. The reservation is the durable "it fired" fact.
struct FiredTrigger {
    subscription_id: String,
    occurrence_id: String,
    reservations: usize,
    process_id: String,
    process_status: String,
}

async fn fire_trigger(storage: &PostgresStorage, tag: &str) -> Result<FiredTrigger> {
    let trigger_store = Arc::new(storage.trigger_store()) as Arc<dyn lash_core::TriggerStore>;
    let env_store = storage.process_env_store();
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().context("stable process env ref")?;
    env_store
        .put_process_execution_env(
            &env_ref,
            &spec.to_store_bytes().context("encode process env spec")?,
        )
        .await
        .context("store process execution env")?;

    let source = json!({"button": "Blue"});
    let source_key =
        lash_core::facade_support::default_trigger_source_key(TRIGGER_SOURCE_TYPE, &source);
    let draft = TriggerSubscriptionDraft::for_process(
        format!("version-bump-{tag}-subscription"),
        env_ref,
        TRIGGER_SOURCE_TYPE,
        source_key.clone(),
        ProcessInput::External {
            metadata: json!({"runbook": "version-bump-recreation"}),
        },
        ProcessIdentity::new("version-bump").with_label(Some(format!("trigger-target-{tag}"))),
    )
    .with_name(format!("version-bump-{tag}"))
    .with_source(source.clone())
    .with_payload_schema(lash_core::LashSchema::any());
    let subscription = trigger_store
        .execute_command(
            &format!("version-bump-{tag}-register"),
            TriggerCommand::Register {
                owner_scope: TriggerOwnerScope::session(SESSION_IDS[0]),
                actor: ProcessOriginator::session(SessionScope::new(SESSION_IDS[0])),
                draft,
            },
        )
        .await
        .context("register trigger subscription")?
        .map_err(|err| anyhow::anyhow!("trigger registration rejected: {err}"))?;
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = subscription else {
        bail!("trigger registration returned no mutation receipt");
    };
    anyhow::ensure!(
        receipt.enabled,
        "the registered trigger subscription is not enabled"
    );

    let ingress = trigger_store
        .ingest_occurrence(TriggerOccurrenceRequest::new(
            TRIGGER_SOURCE_TYPE,
            source_key,
            source,
            format!("version-bump-{tag}-occurrence"),
        ))
        .await
        .context("ingest trigger occurrence")?;
    let reservation = ingress
        .reservations
        .first()
        .context("the fired occurrence reserved no delivery")?;
    let process_id = reservation.process_id.clone();

    // The delivery names a process; run it to a terminal so the fired trigger
    // ends in durable work, not just a reservation row.
    let registry = Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>;
    registry
        .register_process(wake_registration(&process_id, SESSION_IDS[0]))
        .await
        .context("register the trigger-delivered process")?;
    registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(json!(
                "trigger-delivered process finished"
            ))),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .context("complete the trigger-delivered process")?;
    let record = registry
        .get_process(&process_id)
        .await
        .context("read the trigger-delivered process")?
        .context("trigger-delivered process row is absent")?;

    Ok(FiredTrigger {
        subscription_id: receipt.subscription_id.clone(),
        occurrence_id: ingress.occurrence.occurrence_id.clone(),
        reservations: ingress.reservations.len(),
        process_id,
        process_status: format!("{:?}", record.status),
    })
}

async fn seed(database_url: &str) -> Result<()> {
    let storage = PostgresStorage::connect(database_url)
        .await
        .context("create the pre-bump store")?;
    let pool = storage.pool().clone();
    let expected_version = recorded_version(&pool).await?;

    create_sessions(&storage).await?;
    let mut turn_ids = Vec::new();
    for session_id in SESSION_IDS {
        turn_ids.push(commit_one_turn(&storage, session_id, "seed").await?);
    }
    let mut committed_sessions = 0;
    let mut committed_nodes = 0;
    for session_id in SESSION_IDS {
        let (head_revision, nodes) = committed_session_facts(&pool, session_id).await?;
        if head_revision > 0 && nodes > 0 {
            committed_sessions += 1;
        }
        committed_nodes += nodes;
    }

    // A live background process with a wake still pending: in-flight work the
    // recreation is about to destroy.
    let registry = storage.process_registry();
    lash_core::ProcessRegistry::register_process(
        &registry,
        wake_registration(PROCESS_ID, SESSION_IDS[0]),
    )
    .await
    .context("register the live pre-bump process")?;
    let pending_wake = lash_core::ProcessRegistry::append_event(
        &registry,
        PROCESS_ID,
        ProcessEventAppendRequest::new(WAKE_EVENT_TYPE, json!({"wake_input": "pre-bump"})),
    )
    .await
    .context("append the live pre-bump wake")?
    .wake_delivery
    .context("pre-bump wake outbox row was not created")?;

    let trigger_report = fire_trigger(&storage, "seed").await?;
    let live_process = lash_core::ProcessRegistry::get_process(&registry, PROCESS_ID)
        .await
        .context("read the live pre-bump process")?
        .context("live pre-bump process row is absent")?;
    anyhow::ensure!(
        live_process.outcome.is_none(),
        "the seeded process is not live: {:?}",
        live_process.status
    );

    // The probe against the store exactly as this build wrote it: the bytes the
    // refusal phases will meet, before the ledger moves. Deep mode, because an
    // operator auditing ahead of a bump can afford the per-session blob walk
    // that a boot gate on every restart cannot.
    let probe_before_rewind = probe(database_url, PreflightOptions::deep()).await?;

    // Reconstruct the published component-61 receipt shape before rewinding its
    // ledger: the predecessor allowed independently-nullable append identity
    // fields and still carried the readerless requested-ancestor column.
    sqlx::query(
        "ALTER TABLE lash_runtime_turn_commits
             DROP CONSTRAINT lash_runtime_turn_commits_check,
             ADD COLUMN requested_ancestor_node_id TEXT",
    )
    .execute(&pool)
    .await
    .context("restore the component-61 append receipt shape")?;
    let recorded = expected_version - 1;
    stamp_version(&pool, recorded).await?;
    // The walk now sees the exact predecessor shape and stamp. The drain list
    // stays empty: this is a schema recreation boundary, not undecodable durable
    // payload.
    let probe_after_rewind = probe(database_url, PreflightOptions::deep()).await?;

    emit(json!({
        "checkpoint": "seeded_older_deployment",
        "probe_before_rewind": probe_before_rewind,
        "probe_after_rewind": probe_after_rewind,
        "expected_version": expected_version,
        "recorded_version": recorded_version(&pool).await?,
        "session_ids": SESSION_IDS,
        "process_ids": [PROCESS_ID],
        "trigger_subscription_id": trigger_report.subscription_id,
        "trigger_occurrence_id": trigger_report.occurrence_id,
        "trigger_reservations": trigger_report.reservations,
        "trigger_process_ids": [trigger_report.process_id],
        "turn_ids": turn_ids,
        "committed_sessions": committed_sessions,
        "committed_nodes": committed_nodes,
        "pending_wake_sequence": pending_wake.sequence,
        "recorded_after_rewind": recorded,
    }));
    Ok(())
}

async fn refuse(database_url: &str) -> Result<()> {
    let pool = admin_pool(database_url).await?;
    let divergent = recorded_version(&pool).await?;

    let (opened, error) = open_attempt(database_url).await;
    let expected_version = expected_version_from_refusal(&error)?;
    anyhow::ensure!(
        !opened,
        "the divergent store recorded at version {divergent} was opened instead of refused"
    );
    anyhow::ensure!(
        expected_version == divergent + 1,
        "the refusal expected {expected_version}, which is not one ahead of the recorded {divergent}"
    );
    let divergent_kind = refusal_kind("divergent-store", RefusalKind::DivergentArtifacts, &error)?;
    // The refusal must enumerate what the newest generation introduced, not
    // merely mention divergence: that list is what tells an operator which
    // artifacts to inspect.
    for artifact in DIVERGENT_ARTIFACTS {
        anyhow::ensure!(
            error.contains(artifact),
            "the divergence refusal did not enumerate {artifact}: {error}"
        );
    }
    // Summary mode here, deliberately: this is the shape a host runs at boot,
    // and the claim is that the cheap walk already refuses.
    let divergent_probe = probe(database_url, PreflightOptions::summary()).await?;
    emit(json!({
        "checkpoint": "refused_divergent_store",
        "probe": divergent_probe,
        "direction": "recorded predecessor, current schema artifacts",
        "refusal_kind": divergent_kind.as_str(),
        "divergent_artifacts": DIVERGENT_ARTIFACTS,
        "found_version": divergent,
        "expected_version": expected_version,
        "opened": opened,
        "error": error,
    }));

    // The migration floor predates component 61's graph-sequence hard cutover,
    // so restore that published column and index before removing later
    // creation-only artifacts.
    sqlx::query("ALTER TABLE lash_graph_nodes ADD COLUMN seq BIGSERIAL")
        .execute(&pool)
        .await
        .context("restore the migration-floor graph sequence column")?;
    sqlx::query("CREATE INDEX idx_lash_graph_nodes_seq ON lash_graph_nodes(session_id, seq)")
        .execute(&pool)
        .await
        .context("restore the migration-floor graph sequence index")?;

    // Remove every artifact introduced after the migration floor, leaving the
    // catalog the floor generation (`MIGRATION_FLOOR_VERSION`) published, then
    // stamp a version below it. This makes the next refusal and recreation
    // exercise a genuinely older *shape* rather than merely another integer over
    // the current catalog. These lists are generation-pinned: each component bump
    // that introduces a relation must add it here — a table to
    // `POST_FLOOR_TABLES`, an index over a table the floor already had to
    // `POST_FLOOR_INDEXES` — and to `POST_FLOOR_ARTIFACTS`, or the fixture
    // silently stops being the published floor shape. A column added to a table
    // the floor already had goes to `POST_FLOOR_COLUMNS` for the same reason;
    // `scripts/check_version_bump_fixtures.py` is what makes that impossible.
    for artifact in POST_FLOOR_TABLES {
        sqlx::query(&format!("DROP TABLE {artifact}"))
            .execute(&pool)
            .await
            .with_context(|| {
                format!("remove post-floor artifact {artifact} for older-store check")
            })?;
    }
    // `IF EXISTS`, because the table drops above may already have taken the index
    // with them: the derivation that decides which indexes land in this list
    // resolves `CREATE INDEX ... ON <table>` textually, and a DDL shape it cannot
    // read (a quoted or schema-qualified target) resolves to no table and is
    // conservatively listed here. The `current_artifact_count` probe below is what
    // actually proves the floor catalog is clean, so an already-gone index must not
    // fail the drop.
    for index in POST_FLOOR_INDEXES {
        sqlx::query(&format!("DROP INDEX IF EXISTS {index}"))
            .execute(&pool)
            .await
            .with_context(|| format!("remove post-floor index {index} for older-store check"))?;
    }
    for (table, column) in POST_FLOOR_COLUMNS {
        sqlx::query(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
            .execute(&pool)
            .await
            .with_context(|| {
                format!("remove post-floor column {table}.{column} for older-store check")
            })?;
    }
    for (table, column) in POST_FLOOR_COLUMNS {
        let survived: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = $1
                AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&pool)
        .await
        .context("probe post-floor columns in the older-store fixture")?;
        anyhow::ensure!(
            survived == 0,
            "older-store fixture retained the current-only column {table}.{column}"
        );
    }
    let current_artifact_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = current_schema()
            AND class.relname = ANY($1)",
    )
    .bind(POST_FLOOR_ARTIFACTS.to_vec())
    .fetch_one(&pool)
    .await
    .context("count post-floor artifacts in the older-store fixture")?;
    anyhow::ensure!(
        current_artifact_count == 0,
        "older-store fixture retained {current_artifact_count} current-only artifacts"
    );

    // Versions below every explicit migration's source remain the ordinary
    // reject-and-recreate boundary. This must stay *below* the floor, not merely
    // one behind the divergent stamp: this build migrates from both 50 and 51,
    // so either of those would be migrated rather than refused. Leave this stamp
    // in place after all refusal checks so the next phase exercises recreation
    // from that path.
    let older = MIGRATION_FLOOR_VERSION - 1;
    stamp_version(&pool, older).await?;
    let (opened_older, error_older) = open_attempt(database_url).await;
    anyhow::ensure!(
        !opened_older,
        "the genuinely older store at version {older} was opened instead of refused"
    );
    anyhow::ensure!(
        expected_version_from_refusal(&error_older)? == expected_version,
        "the older-version refusal disagrees about the version this binary expects"
    );
    // The claim this phase exists to prove. FIG-1259's incident was exactly here:
    // a fixture off its generation was refused for divergence instead, and the
    // phase passed on a refusal that proves nothing about the boundary.
    let older_kind = refusal_kind(
        "older-store",
        RefusalKind::NoApplicableMigration,
        &error_older,
    )?;
    let older_probe = probe(database_url, PreflightOptions::summary()).await?;
    emit(json!({
        "checkpoint": "refused_older_store",
        "probe": older_probe,
        "direction": "new binary, non-migratable older version",
        "refusal_kind": older_kind.as_str(),
        "found_version": older,
        "expected_version": expected_version,
        "current_artifact_count": current_artifact_count,
        "opened": opened_older,
        "error": error_older,
    }));

    // The forward-only direction: stamp the version a *newer* lash would have
    // created, and this binary is now the old image meeting a recreated store.
    let newer = expected_version + 1;
    stamp_version(&pool, newer).await?;
    let (opened_newer, error_newer) = open_attempt(database_url).await;
    anyhow::ensure!(
        !opened_newer,
        "a store at version {newer} was opened by a binary expecting {expected_version}"
    );
    anyhow::ensure!(
        expected_version_from_refusal(&error_newer)? == expected_version,
        "the two refusals disagree about the version this binary expects"
    );
    let newer_kind = refusal_kind(
        "newer-store",
        RefusalKind::NoApplicableMigration,
        &error_newer,
    )?;
    let newer_probe = probe(database_url, PreflightOptions::summary()).await?;
    emit(json!({
        "checkpoint": "refused_newer_store",
        "probe": newer_probe,
        "direction": "older binary, recreated store",
        "refusal_kind": newer_kind.as_str(),
        "found_version": newer,
        "expected_version": expected_version,
        "opened": opened_newer,
        "error": error_newer,
    }));

    // Leave the non-migratable older version in place so `recreate` proves that
    // path still reaches a clean current store.
    stamp_version(&pool, older).await?;
    Ok(())
}

async fn recreate(database_url: &str) -> Result<()> {
    let pool = admin_pool(database_url).await?;
    let (opened_before, error_before) = open_attempt(database_url).await;
    anyhow::ensure!(
        !opened_before,
        "the pre-bump store was not refused; the recreation step has no premise"
    );
    let expected_version = expected_version_from_refusal(&error_before)?;
    // `refuse` left the below-floor stamp in place, so the premise for recreation
    // is that boundary refusal and no other.
    let premise_kind = refusal_kind(
        "pre-bump",
        RefusalKind::NoApplicableMigration,
        &error_before,
    )?;
    let seeded_sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_sessions")
        .fetch_one(&pool)
        .await
        .context("count pre-bump sessions")?;
    let seeded_processes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_processes")
        .fetch_one(&pool)
        .await
        .context("count pre-bump processes")?;
    let seeded_nodes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_graph_nodes")
        .fetch_one(&pool)
        .await
        .context("count pre-bump graph nodes")?;

    // The recreation bump itself: drop every lash-owned object, which is what a
    // host does by handing the new binary an empty database.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%'
         ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .context("list lash-owned tables")?;
    anyhow::ensure!(
        !tables.is_empty(),
        "found no lash-owned tables to recreate from"
    );
    sqlx::query(&format!("DROP TABLE {} CASCADE", tables.join(", ")))
        .execute(&pool)
        .await
        .context("drop lash-owned tables")?;

    let storage = PostgresStorage::connect(database_url)
        .await
        .context("open the recreated store with the current binary")?;
    let recorded = recorded_version(storage.pool()).await?;
    let surviving_sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lash_sessions WHERE session_id = ANY($1)")
            .bind(SESSION_IDS.map(str::to_string).to_vec())
            .fetch_one(storage.pool())
            .await
            .context("count surviving seeded sessions")?;
    let surviving_processes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lash_processes WHERE process_id = $1")
            .bind(PROCESS_ID)
            .fetch_one(storage.pool())
            .await
            .context("count surviving seeded processes")?;
    let surviving_nodes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lash_graph_nodes WHERE session_id = ANY($1)")
            .bind(SESSION_IDS.map(str::to_string).to_vec())
            .fetch_one(storage.pool())
            .await
            .context("count surviving seeded graph nodes")?;

    // The other half of the controlled pair: on the store recreation just
    // produced, the same probe reports ready with an empty drain list. Without
    // this, every "refused" above would be consistent with a probe that refuses
    // everything.
    let recreated_probe = probe(database_url, PreflightOptions::deep()).await?;
    emit(json!({
        "checkpoint": "recreated_store",
        "probe": recreated_probe,
        "premise_refusal_kind": premise_kind.as_str(),
        "expected_version": expected_version,
        "recorded_version": recorded,
        "dropped_tables": tables.len(),
        "pre_bump_sessions": seeded_sessions,
        "pre_bump_processes": seeded_processes,
        "pre_bump_graph_nodes": seeded_nodes,
        "surviving_seeded_rows": surviving_sessions + surviving_processes + surviving_nodes,
        "surviving_seeded_graph_nodes": surviving_nodes,
    }));
    Ok(())
}

async fn health(database_url: &str) -> Result<()> {
    let storage = PostgresStorage::connect(database_url)
        .await
        .context("open the recreated store")?;
    let pool = storage.pool().clone();
    let recorded = recorded_version(&pool).await?;

    // Gate 1: the same session ids a live deployment used open on the recreated
    // store, and a turn commits.
    create_sessions(&storage).await?;
    let mut committed_sessions = 0;
    let mut committed_nodes = 0;
    for session_id in SESSION_IDS {
        commit_one_turn(&storage, session_id, "health").await?;
        let (head_revision, nodes) = committed_session_facts(&pool, session_id).await?;
        if head_revision > 0 && nodes > 0 {
            committed_sessions += 1;
        }
        committed_nodes += nodes;
    }
    let session_turn_committed = committed_sessions == SESSION_IDS.len();

    // Gate 2: a background process registers, its wake reaches the target
    // session through the queued-work rail, and the process reaches a terminal.
    let registry = Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>;
    let target_session = SESSION_IDS[1];
    registry
        .register_process(wake_registration(PROCESS_ID, target_session))
        .await
        .context("register the post-bump process")?;
    let wake = registry
        .append_event(
            PROCESS_ID,
            ProcessEventAppendRequest::new(WAKE_EVENT_TYPE, json!({"wake_input": "post-bump"})),
        )
        .await
        .context("append the post-bump wake")?
        .wake_delivery
        .context("post-bump wake outbox row was not created")?;
    let drive = WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::new(storage.session_store_factory_with_shared_process_registry())
            as Arc<dyn lash_core::SessionStoreFactory>,
        None,
        Arc::new(lash_core::facade_support::SystemClock),
        32,
    )
    .await
    .context("drive the post-bump wake")?;
    let delivered = storage
        .session_store(target_session)
        .list_queued_work(target_session)
        .await
        .context("list post-bump receiver rows")?
        .into_iter()
        .any(|batch| {
            batch.source_key.as_deref()
                == Some(process_wake_source_key(PROCESS_ID, wake.sequence)).as_deref()
        });
    registry
        .complete_process(
            PROCESS_ID,
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(json!(
                "post-bump process finished"
            ))),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .context("complete the post-bump process")?;
    let terminal = registry
        .get_process(PROCESS_ID)
        .await
        .context("read the post-bump process")?
        .context("post-bump process row is absent")?;
    let process_ran_to_terminal = drive.enqueued == 1
        && delivered
        && matches!(terminal.status, ProcessStatus::Completed)
        && terminal.outcome.is_some();

    // Gate 3: a trigger fires and starts its target process.
    let fired = fire_trigger(&storage, "health").await?;
    let trigger_fired = fired.reservations == 1 && fired.process_status == "Completed";

    emit(json!({
        "checkpoint": "verified_recreated_deployment",
        "recorded_version": recorded,
        "session_ids_reused": SESSION_IDS,
        "session_turn_committed": session_turn_committed,
        "committed_sessions": committed_sessions,
        "committed_nodes": committed_nodes,
        "process_ran_to_terminal": process_ran_to_terminal,
        "wake_enqueued": drive.enqueued,
        "wake_delivered_to_target": delivered,
        "process_status": format!("{:?}", terminal.status),
        "trigger_fired": trigger_fired,
        "trigger_subscription_id": fired.subscription_id,
        "trigger_occurrence_id": fired.occurrence_id,
        "trigger_reservations": fired.reservations,
        "trigger_process_ids": [fired.process_id],
        "trigger_process_status": fired.process_status,
    }));
    // Keep the harness honest: the phase fails loudly rather than reporting a
    // false gate.
    anyhow::ensure!(
        session_turn_committed && process_ran_to_terminal && trigger_fired,
        "post-bump verification did not pass every gate"
    );
    Ok(())
}
