//! A declared start's launch record carries its call's `StartProcess`
//! receipt, so the run possesses the child from that record: exactly once,
//! and again on a replay after the owner died between the launch and the
//! call's presentation (FIG-5136).

use lash_sansio::sync::MutexExt as _;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PLUGIN: &str = "declared-start-possession";
const SESSION: &str = "declared-start-possession-session";

fn definition() -> crate::ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    crate::ToolDefinition::raw(
        "declared-start-possession:spawn",
        "spawn",
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({"type":"string"}),
    )
    .unwrap()
    .with_tool_binding(lash_sansio::ToolBinding::new([PLUGIN], "spawn"))
    .with_declaration(
        crate::ToolDeclaration::deferring().with_intents([crate::ToolIntentKind::StartProcess]),
    )
}

/// Parks on the one child it declares, as `spawn_agent` does.
struct Spawning;

#[async_trait::async_trait]
impl crate::ToolProvider for Spawning {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let owner = call.context.owner().runtime_owner();
        let start = crate::DeclaredStart::new(
            call.context,
            crate::StartProcessIntent {
                owner,
                declaration: crate::ProcessStartDeclaration::new(
                    crate::testing::held_engine_input(serde_json::Value::Null),
                    crate::ProcessOriginator::host(),
                    crate::Lifetime::Detached,
                ),
            },
        )
        .expect("the call declares its child");
        crate::ToolAttemptOutcome::pending(
            crate::PendingCompletion::new().resolved_by_declared_start(start),
        )
    }
}

/// Registers a bound start's child and publishes its terminal at once.
struct Launches {
    work: crate::ProcessWorkWiring,
    launched: Arc<Mutex<Vec<crate::ProcessId>>>,
}

fn unserved<T>() -> Result<T, crate::PluginError> {
    Err(crate::PluginError::Session(
        "the law's process service serves bound starts only".to_owned(),
    ))
}

#[async_trait::async_trait]
impl crate::ProcessService for Launches {
    async fn start_bound(
        &self,
        registration: crate::ProcessStartRegistration,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        let registration = registration
            .stating_input()
            .map_err(|_| crate::PluginError::Session("the start names a definition".to_owned()))?;
        let record = self.work.registry().register_process(registration).await?;
        let output = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
            json!("child done"),
        ));
        self.work
            .registry()
            .complete_process(
                &record.id,
                output.clone(),
                crate::ProcessCompletionAuthority::workflow_key(&record.id),
            )
            .await?;
        self.work
            .port()
            .publish_process_terminal(&record.id, &output, &format!("terminal:{}", record.id))
            .await?;
        self.launched.lock_recover().push(record.id.clone());
        Ok(record)
    }

    async fn release_consumer_hold(
        &self,
        process_id: &crate::ProcessId,
        key: &str,
    ) -> Result<(), crate::PluginError> {
        self.work
            .registry()
            .release_consumer_hold(process_id, key)
            .await
    }

    async fn start_from_recorded_intent(
        &self,
        _: &crate::RuntimeOwner,
        _: crate::ProcessStartRequest,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        unserved()
    }

    async fn start(
        &self,
        _: &crate::SessionId,
        _: crate::ProcessStartRegistration,
        _: crate::ProcessStartOptions,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        unserved()
    }

    async fn await_process(
        &self,
        _: &crate::ProcessId,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        unserved()
    }

    async fn list_visible(
        &self,
        _: &crate::SessionId,
        _: crate::ProcessListMode,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, crate::PluginError> {
        unserved()
    }

    async fn validate_visible(
        &self,
        _: &crate::RuntimeOwner,
        _: &[crate::ProcessId],
        _: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        unserved()
    }

    async fn cancel(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::ProcessId,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        unserved()
    }

    async fn cancel_recorded_intent(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::ProcessId,
        _: crate::ToolIntentIdentity,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessRecord, crate::PluginError> {
        unserved()
    }

    async fn signal_recorded_intent(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::ProcessId,
        _: String,
        _: String,
        _: serde_json::Value,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        unserved()
    }

    async fn emit_event_recorded_intent(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::ProcessId,
        _: String,
        _: String,
        _: serde_json::Value,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        unserved()
    }

    async fn signal_possessed(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::ProcessId,
        _: String,
        _: String,
        _: serde_json::Value,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessEvent, crate::PluginError> {
        unserved()
    }

    async fn transfer(
        &self,
        _: &crate::SessionId,
        _: &crate::SessionId,
        _: Vec<crate::ProcessId>,
        _: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        unserved()
    }
}

/// The owner dies before the call's presentation is stored: after the
/// launch record and before the parent's commit. The redriven owner is
/// served the launch, so it registers nothing, and possesses the child from
/// the receipt that record carries.
#[tokio::test]
async fn a_replayed_declared_start_launch_grants_the_run_possession_of_its_child_once() {
    use crate::session::{
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateLeafReply, ToolAggregateOutcome,
        ToolAggregateRequest,
    };
    use lash_restate_test::{CrashCount, CrashPoint, CrashRule};

    let double =
        crate::support::kernel_double(0x5136, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    double
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeRunResultEnding {
            suffix: ":present".to_owned(),
        }));
    let crashes = CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    let launched = Arc::new(Mutex::new(Vec::new()));
    let possessed = Arc::new(Mutex::new(Vec::<Vec<crate::ProcessId>>::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let launched = launched.clone();
        let possessed = possessed.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let launched = launched.clone();
            let possessed = possessed.clone();
            Box::pin(async move {
                let mut factories = crate::testing::test_standard_protocol_factories();
                factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
                    crate::plugin::PluginDeclaration::initial(PLUGIN),
                    crate::PluginSpec::new().with_tool_provider(Arc::new(Spawning)),
                )));
                let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
                    .session_id(SESSION)
                    .borrowed_effect_controller(scoped)
                    .plugin_factories(factories)
                    .processes(Arc::new(Launches {
                        work: backend.process_work(),
                        launched,
                    }))
                    .build()
                    .into_runtime()
                    .with_tool_material_store(backend.tool_material_store());
                let grant = crate::ToolExecutionGrant::from_definition(
                    crate::plugin::PluginRevision::new(
                        PLUGIN,
                        crate::plugin::BehaviorRevision::ONE,
                    ),
                    definition(),
                );
                let leaves = vec![ToolAggregateLeaf::Tool(
                    crate::session::ToolInvocation::new(
                        crate::ToolCallId::fixture("spawn"),
                        crate::ToolId::new("declared-start-possession:spawn"),
                        json!({}),
                    )
                    .with_execution_grant(grant),
                )];
                let outcome = context
                    .drive_tool_run(None, |context| async move {
                        let outcome = context
                            .call_tool_aggregate(ToolAggregateRequest {
                                leaves,
                                consumer: ToolAggregateConsumer::All,
                                settled_value_after: None,
                                command: crate::CommandReplayKey::new("declared-start-possession"),
                            })
                            .await;
                        match &outcome {
                            ToolAggregateOutcome::HostControl(m) => {
                                eprintln!("DIAG host control {m}")
                            }
                            ToolAggregateOutcome::AllResults(r) => eprintln!(
                                "DIAG all {:?}",
                                r.iter()
                                    .map(|r| match r {
                                        Some(ToolAggregateLeafReply::Tool(t)) =>
                                            format!("{:?}", t.output),
                                        _ => "none".into(),
                                    })
                                    .collect::<Vec<_>>()
                            ),
                            _ => eprintln!("DIAG other"),
                        }
                        context.close_tool_run().await.unwrap();
                        outcome
                    })
                    .await
                    .unwrap();
                assert!(!context.has_nested_effect_error());
                let ToolAggregateOutcome::AllResults(replies) = outcome else {
                    panic!("the spawn settles with its child's value");
                };
                let [Some(ToolAggregateLeafReply::Tool(reply))] = replies.as_slice() else {
                    panic!("one tool reply");
                };
                assert!(
                    reply.output.is_success(),
                    "the call answers its child's value: {:?}",
                    reply.output
                );
                possessed.lock_recover().push(context.started_process_ids());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        double.run_in_handler(crate::AdmittedScope::turn(SESSION, "test-turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(crashes.get(), 1, "the owner dies before its presentation");
    let launched = launched.lock_recover().clone();
    assert_eq!(
        launched.len(),
        1,
        "the replay serves the durable launch instead of registering again"
    );
    assert_eq!(
        *possessed.lock_recover(),
        vec![launched],
        "the redriven run possesses the child its recorded launch receipt names, once"
    );
}
