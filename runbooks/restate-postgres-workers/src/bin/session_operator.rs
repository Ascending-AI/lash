//! Public session operator verbs on live Restate and PostgreSQL (FIG-4491).
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use lash::plugins::{PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin};
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolBinding, ToolCall,
    ToolDefinition, ToolDefinitionBindingExt, ToolOutcome,
};
use lash::{CancelTarget, SessionId, TurnId, TurnInput, TurnStatus};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet};
use lash_restate_postgres_workers_e2e::local_restate::{LocalDeployment, LocalRestate};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

const WAIT: Duration = Duration::from_secs(120);

struct Harness {
    core: lash::LashCore,
    engine: Arc<lash_restate::RestateEngine>,
    restate: LocalRestate,
    pool: PgPool,
    repaired: Arc<AtomicBool>,
    deployment: Option<LocalDeployment>,
}

impl Harness {
    async fn new() -> Result<Self> {
        let restate = LocalRestate::from_env()?;
        let storage = PostgresStorage::connect_with(
            &std::env::var("LASH_POSTGRES_DATABASE_URL")?,
            lash_postgres_store::PostgresStoreConfig {
                max_connections: 16,
                ..Default::default()
            },
        )
        .await?;
        let pool = storage.pool().clone();
        let scratch = std::env::var("LASH_OPERATOR_ARTIFACT_DIR")?;
        let attachments = lash_sqlite_store::SqliteStoreSet::open(
            std::path::Path::new(&scratch).join("attachment-bytes"),
        )
        .await?;
        let stores = Arc::new(PostgresStoreSet::new(
            &storage,
            attachments.attachment_store(),
        ));
        let engine = restate.engine(stores);
        let repaired = Arc::new(AtomicBool::new(false));
        let ledger = pool.clone();
        let provider = lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
            .kind("session-operator")
            .complete(move |request| {
                let pool = ledger.clone();
                async move {
                    let marker = request.messages.iter().flat_map(|message| message.blocks.iter())
                        .find_map(|block| match block {
                            lash::provider::LlmContentBlock::Text { text, .. }
                                if text.starts_with("operator:") => Some(text.clone()),
                            _ => None,
                        }).ok_or_else(|| lash::provider::LlmTransportError::new("operator marker missing"))?;
                    let encoded = serde_json::to_value(&request)
                        .map_err(|error| lash::provider::LlmTransportError::new(error.to_string()))?;
                    let call = encoded["scope"]["request_id"].as_str().unwrap_or_default();
                    let first = call.ends_with(":llm:0");
                    // The second call of a faulted root: its response is the
                    // one whose derivation fails until the repair.
                    let second = call.ends_with(":llm:1");
                    sqlx::query("INSERT INTO operator_model_calls(marker, request_json) VALUES ($1, $2)")
                        .bind(marker.as_ref()).bind(encoded.to_string())
                        .execute(&pool).await
                        .map_err(|error| lash::provider::LlmTransportError::new(error.to_string()))?;
                    let body = if marker.contains("withdraw") || !(first || second) {
                        "finish(\"real answer\");".to_string()
                    } else if second {
                        "const settled = true;".to_string()
                    } else {
                        format!("const child = async () => {{ await waitSignal(\"never\"); return \"child\"; }};\nconst handle = await processes.start({{ definition: child }});\n{}", if marker.contains("running") { "await tools.hold({ running: true });\nfinish(\"real answer\");" } else { "" })
                    };
                    Ok(lash::provider::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: format!("<typescript>\n{body}\n</typescript>"),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            }).build().into_handle();
        let backend = lash::Backend::new(engine.clone());
        let protocol = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash_protocol_rlm::TypescriptDialect),
            &backend,
        )
        .with_worker_service(lash::rlm::WorkerService::subprocess(
            std::env::var_os("LASH_OPERATOR_VM_WORKER").context("VM worker executable")?,
        ));
        let core = lash::LashCore::rlm_builder(backend, protocol)
            .models(Arc::new(
                lash::ModelRegistry::new().register(
                    "session-operator-mock",
                    lash::RegisteredModel::new(
                        lash::ModelMetadata::builder("session-operator-mock")
                            .context_window_tokens(200_000)
                            .build()
                            .map_err(anyhow::Error::msg)?,
                        provider,
                    ),
                )?,
            ))
            .commit_budget(lash::CommitBudget::bounded(4 * 1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .trace_jsonl_path(std::path::Path::new(&scratch).join("worker.trace.jsonl"))
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::starter,
                ),
            ))
            .plugin(Arc::new(FaultPlugin {
                repaired: repaired.clone(),
                pool: pool.clone(),
            }))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "session-operator",
                format!("operator:{}", std::process::id()),
            ))?;
        Ok(Self {
            core,
            engine,
            restate,
            pool,
            repaired,
            deployment: None,
        })
    }

    async fn serve(&mut self) -> Result<()> {
        let worker = lash::durability::DurableProcessWorker::new(
            self.core.durable_process_worker_config()?,
        )?;
        self.deployment = Some(
            self.restate
                .serve_at(
                    &self.engine,
                    std::env::var("LASH_OPERATOR_ENDPOINT")?.parse()?,
                    self.engine.endpoint_builder(worker).build(),
                )
                .await?,
        );
        Ok(())
    }

    async fn open(&self, tag: &str) -> Result<lash::LashSession> {
        self.core
            .session(format!("operator:{tag}"))
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                "session-operator-mock",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await?;
        Ok(self.core.session(format!("operator:{tag}")).open().await?)
    }

    async fn calls(&self, tag: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM operator_model_calls WHERE marker = $1")
                .bind(format!("operator:{tag}"))
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn receipt(
        &self,
        session: &SessionId,
        root: &TurnId,
        park: lash_core::store::ParkId,
        verb: &str,
    ) -> Result<lash_core::store::ControlIntent> {
        let page = self
            .core
            .parked_work()
            .intents(&lash::ControlIntentQuery {
                after: None,
                limit: std::num::NonZeroUsize::new(128).context("receipt page size")?,
            })
            .await?;
        ensure!(page.next.is_none(), "private run fits one receipt page");
        let found: Vec<_> = page
            .intents
            .into_iter()
            .filter(|intent| {
                if intent.session_id != *session || intent.kind.code() != verb {
                    return false;
                }
                match &intent.kind {
                    lash_core::store::ControlIntentKind::Redrive {
                        root: saved,
                        park: token,
                    }
                    | lash_core::store::ControlIntentKind::Cancel {
                        root: saved,
                        park: token,
                    }
                    | lash_core::store::ControlIntentKind::Fork {
                        root: saved,
                        park: token,
                        ..
                    } => saved == root && *token == park,
                    _ => false,
                }
            })
            .collect();
        ensure!(
            found.len() == 1,
            "one retained {verb} decision, got {}",
            found.len()
        );
        found
            .into_iter()
            .next()
            .context("retained operator receipt")
    }

    /// The retained decision once its engine half is acknowledged. The store
    /// half commits first, so a receipt read at once may still be pending and
    /// would differ from every later read by that state alone.
    async fn acknowledged_receipt(
        &self,
        session: &SessionId,
        root: &TurnId,
        park: lash_core::store::ParkId,
        verb: &str,
    ) -> Result<lash_core::store::ControlIntent> {
        tokio::time::timeout(WAIT, async {
            loop {
                let intent = self.receipt(session, root, park, verb).await?;
                if matches!(
                    intent.state,
                    lash_core::store::ControlIntentState::Acknowledged { .. }
                ) {
                    return Ok(intent);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context(format!(
            "{verb} acknowledgement deadline for {session}/{root}"
        ))?
    }

    async fn journal(&self, session: &SessionId, root: &TurnId, label: &str) -> Result<Value> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(WAIT)
            .build()?;
        let key = lash_restate::turn_workflow_key(session, root).replace('\'', "''");
        let invocations: Value = client.post(format!("{}/query", self.restate.admin_url))
            .header("accept", "application/json")
            .json(&json!({"query":format!("SELECT id FROM sys_invocation WHERE target_service_key = '{key}' AND target_handler_name = 'run'")}))
            .send().await?.error_for_status()?.json().await?;
        let ids = invocations["rows"].as_array().context("invocation rows")?;
        ensure!(
            ids.len() == 1,
            "one recorded invocation for root, got {invocations}"
        );
        let id = ids[0]["id"].as_str().context("root invocation id")?;
        let journal: Value = client.post(format!("{}/query", self.restate.admin_url))
            .header("accept", "application/json")
            .json(&json!({"query":format!("SELECT index, entry_type, name, raw FROM sys_journal WHERE id = '{id}' ORDER BY index")}))
            .send().await?.error_for_status()?.json().await?;
        let commands: Vec<_> = journal["rows"]
            .as_array()
            .context("journal rows")?
            .iter()
            .filter(|row| {
                row["entry_type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("Command:"))
            })
            .cloned()
            .collect();
        ensure!(!commands.is_empty(), "root journal must execute commands");
        let evidence = json!({"invocation":id,"commands":commands});
        std::fs::write(
            std::path::Path::new(&std::env::var("LASH_OPERATOR_ARTIFACT_DIR")?)
                .join(format!("{label}.journal.json")),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        Ok(evidence)
    }

    async fn child(&self, session: &SessionId, root: &TurnId) -> Result<(String, String)> {
        let scope = lash_core::ScopeId::Opener(lash_core::EffectOpener::Turn {
            session_id: session.clone(),
            turn_id: root.clone(),
        })
        .storage_id();
        tokio::time::timeout(WAIT, async {
            loop {
                let rows = sqlx::query("SELECT process_id, lifetime_scope_kind, lifetime_scope_id FROM lash_processes WHERE record_json::jsonb ->> 'session_capability' = $1 AND lifetime_scope_id = $2")
                    .bind(session.as_str()).bind(&scope).fetch_all(&self.pool).await?;
                if let Some(row) = rows.first() {
                    ensure!(rows.len() == 1, "one child per root, got {}", rows.len());
                    ensure!(row.get::<String, _>("lifetime_scope_kind") == "turn", "child must live Until(Turn)");
                    return Ok((row.get("process_id"), row.get("lifetime_scope_id")));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context("child registration deadline")?
    }

    async fn terminal(
        &self,
        session: &SessionId,
        root: &TurnId,
        kind: &str,
        child: &(String, String),
        allow_substrate_loss: bool,
    ) -> Result<Value> {
        tokio::time::timeout(WAIT, async {
            loop {
                let row = sqlx::query("SELECT terminal_kind, terminal_cause_json, terminal_at_ms FROM lash_session_roots WHERE session_id = $1 AND root = $2")
                    .bind(session.as_str()).bind(root.as_str()).fetch_one(&self.pool).await?;
                let record: String = sqlx::query_scalar("SELECT record_json FROM lash_processes WHERE process_id = $1")
                    .bind(&child.0).fetch_one(&self.pool).await?;
                let record: lash_core::ProcessRecord = serde_json::from_str(&record)?;
                let substrate_lost = matches!(&record.outcome,
                    Some(lash_core::ProcessAwaitOutput::Abandoned { evidence, .. })
                    if evidence.writer == lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                    });
                let child_ended = record.status == lash_core::ProcessStatus::Cancelled
                    || (allow_substrate_loss && record.status == lash_core::ProcessStatus::Abandoned && substrate_lost);
                let closes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_parent_end_plans WHERE parent_kind = 'turn' AND parent_id = $1 AND settled_at_ms IS NOT NULL")
                    .bind(&child.1).fetch_one(&self.pool).await?;
                let child_scope = lash_core::ScopeId::process(record.id.clone()).storage_id();
                let child_closes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_parent_end_plans WHERE parent_kind = 'process' AND parent_id = $1 AND settled_at_ms IS NOT NULL")
                    .bind(child_scope).fetch_one(&self.pool).await?;
                if row.get::<Option<String>, _>("terminal_kind").as_deref() == Some(kind) && child_ended && closes == 1 && child_closes == 1 {
                    let request = record.cancel_request.as_ref().context("child cancel request")?;
                    ensure!(request.origin == lash_core::CancelOrigin::ParentEnded && request.requester == child.1,
                        "child cancellation must name its closed turn scope");
                    let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_terminal_writes WHERE session_id = $1 AND root = $2")
                        .bind(session.as_str()).bind(root.as_str()).fetch_one(&self.pool).await?;
                    let cancels: i64 = sqlx::query_scalar("SELECT count(*) FROM operator_child_cancels WHERE process_id = $1")
                        .bind(&child.0).fetch_one(&self.pool).await?;
                    ensure!(writes == 1 && cancels == 1, "terminal writes={writes}, child cancels={cancels}");
                    return Ok(json!({"terminal_kind": kind, "terminal_writes": writes, "child_cancels": cancels, "scope_closes": closes, "child_scope_closes": child_closes,
                        "child": child.0, "child_status": record.status, "child_outcome": record.outcome,
                        "child_cancel_request": request, "scope": child.1, "terminal_at_ms": row.get::<Option<i64>, _>("terminal_at_ms"),
                        "terminal_cause": row.get::<Option<String>, _>("terminal_cause_json")}));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context(format!("terminal/scope deadline for {session}/{root}"))?
    }

    async fn parked(
        &self,
        tag: &str,
    ) -> Result<(
        lash::LashSession,
        TurnId,
        lash_core::store::ParkId,
        (String, String),
    )> {
        let session = self.open(tag).await?;
        let handle = session
            .send(TurnInput::text(format!("operator:{tag}")))
            .id(tag)
            .await?;
        let outcome = tokio::time::timeout(WAIT, handle.outcome())
            .await
            .context("park deadline")??;
        let TurnStatus::Parked(park) = outcome.status() else {
            anyhow::bail!("expected park, got {:?}", outcome.status());
        };
        ensure!(
            outcome.output().is_none(),
            "park cannot fabricate an answer"
        );
        let root = TurnId::from(tag);
        let child = self
            .child(&SessionId::from(format!("operator:{tag}")), &root)
            .await?;
        self.assert_open(&SessionId::from(format!("operator:{tag}")), &root, &child)
            .await?;
        ensure!(
            self.calls(tag).await? == 2,
            "park has exactly two recorded model effects"
        );
        Ok((session, root, park.park_id, child))
    }

    async fn assert_open(
        &self,
        session: &SessionId,
        root: &TurnId,
        child: &(String, String),
    ) -> Result<()> {
        let terminal: Option<String> = sqlx::query_scalar(
            "SELECT terminal_kind FROM lash_session_roots WHERE session_id = $1 AND root = $2",
        )
        .bind(session.as_str())
        .bind(root.as_str())
        .fetch_one(&self.pool)
        .await?;
        let closes: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_parent_end_plans WHERE parent_kind = 'turn' AND parent_id = $1")
            .bind(&child.1).fetch_one(&self.pool).await?;
        let cancelled: Option<i64> = sqlx::query_scalar(
            "SELECT cancel_requested_at_ms FROM lash_processes WHERE process_id = $1",
        )
        .bind(&child.0)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            terminal.is_none() && closes == 0 && cancelled.is_none(),
            "park keeps root and child scope open"
        );
        Ok(())
    }
}

struct FaultPlugin {
    repaired: Arc<AtomicBool>,
    pool: PgPool,
}

impl PluginFactory for FaultPlugin {
    fn id(&self) -> &'static str {
        "operator-fault"
    }

    fn declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PluginFactory::id(self))
    }
    fn build(
        &self,
        _: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(Self {
            repaired: self.repaired.clone(),
            pool: self.pool.clone(),
        }))
    }
}

impl SessionPlugin for FaultPlugin {
    fn id(&self) -> &'static str {
        "operator-fault"
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), lash::plugins::PluginError> {
        reg.tools().provider(Arc::new(StaticToolProvider::new(vec![
            ToolDefinition::raw("tool:operator-hold", "hold", "Operator runbook fault boundary.",
                json!({"type":"object","properties":{"running":{"type":"boolean"}},"required":["running"],"additionalProperties":false}),
                json!({"type":"null"})).with_tool_binding(ToolBinding::new(["tools"], "hold")),
        ], Hold)))?;
        let repaired = self.repaired.clone();
        let pool = self.pool.clone();
        // The fault fires once the root's work has started its child: the
        // response derivation of the next model call fails until the
        // operator repairs it. That step retries a live fault, so the engine
        // pauses the root and parks it with the paid completion journaled. A
        // checkpoint records every fault as its outcome, so a fault there
        // fails the root for good (FIG-4636).
        reg.output().response(Arc::new(move |ctx| {
            let repaired = repaired.clone();
            let pool = pool.clone();
            Box::pin(async move {
                if ctx.session_id.as_str() != "operator:running"
                    && !repaired.load(Ordering::SeqCst)
                {
                    let has_child: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lash_processes p JOIN lash_session_roots r ON p.lifetime_scope_id = 'turn:' || octet_length(r.session_id)::text || ':' || r.session_id || ':' || octet_length(r.root)::text || ':' || r.root WHERE r.session_id = $1 AND r.terminal_kind IS NULL)")
                        .bind(ctx.session_id.as_str()).fetch_one(&pool).await
                        .map_err(|error| lash::plugins::PluginError::Runtime(lash_core::RuntimeError::new(lash_core::RuntimeErrorCode::StoreCommitFailed, error.to_string())))?;
                    if has_child {
                        return Err(lash::plugins::PluginError::Runtime(
                            lash_core::RuntimeError::new(
                                lash_core::RuntimeErrorCode::StoreCommitFailed,
                                "operator runbook repairable environment-store fault",
                            ),
                        ));
                    }
                }
                Ok(lash::plugins::AssistantResponseTransform {
                    response: ctx.response,
                    events: Vec::new(),
                })
            })
        }));
        Ok(())
    }
}

struct Hold;
#[async_trait]
impl StaticToolExecute for Hold {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        if call.args["running"] == true {
            let Some(cancel) = call.context.cancellation_token() else {
                return ToolOutcome::err(json!("operator hold has no cancellation token")).into();
            };
            cancel.cancelled().await;
            return ToolOutcome::cancelled("operator hold cancelled").into();
        }
        ToolOutcome::ok(Value::Null).into()
    }
}

fn emit(case: &str, detail: Value) {
    println!("{}", json!({"case":case,"detail":detail,"passed":true}));
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let mut h = Harness::new().await?;
    let session = h.open("withdraw").await?;
    // The endpoint has not been registered: admission cannot race withdrawal.
    let sent = session
        .send(TurnInput::text("operator:withdraw"))
        .id("withdraw")
        .await?;
    let input = sent.input_id().clone();
    ensure!(matches!(
        session.cancel(CancelTarget::Input(input.clone())).await?,
        lash::CancelReceipt::Withdrawn(_)
    ));
    ensure!(sent.outcome().await?.status() == TurnStatus::Cancelled);
    let terminal: bool = sqlx::query_scalar("SELECT state = 'cancelled' AND terminal_at_ms IS NOT NULL AND admitted_root IS NULL FROM lash_pending_turn_inputs WHERE input_id = $1")
        .bind(input.as_str()).fetch_one(&h.pool).await?;
    ensure!(terminal && h.calls("withdraw").await? == 0);
    ensure!(matches!(
        session.cancel(CancelTarget::Input(input)).await?,
        lash::CancelReceipt::Withdrawn(_)
    ));
    h.serve().await?;
    emit(
        "withdrawal",
        json!({"input_terminal":terminal,"model_calls":h.calls("withdraw").await?}),
    );

    let session = h.open("running").await?;
    let sent = session
        .send(TurnInput::text("operator:running"))
        .id("running")
        .await?;
    let root = TurnId::from("running");
    let sid = SessionId::from("operator:running");
    let child = h.child(&sid, &root).await?;
    let request = session
        .cancel(CancelTarget::Root(root.clone()))
        .request_id("running-cancel")
        .await?;
    ensure!(matches!(request, lash::CancelReceipt::Requested { .. }));
    ensure!(tokio::time::timeout(WAIT, sent.outcome()).await??.status() == TurnStatus::Cancelled);
    let before = h.terminal(&sid, &root, "cancelled", &child, false).await?;
    let reopened = h.core.session(sid.clone()).open().await?;
    for _ in 0..3 {
        ensure!(matches!(
            reopened
                .cancel(CancelTarget::Root(root.clone()))
                .request_id("running-cancel")
                .await?,
            lash::CancelReceipt::AlreadySettled { .. }
        ));
    }
    ensure!(before == h.terminal(&sid, &root, "cancelled", &child, false).await?);
    emit("running_cancel", before);

    let (_session, root, park, child) = h.parked("redrive").await?;
    let sid = SessionId::from("operator:redrive");
    let target = lash::ParkedWorkRef::Turn {
        session_id: sid.clone(),
        turn_id: root.clone(),
    };
    let admission: String = sqlx::query_scalar(
        "SELECT admission_json FROM lash_session_roots WHERE session_id = $1 AND root = $2",
    )
    .bind(sid.as_str())
    .bind(root.as_str())
    .fetch_one(&h.pool)
    .await?;
    let journal_before = h.journal(&sid, &root, "redrive-before").await?;
    h.repaired.store(true, Ordering::SeqCst);
    let accepted = h.core.parked_work().redrive(&target, park).await?;
    let lash::RedriveAccepted::Root(accepted) = accepted else {
        anyhow::bail!("root redrive receipt");
    };
    ensure!(accepted.root == root && accepted.applied);
    let output = tokio::time::timeout(WAIT, async {
        loop {
            let output = h
                .core
                .session(sid.clone())
                .open()
                .await?
                .root(root.clone())
                .outcome()
                .await?;
            if !matches!(output.status(), TurnStatus::Parked(_)) {
                return Ok::<_, anyhow::Error>(output);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let answer = output
        .into_output()
        .context("redrive returns real output")?;
    ensure!(answer.is_success() && answer.final_value() == Some(&json!("real answer")));
    let after: String = sqlx::query_scalar(
        "SELECT admission_json FROM lash_session_roots WHERE session_id = $1 AND root = $2",
    )
    .bind(sid.as_str())
    .bind(root.as_str())
    .fetch_one(&h.pool)
    .await?;
    ensure!(
        admission == after && h.calls("redrive").await? == 3,
        "redrive preserves root admission and model journal"
    );
    let journal_after = h.journal(&sid, &root, "redrive-after").await?;
    ensure!(journal_before["invocation"] == journal_after["invocation"]);
    let before_commands = journal_before["commands"]
        .as_array()
        .context("recorded prefix")?;
    let after_commands = journal_after["commands"]
        .as_array()
        .context("continued commands")?;
    ensure!(
        after_commands.starts_with(before_commands),
        "redrive preserves the exact recorded command bytes"
    );
    let mut detail = h.terminal(&sid, &root, "answered", &child, false).await?;
    detail["same_admission"] = json!(true);
    detail["model_calls"] = json!(h.calls("redrive").await?);
    detail["same_journal_prefix"] = json!(true);
    detail["recorded_commands"] = json!(before_commands.len());
    emit("parked_redrive", detail);
    h.repaired.store(false, Ordering::SeqCst);

    let (_, cancel_root, cancel_park, cancel_child) = h.parked("park-cancel").await?;
    let (_, fork_root, fork_park, fork_child) = h.parked("park-fork").await?;
    let cancel_sid = SessionId::from("operator:park-cancel");
    let fork_sid = SessionId::from("operator:park-fork");
    let target = lash::ParkedWorkRef::Turn {
        session_id: cancel_sid.clone(),
        turn_id: cancel_root.clone(),
    };
    // Lose the operator's first reply, then reconnect and repeat its exact address.
    let lost = h.core.parked_work().cancel(&target, cancel_park).await?;
    drop(lost);
    let repeated = h
        .acknowledged_receipt(&cancel_sid, &cancel_root, cancel_park, "cancel")
        .await?;
    let cancel_before = h
        .terminal(&cancel_sid, &cancel_root, "cancelled", &cancel_child, true)
        .await?;
    for _ in 0..3 {
        ensure!(matches!(
            h.core.parked_work().cancel(&target, cancel_park).await,
            Err(lash::ParkVerbRefused::NotParked)
        ));
        ensure!(
            h.receipt(&cancel_sid, &cancel_root, cancel_park, "cancel")
                .await?
                == repeated
        );
    }
    ensure!(
        cancel_before
            == h.terminal(&cancel_sid, &cancel_root, "cancelled", &cancel_child, true)
                .await?
    );
    h.assert_open(&fork_sid, &fork_root, &fork_child).await?;
    emit("parked_cancel", cancel_before);

    let lost = h
        .core
        .parked_work()
        .fork(&fork_sid, &fork_root, fork_park)
        .await?;
    drop(lost);
    let repeated_fork = h
        .acknowledged_receipt(&fork_sid, &fork_root, fork_park, "fork")
        .await?;
    let lash_core::store::ControlIntentKind::Fork {
        new_root: Some(successor),
        ..
    } = repeated_fork.kind.clone()
    else {
        anyhow::bail!("addressed fork successor");
    };
    ensure!(successor != fork_root);
    let successor_outcome = tokio::time::timeout(
        WAIT,
        h.core
            .session(fork_sid.clone())
            .open()
            .await?
            .root(successor.clone())
            .outcome(),
    )
    .await??;
    ensure!(
        matches!(successor_outcome.status(), TurnStatus::Parked(_)),
        "fork successor is addressable"
    );
    let successor_child = h.child(&fork_sid, &successor).await?;
    ensure!(successor_child.0 != fork_child.0 && successor_child.1 != fork_child.1);
    h.assert_open(&fork_sid, &successor, &successor_child)
        .await?;
    let fork_before = h
        .terminal(&fork_sid, &fork_root, "cancelled", &fork_child, true)
        .await?;
    for _ in 0..3 {
        let next = h
            .core
            .parked_work()
            .fork(&fork_sid, &fork_root, fork_park)
            .await;
        ensure!(matches!(next, Err(lash::ParkVerbRefused::NotParked)));
        ensure!(h.receipt(&fork_sid, &fork_root, fork_park, "fork").await? == repeated_fork);
    }
    ensure!(
        fork_before
            == h.terminal(&fork_sid, &fork_root, "cancelled", &fork_child, true)
                .await?
    );
    let mut detail = fork_before;
    detail["successor"] = json!(successor);
    detail["original"] = json!(fork_root);
    detail["successor_scope_open"] = json!(true);
    emit("parked_fork", detail);
    let cancel_calls = h.calls("park-cancel").await?;
    let fork_calls = h.calls("park-fork").await?;
    let duplicate_effects: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT request_json::jsonb->'scope'->>'request_id' AS id FROM operator_model_calls GROUP BY id HAVING count(*) > 1) duplicates")
        .fetch_one(&h.pool).await?;
    ensure!(cancel_calls == 2 && fork_calls == 4 && duplicate_effects == 0);
    emit(
        "lost_reply_repeat",
        json!({"cancel_intent":repeated.id,"fork_intent":repeated_fork.id,"receipts_preserved":true,"stale_requests_refused":6,"repeats":3,"cancel_model_calls":cancel_calls,"fork_model_calls":fork_calls,"duplicate_model_effects":duplicate_effects}),
    );
    Ok(())
}
