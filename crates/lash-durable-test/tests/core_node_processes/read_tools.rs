//! A sessionless workflow reads through its recorded component routes.

use super::*;
use lash::observe::Stream as _;

/// The component that contributes `tools.lookup`.
const KNOWLEDGE: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "core-node-knowledge",
    lash_kernel_value::providers::FormatVersion::ONE,
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
    .declared_contract(lash_kernel_value::operations::ContractId::new("1"))
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(lash_kernel_value::tool_contract::ExecutionPolicy::Read)
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
    knowledge_with_reads(answered, Arc::new(LookupReads(Arc::clone(answered))))
}

fn knowledge_with_reads(
    answered: &Arc<Answered>,
    reads: Arc<dyn lash::tools::ToolReads>,
) -> Arc<dyn lash::process::ProcessComponentFactory> {
    knowledge_with_contract(answered, reads, lookup_definition(), false)
}

fn knowledge_with_contract(
    answered: &Arc<Answered>,
    reads: Arc<dyn lash::tools::ToolReads>,
    definition: lash_vocabulary::tool_contract::ToolDefinition,
    deny: bool,
) -> Arc<dyn lash::process::ProcessComponentFactory> {
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        vec![definition.clone()],
        LookupBody(Arc::clone(answered)),
    ));
    Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(KNOWLEDGE.component()),
            {
                let spec = lash_core::facade_support::PluginSpec::new();
                let spec = if deny {
                    spec.with_tool_args_check(
                        lash::hook_key!("deny-read"),
                        Arc::new(|_| {
                            Box::pin(async {
                                Ok(lash_core::plugin::BeforeToolDecision::Deny(
                                    lash_core::ToolFailure::tool(
                                        lash_core::ToolFailureClass::PermissionDenied,
                                        "read_denied",
                                        "read denied",
                                    ),
                                ))
                            })
                        }),
                    )
                } else {
                    spec
                };
                spec.with_tool_provider_registration(
                    lash::tool_provider_key!("tools"),
                    lash::tools::ToolProviderRegistration::new(provider).with_reads(reads),
                )
                .with_package_provider(
                    lash::tool_provider_key!("lookup-package"),
                    Arc::new(operations::Packages {
                        provider: KNOWLEDGE.tool_provider(lash::tool_provider_key!("tools")),
                        definitions: vec![definition],
                    }),
                )
            },
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
                        .observing(lash_kernel_value::observation::RunObservation::Counts)
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

#[path = "../support/sim.rs"]
mod sim;

struct HeldRead {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash::tools::ToolReads for HeldRead {
    async fn read(
        &self,
        _: lash::tools::ReadCall<'_>,
    ) -> Result<serde_json::Value, lash::tools::ReadError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(serde_json::json!({ "says": "durable" }))
    }
}

/// D-LEASE / FIG-7234: the production process activation renews its root
/// lease throughout an inline host read, then publishes its next park.
/// Both the store's expiry and the activation's schedule use virtual time.
#[tokio::test(flavor = "current_thread")]
async fn a_resumed_process_renews_its_page_lease_during_a_long_host_read_on_sqlite_memory() {
    Box::pin(tokio::time::timeout(Duration::from_secs(60), async {
        let clock = lash_durable_test::SimClock::new();
        let stores = Arc::new(sim::memory(Arc::clone(&clock)).await);
        let ports: Arc<dyn StoreSet> = stores.clone();
        let answered = Arc::new(Answered::default());
        let read = Arc::new(HeldRead {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let (backend, core) =
            super::session_and_isolated_envs::booted(&ports, Vec::new(), "page-lease", |backend| {
                // Keep the 75-second read within its declared tool and parent
                // allowances while it crosses the 60-second page lease.
                let mut config = sim::untimed_workers().config().clone();
                config.deadlines.host_read = lookup_definition().manifest.execution();
                let worker_clock = Arc::clone(&clock);
                let workers = lash::vm::WorkerService::new(config)
                    .with_call_hold(Arc::new(move || Box::new(worker_clock.hold())));
                let bounds = workers.config().run_bounds;
                lash::LashCore::standard_builder(backend.clone())
                    .plugin(Arc::new(lash_vm_runtime::KernelProcessPluginFactory::new(
                        workers,
                        "typescript",
                        bounds,
                    )))
                    .process_component(knowledge_with_reads(&answered, read.clone()), KNOWLEDGE)
            });
        let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
            .component(KNOWLEDGE, &())
            .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
            .build()
            .unwrap();
        // The first sleep commits a checkpoint. The read therefore runs
        // in a resumed activation holding a real fenced root lease.
        let source = LOOKUP.replace(
            "  let input",
            "  do perform 1(1) as Null then yield\n  let input",
        ).replace(
            "  return got",
            "  do perform 1(600000) as Null then yield\n  return got",
        );
        let (payload, env) = kernel_process::publish(&core, &source, &env).await;
        let process = start_under(
            &core, &env, lash_vm_runtime::LASH_VM_ENGINE_KIND, payload, |request| request,
        ).await;
        loop {
            let record = backend.process_registry().get_process(&process).await.unwrap().unwrap();
            assert!(record.terminal().is_none(), "first park: {:?}", record.terminal());
            if !record.waits().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        clock.advance_to(1).await;
        read.entered.notified().await;
        let actor = lash_durable::ActorKey::process(process.as_str()).unwrap();
        let lease_rows = || {
            lash_sqlite_store::testing::root_leases_for_testing(&stores, &actor).unwrap()
        };
        let initial = lease_rows();
        assert_eq!(initial.len(), 1, "the resumed activation holds one root lease");
        let lease_id = initial[0].0;
        let expires = initial[0].1;
        let mut renewals = Vec::new();
        // Keep the read blocked for 75 seconds, crossing the original
        // 60-second expiry without any worker page traffic.
        for at in [25_001, 50_001, 75_001] {
            clock.advance_to(at).await;
            lash_durable_test::settle().await;
            renewals.extend(lease_rows());
        }
        assert!(renewals.iter().any(|row|
            row.0 == lease_id && row.1 > expires
        ), "the same stored lease was actually renewed during the host read: {renewals:?}");
        read.release.notify_one();
        loop {
            let steps = lash_core_execution::runtime::process::steps::read_process_steps(
                &core.effect_host(), &process,
            ).await.unwrap();
            for step in &steps {
                assert!(
                    !matches!(step.outcome, Some(lash_core_store::tool_run::SettledOutput::Failed(_))),
                    "the resumed run must publish its park after the long host read: {:?}",
                    step.outcome,
                );
            }
            let record = backend.process_registry().get_process(&process).await.unwrap().unwrap();
            assert!(record.terminal().is_none(), "second park: {:?}", record.terminal());
            if record.waits().iter().any(|wait| matches!(
                wait.kind,
                lash::process::WaitKind::Call { resume_at_ms: Some(deadline), .. }
                    if deadline == i64::try_from(lash_durable_test::SimClock::timestamp_ms_at(675_001)).unwrap()
            )) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(lease_rows().is_empty(), "the parked activation releases its lease");
        assert_eq!(answered.bodies.load(Ordering::SeqCst), 0);
        core.shutdown().await.unwrap();
    })).await.expect("the process reaches both parks without wall-clock lease waits");
}

/// PROCESS-READ-LIVE: read-mode performs reach the process feed as settled,
/// provisional tool events, with ordinals continuing across a committed park.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sessionless_workflow_streams_reads_across_a_park_on_sqlite_memory() {
    let (stores, _keep) = stores(Tier::SqliteMemory).await;
    let answered = Arc::new(Answered::default());
    let (_, core) =
        super::session_and_isolated_envs::booted(&stores, Vec::new(), "read-live", |backend| {
            let workers = lash::vm::WorkerService::default();
            let bounds = workers.config().run_bounds;
            lash::LashCore::standard_builder(backend.clone())
                .plugin(Arc::new(lash_vm_runtime::KernelProcessPluginFactory::new(
                    workers,
                    "typescript",
                    bounds,
                )))
                .process_component(knowledge_component(&answered), KNOWLEDGE)
        });
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(KNOWLEDGE, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let source = LOOKUP.replace("  return got", "  do perform 1(1) as Null then yield\n  let again = perform 0({topic: \"second\"}) as Any\n  return [got, again]");
    let (payload, env) = kernel_process::publish(&core, &source, &env).await;
    let process = start_under(
        &core,
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
                        .observing(lash_kernel_value::observation::RunObservation::Counts)
                        .into_engine_settings(),
                );
            }
            request
        },
    )
    .await;
    let observed = core.processes().observe(&process);
    let snapshot = observed.snapshot().await.unwrap();
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let activities = tokio::time::timeout(Duration::from_secs(60), async {
        let mut activities = Vec::new();
        while activities.len() < 4 {
            match std::future::poll_fn(|cx| std::pin::Pin::new(&mut feed).poll_next(cx))
                .await
                .unwrap()
                .unwrap()
            {
                lash::process::ProcessObservationStreamItem::Event(event) => {
                    if let lash::process::ProcessObservationEventPayload::Activity {
                        event, ..
                    } = &event.payload
                    {
                        activities.push(serde_json::to_value(event).unwrap());
                    }
                }
                gap => panic!("read feed lost continuity: {gap:?}"),
            }
        }
        activities
    })
    .await
    .expect("both read start/return pairs reach the process feed");
    let facts: Vec<_> = activities
        .iter()
        .map(|event| {
            serde_json::json!({
                "type":event["type"], "execution":event["execution"],
                "caller":event["read"]["caller"], "ordinal":event["read"]["ordinal"],
                "name":event["name"], "provider":event["read"]["provider"],
                "args":event["args"], "outcome":event["output"]["outcome"]["status"],
            })
        })
        .collect();
    let mut expected = Vec::new();
    for (ordinal, topic) in [(1, "lash"), (2, "second")] {
        for (kind, outcome) in [
            ("tool_call_started", serde_json::Value::Null),
            ("tool_call_returned", serde_json::json!("success")),
        ] {
            expected.push(serde_json::json!({
                "type":kind, "execution":"read", "caller":{"kind":"process", "process_id":process},
                "ordinal":ordinal, "name":LOOKUP_TOOL,
                "provider":{"registry":"process", "owner":KNOWLEDGE.component(), "key":"tools"},
                "args":{"topic":topic}, "outcome":outcome,
            }));
        }
    }
    assert_eq!(facts, expected);
    assert_eq!(activities[0]["call_id"], activities[1]["call_id"]);
    assert_eq!(activities[2]["call_id"], activities[3]["call_id"]);
    assert_ne!(activities[0]["call_id"], activities[2]["call_id"]);
    assert_eq!(
        success(&ended(&core, &process).await),
        serde_json::json!([
            {"topic":"lash", "draw":1}, {"topic":"second", "draw":2}
        ])
    );
    let replayed = core
        .processes()
        .replay(&process, lash::workflow::ReplaySegments::All)
        .await
        .unwrap();
    assert_eq!(replayed.len(), 2, "both sides of the park replay");
    assert!(
        replayed.iter().all(|segment| matches!(
            segment.answer,
            lash::workflow::SegmentAnswer::Verified { .. }
        )),
        "{replayed:?}"
    );
    assert_eq!(
        answered.reads.load(Ordering::SeqCst),
        2,
        "replay consumes its tape without calling a reader"
    );
    for event in activities
        .iter()
        .filter(|event| event["type"] == "tool_call_returned")
    {
        let id = lash::ToolCallId::parse(event["call_id"].as_str().unwrap()).unwrap();
        let saved = core
            .processes()
            .tool_call(&process, &id)
            .await
            .unwrap()
            .expect("the caller's save retains the live read row");
        assert_eq!(saved.args, event["args"]);
        assert_eq!(
            serde_json::to_value(lash_protocol_types::Durable(&saved.output)).unwrap(),
            event["output"]
        );
    }
    assert_eq!(answered.bodies.load(Ordering::SeqCst), 0);
    core.shutdown().await.unwrap();
}

struct RefusingRead {
    kind: &'static str,
    entered: tokio::sync::Notify,
    count: AtomicUsize,
}

#[async_trait::async_trait]
impl lash::tools::ToolReads for RefusingRead {
    async fn read(
        &self,
        _: lash::tools::ReadCall<'_>,
    ) -> Result<serde_json::Value, lash::tools::ReadError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        match self.kind {
            "panic" => panic!("reader invariant failed"),
            "invalid" => Ok(serde_json::json!(-1)),
            "pending" => std::future::pending().await,
            _ => panic!("a denying hook must not enter its reader"),
        }
    }
}

async fn process_read_refusal(kind: &'static str) {
    let clock = lash_durable_test::SimClock::new();
    let stores = Arc::new(sim::memory(Arc::clone(&clock)).await);
    let ports: Arc<dyn StoreSet> = stores;
    let answered = Arc::new(Answered::default());
    let reader = Arc::new(RefusingRead {
        kind,
        entered: tokio::sync::Notify::new(),
        count: AtomicUsize::new(0),
    });
    let mut definition = lookup_definition();
    if kind == "invalid" {
        definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
            LOOKUP_TOOL,
            "tools.lookup",
            "",
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"integer","minimum":0}),
        )
        .unwrap()
        .declared_contract(lash_kernel_value::operations::ContractId::new("1"))
        .with_execution(Duration::from_secs(120))
        .with_execution_policy(lash_kernel_value::ExecutionPolicy::Read);
    }
    if kind == "pending" {
        definition = definition.with_execution(Duration::from_millis(1_137));
    }
    let (_, core) =
        super::session_and_isolated_envs::booted(&ports, Vec::new(), "read-refusal", |backend| {
            let workers = sim::workers(&clock);
            let bounds = workers.config().run_bounds;
            lash::LashCore::standard_builder(backend.clone())
                .plugin(Arc::new(lash_vm_runtime::KernelProcessPluginFactory::new(
                    workers,
                    "typescript",
                    bounds,
                )))
                .process_component(
                    knowledge_with_contract(&answered, reader.clone(), definition, kind == "hook"),
                    KNOWLEDGE,
                )
        });
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(KNOWLEDGE, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let source = if kind == "panic" {
        LOOKUP.replace("  let got = perform 0(input) as Any\n  return got", "  try { do perform 0(input) as Any } catch error { return {continued: true} }\n  return {continued: true}")
    } else {
        LOOKUP.to_owned()
    };
    let (payload, env) = kernel_process::publish(&core, &source, &env).await;
    let process = start_under(
        &core,
        &env,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        payload,
        |request| request,
    )
    .await;
    if kind == "pending" {
        tokio::time::timeout(Duration::from_secs(30), reader.entered.notified())
            .await
            .unwrap();
        let deadline = clock.logical_ms() + 1_137;
        tokio::time::timeout(Duration::from_secs(2), clock.wait_for_sleep(deadline))
            .await
            .expect("the tool deadline is armed on the injected clock");
        clock.advance_to(deadline + 1).await;
    }
    let output = ended(&core, &process).await;
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("a refused read must fail its process: {output:?}");
    };
    if kind == "panic" {
        assert!(
            matches!(failure.cause.as_deref(), Some(lash_core::ToolFailureCause::ReadPanicked { panic }) if panic.message == "reader invariant failed"),
            "{failure:?}"
        );
    }
    let steps = lash_core_execution::runtime::process::steps::read_process_steps(
        &core.effect_host(),
        &process,
    )
    .await
    .unwrap();
    let mut facts = Vec::new();
    for step in &steps {
        if step.tool
            == format!(
                "{}/{}",
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                lash_vm_runtime::KERNEL_RUN_STEP
            )
            && let Some(payload) = step
                .outcome
                .as_ref()
                .and_then(lash_core_store::tool_run::SettledOutput::payload)
        {
            facts.extend(lash_vm_runtime::process::saved_read_facts(payload).unwrap());
        }
    }
    if kind == "hook" {
        assert_eq!(reader.count.load(Ordering::SeqCst), 0);
        assert_eq!(answered.bodies.load(Ordering::SeqCst), 0);
        assert!(facts.is_empty(), "hooks bind Recorded");
        assert!(format!("{steps:?}").contains("read_denied"));
    } else {
        let [fact] = facts.as_slice() else {
            panic!("one saved refusal: {facts:?}");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = &fact.outcome else {
            panic!("the refused read succeeded");
        };
        match kind {
            "invalid" => assert!(
                matches!(
                    failure.cause.as_deref(),
                    Some(lash_core::ToolFailureCause::Declaration { .. })
                ),
                "{failure:?}"
            ),
            "pending" => assert_eq!(failure.code, "read_timed_out"),
            _ => {}
        }
        assert_eq!(
            core.processes()
                .tool_call(&process, &fact.call_id)
                .await
                .unwrap(),
            Some(fact.record())
        );
    }
    core.shutdown().await.unwrap();
}

/// READ-PANIC-TERMINAL: a process cannot catch a provider panic and continue.
#[tokio::test]
async fn a_reader_panic_stops_its_process_before_guest_catch_on_sqlite_memory() {
    Box::pin(process_read_refusal("panic")).await;
}

/// READ-OUTPUT-ADMISSION: a process refuses the same schema-only constraint.
#[tokio::test]
async fn a_process_read_refuses_an_undeclared_negative_output_on_sqlite_memory() {
    Box::pin(process_read_refusal("invalid")).await;
}

/// READ-INJECTED-CLOCK: a pending process read uses the injected tool deadline.
#[tokio::test]
async fn a_pending_process_read_times_out_on_the_injected_clock_on_sqlite_memory() {
    Box::pin(process_read_refusal("pending")).await;
}

/// READ-HOOK-BINDING: a denying check forces Recorded and enters no reader.
#[tokio::test]
async fn a_denying_hook_prevents_process_reads_on_sqlite_memory() {
    Box::pin(process_read_refusal("hook")).await;
}
