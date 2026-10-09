use super::{Args, Case, Meter, Receipt};
use anyhow::{Result, ensure};
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmResponse};
use lash_core::provider::{ProviderHandle, ProviderOptions, ProviderReliability};
use lash_core::testing::TestProvider;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn metadata() -> Result<lash::LlmProfileMetadata> {
    Ok(lash::LlmProfileMetadata::builder("boundary-model")
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()?)
}
pub(super) fn spec() -> Result<lash::SessionSpec> {
    Ok(lash::SessionSpec::new(
        metadata()?.wire_model,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(12)))
}
pub(super) fn backend(stores: Arc<dyn lash_core::StoreSet>) -> Result<lash::Backend> {
    let mut settings = lash::durable::DurableSettings::standard();
    settings.activation_loop_budget = 1;
    Ok(lash::durable::DurableBackendBuilder::new(stores)
        .config(settings)
        .build()?)
}
pub(super) fn response(text: String) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
        ..Default::default()
    }
}
pub(super) fn provider(meter: &Meter, pending: bool) -> ProviderHandle {
    let meter = meter.clone();
    TestProvider::builder().kind("boundary-synthetic")
        .options(ProviderOptions { reliability: ProviderReliability::disabled(), ..Default::default() })
        .complete(move |request| {
            let meter = meter.clone();
            async move {
                let start = Instant::now();
                if request.messages.iter().any(|m| m.blocks.iter().any(|b|
                    matches!(b, LlmContentBlock::Text { text, .. } if text.contains("boundary-process-hold")))) {
                    meter.record("process.provider.held", 1, start);
                    std::future::pending::<()>().await;
                }
                let result = if pending && !request.messages.iter().any(|m|
                    m.blocks.iter().any(|b| matches!(b, LlmContentBlock::ToolResult { .. }))) {
                    LlmResponse { parts: vec![LlmOutputPart::ToolCall {
                        call_id: "boundary-deferred".into(), tool_name: "boundary_wait".into(),
                        input_json: "{}".into(), replay: None }], ..Default::default() }
                } else { response("boundary answered".into()) };
                meter.record("provider.complete", 1, start);
                Ok(result)
            }
        }).build().into_handle()
}

pub(super) fn build(
    backend: lash::Backend,
    owner: &str,
    serve: bool,
    registered: bool,
    provider: ProviderHandle,
) -> Result<lash::LashCore> {
    let mut builder = lash::LashCore::standard_builder(backend)
        .serve_sessions(serve)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .tools(Arc::new(DeferredTools(definition()?)));
    if registered {
        builder = builder.serve_test_llm_profile(provider, metadata()?);
    }
    Ok(builder.build(lash::persistence::LeaseOwnerIdentity::opaque(
        "boundary", owner,
    ))?)
}

fn definition() -> Result<lash_core::ToolDefinition> {
    Ok(lash_core::ToolDefinition::raw(
        "tool:boundary_wait",
        "boundary_wait",
        "Synthetic deferred call",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )?
    .with_execution(Duration::from_secs(120))
    .with_declaration(lash_core::ToolDeclaration::deferring())
    .with_park(lash_core::ParkBound::Within(Duration::from_secs(120))))
}
struct DeferredTools(lash_core::ToolDefinition);
#[async_trait::async_trait]
impl lash_core::ToolProvider for DeferredTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.0.manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "boundary_wait").then(|| Arc::new(self.0.contract()))
    }
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
    }
}

pub(super) async fn create(core: &lash::LashCore, name: &str) -> Result<lash::DurableSession> {
    Ok(core
        .session(lash::SessionId::try_from(name.to_owned())?)
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec()?,
        ))
        .await?)
}
pub(super) async fn send(session: &lash::DurableSession, name: &str, meter: &Meter) -> Result<()> {
    let start = Instant::now();
    let handle = session
        .send(lash::TurnInput::text(name))
        .id(lash::TurnId::try_from(name.to_owned())?)
        .await?;
    meter.record("send.accept", 1, start);
    let start = Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(60), handle.output()).await??;
    ensure!(
        matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
        "send stopped: {:?}",
        output.result.outcome
    );
    meter.record("send.settle", 1, start);
    Ok(())
}

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::default();
    let mut storage = Vec::new();
    let mut cores = Vec::new();
    let store = if matches!(args.case, Case::PgFacade) {
        "postgres18-product"
    } else {
        "sqlite-file-product"
    };
    let nodes = if matches!(args.case, Case::PgFacade) {
        args.callers
    } else {
        1
    };
    for n in 0..nodes {
        let stores: Arc<dyn lash_core::StoreSet> = if matches!(args.case, Case::PgFacade) {
            let url = args
                .postgres_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("PG18 case requires --postgres-url"))?;
            let endpoints = lash_postgres_store::PostgresEndpoints::from_url(url)?;
            let pg = lash_postgres_store::PostgresStorage::connect(
                &endpoints,
                &Default::default(),
                Default::default(),
            )
            .await?;
            let version: String = sqlx::query_scalar("SHOW server_version_num")
                .fetch_one(pg.pool())
                .await?;
            ensure!(
                version.parse::<u32>()? / 10000 == 18,
                "PG facade requires PostgreSQL 18"
            );
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &pg,
                Arc::new(lash_core::attachments::UnavailableAttachmentStore),
            );
            storage.push(pg);
            Arc::new(stores)
        } else {
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(
                    args.store_dir.join("lash.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await?,
            )
        };
        let b = backend(stores)?;
        let core = build(
            b,
            &format!("node-{n}"),
            !matches!(args.case, Case::RootRedrive),
            true,
            provider(&meter, matches!(args.case, Case::ParkedTakeover)),
        )?;
        cores.push(core);
    }
    let result = match args.case {
        Case::RootRedrive => root_redrive(args.operations, &cores[0], &meter).await,
        Case::ParkedTakeover => takeover(args.operations, &cores[0], &meter).await,
        Case::ProcessLifecycle => lifecycle(args.operations, &cores[0], &meter).await,
        Case::TypedHistory => history(args.operations, &cores[0], &meter).await,
        Case::PgFacade => traffic(args.operations, &cores, &meter).await,
        _ => anyhow::bail!("not a facade workload"),
    };
    for core in &cores {
        core.shutdown().await?;
    }
    for pg in storage {
        pg.pool().close().await;
    }
    let evidence = result?;
    Ok(Receipt::new(
        args.case,
        if matches!(args.case, Case::RootRedrive) {
            "facade-send+durable-redrive-mail"
        } else {
            "facade"
        },
        store,
        args.operations,
        &meter,
        evidence,
    ))
}

async fn traffic(
    operations: usize,
    cores: &[lash::LashCore],
    meter: &Meter,
) -> Result<serde_json::Value> {
    let mut sessions = Vec::new();
    for (n, core) in cores.iter().enumerate() {
        sessions.push(create(core, &format!("traffic-{n}")).await?);
    }
    futures_util::future::try_join_all(sessions.iter().enumerate().map(
        |(lane, session)| async move {
            for n in (lane..operations).step_by(cores.len()) {
                send(session, &format!("traffic-{n}"), meter).await?;
            }
            anyhow::Ok(())
        },
    ))
    .await?;
    ensure!(
        meter.count("send.settle") == operations,
        "traffic lost sends"
    );
    Ok(serde_json::json!({"nodes": cores.len(), "settled": operations}))
}

async fn history(
    operations: usize,
    core: &lash::LashCore,
    meter: &Meter,
) -> Result<serde_json::Value> {
    let session = create(core, "history").await?;
    for n in 0..operations {
        send(&session, &format!("history-{n}"), meter).await?;
    }
    let start = Instant::now();
    let mut anchor = lash_core::store::HistoryAnchor::Head;
    let budget = lash_core::store::HistoryBudget {
        max_nodes: NonZeroU32::MIN,
        max_bytes: NonZeroU64::new(1024 * 1024).ok_or_else(|| anyhow::anyhow!("zero budget"))?,
    };
    let mut nodes = 0;
    loop {
        let at = Instant::now();
        let page = session.history(anchor, budget).await?;
        meter.record("history.snapshot.page", 1, at);
        nodes += page.nodes.len();
        match page.next {
            Some(cursor) => anchor = lash_core::store::HistoryAnchor::Cursor(cursor),
            None => break,
        }
    }
    meter.record("history.snapshot.nodes", nodes, start);
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = 0;
    loop {
        let at = Instant::now();
        let page = session
            .committed_turns(cursor.as_ref(), NonZeroU32::MIN)
            .await?;
        meter.record("history.typed.decode", page.turns.len(), at);
        let at = Instant::now();
        for turn in &page.turns {
            ensure!(
                seen.insert(turn.turn_id.clone()),
                "typed history repeated a turn"
            );
            entries += turn.entries.len();
        }
        meter.record("history.typed.fold", page.turns.len(), at);
        if page.turns.is_empty() {
            break;
        }
        cursor = Some(page.next);
    }
    ensure!(
        seen.len() == operations && entries > 0 && nodes > 0,
        "typed history omitted committed work"
    );
    Ok(serde_json::json!({"turns": seen.len(), "entries": entries, "nodes": nodes}))
}

async fn root_redrive(
    operations: usize,
    producer: &lash::LashCore,
    meter: &Meter,
) -> Result<serde_json::Value> {
    use lash::durable::{CommitLabel, MailAnswer, MailDomainWrite, MailTx, domain};
    for n in 0..operations {
        let session = create(producer, &format!("redrive-{n}")).await?;
        let handle = session.send(lash::TurnInput::text("redrive")).await?;
        let start = Instant::now();
        let broken = build(
            producer.backend().clone(),
            &format!("unbound-{n}"),
            true,
            false,
            provider(meter, false),
        )?;
        let actor = lash::durable::ActorKey::session(session.session_id().as_str())?;
        let park = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(snapshot) = broken.backend().durable().actor(&actor).await?
                    && snapshot.state == lash::durable::ActorState::Parked
                {
                    return anyhow::Ok(snapshot.park);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        ensure!(
            park.as_deref()
                .is_some_and(|reason| reason.contains("llm_profile_unavailable")),
            "root parked for an unexpected cause: {park:?}"
        );
        meter.record("root.park", 1, start);
        broken.shutdown().await?;
        let start = Instant::now();
        let serving = build(
            producer.backend().clone(),
            &format!("redriven-{n}"),
            true,
            true,
            provider(meter, false),
        )?;
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::Redrive(domain::RedriveRequest {
            actor: lash::durable::ActorKey::session(session.session_id().as_str())?,
            requester: "boundary-operator".into(),
        }));
        let receipt = serving
            .backend()
            .durable()
            .commit_mail(tx, CommitLabel::MAIL_PROCESS)
            .await?;
        ensure!(
            receipt
                .answers
                .iter()
                .any(|a| matches!(a, MailAnswer::Redrive(domain::RedriveAnswer::Redriven))),
            "root was not redriven"
        );
        meter.record("root.redrive.mail", 1, start);
        let start = Instant::now();
        let output = tokio::time::timeout(Duration::from_secs(60), handle.output()).await??;
        ensure!(
            matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
            "redrive did not finish"
        );
        meter.record("root.redrive.settle", 1, start);
        serving.shutdown().await?;
    }
    Ok(
        serde_json::json!({"parked": operations, "explicit_redrives": operations, "settled": operations}),
    )
}

async fn parked(
    core: &lash::LashCore,
    session: &lash::DurableSession,
) -> Result<lash::admin::ParkedCall> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let calls = core
                .completions()
                .parked(lash::admin::CallOwner::Session(
                    session.session_id().clone(),
                ))
                .await?;
            if let Some(call) = calls.into_iter().next() {
                return anyhow::Ok(call);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}
async fn takeover(
    operations: usize,
    first: &lash::LashCore,
    meter: &Meter,
) -> Result<serde_json::Value> {
    let mut current = first.clone();
    for n in 0..operations {
        let session = create(&current, &format!("takeover-{n}")).await?;
        let handle = session.send(lash::TurnInput::text("pending call")).await?;
        let start = Instant::now();
        let before = parked(&current, &session).await?;
        meter.record("call.park", 1, start);
        let start = Instant::now();
        current.shutdown().await?;
        meter.record("owner.release", 1, start);
        let actor = lash::durable::ActorKey::session(session.session_id().as_str())?;
        let released = first
            .backend()
            .durable()
            .actor(&actor)
            .await?
            .ok_or_else(|| anyhow::anyhow!("released actor missing"))?;
        let takeover_start = Instant::now();
        let start = Instant::now();
        current = build(
            first.backend().clone(),
            &format!("takeover-node-{n}"),
            true,
            true,
            provider(meter, true),
        )?;
        let after = parked(&current, &session).await?;
        ensure!(
            before == after,
            "takeover changed pinned call identity or deadline"
        );
        meter.record("call.restore.pinned", 1, start);
        let start = Instant::now();
        ensure!(
            matches!(
                current
                    .completions()
                    .resolve(
                        after.key.as_str(),
                        lash_core::Resolution::Ok(serde_json::json!({}))
                    )
                    .await?,
                lash_core::ResolveAnswer::Resolved
            ),
            "completion refused"
        );
        meter.record("call.resolve", 1, start);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(claimed) = current.backend().durable().actor(&actor).await?
                    && claimed.epoch > released.epoch
                {
                    return anyhow::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await??;
        meter.record("call.takeover", 1, takeover_start);
        let start = Instant::now();
        let output = tokio::time::timeout(Duration::from_secs(60), handle.output()).await??;
        ensure!(
            matches!(output.result.outcome, lash::TurnOutcome::Finished(_)),
            "takeover failed"
        );
        meter.record("call.settle", 1, start);
    }
    current.shutdown().await?;
    Ok(
        serde_json::json!({"takeovers": operations, "stable_call_keys": operations, "settled": operations, "loss": "graceful-node-stop", "new_owner_claims": operations}),
    )
}

async fn lifecycle(
    operations: usize,
    core: &lash::LashCore,
    meter: &Meter,
) -> Result<serde_json::Value> {
    create(core, "lifecycle-parent").await?;
    let mut ids = std::collections::BTreeSet::new();
    for n in 0..operations {
        let start = Instant::now();
        let request = lash_core::ProcessStartRequest::new(
            lash_core::ProcessInput::SessionTurn {
                definition_key: "boundary-child".into(),
                create_request: Box::new(lash_core::SessionCreateRequest::child(
                    lash::plugins::SessionToolAccess::ambient(),
                    "lifecycle-parent",
                    lash_core::SessionStartPoint::Empty,
                    lash_core::SessionPolicy {
                        model: Some(lash::LlmProfileConfig::new(lash::RecordedLlmProfile::mint(
                            lash::LlmProfileKey::new("boundary-model"),
                            metadata()?,
                        ))),
                        ..lash_core::SessionPolicy::new(
                            lash::TurnBudget::Unbounded,
                            lash::MaxToolCalls::new(1024),
                            lash::NoProgressBudget::bounded(12),
                        )
                    },
                    Default::default(),
                )),
                turn_input: Box::new(lash::TurnInput::text(if n % 2 == 1 {
                    "boundary-process-hold".to_owned()
                } else {
                    format!("child-{n}")
                })),
                result: lash_core::SessionTurnOutcome::Turn,
            },
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        );
        let started = core.processes().start(request, core.effect_host()).await?;
        meter.record("process.start", 1, start);
        ensure!(
            ids.insert(started.process_id.clone()),
            "process identity reused"
        );
        if n % 2 == 1 {
            tokio::time::timeout(Duration::from_secs(60), async {
                while meter.count("process.provider.held") < n.div_ceil(2) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            let start = Instant::now();
            core.processes()
                .cancel(&started.process_id, core.effect_host())
                .await?;
            meter.record("process.cancel", 1, start);
        }
        let start = Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            core.processes().await_output(&started.process_id),
        )
        .await??;
        let status = if n % 2 == 1 {
            lash_sansio::ToolCallStatus::Cancelled
        } else {
            lash_sansio::ToolCallStatus::Success
        };
        ensure!(
            matches!(&output, lash_core::ProcessAwaitOutput::Settled { output } if output.status() == status),
            "process failed: {output:?}"
        );
        meter.record("process.await_terminal", 1, start);
        let start = Instant::now();
        let record = core.processes().get(&started.process_id).await?;
        ensure!(record.is_some(), "process terminal not retained");
        meter.record("process.observe", 1, start);
    }
    Ok(
        serde_json::json!({"started": ids.len(), "terminal": operations, "waves": operations,
        "engine": "session-turn", "resident_population": 1, "cancelled": operations / 2, "completed": operations.div_ceil(2)}),
    )
}
