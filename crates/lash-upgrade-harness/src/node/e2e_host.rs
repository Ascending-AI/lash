//! H1's prebuilt node fixture. Its controls use the public durable session
//! transport; only Lash's registered worker executes turns.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::{RestateArgs, StoreArgs, e2e_provider, e2e_tools::ToolFixtureArgs};
use crate::e2e::provider_http::wire;

#[derive(Debug, Args)]
pub struct ProviderHostArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    /// Scrubbed H1 configuration; no host credentials are accepted.
    #[arg(long)]
    pub config: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHostConfig {
    pub provider_url: String,
    pub tools: ToolFixtureArgs,
    pub worker_bind: SocketAddr,
    pub control_bind: SocketAddr,
    /// The owned V7 proxy URI, when ACK/proposal cuts are installed.
    pub deployment_uri: Option<String>,
    pub ready_file: PathBuf,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderHostCommand {
    Shutdown,
    Submit {
        session: String,
        id: String,
        text: String,
    },
    Attach {
        session: String,
        id: String,
    },
    Cancel {
        session: String,
        id: String,
    },
    Snapshot {
        session: String,
    },
    Address {
        session: String,
        id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderHostReady {
    pub worker: String,
    pub control: String,
    pub deployment: String,
    pub pid: u32,
    pub generation: String,
}

pub async fn serve(args: ProviderHostArgs) -> Result<()> {
    let config: ProviderHostConfig = serde_json::from_slice(&std::fs::read(args.config)?)?;
    ensure!(
        config.timeout_ms > 0,
        "host fixture needs a bounded deadline"
    );
    let stores = super::open_stores(&args.store).await?;
    let engine = super::engine(Arc::clone(&stores), &args.restate)?;
    let core = e2e_provider::core_with_tools(
        lash::Backend::new(engine.clone()),
        &config.provider_url,
        lash::provider::ProviderReliability::default()
            .max_attempts(2)
            .base_delay_ms(0)
            .max_delay_ms(0),
        config.tools.clone(),
    )?;
    let worker = lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)
        .map_err(|error| anyhow!("H1 worker: {error}"))?;
    let endpoint = engine.endpoint_builder(worker)?.build();
    let listener = tokio::net::TcpListener::bind(config.worker_bind).await?;
    let worker_uri = format!("http://{}", listener.local_addr()?);
    let control = tokio::net::TcpListener::bind(config.control_bind).await?;
    let control_uri = format!("http://{}", control.local_addr()?);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
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
    let deployment = config
        .deployment_uri
        .clone()
        .unwrap_or_else(|| worker_uri.clone());
    let result = async {
        engine.register_deployment(&deployment).await?;
        super::write_atomically(&config.ready_file, &serde_json::to_vec(&ProviderHostReady {
            worker: worker_uri, control: control_uri, deployment, pid: std::process::id(),
            generation: engine.build_generation()?.to_string(),
        })?)?;
        let mut commands = JoinSet::new();
        let (shutdown, mut shutdown_requested) = tokio::sync::watch::channel(false);
        let signal = super::shutdown_signal();
        tokio::pin!(signal);
        loop {
            tokio::select! {
                signal = &mut signal => { signal?; break; }
                _ = shutdown_requested.changed() => break,
                accepted = control.accept() => {
                    let (mut stream, _) = accepted?;
                    let core = core.clone();
                    let stores = Arc::clone(&stores);
                    let timeout = Duration::from_millis(config.timeout_ms);
                    let restate = args.restate.clone();
                    let shutdown = shutdown.clone();
                    commands.spawn(async move {
                        let outcome = tokio::time::timeout(timeout, async {
                            let request = wire::read(&mut stream, false)
                                .await?
                                .context("HTTP request ended before headers")?;
                            ensure!(request.method == "POST" && request.path == "/command", "unknown H1 host transport");
                            let request: ProviderHostCommand = serde_json::from_value(request.body)?;
                            if matches!(request, ProviderHostCommand::Shutdown) {
                                wire::json(&mut stream, 200, &serde_json::json!({"shutdown":true})).await?;
                                shutdown.send_replace(true);
                                return Ok(None);
                            }
                            command(&core, stores.as_ref(), &restate, request).await.map(Some)
                        }).await.unwrap_or_else(|error| Err(error.into()));
                        let (status, value) = match outcome {
                            Ok(Some(value)) => (200, value),
                            Ok(None) => return Ok(()),
                            Err(error) => (500, serde_json::json!({ "fixture_error": format!("{error:#}") })),
                        };
                        wire::json(&mut stream, status, &value).await
                    });
                }
                Some(outcome) = commands.join_next(), if !commands.is_empty() => { outcome??; }
            }
        }
        commands.abort_all();
        while let Some(outcome) = commands.join_next().await {
            if let Err(error) = outcome { ensure!(error.is_cancelled(), "host control panicked: {error}"); }
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    let _ = stop.send(());
    serving.await?;
    core.shutdown().await?;
    drop(core);
    result
}

async fn command(
    core: &lash::LashCore,
    stores: &dyn lash::StoreSet,
    restate: &RestateArgs,
    command: ProviderHostCommand,
) -> Result<serde_json::Value> {
    let session_name = match &command {
        ProviderHostCommand::Submit { session, .. }
        | ProviderHostCommand::Attach { session, .. }
        | ProviderHostCommand::Cancel { session, .. }
        | ProviderHostCommand::Snapshot { session }
        | ProviderHostCommand::Address { session, .. } => session,
        ProviderHostCommand::Shutdown => anyhow::bail!("shutdown bypassed transport dispatcher"),
    };
    let id = lash::SessionId::parse(session_name.clone())?;
    if matches!(command, ProviderHostCommand::Submit { .. }) {
        match core
            .session(id.clone())
            .create(lash::SessionCreation::root(e2e_provider::spec()?))
            .await
        {
            Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let session = core.session(id.clone()).durable().await?;
    match command {
        ProviderHostCommand::Shutdown => anyhow::bail!("shutdown bypassed transport dispatcher"),
        ProviderHostCommand::Submit { id, text, .. } => {
            let accepted = session
                .send(lash::TurnInput::text(text))
                .id(lash::TurnId::parse(id)?)
                .await?;
            // Acceptance can precede Run admission. Observe the recorded
            // binding before returning an addressable host receipt.
            let run = loop {
                if let Some(run) = accepted.run().await? {
                    break run;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            Ok(serde_json::json!({ "acceptance": accepted.receipt(), "run": run }))
        }
        ProviderHostCommand::Attach { id, .. } => Ok(serde_json::to_value(
            session.attach_id(lash::TurnId::parse(id)?).output().await?,
        )?),
        ProviderHostCommand::Cancel { id, .. } => {
            let receipt = session.attach_id(lash::TurnId::parse(id)?).cancel().await?;
            match receipt {
                lash::CancelReceipt::Requested { run, receipt } => {
                    Ok(serde_json::json!({ "status": "requested", "run": run, "receipt": receipt }))
                }
                lash::CancelReceipt::AlreadySettled { run } => {
                    Ok(serde_json::json!({ "status": "already_settled", "run": run }))
                }
                lash::CancelReceipt::Withdrawn(receipt) => {
                    Ok(serde_json::json!({ "status": "withdrawn", "receipt": receipt }))
                }
                other => anyhow::bail!("unexpected session cancel receipt {other:?}"),
            }
        }
        ProviderHostCommand::Address { id: source, .. } => {
            let accepted = session.attach_id(lash::TurnId::parse(source)?);
            let run = accepted
                .run()
                .await?
                .ok_or_else(|| anyhow!("input has no admitted Run"))?;
            let store = stores.session_store_factory();
            let view =
                crate::restate_view::RestateView::new(&restate.admin_url, &restate.namespace)?;
            let mut reader =
                crate::e2e::evidence::RestateEvidenceReader::new("H1-address".into(), view, 7);
            let work = reader
                .bind_public_run(store.as_ref(), &id, &run, accepted.input_id().to_string())
                .await?;
            Ok(serde_json::json!({ "work": work, "invocation": work.segment, "protocol": 7 }))
        }
        ProviderHostCommand::Snapshot { .. } => {
            let view = session
                .read()
                .await?
                .ok_or_else(|| anyhow!("session has no committed view"))?;
            let store = stores.session_store_factory();
            let window = store
                .load_session_window(&id, lash_core::store::WindowSelector::Current)
                .await?
                .ok_or_else(|| anyhow!("session has no committed window"))?;
            let state = lash_core::store::window_state(window, store.fleet_format())?.state;
            let namespace_total = state
                .plugin_state()
                .and_then(|state| state.plugins.get(super::e2e_tools::PLUGIN))
                .and_then(|namespace| namespace.values.get("total"))
                .and_then(serde_json::Value::as_u64);
            let applications = session.turn_input_applications().await?;
            let observation = crate::e2e::provider_http::scenarios::ProviderStoreObservation {
                assistant_replies: view
                    .messages()
                    .iter()
                    .filter(|message| {
                        message.role == lash::messages::MessageRole::Assistant
                            && message.reply_marker.is_some()
                    })
                    .count(),
                input_applications: applications.len(),
                unfinished: session.unfinished_run().await?.is_some(),
                pending_inputs: session.pending_turn_inputs().await?.len(),
                namespace_total,
                input_tokens: view.token_usage().input_tokens,
                output_tokens: view.token_usage().output_tokens,
            };
            Ok(
                serde_json::json!({ "observation": observation, "view": view.to_snapshot(), "applications": applications }),
            )
        }
    }
}
