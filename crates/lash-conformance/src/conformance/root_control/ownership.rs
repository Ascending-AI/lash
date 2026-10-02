//! FIG-3607 contract 4 (FIG-4489): every logical turn a drive runs is owned
//! by its logical root.
//!
//! Each case runs a root on the tier and has a probe tool, called inside the
//! root's turns, record the start context its attempt draws child lifetimes
//! from and start a process living until that context's starter. The
//! attempt's start context is materialized from the admitted scope the turn
//! journals its effects under, so it names that scope exactly.
//!
//! - A host-input root and a wake root run their turn under `Turn(root)`.
//! - A frame follow-on and a terminal-checkpoint follow-on run their next
//!   physical turn inside the same root, under the same `Turn(root)`.
//! - A follow-on recovered by a later drive, after the drive that switched
//!   frames died, runs under `Turn(logical root)`: the root that owed it,
//!   whose evidence its final commit writes and whose scope that evidence
//!   closes. It is admitted under a recovery root of its own, which owns no
//!   effect of the turn.
//! - A parked root keeps its scope open: no evidence, no close, and the
//!   process living until it owes nothing, until an operator cancel writes
//!   its evidence and closes it.
//!
//! Each ended root has positive terminal evidence, and its scope closes
//! exactly once, after that evidence is durable, in the process registry:
//! every process its turns started is then owed its cancel.

use super::*;

const PROBE_TOOL: &str = "ownership_probe";
const SWITCH_TOOL: &str = "ownership_switch";
/// How long one drive may take before the law fails it.
const DRIVE_BOUND: std::time::Duration = std::time::Duration::from_secs(45);

/// What one probe attempt saw.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Capture {
    /// The start context's starter.
    starter: crate::ScopeId,
    /// The start context's ancestry, nearest first.
    ancestors: Vec<crate::ScopeId>,
    /// The logical root the attempt's admitted scope names.
    logical_root: Option<TurnId>,
}

/// One answer of the law's scripted model.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Call the probe tool.
    Probe,
    /// Call the frame-switch tool.
    Switch,
    /// Answer with text, ending the turn.
    Answer,
    /// Kill the execution here, inside the model call.
    Crash,
}

/// The probe and frame-switch tools of one case.
struct OwnershipTools {
    registry: Arc<dyn crate::ProcessRegistry>,
    session_id: SessionId,
    /// Keyed by the call id lash minted: a call is delivered at least once.
    captures: Mutex<Vec<(String, Capture)>>,
    children: Mutex<Vec<crate::ProcessId>>,
    /// An input the first probe call addresses to the running turn, admitted
    /// at its terminal checkpoint.
    steer: Mutex<Option<(Arc<dyn crate::RuntimeStore>, TurnId)>>,
}

struct ProbeEvidence<'a>(&'a OwnershipTools);

impl Drop for ProbeEvidence<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "ownership probe for {}: captures={:?}, children={:?}, pending_steer={:?}",
                self.0.session_id,
                self.0
                    .captures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                self.0
                    .children
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                self.0
                    .steer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|(_, root)| root),
            );
        }
    }
}

impl OwnershipTools {
    fn new(registry: Arc<dyn crate::ProcessRegistry>, session_id: SessionId) -> Arc<Self> {
        Arc::new(Self {
            registry,
            session_id,
            captures: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            steer: Mutex::new(None),
        })
    }

    fn captures(&self) -> Vec<Capture> {
        self.captures
            .lock()
            .expect("captures")
            .iter()
            .map(|(_, capture)| capture.clone())
            .collect()
    }

    fn children(&self) -> Vec<crate::ProcessId> {
        self.children.lock().expect("children").clone()
    }

    fn factory(self: &Arc<Self>) -> Arc<dyn crate::plugin::PluginFactory> {
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-ownership-probe",
            crate::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::clone(self) as Arc<dyn crate::ToolProvider>),
        ))
    }

    fn definition(name: &str) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "A conformance probe of the turn's ownership.",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "object", "additionalProperties": true}),
        )
    }

    async fn probe(&self, context: &crate::AttemptContext<'_>) -> crate::ToolAttemptOutcome {
        let cx = context
            .start_cx()
            .expect("a session turn's attempt has a start context");
        let capture = Capture {
            starter: cx.starter().id().clone(),
            ancestors: cx
                .ancestors()
                .iter()
                .map(|scope| scope.id().clone())
                .collect(),
            logical_root: context.logical_root(),
        };
        let call_id = context.call_id().to_string();
        {
            let mut captures = self.captures.lock().expect("captures");
            if !captures.iter().any(|(seen, _)| *seen == call_id) {
                captures.push((call_id, capture));
            }
        }
        let child = self
            .registry
            .register_process(crate::started_until_starter(
                crate::ProcessRegistration::new(
                    crate::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::ProcessProvenance::session(crate::SessionScope::new(
                        self.session_id.as_str(),
                    )),
                    lash_core::Lifetime::Detached,
                ),
                cx.starter().id().clone(),
            ))
            .await
            .expect("a start living until the running turn's starter is admitted");
        self.children.lock().expect("children").push(child.id);
        let steer = self.steer.lock().expect("steer").take();
        if let Some((store, turn)) = steer {
            store
                .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                    self.session_id.clone(),
                    crate::TurnInputIngress::active_turn(
                        turn,
                        crate::TurnInputCheckpointBoundary::BeforeCompletion,
                    ),
                    crate::TurnInput::text("steer the running root at its terminal checkpoint"),
                ))
                .await
                .expect("accept an input addressed to the running turn");
        }
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"probed": true})),
        ))
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for OwnershipTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![
            Self::definition(PROBE_TOOL).manifest(),
            Self::definition(SWITCH_TOOL).manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        [PROBE_TOOL, SWITCH_TOOL]
            .contains(&name)
            .then(|| Arc::new(Self::definition(name).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name() == PROBE_TOOL {
            return self.probe(call.context).await;
        }
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("ownership-follow-on")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("ownership follow-on".to_string()),
                },
            ),
        ))
    }
}

/// Serve the case's model from `script`, one step per call in order; a call
/// past the script answers. A [`Step::Crash`] fires `crash` and never
/// returns.
fn script_model(parts: &mut DriveParts, script: Vec<Step>, crash: crate::ConformanceCrash) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let step = script.get(index).copied().unwrap_or(Step::Answer);
            let crash = crash.clone();
            async move {
                let tool = |name: &str| crate::LlmOutputPart::ToolCall {
                    call_id: format!("ownership-call-{index}"),
                    tool_name: name.into(),
                    input_json: "{}".into(),
                    replay: None,
                };
                let part = match step {
                    Step::Probe => tool(PROBE_TOOL),
                    Step::Switch => tool(SWITCH_TOOL),
                    Step::Answer => crate::LlmOutputPart::Text {
                        text: format!("answer {}", index + 1),
                        response_meta: None,
                    },
                    Step::Crash => {
                        crash.fire();
                        std::future::pending::<()>().await;
                        unreachable!("a crashed execution never resumes")
                    }
                };
                Ok(crate::LlmResponse {
                    parts: vec![part],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build();
    parts.host.providers.models = crate::testing::standard_test_models(provider.into_handle());
}

async fn runtime_with(parts: &DriveParts, tools: &Arc<OwnershipTools>) -> crate::LashRuntime {
    let state = parts.initial_state();
    let policy = state.policy.clone();
    Box::pin(
        crate::LashRuntime::builder(parts.host.clone(), crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain([tools.factory()])
                    .collect(),
            )
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the ownership conformance runtime")
}

/// One drive of the session, request `request`, on the tier under driver
/// scope `driver`.
fn drive_attempt(
    parts: &DriveParts,
    tools: &Arc<OwnershipTools>,
    request: &str,
    tx: Option<tokio::sync::mpsc::UnboundedSender<DriveOutcome>>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let tools = Arc::clone(tools);
    let request = parts.request(request);
    Arc::new(move |scope| {
        let parts = parts.clone();
        let tools = Arc::clone(&tools);
        let request = request.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let _evidence = ProbeEvidence(&tools);
            let mut runtime = runtime_with(&parts, &tools).await;
            let outcome = lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs");
            if let Some(tx) = tx {
                let _ = tx.send(outcome);
            }
            crate::ConformanceTurnEnd::Settled
        })
    })
}

fn driver(parts: &DriveParts, name: &str) -> crate::AdmittedScope {
    crate::admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::from(name),
    ))
}

async fn drive_with(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    tools: &Arc<OwnershipTools>,
    request: &str,
) -> DriveOutcome {
    let _evidence = ProbeEvidence(tools);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // A drive that diverged from its journal never ends: the tier retries it
    // until it rests. Bound the wait so the divergence fails the law.
    tokio::time::timeout(
        DRIVE_BOUND,
        runner.run_turn(
            driver(parts, &format!("{request}-driver")),
            drive_attempt(parts, tools, request, Some(tx)),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("the `{request}` drive ends: probes {:?}", tools.captures()));
    rx.recv().await.expect("the tier ran the drive")
}

/// A scope owner that closes in the process registry and records every root
/// close with whether the root's evidence was durable when it was asked.
struct CountedClose {
    store: Arc<dyn crate::RuntimeStore>,
    registry: crate::RegistryScopeClose,
    closes: Mutex<Vec<(TurnId, bool)>>,
}

impl CountedClose {
    fn over(parts: &mut DriveParts, stores: &Arc<dyn crate::StoreSet>) -> Arc<Self> {
        let close = Arc::new(Self {
            store: Arc::clone(&parts.store),
            registry: crate::RegistryScopeClose::new(stores.process_registry(), stores.clock())
                .with_session_store_factory(stores.session_store_factory()),
            closes: Mutex::new(Vec::new()),
        });
        parts.host.control.scope_close = close.clone();
        close
    }

    fn closes_of(&self, root: &TurnId) -> Vec<bool> {
        self.closes
            .lock()
            .expect("closes")
            .iter()
            .filter(|(closed, _)| closed == root)
            .map(|(_, durable)| *durable)
            .collect()
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for CountedClose {
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), crate::StoreError> {
        let durable = self
            .store
            .root_terminal(&terminal.session_id, &terminal.root)
            .await?
            .is_some_and(|stored| stored.same_terminal(terminal));
        self.closes
            .lock()
            .expect("closes")
            .push((terminal.root.clone(), durable));
        self.registry.close_root_scope(terminal).await
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        self.registry
            .close_session_scope(session, intent, roots)
            .await
    }
}

/// `root` of case `case` is owned by its logical root: every probe attempt
/// ran under `Turn(root)` and named `root` its logical root, the root has
/// evidence of `kind`, and its scope closed once, after that evidence was
/// durable, owing every process the probe started its cancel.
#[allow(
    clippy::too_many_arguments,
    reason = "the law's one assertion takes the case's fixture beside the root and what it expects of it"
)]
async fn assert_owned(
    case: &str,
    parts: &DriveParts,
    stores: &Arc<dyn crate::StoreSet>,
    tools: &OwnershipTools,
    closes: &CountedClose,
    root: &TurnId,
    probes: usize,
    kind: RootTerminalKind,
) {
    let _evidence = ProbeEvidence(tools);
    let turn = crate::ScopeId::turn(parts.session_id.clone(), root.clone());
    let owned = Capture {
        starter: turn.clone(),
        ancestors: vec![
            turn.clone(),
            crate::ScopeId::session(parts.session_id.clone()),
        ],
        logical_root: Some(root.clone()),
    };
    let captures = tools.captures();
    assert_eq!(
        captures.len(),
        probes,
        "{case}: every probe call ran once: {captures:?}"
    );
    for capture in &captures {
        assert_eq!(
            capture, &owned,
            "{case}: the turn's effects and process starts are owned by `Turn({root})`"
        );
    }
    let terminal = parts
        .store
        .root_terminal(&parts.session_id, root)
        .await
        .expect("read the root's terminal evidence")
        .unwrap_or_else(|| panic!("{case}: root `{root}` has terminal evidence"));
    assert_eq!(
        terminal.kind(),
        kind,
        "{case}: root `{root}` ended {kind:?}"
    );
    assert_eq!(
        closes.closes_of(root),
        vec![true],
        "{case}: root `{root}`'s scope closed exactly once, after its evidence was durable"
    );
    let registry = stores.process_registry();
    let plan = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the root's close row")
        .unwrap_or_else(|| panic!("{case}: root `{root}`'s end closed `Turn({root})`"));
    assert_eq!(plan.parent, turn);
    let owed = registry
        .list_parent_end_children(&turn, None, NonZeroUsize::new(64).expect("non-zero"))
        .await
        .expect("page the ended root's children")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    for child in tools.children() {
        assert!(
            owed.contains(&child),
            "{case}: the process `{child}` the root's turns started is owed its cancel \
             by `Turn({root})`'s close: {owed:?}"
        );
    }
}

/// The committed root of `outcome`, the only root it ran.
fn committed_root(case: &str, outcome: &DriveOutcome) -> TurnId {
    match &outcome.ran[..] {
        [RootOutcome::Committed { root, .. }] => root.clone(),
        ran => panic!("{case}: the drive committed one root: {ran:?}"),
    }
}

/// FIG-3607 contract 4 (FIG-4489): every logical turn a drive runs — a
/// host-input root's, a wake root's, a frame or terminal-checkpoint
/// follow-on's, and a follow-on a later drive recovered — journals its
/// effects and draws its process starts' ancestry under `Turn(logical
/// root)`, writes positive terminal evidence for that root, and closes that
/// root's scope exactly once. A parked root keeps its scope open until a
/// cancel ends it.
#[expect(
    clippy::too_many_lines,
    reason = "one law walks every kind of driver-run logical turn"
)]
pub async fn every_driver_turn_is_owned_by_its_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let registry = stores.process_registry();
    let no_crash = crate::ConformanceCrash::new;

    // A host-input root, named by its host id.
    let case = "host-input root";
    let mut parts = DriveParts::new(prefix, "owned-input", &effect_host, &stores, 1).await;
    let closes = CountedClose::over(&mut parts, &stores);
    script_model(&mut parts, vec![Step::Probe, Step::Answer], no_crash());
    let tools = OwnershipTools::new(Arc::clone(&registry), parts.session_id.clone());
    let root = TurnId::from("owned-input-root");
    parts.enqueue("ask", Some(root.as_str())).await;
    let outcome = drive_with(&runner, &parts, &tools, "owned-input").await;
    assert_eq!(committed_root(case, &outcome), root);
    assert_owned(
        case,
        &parts,
        &stores,
        &tools,
        &closes,
        &root,
        1,
        RootTerminalKind::Answered,
    )
    .await;
    if let Some(keys) = runner
        .recorded_replay_keys(&crate::ExecutionScope::turn(
            &parts.session_id,
            root.clone(),
        ))
        .await
    {
        assert!(
            keys.iter().any(|key| key.contains("llm")),
            "{case}: the root's model call is journaled under its turn scope: {keys:?}"
        );
    }
    runner.scenario_finished().await;

    // A wake root, named by its admission.
    let case = "wake root";
    let mut parts = DriveParts::new(prefix, "owned-wake", &effect_host, &stores, 1).await;
    let closes = CountedClose::over(&mut parts, &stores);
    script_model(&mut parts, vec![Step::Probe, Step::Answer], no_crash());
    let tools = OwnershipTools::new(Arc::clone(&registry), parts.session_id.clone());
    parts
        .store
        .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
            &parts.session_id,
            "owned-wake",
            1,
            "wake",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the wake");
    let outcome = drive_with(&runner, &parts, &tools, "owned-wake").await;
    let root = committed_root(case, &outcome);
    assert_owned(
        case,
        &parts,
        &stores,
        &tools,
        &closes,
        &root,
        1,
        RootTerminalKind::Answered,
    )
    .await;
    runner.scenario_finished().await;

    // A frame follow-on: the root's first turn switches frames, and the
    // follow-on physical turn answers inside the same root.
    let case = "frame follow-on";
    let mut parts = DriveParts::new(prefix, "owned-frame", &effect_host, &stores, 1).await;
    let closes = CountedClose::over(&mut parts, &stores);
    script_model(
        &mut parts,
        vec![Step::Probe, Step::Switch, Step::Probe, Step::Answer],
        no_crash(),
    );
    let tools = OwnershipTools::new(Arc::clone(&registry), parts.session_id.clone());
    let root = TurnId::from("owned-frame-root");
    parts
        .enqueue("switch, then answer", Some(root.as_str()))
        .await;
    let outcome = drive_with(&runner, &parts, &tools, "owned-frame").await;
    assert_eq!(committed_root(case, &outcome), root);
    assert_owned(
        case,
        &parts,
        &stores,
        &tools,
        &closes,
        &root,
        2,
        RootTerminalKind::Answered,
    )
    .await;
    runner.scenario_finished().await;

    // A terminal-checkpoint follow-on: an input addressed to the running
    // turn is admitted at its terminal checkpoint and withheld from it, and
    // the root drives it in a follow-on physical turn.
    let case = "checkpoint follow-on";
    let mut parts = DriveParts::new(prefix, "owned-checkpoint", &effect_host, &stores, 1).await;
    let closes = CountedClose::over(&mut parts, &stores);
    script_model(
        &mut parts,
        vec![Step::Probe, Step::Answer, Step::Probe, Step::Answer],
        no_crash(),
    );
    let tools = OwnershipTools::new(Arc::clone(&registry), parts.session_id.clone());
    let root = TurnId::from("owned-checkpoint-root");
    *tools.steer.lock().expect("steer") = Some((Arc::clone(&parts.store), root.clone()));
    parts
        .enqueue("answer, then take the steer", Some(root.as_str()))
        .await;
    let outcome = drive_with(&runner, &parts, &tools, "owned-checkpoint").await;
    assert_eq!(committed_root(case, &outcome), root);
    assert_owned(
        case,
        &parts,
        &stores,
        &tools,
        &closes,
        &root,
        2,
        RootTerminalKind::Answered,
    )
    .await;
    runner.scenario_finished().await;

    // A recovered follow-on: the drive that switched frames dies inside the
    // follow-on's model call, and a later drive recovers the follow-on the
    // head owes under a recovery root of its own.
    let case = "recovered follow-on";
    let mut parts = DriveParts::new(prefix, "owned-recovered", &effect_host, &stores, 1).await;
    let closes = CountedClose::over(&mut parts, &stores);
    let crash = crate::ConformanceCrash::new();
    script_model(
        &mut parts,
        vec![
            Step::Probe,
            Step::Switch,
            Step::Crash,
            Step::Probe,
            Step::Answer,
        ],
        crash.clone(),
    );
    let tools = OwnershipTools::new(Arc::clone(&registry), parts.session_id.clone());
    let root = TurnId::from("owned-recovered-root");
    parts.enqueue("switch, then die", Some(root.as_str())).await;
    tokio::time::timeout(
        DRIVE_BOUND,
        runner.run_turn_until_crash(
            driver(&parts, "owned-recovered-crashed-driver"),
            drive_attempt(&parts, &tools, "owned-recovered-crashed", None),
            crash.clone(),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("{case}: the crashing drive reaches its crash"));
    assert!(
        crash.has_fired(),
        "{case}: the drive died inside the follow-on"
    );
    let owed = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the session head")
        .and_then(|head| head.pending_follow_on)
        .unwrap_or_else(|| panic!("{case}: the switch commit left its follow-on owed"));
    assert_eq!(owed.root_turn_id(), root);
    let outcome = drive_with(&runner, &parts, &tools, "owned-recovered").await;
    assert!(
        matches!(
            &outcome.ran[..],
            [RootOutcome::Committed { root: recovery, .. }] if owed.names_recovery(recovery)
        ),
        "{case}: the later drive recovered the follow-on under its recovery root: {outcome:?}"
    );
    assert_owned(
        case,
        &parts,
        &stores,
        &tools,
        &closes,
        &root,
        2,
        RootTerminalKind::Answered,
    )
    .await;
    assert!(
        closes.closes_of(&owed.recovery_root()).is_empty(),
        "{case}: the recovery root owns no scope of its own to close"
    );
    runner.scenario_finished().await;

    // A parked root keeps its scope open until a cancel ends it.
    let case = "parked root";
    let mut parked = Fixture::new(prefix, "owned-parked", &effect_host, &stores).await;
    let closes = CountedClose::over(&mut parked.parts, &stores);
    let tools = OwnershipTools::new(Arc::clone(&registry), parked.parts.session_id.clone());
    let turn = crate::ScopeId::turn(parked.parts.session_id.clone(), parked.root.clone());
    let child = registry
        .register_process(crate::started_until_starter(
            crate::ProcessRegistration::new(
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::ProcessProvenance::session(crate::SessionScope::new(
                    parked.parts.session_id.as_str(),
                )),
                lash_core::Lifetime::Detached,
            ),
            turn.clone(),
        ))
        .await
        .expect("a start living until the parked root is admitted");
    tools
        .children
        .lock()
        .expect("children")
        .push(child.id.clone());
    let outcome = drive_with(&runner, &parked.parts, &tools, "owned-parked").await;
    assert!(
        outcome
            .ran
            .iter()
            .all(|ran| !matches!(ran, RootOutcome::Committed { .. })),
        "{case}: the parked root ran nothing: {outcome:?}"
    );
    assert!(
        parked
            .parts
            .store
            .root_terminal(&parked.parts.session_id, &parked.root)
            .await
            .expect("read the parked root's evidence")
            .is_none(),
        "{case}: a parked root has no evidence"
    );
    assert!(
        closes.closes_of(&parked.root).is_empty(),
        "{case}: no close was asked for the parked root"
    );
    assert!(
        registry
            .get_parent_end_plan(&turn)
            .await
            .expect("read the parked root's close row")
            .is_none(),
        "{case}: the parked root's scope stays open"
    );
    assert!(
        registry
            .get_process(&child.id)
            .await
            .expect("read the child")
            .expect("the child is recorded")
            .cancel_request
            .is_none(),
        "{case}: the process living until the parked root owes nothing"
    );
    let intent = parked
        .verb(RootVerb::Cancel)
        .await
        .expect("cancel the parked root");
    let (work, _) = parked.control(false, false);
    let relay = lash_core::runtime::drive::ControlIntentRelay::new(
        Arc::clone(&parked.intents),
        Arc::clone(&parked.factory),
        work as Arc<dyn crate::SessionWorkEngine>,
        closes.clone() as Arc<dyn ScopeCloseSink>,
        Arc::new(lash_core::runtime::drive::ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&parked.factory),
            closes.clone() as Arc<dyn ScopeCloseSink>,
        )),
        Arc::clone(&parked.parts.host.clock),
    );
    for _ in 0..2 {
        assert!(
            matches!(
                relay
                    .deliver_intent(&intent)
                    .await
                    .expect("deliver the cancel"),
                ControlIntentState::Acknowledged { .. }
            ),
            "{case}: the cancel is acknowledged"
        );
    }
    let root = parked.root.clone();
    assert_owned(
        case,
        &parked.parts,
        &stores,
        &tools,
        &closes,
        &root,
        0,
        RootTerminalKind::Cancelled,
    )
    .await;
}
