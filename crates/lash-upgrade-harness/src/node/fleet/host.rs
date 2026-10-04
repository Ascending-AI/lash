//! Separate fleet worker processes with the public send/attach/cancel facade.
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::e2e::case::Channel;
use crate::e2e::control::{Barrier, BarrierKind, BarrierProof, FileBarriers, WorkIdentity};
use crate::e2e::host::{HostCommand, HostObservation};
use crate::node::tools::{BodyResult, BodyStep, FixtureProtocol, ToolBodies, ToolDelivery};
use crate::node::{RestateArgs, ServeReady, StoreArgs};
use crate::restate_view::RestateView;

mod body_identity;
mod control;
mod receiver;

#[derive(Clone, Debug, Args)]
pub struct FleetServeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long)]
    pub scenario: String,
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub directory: PathBuf,
    #[arg(long)]
    pub barrier_directory: PathBuf,
    #[arg(long)]
    pub bind: SocketAddr,
    #[arg(long)]
    pub control_socket: PathBuf,
    #[arg(long)]
    pub ready_file: PathBuf,
    #[arg(long)]
    pub publication_cut: bool,
    #[arg(long, default_value_t = 180)]
    pub timeout_secs: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FleetReady {
    pub serving: ServeReady,
    pub control: String,
}

struct State {
    core: lash::LashCore,
    engine: Arc<lash::restate::RestateEngine>,
    receiver: Arc<std::sync::OnceLock<lash::ProcessId>>,
    stores: Arc<dyn lash::StoreSet>,
    args: FleetServeArgs,
    deadline: Instant,
}

pub async fn serve(args: FleetServeArgs) -> Result<()> {
    ensure!(
        matches!(args.scenario.as_str(), "S14" | "S15" | "S16"),
        "unknown fleet scenario"
    );
    std::fs::create_dir_all(&args.directory)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let session = lash::SessionId::parse(&args.session)?;
    let stores = super::super::open_stores(&args.store).await?;
    let (stores, held) = if args.publication_cut {
        let (stores, held) = super::intercept_publication(stores, session.clone());
        (stores, Some(held))
    } else {
        (stores, None)
    };
    let engine = super::super::engine(stores.clone(), &args.restate)?;
    let backend = lash::Backend::new(engine.clone());
    let labels: &[&str] = if args.scenario == "S15" {
        &["intent"]
    } else {
        &["a", "b"]
    };
    let scripted = if args.scenario == "S15" { "S08" } else { "S02" };
    let provider = crate::node::tools::scripted_provider(
        scripted,
        labels,
        FixtureProtocol::Standard,
        &args.directory.join("provider.jsonl"),
    )?;
    let profiles = lash::LlmProfileRegistry::new().register(
        super::super::PROFILE_KEY,
        lash::RegisteredLlmProfile::new(super::super::model()?, provider),
    )?;
    let callback_args = args.clone();
    let callback_stores = stores.clone();
    let body_identity_lock = Arc::new(tokio::sync::Mutex::new(()));
    let callback = Arc::new(move |delivery: ToolDelivery| -> BodyStep {
        let args = callback_args.clone();
        let stores = callback_stores.clone();
        let identity_lock = body_identity_lock.clone();
        Box::pin(async move {
            let run = delivery
                .logical_run
                .as_ref()
                .context("body has no logical Run")?;
            let mut work = {
                let _capture = identity_lock.lock().await;
                body_identity::capture(
                    &args.barrier_directory.join("body-owner.json"),
                    &args.session,
                    run,
                    observe_work(&args, stores.as_ref(), run, deadline),
                )
                .await?
            };
            work.call = Some(delivery.call_id.to_string());
            work.ordinal = Some(delivery.ordinal);
            let barrier = Barrier {
                work,
                kind: BarrierKind::BodyEntered,
            };
            let barriers = FileBarriers::new(args.barrier_directory.clone(), deadline)?;
            if (args.scenario == "S14" && delivery.label == "b") || args.scenario == "S15" {
                barriers.hold(&barrier)?;
            }
            let artifact = args
                .directory
                .join(format!("body-{}.json", delivery.call_id));
            super::super::write_atomically(&artifact, &serde_json::to_vec(&delivery)?)?;
            barriers
                .enter(&barrier, artifact.display().to_string())
                .await
        })
    });
    let mut plan = BTreeMap::new();
    let receiver = Arc::new(std::sync::OnceLock::new());
    for label in labels {
        plan.insert(
            (*label).to_owned(),
            if args.scenario == "S15" {
                BodyResult::EmitToReceiver {
                    value: serde_json::json!("intent"),
                    receiver: receiver.clone(),
                    event_type: "tool_receipt".into(),
                }
            } else {
                BodyResult::Inline {
                    value: serde_json::json!(match *label {
                        "a" => "A",
                        "b" => "B",
                        value => value,
                    }),
                    intents: Default::default(),
                }
            },
        );
    }
    let tools =
        ToolBodies::open(&args.directory.join("deliveries.jsonl"), plan, callback)?.provider()?;
    let artifacts = lashlang::LashlangArtifacts::of_backend(&backend);
    let core = crate::node::tools::builder(
        backend.clone(),
        &Channel::Standard,
        Arc::new(profiles),
        tools,
    )
    .plugin(Arc::new(super::FleetFrontier))
    .plugin(Arc::new(super::super::process::ProcessEnginePlugin(
        artifacts.clone(),
        backend.worker_recovery(),
    )))
    .recovery_lease(super::super::recovery_lease())
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "fleet-host",
        format!("{}", std::process::id()),
    ))?;
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)?;
    let builder = super::super::process::bind(
        engine.endpoint_builder(worker)?,
        &args.restate.namespace,
        super::super::process::HarnessProcesses {
            core: core.clone(),
            artifacts,
            build_generation: engine.build_generation()?.clone(),
            authority: lash::restate::RestateAuthorityId::new(&args.restate.authority)?,
            namespace: lash::restate::RestateNamespace::new(&args.restate.namespace)?,
            model: super::super::llm_profile_config()?,
        },
    )?;
    let endpoint = receiver::bind(builder, &args.restate, core.clone())?.build();
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    let uri = format!("http://{}", listener.local_addr()?);
    let control = tokio::net::UnixListener::bind(&args.control_socket).with_context(|| {
        format!(
            "bind fleet control socket {}",
            args.control_socket.display()
        )
    })?;
    let control_address = args.control_socket.display().to_string();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let endpoint_task = tokio::spawn(async move {
        lash::restate::serve_endpoint(
            listener,
            endpoint,
            lash::restate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
            async move {
                let _ = stopped.await;
            },
        )
        .await;
    });
    let state = Arc::new(State {
        core,
        engine: engine.clone(),
        receiver,
        stores,
        args: args.clone(),
        deadline,
    });
    let publication = held.map(|mut held| {
        let state = state.clone();
        tokio::spawn(async move {
            let request = held
                .recv()
                .await
                .context("publication cut closed without a request")?;
            let run = &request
                .commit()
                .run_terminal
                .as_deref()
                .context("missing held terminal")?
                .run;
            let (work, _) = observe_work(&state.args, state.stores.as_ref(), run, deadline).await?;
            let path = state.args.directory.join("publication-request.json");
            request.capture(&path)?;
            let barriers = FileBarriers::new(state.args.barrier_directory.clone(), deadline)?;
            let cut = Barrier {
                work: work.clone(),
                kind: BarrierKind::PublicationRequest,
            };
            barriers.hold(&cut)?;
            barriers.enter(&cut, path.display().to_string()).await?;
            let receipt = request.release_terminal().await?;
            let path = state.args.directory.join("publication-committed.json");
            super::super::write_atomically(&path, &serde_json::to_vec(&receipt)?)?;
            barriers.publish(&BarrierProof {
                barrier: Barrier {
                    work,
                    kind: BarrierKind::SideEffectAccepted,
                },
                artifact: path.display().to_string(),
                journal_index: None,
            })?;
            Ok::<_, anyhow::Error>(())
        })
    });
    let ready = FleetReady {
        serving: ServeReady {
            build: crate::identity::BuildLabel::current(),
            generation: engine.build_generation()?.to_string(),
            uri,
        },
        control: control_address,
    };
    super::super::write_atomically(&args.ready_file, &serde_json::to_vec(&ready)?)?;
    let mut commands = JoinSet::new();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                shutdown = super::super::shutdown_signal() => { shutdown?; break; },
                Some(completed) = commands.join_next(), if !commands.is_empty() => { completed??; },
                accepted = control.accept() => {
                    let (socket, _) = accepted?;
                    let state = state.clone();
                    commands.spawn(async move {
                        let (input, mut output) = socket.into_split();
                        let Some(command) = control::read_command(BufReader::new(input)).await? else { return Ok(()); };
                        let answer = command_host(&state, command).await.map_err(|error| format!("{error:#}"));
                        let mut bytes = serde_json::to_vec(&answer)?;
                        bytes.push(b'\n');
                        output.write_all(&bytes).await?;
                        output.shutdown().await?;
                        Ok::<_,anyhow::Error>(())
                    });
                }
            }
        }
        Ok(())
    }.await;
    drop(control);
    std::fs::remove_file(&args.control_socket).context("remove owned fleet control socket")?;
    commands.abort_all();
    while let Some(result) = commands.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            return Err(error.into());
        }
    }
    let _ = stop.send(());
    endpoint_task.await.context("fleet endpoint panicked")?;
    if let Some(publication) = publication {
        publication
            .await
            .context("publication controller panicked")??;
    }
    result
}

async fn command_host(state: &State, command: HostCommand) -> Result<HostObservation> {
    let session_id = lash::SessionId::parse(&state.args.session)?;
    let session = state.core.session(session_id.clone());
    if let HostCommand::Process { action, input } = &command {
        let output = match action.as_str() {
            "register-uri" => {
                let uri = input
                    .as_str()
                    .context("registration URI must be a string")?;
                state.engine.register_deployment(uri).await?;
                serde_json::json!({"registered":uri,"generation":state.engine.build_generation()?})
            }
            "receiver-register" => {
                match session
                    .create(lash::SessionCreation::root(super::super::session_spec()))
                    .await
                {
                    Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
                let receipt = receiver::register(&state.args.restate, &session_id).await?;
                if let Some(previous) = state.receiver.get() {
                    ensure!(*previous == receipt.process_id, "receiver identity changed");
                } else {
                    state
                        .receiver
                        .set(receipt.process_id.clone())
                        .map_err(|_| anyhow::anyhow!("receiver concurrently bound"))?;
                }
                serde_json::to_value(receipt)?
            }
            "receiver-bind" => {
                let receipt: lash::process::ProcessStartReceipt =
                    serde_json::from_value(input.clone())?;
                let record = state
                    .core
                    .process_registry()
                    .get_process(&receipt.process_id)
                    .await?
                    .context("receiver binding names no retained process")?;
                ensure!(
                    record.start_key == receipt.start_key && receipt.start_key.is_some(),
                    "receiver binding does not match its actual start receipt"
                );
                ensure!(
                    record.input.as_ref()
                        == &lash::process::ProcessInput::External {
                            metadata: serde_json::json!({"fixture":"h2-receiver","session":&session_id}),
                        },
                    "receiver binding names another fixture or session"
                );
                ensure!(
                    record
                        .event_types
                        .iter()
                        .any(|kind| kind.name == "tool_receipt"),
                    "receiver did not admit tool_receipt events"
                );
                state
                    .receiver
                    .set(receipt.process_id.clone())
                    .map_err(|_| anyhow::anyhow!("receiver already bound"))?;
                serde_json::to_value(receipt)?
            }
            "receiver-events" => serde_json::to_value(
                crate::node::tools::receiver_events(
                    &state.core,
                    state.receiver.get().context("receiver not registered")?,
                )
                .await?,
            )?,
            "drain" | "drain-status" => {
                use lash_core::ClockWallTime as _;
                let generation = state.engine.build_generation()?.clone();
                let now = lash_core::facade_support::SystemClock.timestamp_ms();
                let drain = state.stores.generation_drain();
                if action == "drain" {
                    drain.mark_draining(&generation, now).await?;
                }
                let registry = lash::restate::RestateDeploymentRegistry::new(
                    lash::restate::RestateAdminClient::new(state.args.restate.admin_url.clone()),
                );
                let status = lash_core::store::generation_drain::GenerationDrainStatus::collect(
                    drain.as_ref(),
                    state.stores.session_delete_ledger().as_ref(),
                    |kind| state.stores.obligation_ledger(kind),
                    &registry,
                    &generation,
                    now,
                )
                .await?;
                serde_json::json!({"generation":generation,"drained":status.drained()})
            }
            _ => bail!("unsupported fleet setup command {action}"),
        };
        // Setup has no accepted input, logical Run or execution segment.
        return Ok(HostObservation {
            work: WorkIdentity {
                ingress: String::new(),
                run: String::new(),
                segment: String::new(),
                call: None,
                ordinal: None,
            },
            output,
        });
    }
    let (run, output) = match command {
        HostCommand::Submit {
            session: requested,
            idempotency_key,
            input,
        } => {
            ensure!(
                requested == state.args.session,
                "submission names another fixture session"
            );
            let expected = match state.args.scenario.as_str() {
                "S14" => "partial-result",
                "S15" => "tool-cancel-race",
                "S16" => "stale-publication",
                _ => unreachable!(),
            };
            ensure!(
                input == serde_json::json!({"scenario":expected}),
                "submission differs from admitted fixture scenario"
            );
            match session
                .create(lash::SessionCreation::root(super::super::session_spec()))
                .await
            {
                Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            let scenario = if state.args.scenario == "S15" {
                "S08"
            } else {
                "S02"
            };
            let session = state.core.session(session_id.clone()).durable().await?;
            let handle = session
                .send(lash::TurnInput::text(scenario))
                .id(lash::TurnId::parse(idempotency_key)?)
                .into_future()
                .await?;
            let receipt = handle.receipt().clone();
            let run = loop {
                if let Some(run) = state
                    .stores
                    .session_store_factory()
                    .run_of_input(&session_id, &receipt.input_id)
                    .await?
                {
                    break run;
                }
                ensure!(
                    Instant::now() < state.deadline,
                    "accepted input never acquired a Run binding"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            (run, serde_json::to_value(receipt)?)
        }
        HostCommand::Attach { run } => {
            let run = lash::TurnId::parse(run)?;
            let session = state.core.session(session_id.clone()).durable().await?;
            let outcome = session.run(run.clone()).outcome().await?;
            (
                run,
                serde_json::json!({"status":format!("{:?}",outcome.status()),
                "reply":outcome.output().and_then(|output| output.assistant_message())}),
            )
        }
        HostCommand::Cancel { run } => {
            let run = lash::TurnId::parse(run)?;
            let session = state.core.session(session_id.clone()).durable().await?;
            let receipt = session.cancel(lash::CancelTarget::Run(run.clone())).await?;
            let output = match receipt {
                lash::CancelReceipt::Requested {
                    run: target,
                    receipt,
                } => {
                    ensure!(target == run, "cancel receipt names another Run");
                    serde_json::json!({"kind":"requested", "receipt":receipt})
                }
                lash::CancelReceipt::AlreadySettled { run: target } => {
                    ensure!(target == run, "cancel receipt names another Run");
                    serde_json::json!({"kind":"already_settled"})
                }
                other => bail!("unexpected fleet cancel outcome: {other:?}"),
            };
            (run, output)
        }
        other => bail!("unsupported fleet public command: {other:?}"),
    };
    let (work, protocol) =
        observe_work(&state.args, state.stores.as_ref(), &run, state.deadline).await?;
    let path = state.args.directory.join("accepted-work.json");
    super::super::write_atomically(
        &path,
        &serde_json::to_vec(&serde_json::json!({"work":&work,"protocol":protocol}))?,
    )?;
    Ok(HostObservation { work, output })
}

async fn observe_work(
    args: &FleetServeArgs,
    stores: &dyn lash::StoreSet,
    run: &lash::TurnId,
    deadline: Instant,
) -> Result<(WorkIdentity, u32)> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Invocation {
        id: String,
        pinned_service_protocol_version: Option<u32>,
    }
    let session = lash::SessionId::parse(&args.session)?;
    let url = match &args.store.store {
        crate::node::StoreSpec::Postgres(url) => url,
        _ => bail!("fleet requires PostgreSQL"),
    };
    let pool = sqlx::PgPool::connect(url).await?;
    let view = RestateView::new(&args.restate.admin_url, &args.restate.namespace)?;
    loop {
        if let Some(key) = lash::restate::recorded_turn_invocation_key(
            stores.session_store_factory().as_ref(),
            &session,
            run,
        )
        .await?
        {
            let key = key.replace('\'', "''");
            let prefix = view.service_name("LashTurn").replace('\'', "''");
            let rows: Vec<Invocation> = view.query(&format!("SELECT id, pinned_service_protocol_version FROM sys_invocation WHERE target_service_name LIKE '{prefix}%' AND target_service_key = '{key}' AND target_handler_name = 'run'")).await?;
            if rows.len() == 1 {
                let protocol = rows[0]
                    .pinned_service_protocol_version
                    .context("accepted invocation has no negotiated protocol")?;
                ensure!(protocol == 7, "fleet requires actual negotiated V7");
                let inputs: Vec<String> = sqlx::query_scalar("SELECT input_id FROM lash_session_run_inputs WHERE session_id=$1 AND run=$2 ORDER BY input_id")
                    .bind(session.as_str()).bind(run.as_str()).fetch_all(&pool).await?;
                pool.close().await;
                ensure!(
                    inputs.len() == 1,
                    "fleet Run must bind exactly one accepted input"
                );
                return Ok((
                    WorkIdentity {
                        ingress: inputs[0].clone(),
                        run: run.to_string(),
                        segment: rows[0].id.clone(),
                        call: None,
                        ordinal: None,
                    },
                    protocol,
                ));
            }
            ensure!(
                rows.len() <= 1,
                "accepted Run has conflicting engine invocations"
            );
        }
        ensure!(
            Instant::now() < deadline,
            "actual ingress/Run/segment binding never became observable"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
