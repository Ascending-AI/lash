//! A sessionless workflow reads through its recorded component routes.

use super::*;

/// The component that contributes `tools.lookup`.
const KNOWLEDGE: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "core-node-knowledge",
    lash_vocabulary::providers::FormatVersion::ONE,
);

/// The lookup tool's id: what its step's admission names. Its effect is
/// `tools.lookup`.
const LOOKUP_TOOL: &str = "core-node-knowledge-lookup";

/// `lookup()`: one `perform` of the knowledge component's `Read` tool.
const LOOKUP: &str = r#"kernel 1
numbers float
import 0 operation tools.lookup(input: Any) -> Any
import 1 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let input = {topic: "lash"}
  let got = perform 0(input) as Any
  return got
}
"#;

/// The `Read` tool `lookup`: what the knowledge base says about a topic.
fn lookup_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        LOOKUP_TOOL,
        "tools.lookup",
        "Looks a topic up.",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the lookup tool's schemas")
    // The contract the laws' kernel texts pin.
    .declared_contract(lash_vocabulary::operations::ContractId::new("1"))
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(lash_vocabulary::tool_contract::ExecutionPolicy::Read)
}

/// What the knowledge base says about the topic `args` names.
fn looked_up(args: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "topic": args["topic"], "says": "durable" })
}

/// How often the lookup's body and its reader answered.
#[derive(Default)]
struct Answered {
    bodies: AtomicUsize,
    reads: AtomicUsize,
    gates: AtomicUsize,
}

struct LookupBody(Arc<Answered>);

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for LookupBody {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.bodies.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(looked_up(call.args)).into()
    }
}

struct LookupReads(Arc<Answered>);

#[async_trait::async_trait]
impl lash::tools::ToolReads for LookupReads {
    async fn read(
        &self,
        call: lash::tools::ReadCall<'_>,
    ) -> Result<serde_json::Value, lash::tools::ReadError> {
        if call.args["topic"] == "gate" {
            if self.0.gates.fetch_add(1, Ordering::SeqCst) == 0 {
                call.cancel.cancelled().await;
                return Err(lash::tools::ReadError::new("the first unsaved run stopped"));
            }
            return Ok(serde_json::json!({ "open": true }));
        }
        let draw = self.0.reads.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(serde_json::json!({ "topic": call.args["topic"], "draw": draw }))
    }
}

/// The knowledge component as a host declares it: a session plugin's spec
/// whose provider registers its reader, offered as a process component.
fn knowledge_component(
    answered: &Arc<Answered>,
) -> Arc<dyn lash::process::ProcessComponentFactory> {
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        vec![lookup_definition()],
        LookupBody(Arc::clone(answered)),
    ));
    Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(KNOWLEDGE.component()),
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider_registration(
                    lash::tool_provider_key!("tools"),
                    lash::tools::ToolProviderRegistration::new(provider)
                        .with_reads(Arc::new(LookupReads(Arc::clone(answered)))),
                )
                .with_package_provider(
                    lash::tool_provider_key!("lookup-package"),
                    Arc::new(operations::Packages {
                        provider: KNOWLEDGE.tool_provider(lash::tool_provider_key!("tools")),
                        definitions: vec![lookup_definition()],
                    }),
                ),
        )
        .as_component(KNOWLEDGE.format())
        .expect("a provider with its reader is a process component"),
    )
}

/// PROCESS-READ-COLD: reads use the recorded route without a session, and
/// a cold continuation draws a fresh answer rather than consuming its tape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sessionless_workflow_reads_fresh_after_a_cold_resume_on_sqlite_memory() {
    let (stores, _keep) = stores(Tier::SqliteMemory).await;
    let answered = Arc::new(Answered::default());
    let build = |backend: &lash::Backend| {
        let workers = lash::vm::WorkerService::default();
        let bounds = workers.config().run_bounds;
        lash::LashCore::standard_builder(backend.clone())
            .plugin(Arc::new(lash_vm_runtime::KernelProcessPluginFactory::new(
                workers,
                "typescript",
                bounds,
            )))
            .process_component(knowledge_component(&answered), KNOWLEDGE)
    };
    let (backend, first) =
        super::session_and_isolated_envs::booted(&stores, Vec::new(), "read-first", build);
    let source = LOOKUP.replace(
        "  return got",
        "  let gate = perform 0({topic: \"gate\"}) as Any\n  do perform 1(5000) as Null then yield\n  let again = perform 0(input) as Any\n  return [got, again]",
    );
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(KNOWLEDGE, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let (payload, env) = kernel_process::publish(&first, &source, &env).await;
    let process = start_under(
        &first,
        &env,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        payload,
        |mut request| {
            if let lash_execution_contracts::process::ProcessStartTarget::Input(
                lash_execution_contracts::process::ProcessInput::Engine { launch, .. },
            ) = &mut request.input
            {
                let settings = lash_vm_runtime::KernelRecordedSettings::from_engine_settings(
                    launch.settings.as_ref().unwrap(),
                )
                .unwrap();
                launch.settings = Some(
                    settings
                        .observing(lash_vocabulary::observation::RunObservation::Counts)
                        .into_engine_settings(),
                );
            }
            request
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let steps = lash_core_execution::runtime::process::steps::read_process_steps(
                &first.effect_host(),
                &process,
            )
            .await
            .unwrap();
            for step in &steps {
                if let Some(text) = step.outcome.as_ref().and_then(|outcome| outcome.payload()) {
                    assert!(
                        !text.contains("binds no read"),
                        "the process read host faulted: {text}"
                    );
                }
            }
            if answered.gates.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the read answered before the unsaved run blocked");
    assert_eq!(answered.reads.load(Ordering::SeqCst), 1);
    first
        .shutdown()
        .await
        .expect("the first node stops before saving the read");
    drop(first);
    drop(backend);
    let (backend, first) =
        super::session_and_isolated_envs::booted(&stores, Vec::new(), "read-redrawn", build);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let record = backend
                .process_registry()
                .get_process(&process)
                .await
                .unwrap()
                .unwrap();
            assert!(
                record.terminal().is_none(),
                "the process ended before parking: {:?}",
                record.terminal()
            );
            let steps = lash_core_execution::runtime::process::steps::read_process_steps(
                &first.effect_host(),
                &process,
            )
            .await
            .expect("the process steps read");
            for step in &steps {
                if let Some(text) = step.outcome.as_ref().and_then(|outcome| outcome.payload()) {
                    assert!(
                        !text.contains("binds no read"),
                        "the process read host faulted: {text}"
                    );
                }
            }
            if answered.reads.load(Ordering::SeqCst) == 2 && !record.waits().is_empty() {
                let wait = &record.waits()[0];
                let lash::process::WaitKind::Call { resume_at_ms: Some(deadline), .. } = wait.kind else {
                    panic!("TIMER-OBSERVE: the ordinary pending call retains its timer deadline: {wait:?}");
                };
                let actor = lash_durable::ActorKey::process(process.as_str()).unwrap();
                let timers = backend.durable().pending_waits(&actor).await.unwrap();
                assert!(timers.iter().any(|timer| matches!(timer.purpose,
                    lash_durable::domain::WaitPurpose::Timer { deadline: at } if at.0 == deadline)),
                    "the observed deadline is the exact committed timer's: {timers:?}");
                let snapshot = first.processes().observe(&process).snapshot().await.unwrap();
                let lash::process::ProcessReadView::Retained(read) = snapshot.read_view else { panic!("the parked process is retained") };
                assert!(read.workflow.unwrap().sites.iter().any(|row| row.state == lash::workflow::ProcessWorkflowSiteState::Waiting {
                    kind: lash::workflow::ProcessWorkflowWaitKind::Effect, resume_at_ms: Some(deadline),
                }), "the workflow overlay carries the same committed deadline");
                break;
            }
            if answered.bodies.load(Ordering::SeqCst) != 0 {
                panic!("a read-mode perform ran a durable body instead of its reader");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first read parks");
    assert_eq!(
        answered.reads.load(Ordering::SeqCst),
        2,
        "the unsaved read was redrawn"
    );
    first
        .shutdown()
        .await
        .expect("the redrawn run stops at its park");
    drop(first);
    drop(backend);
    let (_, fresh) =
        super::session_and_isolated_envs::booted(&stores, Vec::new(), "read-fresh", build);
    let output = ended(&fresh, &process).await;
    assert_eq!(
        success(&output),
        serde_json::json!([
            { "topic": "lash", "draw": 2 }, { "topic": "lash", "draw": 3 }
        ])
    );
    assert_eq!(answered.reads.load(Ordering::SeqCst), 3);
    assert_eq!(answered.bodies.load(Ordering::SeqCst), 0);
    let replayed = fresh
        .processes()
        .replay(&process, lash::workflow::ReplaySegments::All)
        .await
        .unwrap();
    assert_eq!(
        replayed.len(),
        2,
        "both sides of the park replay: {replayed:?}"
    );
    assert!(
        replayed.iter().all(|segment| matches!(
            segment.answer,
            lash::workflow::SegmentAnswer::Verified { .. }
        )),
        "{replayed:?}"
    );
    assert_eq!(
        answered.reads.load(Ordering::SeqCst),
        3,
        "diagnostic replay never calls a reader"
    );
    fresh.shutdown().await.expect("the fresh node stops");
}
