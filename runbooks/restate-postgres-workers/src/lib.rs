use anyhow::{Context, Result, bail};
use lash::durability::EffectHost;
use lash::persistence::{
    AttachmentStore, LashlangArtifactStore, ProcessExecutionEnvStore, SessionStoreFactory,
};
use lash::plugins::{
    PluginExtensionContribution, PluginFactory, PluginRegistrar, PluginSessionContext,
    SessionPlugin,
};
use lash::process::{ProcessRegistry, ProcessWorkDriver};
use lash::rlm::{
    LASHLANG_SURFACE_EXTENSION_ID, LashlangAbilities, LashlangHostCatalog,
    LashlangLanguageFeatures, LashlangSurfaceContribution, NamedDataType, RlmProtocolPluginConfig,
    TypeExpr, TypeField,
};
use lash::tools::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition,
    ToolDefinitionLashlangExt, ToolProvider, ToolResult,
};
use lash_core::AwaitEventResolver as _;
use lash_provider_openai::OpenAiCompatibleProvider;
use lash_restate::RestateEffectHost;
use lash_s3_store::{S3AttachmentStore, S3AttachmentStoreConfig};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_SESSION_ID: &str = "restate-postgres-workers-e2e";
pub const ENGINE_RESTART_SLEEP_SESSION_ID: &str =
    "restate-postgres-workers-e2e-engine-restart-sleep";
pub const ENGINE_RESTART_SLEEP_WORKFLOW_ID: &str = "e2e-engine-restart-suspended-sleep";
pub const BREAK_GLASS_SESSION_ID: &str = "restate-postgres-workers-e2e-break-glass";
pub const BREAK_GLASS_WORKFLOW_ID: &str = "e2e-turn-break-glass";

/// Keep the restart timer independent from the pre-existing parked gate turn,
/// since one session intentionally admits only one active turn at a time.
pub fn turn_session_id(workflow_id: &str) -> &'static str {
    match workflow_id {
        ENGINE_RESTART_SLEEP_WORKFLOW_ID => ENGINE_RESTART_SLEEP_SESSION_ID,
        BREAK_GLASS_WORKFLOW_ID => BREAK_GLASS_SESSION_ID,
        _ => DEFAULT_SESSION_ID,
    }
}
pub const TURN_WORKFLOW_NAME: &str = "E2eTurnWorkflow";
pub const EXPECTED_FINAL_TEXT: &str = "kitchen-sink-complete";
pub const EXPECTED_WAKE_TEXT: &str = "wake-consumed";
pub const EXPECTED_ASYNC_TEXT: &str = "async-completion-complete";
pub const EXPECTED_DURABLE_INPUT_TEXT: &str = "durable-input-complete";
pub const EXPECTED_PARENT_DURABLE_INPUT_TEXT: &str = "parent-durable-input-complete";

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DirectDurableWaitResolveRequest {
    pub key: lash_core::AwaitEventKey,
    pub resolution: lash_core::Resolution,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DirectDurableWaitAwaitRequest {
    pub key: lash_core::AwaitEventKey,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DirectDurableWaitAwaitResponse {
    pub worker_id: String,
    pub resolution: lash_core::Resolution,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct DirectDurableWaitResolveResponse {
    pub worker_id: String,
    pub outcome: lash_core::ResolveOutcome,
}
pub const EXPECTED_TOOL_BATCH_TEXT: &str = "tool-batch-complete";
pub const EXPECTED_SEGMENT_LOOP_TEXT: &str = "segment-loop-complete";
pub const EXPECTED_FRAME_SWITCH_TEXT: &str = "frame-switch-complete";
pub const EXPECTED_FRAME_SWITCH_CANCEL_TEXT: &str = "frame-switch-cancel-complete";
pub const BUTTON_SOURCE_TYPE: &str = "ui.button.pressed";
pub const ATTACHMENT_MIME: &str = "image/png";
/// Durable turns cross the Restate handler, replay controller, Lash turn
/// driver, and provider/tool frames on one Tokio poll stack. The composed
/// `e2e-main` turn exhausted Tokio's 2 MiB default before the recorded-effect
/// frame was bounded. Provision 8 MiB: four times that observed failure point,
/// with headroom for substrate and runtime growth beyond the measured turn.
pub const E2E_PRODUCT_STACK_BUDGET_BYTES: usize = 8 * 1024 * 1024;
pub const LASH_E2E_TOKIO_STACK_BYTES_ENV: &str = "LASH_E2E_TOKIO_STACK_BYTES";

pub fn default_session_originator_id() -> String {
    DEFAULT_SESSION_ID.to_string()
}

pub fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

pub fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

pub fn e2e_tokio_thread_stack_bytes() -> Result<usize> {
    e2e_tokio_thread_stack_bytes_from_raw(std::env::var(LASH_E2E_TOKIO_STACK_BYTES_ENV).ok())
}

fn e2e_tokio_thread_stack_bytes_from_raw(raw: Option<String>) -> Result<usize> {
    let Some(raw) = raw else {
        return Ok(E2E_PRODUCT_STACK_BUDGET_BYTES);
    };
    let stack_bytes = raw.parse::<usize>().with_context(|| {
        format!("{LASH_E2E_TOKIO_STACK_BYTES_ENV} must be an integer byte count")
    })?;
    if stack_bytes > E2E_PRODUCT_STACK_BUDGET_BYTES {
        bail!(
            "{LASH_E2E_TOKIO_STACK_BYTES_ENV}={stack_bytes} exceeds durable-worker stack budget {E2E_PRODUCT_STACK_BUDGET_BYTES}"
        );
    }
    Ok(stack_bytes)
}

pub fn s3_store_from_env() -> Result<S3AttachmentStore> {
    S3AttachmentStore::from_config(S3AttachmentStoreConfig {
        endpoint_url: Some(env("MINIO_ENDPOINT", "http://minio:9000")),
        region: env("MINIO_REGION", "us-east-1"),
        bucket: env("MINIO_BUCKET", "lash-attachments"),
        prefix: Some(env("MINIO_PREFIX", "runbooks/restate-postgres-workers")),
        access_key_id: Some(env("MINIO_ACCESS_KEY", "minioadmin")),
        secret_access_key: Some(env("MINIO_SECRET_KEY", "minioadmin")),
        path_style: true,
    })
    .context("build S3 attachment store")
}

#[cfg(test)]
mod stack_policy_tests {
    use super::{E2E_PRODUCT_STACK_BUDGET_BYTES, e2e_tokio_thread_stack_bytes_from_raw};

    #[test]
    fn e2e_tokio_stack_policy_defaults_to_durable_worker_budget_and_rejects_growth() {
        assert_eq!(
            e2e_tokio_thread_stack_bytes_from_raw(None).expect("default stack budget"),
            E2E_PRODUCT_STACK_BUDGET_BYTES
        );
        assert_eq!(
            e2e_tokio_thread_stack_bytes_from_raw(Some(E2E_PRODUCT_STACK_BUDGET_BYTES.to_string()))
                .expect("exact stack budget"),
            E2E_PRODUCT_STACK_BUDGET_BYTES
        );
        let too_large = e2e_tokio_thread_stack_bytes_from_raw(Some(
            (E2E_PRODUCT_STACK_BUDGET_BYTES + 1).to_string(),
        ))
        .expect_err("larger product/e2e stacks must fail loudly");
        assert!(
            too_large
                .to_string()
                .contains("exceeds durable-worker stack budget"),
            "unexpected stack-budget error: {too_large:#}"
        );
        let invalid = e2e_tokio_thread_stack_bytes_from_raw(Some("not-a-number".to_string()))
            .expect_err("invalid stack byte count must fail");
        assert!(
            invalid
                .to_string()
                .contains("must be an integer byte count"),
            "unexpected parse error: {invalid:#}"
        );
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnScenario {
    #[default]
    KitchenSink,
    TriggerSetup,
    DrainQueued,
    SignalSuspend,
    SignalProcess,
    AsyncCompletion,
    ProcessLlmQuery,
    DurableInputRequest,
    ParentDurableInputAfterChild,
    ToolBatch,
    SegmentLoop,
    FrameSwitchQueued,
    FrameSwitchPrepared,
    FrameSwitchCancel,
    TurnControlHold,
    TurnControlSleep,
    TurnControlComplete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSignalRequest {
    pub process_id: String,
    pub signal_name: String,
    pub signal_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnRequest {
    pub workflow_id: String,
    #[serde(default)]
    pub fail_once: bool,
    #[serde(default)]
    pub scenario: TurnScenario,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ProcessSignalRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnResponse {
    pub workflow_id: String,
    pub worker_id: String,
    pub process_id: String,
    #[serde(default)]
    pub process_ids: Vec<String>,
    pub attachment_id: String,
    pub final_text: String,
    #[serde(default)]
    pub final_value: serde_json::Value,
    #[serde(default)]
    pub streamed_event_count: usize,
    #[serde(default)]
    pub replay_cursor: Option<String>,
    #[serde(default)]
    pub queued_turn_ran: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub worker_id: String,
    pub ok: bool,
}

pub async fn ensure_e2e_schema(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await.context("begin e2e schema transaction")?;
    sqlx::query("SELECT pg_advisory_xact_lock(715421, 907002)")
        .execute(&mut *tx)
        .await
        .context("acquire e2e schema lock")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_worker_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            detail_json TEXT NOT NULL DEFAULT '{}',
            created_at_ms BIGINT NOT NULL,
            UNIQUE (workflow_id, worker_id, event_type)
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e worker events table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_terminal_results (
            workflow_id TEXT PRIMARY KEY,
            process_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            attachment_id TEXT NOT NULL,
            final_text TEXT NOT NULL,
            submitted_json TEXT NOT NULL DEFAULT '{}',
            queued_turn_ran BOOLEAN NOT NULL DEFAULT FALSE,
            streamed_event_count BIGINT NOT NULL DEFAULT 0,
            replay_cursor TEXT,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e terminal results table")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS submitted_json TEXT NOT NULL DEFAULT '{}'",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e submitted_json column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS queued_turn_ran BOOLEAN NOT NULL DEFAULT FALSE",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e queued_turn_ran column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS streamed_event_count BIGINT NOT NULL DEFAULT 0",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e streamed_event_count column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS replay_cursor TEXT",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e replay_cursor column")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_failover_markers (
            workflow_id TEXT PRIMARY KEY,
            worker_id TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e failover markers table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_harness_signals (
            signal_name TEXT PRIMARY KEY,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e harness signals table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_provider_calls (
            call_id BIGSERIAL PRIMARY KEY,
            request_id TEXT NOT NULL,
            scenario TEXT NOT NULL,
            workflow_id TEXT NOT NULL,
            model TEXT NOT NULL,
            request_json TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e provider calls table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_tool_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            call_id TEXT,
            args_json TEXT NOT NULL DEFAULT '{}',
            result_json TEXT NOT NULL DEFAULT '{}',
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e tool events table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_tool_attempt_counts (
            workflow_id TEXT NOT NULL,
            step_id TEXT NOT NULL,
            count BIGINT NOT NULL,
            last_worker_id TEXT NOT NULL,
            updated_at_ms BIGINT NOT NULL,
            PRIMARY KEY (workflow_id, step_id)
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e tool attempt counts table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_turn_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            stream_name TEXT NOT NULL,
            cursor TEXT,
            activity_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e turn events table")?;
    tx.commit().await.context("commit e2e schema transaction")?;
    Ok(())
}

pub async fn reset_e2e_rows(pool: &PgPool) -> Result<()> {
    for statement in [
        "DELETE FROM lash_e2e_terminal_results",
        "DELETE FROM lash_e2e_worker_events",
        "DELETE FROM lash_e2e_failover_markers",
        "DELETE FROM lash_e2e_harness_signals",
        "DELETE FROM lash_e2e_tool_attempt_counts",
        "DELETE FROM lash_e2e_provider_calls",
        "DELETE FROM lash_e2e_tool_events",
        "DELETE FROM lash_e2e_turn_events",
        "DELETE FROM lash_trigger_deliveries",
        "DELETE FROM lash_trigger_occurrences",
        "DELETE FROM lash_trigger_subscriptions",
        "DELETE FROM lash_queued_work_items",
        "DELETE FROM lash_queued_work_batches",
        "DELETE FROM lash_consumed_wake_high_water",
        "DELETE FROM lash_process_leases",
        "DELETE FROM lash_process_observers",
        "DELETE FROM lash_process_tombstones",
        "DELETE FROM lash_process_segment_handovers",
        "DELETE FROM lash_process_wake_deliveries",
        "DELETE FROM lash_process_events",
        "DELETE FROM lash_processes",
        "DELETE FROM lash_lashlang_artifacts",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .with_context(|| format!("reset e2e rows with `{statement}`"))?;
    }
    for session_id in [
        DEFAULT_SESSION_ID,
        "restate-postgres-workers-frame-crash-e2e",
    ] {
        for statement in [
            "DELETE FROM lash_sessions WHERE session_id = $1",
            "DELETE FROM lash_graph_nodes WHERE session_id = $1",
            "DELETE FROM lash_usage_deltas WHERE session_id = $1",
            "DELETE FROM lash_session_meta WHERE session_id = $1",
            "DELETE FROM lash_runtime_turn_commits WHERE session_id = $1",
            "DELETE FROM lash_attachment_manifest WHERE session_id = $1",
            "DELETE FROM lash_session_execution_leases WHERE session_id = $1",
            "DELETE FROM lash_pending_turn_inputs WHERE session_id = $1",
        ] {
            sqlx::query(statement)
                .bind(session_id)
                .execute(pool)
                .await
                .with_context(|| format!("reset e2e session rows with `{statement}`"))?;
        }
    }
    Ok(())
}

pub async fn record_worker_event(
    pool: &PgPool,
    workflow_id: &str,
    worker_id: &str,
    event_type: &str,
    detail: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO lash_e2e_worker_events (
            workflow_id, worker_id, event_type, detail_json, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workflow_id, worker_id, event_type) DO NOTHING
        "#,
    )
    .bind(workflow_id)
    .bind(worker_id)
    .bind(event_type)
    .bind(detail.to_string())
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    .with_context(|| format!("record e2e worker event `{event_type}` for `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_terminal_result(pool: &PgPool, response: &TurnResponse) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO lash_e2e_terminal_results (
            workflow_id, process_id, worker_id, attachment_id, final_text, submitted_json,
            queued_turn_ran, streamed_event_count, replay_cursor, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (workflow_id) DO UPDATE SET
            process_id = EXCLUDED.process_id,
            worker_id = EXCLUDED.worker_id,
            attachment_id = EXCLUDED.attachment_id,
            final_text = EXCLUDED.final_text,
            submitted_json = EXCLUDED.submitted_json,
            queued_turn_ran = EXCLUDED.queued_turn_ran,
            streamed_event_count = EXCLUDED.streamed_event_count,
            replay_cursor = EXCLUDED.replay_cursor,
            created_at_ms = EXCLUDED.created_at_ms
        "#,
    )
    .bind(&response.workflow_id)
    .bind(&response.process_id)
    .bind(&response.worker_id)
    .bind(&response.attachment_id)
    .bind(&response.final_text)
    .bind(response.final_value.to_string())
    .bind(response.queued_turn_ran)
    .bind(response.streamed_event_count as i64)
    .bind(response.replay_cursor.as_deref())
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    .with_context(|| format!("record terminal result for `{}`", response.workflow_id))?;
    Ok(())
}

pub async fn record_provider_call(
    pool: &PgPool,
    request_id: &str,
    scenario: &str,
    workflow_id: &str,
    model: &str,
    request: &serde_json::Value,
    response: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO lash_e2e_provider_calls (
            request_id, scenario, workflow_id, model, request_json, response_json, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(request_id)
    .bind(scenario)
    .bind(workflow_id)
    .bind(model)
    .bind(request.to_string())
    .bind(response.to_string())
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    .with_context(|| format!("record provider call for `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_tool_event(
    pool: &PgPool,
    workflow_id: &str,
    worker_id: &str,
    tool_name: &str,
    call_id: Option<&str>,
    args: serde_json::Value,
    result: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO lash_e2e_tool_events (
            workflow_id, worker_id, tool_name, call_id, args_json, result_json, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(workflow_id)
    .bind(worker_id)
    .bind(tool_name)
    .bind(call_id)
    .bind(args.to_string())
    .bind(result.to_string())
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    .with_context(|| format!("record tool event `{tool_name}` for `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_turn_activity(
    pool: &PgPool,
    workflow_id: &str,
    worker_id: &str,
    stream_name: &str,
    cursor: Option<&str>,
    activity: &lash::TurnActivity,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO lash_e2e_turn_events (
            workflow_id, worker_id, stream_name, cursor, activity_json, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workflow_id)
    .bind(worker_id)
    .bind(stream_name)
    .bind(cursor)
    .bind(serde_json::to_string(activity).context("serialize turn activity")?)
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    .with_context(|| format!("record turn activity for `{workflow_id}`"))?;
    Ok(())
}

#[derive(Clone)]
pub struct E2eCoreConfig {
    pub worker_id: String,
    pub storage: lash_postgres_store::PostgresStorage,
    pub attachment_store: Arc<dyn AttachmentStore>,
    pub process_work_driver: ProcessWorkDriver,
    pub restate_ingress_url: String,
    pub mock_provider_base_url: String,
    pub trace_dir: Option<PathBuf>,
    pub fail_once: bool,
}

pub fn build_e2e_core(config: E2eCoreConfig) -> Result<lash::LashCore> {
    let artifact_store =
        Arc::new(config.storage.lashlang_artifact_store()) as Arc<dyn LashlangArtifactStore>;
    let process_env_store =
        Arc::new(config.storage.process_env_store()) as Arc<dyn ProcessExecutionEnvStore>;
    let session_store_factory =
        Arc::new(config.storage.session_store_factory()) as Arc<dyn SessionStoreFactory>;
    let trigger_store =
        Arc::new(config.storage.trigger_store()) as Arc<dyn lash_core::TriggerStore>;
    let provider = lash_core::ProviderHandle::new(
        OpenAiCompatibleProvider::new(
            "e2e-key",
            format!("{}/v1", config.mock_provider_base_url.trim_end_matches('/')),
        )
        .into_components(),
    );
    let mut factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        RlmProtocolPluginConfig::default().with_lashlang_abilities(
            LashlangAbilities::default()
                .with_processes()
                .with_sleep()
                .with_process_signals()
                .with_triggers(),
        ),
        artifact_store,
    );
    if let Some(trace_dir) = config.trace_dir.as_ref() {
        factory = factory.with_lashlang_execution_jsonl_path(
            trace_dir.join(format!("{}.lashlang.jsonl", config.worker_id)),
        );
    }
    let mut builder = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits("e2e-mock", Default::default(), 200_000, None)
                .map_err(|err| anyhow::anyhow!(err))?,
        )
        .store_factory(session_store_factory)
        .attachment_store(config.attachment_store)
        .process_env_store(process_env_store)
        .effect_host(
            Arc::new(RestateEffectHost::new(config.restate_ingress_url.clone()))
                as Arc<dyn EffectHost>,
        )
        .trigger_store(trigger_store)
        .process_work_driver(config.process_work_driver)
        // Restate turns must enter through an explicit handler-scoped effect
        // controller. The harness drains durable ingress from its workflow
        // handlers, so an ambient local queue pump would race those handlers
        // and cannot legally execute their effects.
        .disable_queued_work_driver()
        .plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()))
        .plugin(Arc::new(E2ePluginFactory {
            pool: config.storage.pool().clone(),
            worker_id: config.worker_id.clone(),
            restate_ingress_url: config.restate_ingress_url,
            fail_once: config.fail_once,
        }));
    if let Some(trace_dir) = config.trace_dir.as_ref() {
        builder =
            builder.trace_jsonl_path(trace_dir.join(format!("{}.trace.jsonl", config.worker_id)));
    }
    builder.build().context("build e2e LashCore")
}

pub fn process_registry_from_storage(
    storage: &lash_postgres_store::PostgresStorage,
) -> Arc<dyn ProcessRegistry> {
    Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>
}

pub fn process_continuations_from_storage(
    storage: &lash_postgres_store::PostgresStorage,
) -> Arc<dyn lash_core::ProcessContinuationStore> {
    Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessContinuationStore>
}

#[derive(Clone)]
struct E2ePluginFactory {
    pool: PgPool,
    worker_id: String,
    restate_ingress_url: String,
    fail_once: bool,
}

impl PluginFactory for E2ePluginFactory {
    fn id(&self) -> &'static str {
        "restate-postgres-workers-e2e"
    }

    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        let abilities = LashlangAbilities::default()
            .with_processes()
            .with_sleep()
            .with_process_signals()
            .with_triggers();
        let mut resources = LashlangHostCatalog::new();
        resources
            .add_trigger_source_constructor(
                ["ui", "button", "pressed"],
                TypeExpr::Object(vec![]),
                button_pressed_event_type(),
            )
            .expect("valid e2e button trigger source");
        vec![
            PluginExtensionContribution::new(
                LASHLANG_SURFACE_EXTENSION_ID,
                LashlangSurfaceContribution::new(
                    abilities,
                    LashlangLanguageFeatures::default(),
                    resources,
                ),
            )
            .expect("lashlang surface contribution serializes"),
        ]
    }

    fn build(
        &self,
        _ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(E2eSessionPlugin {
            pool: self.pool.clone(),
            worker_id: self.worker_id.clone(),
            restate_ingress_url: self.restate_ingress_url.clone(),
            fail_once: self.fail_once,
        }))
    }
}

#[derive(Clone)]
struct E2eSessionPlugin {
    pool: PgPool,
    worker_id: String,
    restate_ingress_url: String,
    fail_once: bool,
}

impl SessionPlugin for E2eSessionPlugin {
    fn id(&self) -> &'static str {
        "restate-postgres-workers-e2e"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), lash::plugins::PluginError> {
        reg.triggers().declare(lash::triggers::TriggerEvent::new(
            "Button",
            "ui.button",
            "pressed",
            button_pressed_payload_schema(),
        ))?;
        reg.tools()
            .provider(e2e_tool_provider(
                self.pool.clone(),
                self.worker_id.clone(),
                self.restate_ingress_url.clone(),
                self.fail_once,
            ))
            .map_err(|err| lash::plugins::PluginError::Session(err.to_string()))?;
        Ok(())
    }
}

fn button_pressed_event_type() -> NamedDataType {
    NamedDataType::object(
        "ui.button.Pressed",
        vec![
            TypeField {
                name: "button".into(),
                ty: TypeExpr::Union(vec![
                    TypeExpr::Enum(vec!["Red".into()]),
                    TypeExpr::Enum(vec!["Blue".into()]),
                ]),
                optional: false,
            },
            TypeField {
                name: "message".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
            TypeField {
                name: "pressed_at".into(),
                ty: TypeExpr::Str,
                optional: false,
            },
        ],
    )
    .expect("valid e2e button payload type")
}

fn button_pressed_payload_schema() -> lash::triggers::LashSchema {
    lash::triggers::LashSchema::new(serde_json::json!({
        "type": "object",
        "properties": {
            "button": { "type": "string", "enum": ["Red", "Blue"] },
            "message": { "type": "string" },
            "pressed_at": { "type": "string" }
        },
        "required": ["button", "message", "pressed_at"],
        "additionalProperties": false
    }))
}

fn e2e_tool_provider(
    pool: PgPool,
    worker_id: String,
    restate_ingress_url: String,
    fail_once: bool,
) -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![
            e2e_tool_definition(
                "tool:app_lookup",
                "app_lookup",
                "Deterministic E2E application data lookup.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" }
                    },
                    "required": ["key"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "value": { "type": "string" },
                        "worker_id": { "type": "string" }
                    },
                    "required": ["key", "value", "worker_id"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "app_lookup"),
            ),
            e2e_tool_definition(
                "tool:async_lookup",
                "async_lookup",
                "Deterministic E2E lookup that completes through external AwaitEvent ingress.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" },
                        "key": { "type": "string" }
                    },
                    "required": ["workflow_id", "key"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "value": { "type": "string" },
                        "worker_id": { "type": "string" },
                        "async": { "type": "boolean" }
                    },
                    "required": ["key", "value", "worker_id", "async"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "async_lookup"),
            ),
            e2e_tool_definition(
                "tool:make_attachment",
                "make_attachment",
                "Write a deterministic attachment through the session attachment store.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" },
                        "name": { "type": "string" }
                    },
                    "required": ["workflow_id", "name"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "mime": { "type": "string" },
                        "filename": { "type": "string" },
                        "byte_len": { "type": "integer" }
                    },
                    "required": ["id", "mime", "filename", "byte_len"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "make_attachment"),
            ),
            e2e_tool_definition(
                "tool:batch_side_effect",
                "batch_side_effect",
                "Record a deterministic side effect for aggregate tool-batch replay coverage.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" },
                        "key": { "type": "string" },
                        "delay_ms": { "type": "integer" }
                    },
                    "required": ["workflow_id", "key"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string" },
                        "value": { "type": "string" },
                        "worker_id": { "type": "string" }
                    },
                    "required": ["key", "value", "worker_id"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "batch_side_effect"),
            ),
            e2e_tool_definition(
                "tool:crash_once",
                "crash_once",
                "Crash one worker once for a requested workflow, after previous atomic attempts replay.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" }
                    },
                    "required": ["workflow_id"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "crashed": { "type": "boolean" },
                        "worker_id": { "type": "string" }
                    },
                    "required": ["crashed", "worker_id"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "crash_once"),
            ),
            e2e_tool_definition(
                "tool:durable_input_request",
                "durable_input_request",
                "Open a durable input request from inside a tool and wait for runner resolution.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" },
                        "question": { "type": "string" },
                        "attach_after_resolution": { "type": "boolean" }
                    },
                    "required": ["workflow_id", "question"],
                    "additionalProperties": false
                }),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "request_id": { "type": "string" },
                        "answer": { "type": "string" },
                        "worker_id": { "type": "string" }
                    },
                    "required": ["request_id", "answer", "worker_id"],
                    "additionalProperties": false
                }),
                LashlangToolBinding::new(["tools"], "durable_input_request"),
            ),
            e2e_tool_definition(
                "tool:cancel_gate",
                "cancel_gate",
                "Block a follow-on frame until the public turn-cancel API signals cancellation.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": { "type": "string" }
                    },
                    "required": ["workflow_id"],
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "object" }),
                LashlangToolBinding::new(["tools"], "cancel_gate"),
            ),
        ],
        E2eTools {
            pool,
            worker_id,
            restate_ingress_url,
            fail_once,
        },
    )) as Arc<dyn ToolProvider>
}

fn e2e_tool_definition(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    surface: LashlangToolBinding,
) -> ToolDefinition {
    ToolDefinition::raw(id, name, description, input_schema, output_schema)
        .with_lashlang_binding(surface)
}

#[derive(Clone)]
struct E2eTools {
    pool: PgPool,
    worker_id: String,
    restate_ingress_url: String,
    fail_once: bool,
}

type E2eToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

#[async_trait::async_trait]
impl StaticToolExecute for E2eTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        self.execute_selected_tool(call).await
    }
}

impl E2eTools {
    fn execute_selected_tool<'a>(&'a self, call: ToolCall<'a>) -> E2eToolFuture<'a> {
        match call.name {
            "app_lookup" => Box::pin(self.app_lookup(call)),
            "async_lookup" => Box::pin(self.async_lookup(call)),
            "batch_side_effect" => Box::pin(self.batch_side_effect(call)),
            "make_attachment" => Box::pin(self.make_attachment(call)),
            "crash_once" => Box::pin(self.crash_once(call)),
            "durable_input_request" => Box::pin(self.durable_input_request(call)),
            "cancel_gate" => Box::pin(self.cancel_gate(call)),
            other => {
                Box::pin(
                    async move { ToolResult::err_fmt(format_args!("unknown e2e tool `{other}`")) },
                )
            }
        }
    }

    async fn app_lookup(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let key = call
            .args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let result = serde_json::json!({
            "key": key,
            "value": format!("lookup:{key}"),
            "worker_id": self.worker_id,
        });
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call.context.tool_call_id(),
            call.args.to_owned(),
            result.clone(),
        )
        .await;
        ToolResult::ok(result)
    }

    async fn async_lookup(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let key_arg = call
            .args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default")
            .to_string();
        let result = serde_json::json!({
            "key": key_arg,
            "value": format!("async:{key_arg}"),
            "worker_id": self.worker_id,
            "async": true,
        });
        let completion_key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => return ToolResult::err_fmt(err),
        };
        let call_id = call.context.tool_call_id().map(ToOwned::to_owned);
        let args = call.args.to_owned();
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call_id.as_deref(),
            args.clone(),
            serde_json::json!({
                "pending": true,
                "promise_key": completion_key.promise_key(),
            }),
        )
        .await;

        let pool = self.pool.clone();
        let worker_id = self.worker_id.clone();
        let restate_ingress_url = self.restate_ingress_url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let host = RestateEffectHost::new(restate_ingress_url);
            let resolution = lash_core::Resolution::Ok(result.clone());
            let outcome = host
                .resolve_await_event(&completion_key, resolution)
                .await
                .map(|outcome| serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null))
                .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() }));
            let _ = record_tool_event(
                &pool,
                &workflow_id,
                &worker_id,
                "async_lookup.resolve",
                call_id.as_deref(),
                args,
                serde_json::json!({
                    "resolved": true,
                    "outcome": outcome,
                    "result": result,
                }),
            )
            .await;
        });

        ToolResult::pending(lash_core::PendingCompletion::new())
    }

    async fn batch_side_effect(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let key = call
            .args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default")
            .to_string();
        let delay_ms = call
            .args
            .get("delay_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let result = serde_json::json!({
            "key": key,
            "value": format!("batch:{key}"),
            "worker_id": self.worker_id,
        });
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call.context.tool_call_id(),
            call.args.to_owned(),
            result.clone(),
        )
        .await;
        ToolResult::ok(result)
    }

    async fn make_attachment(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let filename = call
            .args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("kitchen-sink.png");
        let bytes = expected_attachment_bytes(&workflow_id);
        let reference = match call
            .context
            .attachments()
            .put(
                bytes,
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("image/png").unwrap(),
                    Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some(filename.to_string()),
                ),
            )
            .await
        {
            Ok(reference) => reference,
            Err(err) => return ToolResult::err_fmt(err),
        };
        let mut result = BTreeMap::new();
        result.insert(
            "id".to_string(),
            lash_core::ToolValue::String(reference.id.to_string()),
        );
        result.insert(
            "mime".to_string(),
            lash_core::ToolValue::String(ATTACHMENT_MIME.to_string()),
        );
        result.insert(
            "filename".to_string(),
            lash_core::ToolValue::String(filename.to_string()),
        );
        result.insert(
            "byte_len".to_string(),
            lash_core::ToolValue::Number(serde_json::Number::from(reference.byte_len)),
        );
        result.insert(
            "attachment".to_string(),
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(
                reference.clone(),
            )),
        );
        let result = lash_core::ToolValue::Object(result);
        let result_json = result.to_json_value();
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call.context.tool_call_id(),
            call.args.to_owned(),
            result_json,
        )
        .await;
        ToolResult::from_output(lash_core::ToolCallOutput::success(result))
    }

    async fn crash_once(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let result = serde_json::json!({
            "crashed": false,
            "worker_id": self.worker_id,
        });
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call.context.tool_call_id(),
            call.args.to_owned(),
            result.clone(),
        )
        .await;
        // This scenario proves peer takeover, not merely container restart.
        // Once a logical worker claims the crash marker, every reincarnation
        // of that same worker keeps exiting for this workflow until the proxy
        // sends Restate's retry to the other worker.
        if self.fail_once
            && should_exit_for_peer_failover(&self.pool, &workflow_id, &self.worker_id).await
        {
            let _ = record_worker_event(
                &self.pool,
                &workflow_id,
                &self.worker_id,
                "intentional_exit",
                serde_json::json!({"tool": "crash_once"}),
            )
            .await;
            tracing::warn!(
                worker_id = %self.worker_id,
                workflow_id,
                "intentional E2E failover exit from crash_once tool"
            );
            std::process::exit(75);
        }
        ToolResult::ok(result)
    }

    async fn cancel_gate(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let started = serde_json::json!({
            "waiting": true,
            "worker_id": self.worker_id,
        });
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            call.name,
            call.context.tool_call_id(),
            call.args.to_owned(),
            started,
        )
        .await;
        let Some(cancel) = call.context.cancellation_token().cloned() else {
            return ToolResult::err(serde_json::json!(
                "cancel_gate requires a turn cancellation token"
            ));
        };
        cancel.cancelled().await;
        ToolResult::err(serde_json::json!("cancel_gate cancelled"))
    }

    async fn durable_input_request(&self, call: ToolCall<'_>) -> ToolResult {
        let workflow_id = workflow_id_from_args(call.context.session_id(), call.args);
        let question = call
            .args
            .get("question")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("input?")
            .to_string();
        let key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => return ToolResult::err_fmt(err),
        };
        let opened = serde_json::json!({
            "request_id": format!("{workflow_id}:request-1"),
            "question": question,
            "worker_id": self.worker_id,
        });
        if let Err(err) =
            record_tool_attempt(&self.pool, &workflow_id, "attempt", &self.worker_id).await
        {
            return ToolResult::err_fmt(err);
        }
        let call_id = call.context.tool_call_id().map(ToOwned::to_owned);
        let args = call.args.to_owned();
        let key_json = serde_json::to_value(&key).unwrap_or(serde_json::Value::Null);
        let _ = record_tool_event(
            &self.pool,
            &workflow_id,
            &self.worker_id,
            "durable_input_request.opened",
            call_id.as_deref(),
            args.clone(),
            serde_json::json!({
                "opened": opened,
                "await_key": key_json,
            }),
        )
        .await;
        let process_event = lash_core::ProcessEventAppendRequest::new(
            "process.yield",
            serde_json::json!({
                "type": "work.input_request.opened",
                "workflow_id": workflow_id,
                "request_id": opened["request_id"].clone(),
                "await_key_id": key.key_id,
            }),
        )
        .with_replay_key(format!(
            "tool:{}:input-request-opened",
            call_id.as_deref().unwrap_or("unknown")
        ));
        if let Err(err) = call
            .context
            .process_events()
            .emit_request(process_event)
            .await
        {
            return ToolResult::err_fmt(err);
        }
        if args
            .get("attach_after_resolution")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let signal_name = format!("durable-input-attach:{workflow_id}");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                match sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(
                        SELECT 1 FROM lash_e2e_harness_signals WHERE signal_name = $1
                    )",
                )
                .bind(&signal_name)
                .fetch_one(&self.pool)
                .await
                {
                    Ok(true) => break,
                    Ok(false) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(false) => {
                        return ToolResult::err(serde_json::json!(format!(
                            "timed out waiting for harness signal `{signal_name}`"
                        )));
                    }
                    Err(err) => return ToolResult::err_fmt(err),
                }
            }
        }
        ToolResult::pending(lash_core::PendingCompletion::new())
    }
}

async fn record_tool_attempt(
    pool: &PgPool,
    workflow_id: &str,
    step_id: &str,
    worker_id: &str,
) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO lash_e2e_tool_attempt_counts (
            workflow_id, step_id, count, last_worker_id, updated_at_ms
        )
        VALUES ($1, $2, 1, $3, $4)
        ON CONFLICT (workflow_id, step_id) DO UPDATE
        SET count = lash_e2e_tool_attempt_counts.count + 1,
            last_worker_id = EXCLUDED.last_worker_id,
            updated_at_ms = EXCLUDED.updated_at_ms
        RETURNING count
        "#,
    )
    .bind(workflow_id)
    .bind(step_id)
    .bind(worker_id)
    .bind(current_epoch_ms() as i64)
    .fetch_one(pool)
    .await
    .with_context(|| format!("record tool attempt `{step_id}` for `{workflow_id}`"))?;
    Ok(count)
}

async fn should_exit_for_peer_failover(pool: &PgPool, workflow_id: &str, worker_id: &str) -> bool {
    let inserted = match sqlx::query(
        "INSERT INTO lash_e2e_failover_markers (workflow_id, worker_id, created_at_ms)
         VALUES ($1, $2, $3)
         ON CONFLICT (workflow_id) DO NOTHING",
    )
    .bind(workflow_id)
    .bind(worker_id)
    .bind(current_epoch_ms() as i64)
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() == 1,
        Err(err) => {
            tracing::error!(workflow_id, worker_id, error = %err, "failed to claim failover marker");
            return false;
        }
    };
    if inserted {
        return true;
    }

    match sqlx::query_scalar::<_, String>(
        "SELECT worker_id FROM lash_e2e_failover_markers WHERE workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(exit_worker)) => exit_worker == worker_id,
        Ok(None) => false,
        Err(err) => {
            tracing::error!(workflow_id, worker_id, error = %err, "failed to read failover marker");
            false
        }
    }
}

pub fn expected_attachment_bytes(workflow_id: &str) -> Vec<u8> {
    format!("lash-e2e-attachment:{workflow_id}:v1").into_bytes()
}

fn workflow_id_from_args(session_id: &str, args: &serde_json::Value) -> String {
    args.get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(session_id)
        .to_string()
}

pub fn current_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
