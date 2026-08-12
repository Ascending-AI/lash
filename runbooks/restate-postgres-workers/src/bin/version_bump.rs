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
//!   store cannot boot.
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
use lash::process::{WakeDeliveryDriver, process_wake_source_key};
use lash_core::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEventAppendRequest,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvStore as _, ProcessIdentity,
    ProcessInput, ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessStatus,
    ProcessValueSelector, ProcessWakeSpec, RecoveryDisposition, SessionRelation, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory as _, TriggerCommand, TriggerOccurrenceRequest,
    TriggerOwnerScope, TriggerSubscriptionDraft,
};
use lash_postgres_store::PostgresStorage;
use serde_json::json;
use sqlx::PgPool;

const SCHEMA_COMPONENT: &str = "lash-postgres-store";
/// Sessions a live pre-bump deployment owned. `health` reopens the same ids on
/// the recreated store: identifiers are host-chosen and must survive a bump even
/// though their rows do not.
const SESSION_IDS: [&str; 2] = ["version-bump-live-alpha", "version-bump-live-beta"];
const PROCESS_ID: &str = "version-bump-live-process";
const WAKE_EVENT_TYPE: &str = "runbook.wake";
const TRIGGER_SOURCE_TYPE: &str = "runbook.button.pressed";
const TURN_PROMPT: &str = "commit one turn";

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
        RecoveryDisposition::ExternallyOwned,
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
    let provider = lash_core::testing::TestProvider::builder()
        .kind("version-bump-recreation")
        .complete(|_request| async {
            let text = "<lashlang>\nfinish \"ok\"\n</lashlang>";
            Ok(lash::provider::LlmResponse {
                full_text: text.to_string(),
                parts: vec![lash_core::LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..lash::provider::LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::new(
            lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
            lash_protocol_rlm::ExecutionBound::secs(30),
            lash_protocol_rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
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

    let session = core
        .session(session_id)
        .open()
        .await
        .with_context(|| format!("open session `{session_id}`"))?;
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
            ProcessAwaitOutput::Success {
                value: json!("trigger-delivered process finished"),
                control: None,
            },
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

    // Rewind only the ledger. The catalog intentionally retains the current
    // artifacts so the next phase can prove Lash distinguishes divergence from
    // the genuine component-50 migration source shape.
    let recorded = expected_version - 1;
    stamp_version(&pool, recorded).await?;

    emit(json!({
        "checkpoint": "seeded_older_deployment",
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
    emit(json!({
        "checkpoint": "refused_divergent_store",
        "direction": "recorded predecessor, current schema artifacts",
        "found_version": divergent,
        "expected_version": expected_version,
        "opened": opened,
        "error": error,
    }));

    // Remove the current-only artifacts to leave the published component-50
    // catalog, then stamp a non-migratable older version. This makes the next
    // refusal and recreation exercise an older shape rather than merely another
    // integer over the current catalog.
    sqlx::query("DROP TABLE lash_tool_intent_submissions")
        .execute(&pool)
        .await
        .context("remove current tool-intent artifact for older-store check")?;
    sqlx::query("DROP TABLE lash_process_parent_end_plans")
        .execute(&pool)
        .await
        .context("remove current parent-end artifact for older-store check")?;
    let current_artifact_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = current_schema()
            AND class.relname = ANY($1)",
    )
    .bind(vec![
        "idx_lash_tool_intent_submissions_scope",
        "lash_process_parent_end_plans",
        "lash_tool_intent_submissions",
    ])
    .fetch_one(&pool)
    .await
    .context("count current-only artifacts in the older-store fixture")?;
    anyhow::ensure!(
        current_artifact_count == 0,
        "older-store fixture retained {current_artifact_count} current-only artifacts"
    );

    // Versions older than the sole explicit migration remain the ordinary
    // reject-and-recreate boundary. Leave this stamp in place after all refusal
    // checks so the next phase exercises recreation from that path.
    let older = divergent - 1;
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
    emit(json!({
        "checkpoint": "refused_older_store",
        "direction": "new binary, non-migratable older version",
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
    emit(json!({
        "checkpoint": "refused_newer_store",
        "direction": "older binary, recreated store",
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

    emit(json!({
        "checkpoint": "recreated_store",
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
            ProcessAwaitOutput::Success {
                value: json!("post-bump process finished"),
                control: None,
            },
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
