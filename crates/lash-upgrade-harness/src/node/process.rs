//! Durable processes across a roll: a TypeScript process that waits for
//! the signal `go` and ends with its payload.
//!
//! A process is started and signalled through lash's facade, which admits
//! its effects only inside a Restate handler, so every serving node binds
//! one harness workflow of its own beside lash's services: named for its
//! build and namespace, so the build a leg asks is the build that starts or
//! signals ([`HarnessOp`]). `process-start` and `process-signal` call it
//! through ingress; `process-status` reads the process from the store.
//!
//! A segment is dispatched on lash's stable process lane, so the newest
//! deployment at the start runs it, whichever build asked.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use lash_restate::restate_sdk;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use super::{RestateArgs, StoreArgs};
use crate::identity::BuildLabel;

/// The signal the process waits for.
pub const SIGNAL: &str = "go";

/// The process every leg starts: it waits for [`SIGNAL`] and ends with its
/// payload.
const SIGNAL_WAITING_PROCESS: &str = r#"
const worker = async () => {
  return await waitSignal("go");
};
finish(null);
"#;

/// The Restate name of `build`'s harness workflow in `namespace`.
pub fn harness_service(namespace: &str, build: BuildLabel) -> Result<String> {
    let namespace = lash_restate::RestateNamespace::new(namespace)
        .map_err(|error| anyhow!("namespace: {error}"))?;
    let local = match build {
        BuildLabel::N => "UpgradeHarnessProcessesN",
        BuildLabel::Next => "UpgradeHarnessProcessesNext",
    };
    Ok(namespace.service_name(local))
}

/// One facade call a harness workflow makes inside its handler.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HarnessOp {
    /// Start the signal-waiting process under `start_key`.
    Start { start_key: String },
    /// Signal `process_id` with [`SIGNAL`] as `signal_id`.
    Signal {
        process_id: String,
        signal_id: String,
        payload: serde_json::Value,
    },
}

/// What a harness workflow answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum HarnessReply {
    Started {
        build: BuildLabel,
        process_id: String,
        created: bool,
    },
    Signalled {
        build: BuildLabel,
        sequence: u64,
    },
}

// The struct-based service API has no way to bind one type under a name
// chosen at run time, which the per-namespace harness name needs; lash's own
// services are bound the same way.
#[allow(
    deprecated,
    reason = "only the trait-based service API exposes the dispatcher a renamed binding needs"
)]
mod service {
    use super::{HarnessOp, HarnessReply};
    use lash_restate::restate_sdk;
    use restate_sdk::prelude::*;

    #[restate_sdk::workflow]
    #[name = "UpgradeHarnessProcesses"]
    pub(crate) trait UpgradeHarnessProcesses {
        async fn run(op: Json<HarnessOp>) -> HandlerResult<Json<HarnessReply>>;
    }
}
use service::{ServeUpgradeHarnessProcesses, UpgradeHarnessProcesses};

/// A serving node's harness workflow, over the node's own core.
pub(crate) struct HarnessProcesses {
    pub(crate) core: lash::LashCore,
    pub(crate) artifacts: lashlang::LashlangArtifacts,
    pub(crate) authority: lash_restate::RestateAuthorityId,
    pub(crate) namespace: lash_restate::RestateNamespace,
    pub(crate) model: lash::ModelSpec,
}

fn terminal(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

impl UpgradeHarnessProcesses for HarnessProcesses {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(op): Json<HarnessOp>,
    ) -> HandlerResult<Json<HarnessReply>> {
        let operation = format!("upgrade-harness:{}", ctx.key());
        let controller =
            lash_restate::RestateRuntimeEffectController::new(ctx, self.authority.clone())
                .in_namespace(self.namespace.clone());
        let scoped = controller
            .scoped_effect_controller(lash_core::AdmittedScope::runtime_operation(operation))
            .map_err(terminal)?;
        let build = BuildLabel::current();
        match op {
            HarnessOp::Start { start_key } => {
                let request = start_request(&self.artifacts, &self.model, &start_key)
                    .await
                    .map_err(terminal)?;
                let receipt = self
                    .core
                    .processes()
                    .start(request, scoped)
                    .await
                    .map_err(terminal)?;
                Ok(Json(HarnessReply::Started {
                    build,
                    process_id: receipt.process_id.to_string(),
                    created: receipt.disposition
                        == lash_core::ProcessRegistrationDisposition::Created,
                }))
            }
            HarnessOp::Signal {
                process_id,
                signal_id,
                payload,
            } => {
                let process_id = lash_core::ProcessId::parse(&process_id).map_err(terminal)?;
                let event_type = lash_core::facade_support::process_signal_event_type(SIGNAL)
                    .map_err(terminal)?;
                let request = lash_core::ProcessEventAppendRequest::new(event_type, payload)
                    .with_replay_key(lash_core::facade_support::process_signal_wait_key(
                        &process_id,
                        SIGNAL,
                        &signal_id,
                    ));
                let event = self
                    .core
                    .processes()
                    .signal(&process_id, SIGNAL, signal_id, request, scoped)
                    .await
                    .map_err(terminal)?;
                Ok(Json(HarnessReply::Signalled {
                    build,
                    sequence: event.sequence,
                }))
            }
        }
    }
}

/// Bind `processes` into `builder` under this build's harness name in
/// `namespace`.
pub(crate) fn bind(
    builder: restate_sdk::endpoint::Builder,
    namespace: &str,
    processes: HarnessProcesses,
) -> Result<restate_sdk::endpoint::Builder> {
    use restate_sdk::service::Discoverable as _;
    let name = harness_service(namespace, BuildLabel::current())?;
    let mut discovery = ServeUpgradeHarnessProcesses::<HarnessProcesses>::discover();
    discovery.name = restate_sdk::discovery::ServiceName::try_from(name.clone())
        .map_err(|error| anyhow!("`{name}` is not a Restate service name: {error}"))?;
    Ok(
        builder.bind(restate_sdk::service::macro_support::service_definition(
            processes.serve(),
            discovery,
        )),
    )
}

/// The process's module: linked, and published to the store's artifacts.
async fn start_request(
    artifacts: &lashlang::LashlangArtifacts,
    model: &lash::ModelSpec,
    start_key: &str,
) -> Result<lash_core::ProcessStartRequest> {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash::typescript::link(SIGNAL_WAITING_PROCESS, &environment)
        .map_err(|error| anyhow!("link the signal-waiting process: {error:?}"))?;
    // A host pin keeps the module alive for the process (ADR 0113).
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .map_err(|error| anyhow!("the module's referrer claim: {error}"))?;
    artifacts
        .publish_module_artifact(&claim, &linked.artifact)
        .await
        .map_err(|error| anyhow!("publish the process module: {error}"))?;
    let process_name = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) => Some(process.name.to_string()),
            _ => None,
        })
        .context("the linked module declares no process")?;
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref(&process_name)
            .context("the process ref")?
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name,
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .map_err(|error| anyhow!("encode the process input: {error}"))?;
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: model.clone(),
            ..lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded)
        },
    );
    Ok(lash_core::ProcessStartRequest::new(
        input,
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(
        lash_core::StartKeyOwner::HOST,
        start_key,
    )))
    .with_env_spec(env)
    .with_extra_event_types(
        lash_lashlang_runtime::lashlang_process_event_types()
            .into_iter()
            .chain([lash_core::ProcessEventType {
                name: lash_core::facade_support::process_signal_event_type(SIGNAL)
                    .map_err(|error| anyhow!("signal event type: {error}"))?,
                payload_schema: lash_core::LashSchema::any(),
                semantics: Default::default(),
            }]),
    ))
}

/// Contributes the Lashlang process engine to a node's core, as the RLM
/// protocol does for a host that runs it.
pub(crate) struct ProcessEnginePlugin(pub(crate) lashlang::LashlangArtifacts);

struct NoSessionPlugin;

impl lash_core::facade_support::SessionPlugin for NoSessionPlugin {
    fn id(&self) -> &'static str {
        "lash-upgrade-harness-processes"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::facade_support::PluginFactory for ProcessEnginePlugin {
    fn id(&self) -> &'static str {
        "lash-upgrade-harness-processes"
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![
            lash_lashlang_runtime::lashlang_process_engine_registration(
                lash_lashlang_runtime::LashlangProcessEngine::new(
                    self.0.clone(),
                    lash_lashlang_runtime::LashlangSurface::default(),
                ),
            ),
        ])
    }

    fn build(
        &self,
        _context: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionPlugin))
    }
}

#[derive(Clone, Debug, Args)]
pub struct ProcessStartArgs {
    #[command(flatten)]
    pub restate: RestateArgs,
    /// The start's idempotency key.
    #[arg(long)]
    pub key: String,
}

#[derive(Clone, Debug, Args)]
pub struct ProcessSignalArgs {
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long)]
    pub process: String,
    /// The signal's id: a repeat of one id is one delivery.
    #[arg(long)]
    pub signal_id: String,
    /// The signal's payload as JSON.
    #[arg(long)]
    pub payload: String,
}

#[derive(Clone, Debug, Args)]
pub struct ProcessStatusArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long)]
    pub process: String,
}

/// What `process-start` and `process-signal` report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessOpReport {
    /// The build whose harness workflow the caller asked.
    pub caller: BuildLabel,
    pub reply: HarnessReply,
}

/// What `process-status` reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessStatusReport {
    pub process_id: String,
    /// The lifecycle, as the process record folds it (`Waiting`, …).
    pub lifecycle: String,
    /// The signal the process waits for, while it waits for one.
    pub waiting_for: Option<String>,
    /// Every signal event the process recorded, by payload: a delivery
    /// appears once.
    pub signals: Vec<serde_json::Value>,
    /// The value it ended with, once it ended successfully.
    pub output: Option<serde_json::Value>,
    /// Why it failed, once it failed.
    pub error: Option<String>,
}

async fn call_harness(restate: &RestateArgs, key: &str, op: &HarnessOp) -> Result<HarnessReply> {
    let service = harness_service(&restate.namespace, BuildLabel::current())?;
    let client = lash_restate::RestateIngressClient::new(lash_restate::RestateConnection::new(
        restate.ingress_url.clone(),
    ));
    client
        .call_workflow_json::<_, HarnessReply>(&service, key, "run", op)
        .await
        .map_err(|error| anyhow!("{service}/{key}/run: {error}"))
}

pub(super) async fn start(args: ProcessStartArgs) -> Result<HarnessOpReport> {
    let key = format!("start:{}", args.key);
    let reply = call_harness(
        &args.restate,
        &key,
        &HarnessOp::Start {
            start_key: args.key,
        },
    )
    .await?;
    Ok(HarnessOpReport {
        caller: BuildLabel::current(),
        reply,
    })
}

pub(super) async fn signal(args: ProcessSignalArgs) -> Result<HarnessOpReport> {
    let payload =
        serde_json::from_str(&args.payload).map_err(|error| anyhow!("--payload: {error}"))?;
    // One workflow per caller and signal: both builds may send one id.
    let key = format!(
        "signal:{}:{}:{}",
        args.process,
        args.signal_id,
        BuildLabel::current()
    );
    let reply = call_harness(
        &args.restate,
        &key,
        &HarnessOp::Signal {
            process_id: args.process,
            signal_id: args.signal_id,
            payload,
        },
    )
    .await?;
    Ok(HarnessOpReport {
        caller: BuildLabel::current(),
        reply,
    })
}

pub(super) async fn status(args: ProcessStatusArgs) -> Result<ProcessStatusReport> {
    let stores = super::open_stores(&args.store).await?;
    use lash_core::ProcessEventLogTestSupport as _;
    let registry = stores.process_registry();
    let process_id = lash_core::ProcessId::parse(&args.process)
        .map_err(|error| anyhow!("--process: {error}"))?;
    let Some(record) = registry
        .get_process(&process_id)
        .await
        .map_err(|error| anyhow!("read {process_id}: {error}"))?
    else {
        bail!("no process {process_id}");
    };
    let signal_type = lash_core::facade_support::process_signal_event_type(SIGNAL)
        .map_err(|error| anyhow!("signal event type: {error}"))?;
    let signals = registry
        .full_event_window(&process_id, 0)
        .await
        .map_err(|error| anyhow!("read {process_id}'s events: {error}"))?
        .into_iter()
        .filter(|event| event.event_type == signal_type)
        .map(|event| event.payload)
        .collect();
    let waiting_for = record.wait.as_ref().map(|wait| match &wait.kind {
        lash_core::WaitKind::Signal { name, .. } => name.clone(),
    });
    let (output, error) = if record.is_terminal() {
        let engine = super::engine(Arc::clone(&stores), &args.restate)?;
        let core = super::core(lash::Backend::new(engine), &super::ProviderArgs::default())?;
        match core
            .processes()
            .await_output(&process_id)
            .await
            .map_err(|error| anyhow!("await {process_id}'s output: {error}"))?
        {
            lash_core::ProcessAwaitOutput::Settled { output } => match output.outcome {
                lash_core::ToolCallOutcome::Success(value) => (Some(value.to_json_value()), None),
                other => (None, Some(format!("{other:?}"))),
            },
            other => (None, Some(format!("{other:?}"))),
        }
    } else {
        (None, None)
    };
    Ok(ProcessStatusReport {
        process_id: args.process,
        lifecycle: format!("{:?}", record.status),
        waiting_for,
        signals,
        output,
        error,
    })
}
