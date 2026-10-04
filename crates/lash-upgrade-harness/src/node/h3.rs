//! H3's operation fixture executes native A/X/D/V on the engine-owned Run.
//! Controls only submit, follow and cancel; no host drives the task's turn.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::Args;
use lash_core::plugin::{BehaviorRevision, PluginRevision, StaticPluginFactory};
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, DeclaredStartObligation, RunCoordinator, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonPresentationError,
    SingletonToolCall, SingletonToolHandlers,
};
use lash_core::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, ExternalCancelPolicy,
    PresentationBinding, SegmentOrdinal, ToolDeclaration,
};
use serde::{Deserialize, Serialize};

use super::{RestateArgs, StoreArgs};

const PLUGIN: &str = "e2e-h3";
const TASK: &str = "e2e.h3.operation";

pub mod retirement;
pub mod rlm;
mod tasks;

/// Node controls deliberately preserve the public Run vocabulary.
#[derive(Debug, Args)]
pub struct H3Args {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long)]
    pub session: String,
    /// JSON-encoded H3 command, never executable code.
    #[arg(long)]
    pub command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum H3Command {
    Operation {
        key: String,
        output: String,
    },
    Deferred {
        key: String,
    },
    Resolve {
        source: lash_core::AwaitEventKey,
        value: serde_json::Value,
    },
    Follow {
        run: lash_core::TurnId,
    },
    Cancel {
        run: lash_core::TurnId,
    },
    Snapshot {
        run: lash_core::TurnId,
    },
}

/// Register the fixture on both the submitter and the serving node. Its
/// revision is identical across the candidate/synthetic successor pair.
pub(super) fn plugin(namespace: &str) -> Arc<StaticPluginFactory> {
    let namespace = namespace.to_owned();
    let spec = lash_core::facade_support::PluginSpec::new()
        .with_plugin_task_typed::<Operation, _, _>(move |ctx, output| {
            let namespace = namespace.clone();
            async move {
                let lash_core::ExecutionScope::SessionOperation {
                    session_id,
                    operation_id,
                } = ctx.scoped_effect_controller.execution_scope()
                else {
                    return Err("H3 task has no admitted operation owner".to_owned());
                };
                let revision = PluginRevision::new(PLUGIN, BehaviorRevision::ONE);
                let callback = PluginCallbackIdentity {
                    owner: revision.clone(),
                    key: "tool:echo".into(),
                };
                let call = SingletonToolCall {
                    owner: lash_core::EffectOpener::session_operation(
                        session_id.clone(),
                        operation_id.clone(),
                    ),
                    segment: SegmentOrdinal(0),
                    call_id: lash_core::ToolCallId::derive(
                        &namespace,
                        lash_core::ToolCallRoot::host_submission(operation_id)
                            .map_err(|error| error.to_string())?,
                        &[],
                    ),
                    tool_name: "h3.echo".into(),
                    arguments: serde_json::json!(output),
                    declaration: ToolDeclaration::default(),
                    binding: AdmittedBinding {
                        executable: callback.clone(),
                        preparation: callback,
                        presentation: PresentationBinding {
                            presenter: None,
                            steps: Vec::new(),
                        },
                    },
                    available: vec![revision.clone()],
                    cancel: ExternalCancelPolicy::Ignore,
                    environment: None,
                };
                let token = ctx.cancellation_token.clone();
                let handlers = Echo {
                    output: output.clone(),
                    cancelled: Arc::new(move || token.is_cancelled()),
                };
                let mut run = RunCoordinator::open(
                    &ctx.scoped_effect_controller,
                    call.owner.clone(),
                    call.segment,
                    vec![revision],
                );
                run.decide(&call, &handlers)
                    .await
                    .map_err(|error| error.to_string())?;
                run.close().await.map_err(|error| error.to_string())?;
                Ok(lash_core::plugin::PluginOperationOutcome::new(output))
            }
        });
    let spec = tasks::register(spec);
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(PLUGIN),
        spec,
    ))
}

struct Operation;
impl lash_core::facade_support::PluginOperation for Operation {
    const NAME: &'static str = TASK;
    const DESCRIPTION: &'static str = "H3 engine-owned operation with one native tool";
    const SESSION_PARAM: lash_core::facade_support::SessionParam =
        lash_core::facade_support::SessionParam::Required;
    type Args = String;
    type Output = String;
    type Error = String;
    const ERROR_TYPE: &'static str = TASK;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &String) -> lash_core::plugin::PluginFailureClass {
        lash_core::plugin::PluginFailureClass::Terminal
    }
}
impl lash_core::facade_support::PluginTask for Operation {}

struct Echo {
    output: String,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

#[lash_core::async_trait]
impl SingletonToolHandlers for Echo {
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(call.arguments.clone())
    }
    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }
    async fn execute(&self, _: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        Ok(SingletonBodyOutcome::Done {
            output: self.output.clone(),
            commands: Default::default(),
            intents: Vec::new(),
            start: None,
        })
    }
    async fn after_checks(
        &self,
        _: &lash_core::ToolCallId,
        _: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }
    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok((self.cancelled)())
    }
    async fn wait_run_retry(
        &self,
        _call_id: &lash_core::ToolCallId,
        timer: lash_core::tool_dispatch::RunRetryTimer<'_>,
    ) -> Result<lash_core::tool_dispatch::RunRetryWake, lash_core::RuntimeEffectControllerError>
    {
        timer.await?;
        Ok(lash_core::tool_dispatch::RunRetryWake::Elapsed)
    }
    async fn realize_declarations(
        &self,
        _: &lash_core::ToolCallId,
        intents: &[lash_core::ToolIntentKind],
    ) -> Result<(), String> {
        if intents.is_empty() {
            Ok(())
        } else {
            Err("H3 echo admits no declarations".into())
        }
    }
    async fn present(
        &self,
        _: &lash_core::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        Ok(capture.output().unwrap_or_default().to_owned())
    }
    fn emit_stream(&self, _: &lash_core::ToolCallId, _: &lash_core::runtime::AttemptStream) {}
    async fn launch_start(
        &self,
        _: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessId, String> {
        Err("H3 echo admits no process start".into())
    }
    async fn discharge_start(
        &self,
        _: &DeclaredStartObligation,
        _: &lash_core::ProcessId,
        _: bool,
    ) -> Result<(), String> {
        Err("H3 echo has no consumer hold".into())
    }
}

pub(super) async fn run(args: H3Args) -> Result<serde_json::Value> {
    let command: H3Command = serde_json::from_str(&args.command)?;
    let stores = super::open_stores(&args.store).await?;
    let snapshots = stores.session_store_factory();
    let engine = super::engine(stores, &args.restate)?;
    let core = super::core(
        lash::Backend::new(engine.clone()),
        &super::ProviderArgs::default(),
    )?;
    let session_id = lash::SessionId::fixture(args.session.clone());
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(super::session_spec()))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => return Err(anyhow!(error)),
    }
    let session = core.session(session_id.clone()).open().await?;
    match command {
        H3Command::Operation { key, output } => {
            let handle = session
                .plugin_operations()
                .start_task::<Operation>(output, key)
                .await?;
            let run = handle.run().clone();
            drop(handle);
            Ok(serde_json::json!({"run": run, "admitted": true}))
        }
        H3Command::Deferred { key } => {
            let handle = session
                .plugin_operations()
                .start_task_raw(tasks::DEFERRED, serde_json::json!(key), key)
                .await?;
            let run = handle.run().clone();
            drop(handle);
            Ok(serde_json::json!({"run": run, "admitted": true}))
        }
        H3Command::Resolve { source, value } => {
            use lash_core::AwaitEventResolver as _;
            let outcome = engine
                .restate_effect_host()
                .resolve_await_event(&source, lash_core::Resolution::Ok(value))
                .await?;
            Ok(serde_json::to_value(outcome)?)
        }
        H3Command::Follow { run } => {
            let result = session.run(run.clone()).result().await?;
            Ok(serde_json::json!({"run": run, "output": result.output}))
        }
        H3Command::Cancel { run } => {
            session.run(run.clone()).cancel().await?;
            Ok(serde_json::json!({"run": run, "cancel_requested": true}))
        }
        H3Command::Snapshot { run } => {
            let unfinished = session.durable().unfinished_run().await?;
            if unfinished
                .as_ref()
                .is_some_and(|unfinished| unfinished.run != run)
            {
                bail!("snapshot belongs to a different unfinished Run");
            }
            Ok(serde_json::json!({
                "run": run,
                "session": session_id,
                "store_source": "deployment_store.session_head.pending_follow_on_json",
                "unfinished": unfinished.is_some(),
                "terminal": snapshots.run_terminal(&session_id, &run).await?,
                "continuation": snapshots.load_pending_follow_on(&session_id).await?,
                "park": snapshots.load_turn_park(&session_id).await?,
            }))
        }
    }
}

/// Cheapest execution tier for the same operation fixture served by the node.
pub async fn double_fixture(
    seed: u64,
) -> Result<(
    lash::LashCore,
    lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
)> {
    double_fixture_replay(seed, false).await
}

/// Suspends at each uncompleted durable await, preserving real journal replay.
pub async fn double_fixture_replay(
    seed: u64,
    always_replay: bool,
) -> Result<(
    lash::LashCore,
    lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
)> {
    let stores: Arc<dyn lash::StoreSet> = Arc::new(lash::sqlite::SqliteStoreSet::memory().await?);
    let args = RestateArgs {
        ingress_url: "http://127.0.0.1:9".into(),
        admin_url: "http://127.0.0.1:9".into(),
        authority: format!("lash-restate-test-{seed}"),
        namespace: String::new(),
    };
    let engine = super::engine(stores.clone(), &args)?;
    let generation = super::core(lash::Backend::new(engine), &super::ProviderArgs::default())?
        .build_generation()
        .clone();
    let double = lash_restate_test::backend_with_store_set(
        seed,
        lash_restate_test::ServerConfig {
            build_generation: generation,
            protocol: lash_restate_test::ProtocolVersion::V7,
            always_replay,
            ..Default::default()
        },
        lash_restate_test::DeploymentHooks::default(),
        |_| async move { Ok(stores) },
    )
    .await?;
    let core = super::core(double.lash_backend(), &super::ProviderArgs::default())?;
    Ok((core, double))
}

/// Resolve a public logical Run through the authoritative executor admission;
/// the physical Restate key is an ingress request, not the public Run string.
pub async fn operation_invocation(
    double: &lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
    session: &lash::SessionId,
    run: &lash::TurnId,
) -> Result<lash_restate_test::InvocationView> {
    let stores = double.stores().session_store_factory();
    let key = lash_restate::recorded_turn_invocation_key(stores.as_ref(), session, run)
        .await?
        .ok_or_else(|| anyhow!("Run {run} has no retained executor admission"))?;
    let suffix = format!("/{key}/run");
    let service = double.service_name("LashTurn");
    let matches: Vec<_> = double
        .server()
        .invocations()
        .into_iter()
        .filter(|invocation| {
            let route = invocation.target.split('/').next().unwrap_or_default();
            (route == service || route.starts_with(&format!("{service}_g")))
                && invocation.target.ends_with(&suffix)
        })
        .collect();
    if matches.len() != 1 {
        bail!(
            "Run {run} executor {key} has {} physical journals",
            matches.len()
        );
    }
    matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("missing operation journal"))
}

/// The real durable-wait registry boundary used by S21. Material is retained
/// through the backing SQLite store before its reference can become a seal.
pub struct SourceFixture {
    pub descriptor: lash_core::tool_run::SourceDescriptor,
    client: lash_restate::RestateIngressClient,
    service: String,
    index: String,
    materials: Arc<dyn lash_core::store::ToolMaterialStore>,
}

impl SourceFixture {
    pub async fn new(
        double: &lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
        session: &str,
        operation: &str,
    ) -> Result<Self> {
        use lash_core::AwaitEventResolver as _;
        let call_id = lash_core::ToolCallId::derive(
            "",
            lash_core::ToolCallRoot::host_submission(operation)?,
            &[],
        );
        let source = double
            .restate()
            .restate_effect_host()
            .await_event_key(
                &lash_core::ExecutionScope::SessionOperation {
                    session_id: lash_core::SessionId::fixture(session.to_owned()),
                    operation_id: operation.into(),
                },
                lash_core::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
            )
            .await?;
        let index = lash_restate::RestateDurableWaitAddress::for_key(&source).index_key();
        Ok(Self {
            descriptor: lash_core::tool_run::SourceDescriptor {
                source,
                call_id,
                owner: lash_core::EffectOpener::session_operation(
                    lash_core::SessionId::fixture(session.to_owned()),
                    operation,
                ),
                resolver: PluginRevision::new(PLUGIN, BehaviorRevision::ONE),
                authority: lash_core::tool_run::SourceAuthority::ExternalCompletion,
                cancel: ExternalCancelPolicy::CancelExternalWork,
            },
            client: double.ingress(),
            service: double.service_name("LashDurableWaitIndex"),
            index,
            materials: double.stores().tool_material_store(),
        })
    }

    pub async fn arm(&self) -> Result<SourceArmReply> {
        self.call(
            "arm_source",
            serde_json::json!({"descriptor": self.descriptor}),
        )
        .await
    }

    pub async fn retained(&self, value: &str) -> Result<lash_core::tool_run::SourceSeal> {
        use lash_core::tool_run::{
            MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole,
            SourceSeal,
        };
        let owner = MaterialOwner::Source {
            source: self.descriptor.source.clone(),
        };
        let bundle = MaterialBundle::of([MaterialPayload::new(
            owner,
            MaterialRole::AttemptOutput,
            Some(self.descriptor.resolver.clone()),
            value.to_owned(),
        )])?
        .ok_or_else(|| anyhow!("source result bundle is empty"))?;
        let retained = self
            .materials
            .retain_material(
                &MaterialHolder::Source {
                    source: self.descriptor.source.clone(),
                },
                &bundle,
            )
            .await?;
        let result = retained
            .references
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("retained source result has no reference"))?;
        Ok(SourceSeal::Resolved {
            result: Box::new(result),
        })
    }

    pub async fn seal(
        &self,
        writer: lash_core::tool_run::SealWriter,
        seal: lash_core::tool_run::SourceSeal,
    ) -> Result<SourceSealReply> {
        self.call(
            "seal_source",
            serde_json::json!({"source": self.descriptor.source, "writer": writer, "seal": seal}),
        )
        .await
    }

    /// S21 subscribes only after a seal exists, so no invented awakeable is
    /// resolved. The sealed reply must return before registering any observer.
    pub async fn subscribe_sealed(
        &self,
        owner: lash_core::EffectOpener,
        segment: SegmentOrdinal,
    ) -> Result<SourceSubscribeReply> {
        self.call(
            "subscribe_source",
            serde_json::json!({"subscription": lash_core::tool_run::SourceSubscription {
                source: self.descriptor.source.clone(), owner, segment,
            }, "awakeable_id": ""}),
        )
        .await
    }

    async fn call<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        handler: &str,
        body: T,
    ) -> Result<R> {
        let reply: lash_restate::Reply<R> = self
            .client
            .call_object_json(
                &self.service,
                &self.index,
                handler,
                &lash_restate::Call::new(body),
            )
            .await?;
        Ok(reply.body)
    }
}

/// Typed replies decoded from the real registry wire boundary. These fixture
/// DTOs are needed because the handler reply types are internal to the adapter.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceArmReply {
    Armed {
        seal: Option<lash_core::tool_run::SourceSeal>,
    },
    Refused {
        refusal: lash_core::tool_run::SourceRefusal,
    },
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSealReply {
    Outcome {
        outcome: lash_core::tool_run::SealOutcome,
    },
    Refused {
        refusal: lash_core::tool_run::SourceRefusal,
    },
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSubscribeReply {
    Subscribed,
    Sealed {
        seal: lash_core::tool_run::SourceSeal,
    },
    Refused {
        refusal: lash_core::tool_run::SourceRefusal,
    },
}
