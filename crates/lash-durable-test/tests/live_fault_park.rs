//! PARK-FAULT: a live fault at a session actor's surface install is the
//! pass's fault, never the turn's outcome (ADR 0132; FIG-4651's laws, which
//! FIG-5194 deleted with the engine they ran on).
//!
//! A host's session runs one turn on the core's own node. A plugin's tool
//! catalog contributor runs at every install of the session's tool surface;
//! while the law arms it, it answers the attempt fault a store that did not
//! answer would. A fault that clears inside the activation budget leaves the
//! next pass to complete the turn once. A fault that outlasts the budget
//! parks the session with its typed `PassLoop` park, naming the fault, and
//! the turn's model is never called.
//!
//! PANICKING-SESSION-ACTIVATION-PARKS-AT-BUDGET (FIG-6096): a contributor
//! that panics instead hands every claim's activation back with nothing
//! committed. The session parks with its typed `ActivationLoop` park once
//! the store counts the budget's claims in a row, and a cancel of the parked
//! turn still ends it.
//!
//! CANCEL-DURING-SESSION-PARK-HONOURED (FIG-6099): a turn cancel committed
//! while either park is on its way, after the park's read and before its
//! commit, is not stranded by it: the session stays claimable, and the
//! cancel ends the turn.
//!
//! A-PARKED-TURN-RESUMES-AT-THE-NEXT-BOOT (FIG-6310): a session parked
//! `PassLoop` with its turn open in its tools phase, no turn deadline, no
//! end and no claim (the state FIG-6289's ordinal miss left on a live
//! deployment through three restarts) is redriven by the next node's boot,
//! which finishes the turn; a session parked `ActivationLoop` stays parked
//! for an operator's redrive.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::durable_port::domain::UnfinishedPhase;
use lash_core::durable_port::{ActorKey, ActorState, CommitLabel, Release};
use lash_core::llm::types::LlmRequest;
use lash_core::testing::DurableCommitHook;
use lash_protocol_types::sync::MutexExt as _;

use served::{Tier, World};

const PLUGIN: &str = "live-fault-catalog";
const FAULT: &str = "park-fault: the contributor's store did not answer";

/// How long a cancel the park left claimable may take to end its turn.
const ENDS_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// The contributor's faults: how many of its next runs answer [`FAULT`],
/// and whether every run panics instead.
#[derive(Default)]
struct Faults {
    left: AtomicUsize,
    panics: AtomicBool,
    runs: AtomicUsize,
}

impl Faults {
    fn arm(&self, runs: usize) {
        self.left.store(runs, Ordering::SeqCst);
    }

    fn run(&self) -> Result<(), lash::plugins::PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panics.load(Ordering::SeqCst),
            "park-fault: the contributor panics"
        );
        match self
            .left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            }) {
            Ok(_) => Err(lash::plugins::PluginError::attempt_fault(FAULT)),
            Err(_) => Ok(()),
        }
    }
}

/// The plugin whose catalog contributor runs at every surface install.
#[derive(Clone)]
struct FaultingCatalog {
    faults: Arc<Faults>,
}

impl FaultingCatalog {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}
impl lash::plugins::PluginMetadata for FaultingCatalog {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

impl lash::plugins::PluginFactory for FaultingCatalog {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<FaultingCatalog>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        FaultingCatalog::declare(&mut reg)?;
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
        ))
    }
}

impl lash::plugins::SessionPlugin for FaultingCatalog {
    fn id(&self) -> &'static str {
        PLUGIN
    }
}
impl FaultingCatalog {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.tool_catalog().contribute(
            lash::hook_key!("faulting-catalog"),
            move |this, _state| {
                let faults = Arc::clone(&this.faults);
                Ok(Arc::new(move |_| {
                    faults.run()?;
                    Ok(Default::default())
                }))
            },
        )?;
        Ok(())
    }
}

/// A model that answers every request with `done` and counts its calls.
fn counted_model(calls: Arc<AtomicUsize>) -> lash::provider::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("park-fault-model")
        .complete(move |request: LlmRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(served::text(&request, "done")) }
        })
        .build()
        .into_handle()
}

/// A world whose sessions run under the faulting catalog.
async fn world(tier: Tier) -> Option<(World, Arc<Faults>, Arc<AtomicUsize>)> {
    let faults = Arc::new(Faults::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin = FaultingCatalog {
        faults: Arc::clone(&faults),
    };
    let world = World::with_model(
        tier,
        Vec::new(),
        counted_model(Arc::clone(&calls)),
        move |backend| lash::LashCore::standard_builder(backend.clone()).plugin(Arc::new(plugin)),
    )
    .await?;
    Some((world, faults, calls))
}

/// A world whose sessions run under the faulting catalog, over a durable
/// store that runs the law's hook before each owner commit.
async fn hooked_world(
    tier: Tier,
) -> Option<(World, Arc<Faults>, Arc<AtomicUsize>, Arc<DurableCommitHook>)> {
    let faults = Arc::new(Faults::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let hook = Arc::new(std::sync::OnceLock::<Arc<DurableCommitHook>>::new());
    let plugin = FaultingCatalog {
        faults: Arc::clone(&faults),
    };
    let world = World::layered_with_model(
        tier,
        {
            let hook = Arc::clone(&hook);
            move |stores| {
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_durable_store(move |inner| {
                        let hooked = Arc::new(DurableCommitHook::over(inner));
                        let _ = hook.set(Arc::clone(&hooked));
                        hooked
                    })
                    .into_store_set()
            }
        },
        counted_model(Arc::clone(&calls)),
        move |backend| lash::LashCore::standard_builder(backend.clone()).plugin(Arc::new(plugin)),
    )
    .await?;
    let hook = Arc::clone(hook.get().expect("the durable store is hooked"));
    Some((world, faults, calls, hook))
}

/// The session actor's park, once it parked: its recorded reason.
async fn parked(world: &World, session: &str) -> String {
    let actor = lash_core::durable_port::ActorKey::session(session).expect("a session actor key");
    let durable = Arc::clone(world.backend.durable());
    tokio::time::timeout(served::WATCHDOG, async {
        loop {
            if let Ok(Some(snapshot)) = durable.actor(&actor).await
                && snapshot.state == lash_core::durable_port::ActorState::Parked
            {
                return snapshot.park.unwrap_or_default();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the session `{session}` never parked"))
}

/// A live fault at the session's surface install that clears before the
/// activation budget is spent is retried by the session's next pass, which
/// completes the turn once; one that outlasts the budget parks the session
/// with its typed `PassLoop` park naming the fault, and no model is called.
async fn a_live_surface_install_fault_parks_typed_at_the_budget_and_one_that_clears_completes_once(
    tier: Tier,
) {
    let Some((world, faults, calls)) = world(tier).await else {
        return;
    };
    let budget = world.backend.config().settings().activation_loop_budget;

    // A fault that clears inside the budget.
    let session = world.session("park-fault-clears", served::spec(16)).await;
    faults.arm(usize::try_from(budget).expect("a small budget") - 1);
    let output = world.send(&session, "park-fault-clears").await;
    served::assert_answered("a fault that clears", &output);
    assert_eq!(
        output.assistant_message(),
        Some("done"),
        "the pass after the fault cleared completed the turn"
    );
    assert_eq!(faults.left.load(Ordering::SeqCst), 0, "every fault was met");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the turn's model ran once");

    // A fault that outlasts the budget.
    let session = world.session("park-fault-parks", served::spec(16)).await;
    faults.arm(usize::MAX);
    let runs = faults.runs.load(Ordering::SeqCst);
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .send(lash::TurnInput::text("park-fault-parks"))
                .output()
                .await
        }
    });
    let park = parked(&world, "park-fault-parks").await;
    assert!(
        park.contains("pass_loop") && park.contains(FAULT),
        "the session parks with its typed pass-loop park naming the fault: {park}"
    );
    assert!(
        faults.runs.load(Ordering::SeqCst) - runs >= usize::try_from(budget).unwrap(),
        "every pass of the budget met the fault"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the parked turn never called its model"
    );
    pending.abort();
    faults.arm(0);
    world.shutdown().await;
}

/// PANICKING-SESSION-ACTIVATION-PARKS-AT-BUDGET (FIG-6096): a session whose
/// activation panics at its surface install on every claim, before any
/// commit of the turn's, parks with `ActivationLoop` at the budget: the claim
/// that admitted the turn, then exactly the budget's claims that committed
/// nothing. Its model is never called, and an `Immediate` cancel of the
/// parked turn still ends it, with no surface installed for the cancel.
async fn a_session_whose_activation_panics_parks_at_its_budget_and_a_cancel_still_ends_its_turn(
    tier: Tier,
) {
    let Some((world, faults, calls)) = world(tier).await else {
        return;
    };
    let budget = world.backend.config().settings().activation_loop_budget;
    let session = world.session("panic-parks", served::spec(16)).await;
    faults.panics.store(true, Ordering::SeqCst);
    let runs = faults.runs.load(Ordering::SeqCst);
    let turn = session
        .send(lash::TurnInput::text("panic-parks"))
        .await
        .expect("the turn's input is accepted");
    let park = parked(&world, "panic-parks").await;
    assert_eq!(
        serde_json::from_str::<lash::SessionParkReason>(&park).expect("a session park"),
        lash::SessionParkReason::ActivationLoop {
            failed_activations: budget
        },
        "the panicking session parks at its activation-loop budget"
    );
    assert_eq!(
        faults.runs.load(Ordering::SeqCst) - runs,
        usize::try_from(budget).expect("a small budget") + 1,
        "the claim that admitted the turn and the budget's claims that \
         committed nothing each panicked once"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was called");

    let runs = faults.runs.load(Ordering::SeqCst);
    let cancelled = turn
        .cancel()
        .request_id("cancel-the-parked-turn")
        .mode(lash::TurnCancelMode::Immediate)
        .await
        .expect("the cancel is requested");
    assert!(
        matches!(cancelled, lash::CancelReceipt::Cancelled { .. }),
        "the cancel addressed the parked turn: {cancelled:?}"
    );
    let output = tokio::time::timeout(served::WATCHDOG, turn.output())
        .await
        .expect("the parked turn's cancel ends it")
        .expect("the cancelled turn answers its output");
    let evidence = output
        .outcome
        .cancellation()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.outcome));
    assert_eq!(evidence.request_id, "cancel-the-parked-turn");
    assert_eq!(
        faults.runs.load(Ordering::SeqCst),
        runs,
        "the cancel installed no surface"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was called");
    faults.panics.store(false, Ordering::SeqCst);
    world.shutdown().await;
}

/// CANCEL-DURING-SESSION-PARK-HONOURED (FIG-6099): an `Immediate` cancel
/// of a session's turn that commits while the session parks, after the
/// park's read and before its commit, is not stranded by the park, for an
/// `ActivationLoop` park (a contributor that panics on every claim) and a
/// `PassLoop` one (a contributor that fails every pass) alike. The park
/// commit is held at that point while the cancel commits: the session is
/// left claimable, its park kept, and the cancel ends the turn with no model
/// call and no surface installed for it.
async fn a_cancel_committed_while_a_session_parks_still_ends_its_turn(tier: Tier) {
    let Some((world, faults, calls, hook)) = hooked_world(tier).await else {
        return;
    };
    for (name, panics) in [
        ("park-cancel-activation", true),
        ("park-cancel-pass", false),
    ] {
        let actor = ActorKey::session(name).expect("a session actor key");
        let (reached, at_park) = tokio::sync::oneshot::channel::<()>();
        let (go, released) = tokio::sync::oneshot::channel::<()>();
        let hold = Arc::new(Mutex::new(Some((reached, released))));
        hook.arm(move |tx, _| {
            let held = if tx.actor() == &actor && tx.release() == Some(Release::Parked) {
                hold.lock_recover().take()
            } else {
                None
            };
            async move {
                if let Some((reached, released)) = held {
                    let _ = reached.send(());
                    let _ = released.await;
                }
                Ok(())
            }
        });
        let session = world.session(name, served::spec(16)).await;
        if panics {
            faults.panics.store(true, Ordering::SeqCst);
        } else {
            faults.arm(usize::MAX);
        }
        let turn = session
            .send(lash::TurnInput::text(name))
            .await
            .expect("the turn's input is accepted");
        tokio::time::timeout(served::WATCHDOG, at_park)
            .await
            .unwrap_or_else(|_| panic!("{name}: the session never parked"))
            .expect("the held park reached the hook");
        let runs = faults.runs.load(Ordering::SeqCst);
        let cancelled = turn
            .cancel()
            .request_id(name)
            .mode(lash::TurnCancelMode::Immediate)
            .await
            .expect("the cancel is requested");
        assert!(
            matches!(cancelled, lash::CancelReceipt::Cancelled { .. }),
            "{name}: the cancel addressed the turn: {cancelled:?}"
        );
        go.send(()).expect("the held park waits");
        let output = tokio::time::timeout(ENDS_WITHIN, turn.output())
            .await
            .unwrap_or_else(|_| panic!("{name}: the park stranded the cancel"))
            .unwrap_or_else(|error| panic!("{name}: the park stranded the cancel: {error:?}"));
        let evidence = output
            .outcome
            .cancellation()
            .unwrap_or_else(|| panic!("{name}: the turn ended cancelled: {:?}", output.outcome));
        assert_eq!(evidence.request_id, name);
        assert_eq!(
            faults.runs.load(Ordering::SeqCst),
            runs,
            "{name}: the cancel installed no surface"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{name}: no model was called"
        );
        hook.disarm();
        faults.panics.store(false, Ordering::SeqCst);
        faults.arm(0);
    }
    world.shutdown().await;
}

/// The park FIG-6289's ordinal miss left on a live session (feedback-15),
/// byte for byte as its actor row recorded it.
const ORDINAL_MISS_PARK: &str = r#"{"reason":"pass_loop","failed_passes":8,"error":{"code":"runtime_effect_assistant_response_hook","message":"the save was not committed: durable store Corrupt: the operation can never be answered: none of its members is open"}}"#;

/// The native tool whose first run holds until its node goes.
const HOLD: &str = "hold_probe";

/// The cell's `Repeatable` tool, as FIG-6289's ordinal miss met it.
const FLAKY: &str = "flaky";

/// A tool definition bound as `tools.<name>`.
fn probe_definition(
    name: &str,
    policy: lash_core::ExecutionPolicy,
    output: serde_json::Value,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A park law's probe.",
        lash_core::ToolDefinition::default_input_schema(),
        output,
    )
    .expect("the probe's schemas")
    .with_execution(std::time::Duration::from_secs(600))
    .with_execution_policy(policy)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

/// [`HOLD`]: its first run says it started and never answers; every later
/// run answers at once.
#[derive(Default)]
struct Holding {
    runs: AtomicUsize,
    entered: tokio::sync::Notify,
}

fn hold_definition() -> lash_core::ToolDefinition {
    probe_definition(
        HOLD,
        lash_core::ExecutionPolicy::default(),
        serde_json::json!({ "type": "string" }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Holding {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![hold_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == HOLD).then(|| Arc::new(hold_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        lash_core::ToolOutcome::ok(serde_json::json!("released")).into()
    }
}

/// [`FLAKY`]: `Repeatable` with two attempts and no backoff; its first
/// attempt fails, its second answers its number and marks itself answered.
#[derive(Default)]
struct Flaky {
    attempts: AtomicU32,
    answered: AtomicBool,
}

fn flaky_definition() -> lash_core::ToolDefinition {
    probe_definition(
        FLAKY,
        lash_core::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(2).expect("a nonzero attempt bound"),
            0,
            0,
        ),
        serde_json::json!({ "type": "string" }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Flaky {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![flaky_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == FLAKY).then(|| Arc::new(flaky_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                lash_core::ToolFailureClass::Io,
                "flaky_failed",
                "attempt 1 failed",
                Some(0),
            ))
            .into();
        }
        self.answered.store(true, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!(format!("attempt {attempt}"))).into()
    }
}

fn session_id(name: &str) -> lash::SessionId {
    lash::SessionId::try_from(name.to_owned()).expect("a session id")
}

/// The node that served `name`'s open turn is gone: the turn is open in its
/// tools phase with no turn deadline and no end, and no boot owns the
/// session. Park it as FIG-6289's ordinal miss did, and answer its run.
async fn wedge(world: &World, name: &str) -> lash::RunId {
    let durable = Arc::clone(world.backend.durable());
    let session = session_id(name);
    lash::testing::park_session(
        &world.backend,
        &session,
        serde_json::from_str(ORDINAL_MISS_PARK).expect("the live park decodes"),
    )
    .await
    .expect("the session parks");
    let actor = ActorKey::session(name).expect("a session actor key");
    let snapshot = durable
        .actor(&actor)
        .await
        .expect("the actor reads")
        .expect("the actor");
    assert_eq!(
        snapshot.state,
        ActorState::Parked,
        "{name}: no claim holds it"
    );
    let row = durable
        .turn(&session)
        .await
        .expect("the turn reads")
        .unwrap_or_else(|| panic!("{name}: the wedged turn is open"));
    assert!(
        matches!(row.phase, UnfinishedPhase::Tools { .. }) && row.turn_deadline.is_none(),
        "{name}: the turn is wedged in its tools phase with no deadline: {:?}, {:?}",
        row.phase,
        row.turn_deadline
    );
    assert_eq!(
        durable
            .turn_end(&session, &row.run)
            .await
            .expect("the end reads"),
        None,
        "{name}: the wedged run has no end"
    );
    lash::RunId::parse(row.run.as_str()).expect("the persisted logical run id parses")
}

/// The output of `name`'s run `run` once it settles on the serving node, or
/// a panic if it stays parked.
async fn settled(world: &World, name: &str, run: &lash::RunId) -> lash::TurnOutput {
    let session = world
        .core
        .session(session_id(name))
        .durable()
        .await
        .expect("the session opens on the booted node");
    tokio::time::timeout(served::WATCHDOG, async {
        loop {
            let outcome = session
                .run(run.clone())
                .outcome()
                .await
                .unwrap_or_else(|error| panic!("{name}: the run answers: {error:?}"));
            if let Some(output) = outcome.output() {
                return output.clone();
            }
            assert!(
                matches!(outcome.status(), lash::TurnStatus::Parked(_)),
                "{name}: the run neither settled nor stayed parked: {:?}",
                outcome.status()
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name}: the parked turn never resumed after the boot"))
}

fn holding_core(holding: Arc<Holding>) -> impl Fn(&lash::Backend) -> lash::LashCoreBuilder {
    move |backend| {
        lash::LashCore::standard_builder(backend.clone()).tools(
            lash::tool_provider_key!("tools"),
            Arc::clone(&holding) as Arc<dyn lash_core::ToolProvider>,
        )
    }
}

/// A-PARKED-TURN-RESUMES-AT-THE-NEXT-BOOT (FIG-6310): a session parked
/// `PassLoop` with its turn wedged in its tools phase is redriven by the next
/// boot of a node, which finishes the turn without an operator. A session
/// parked `ActivationLoop`, whose claims may have taken nodes down, stays
/// parked through that boot until an operator redrives it.
async fn a_turn_wedged_in_its_tools_phase_by_a_pass_loop_park_finishes_at_the_next_boot(
    tier: Tier,
) {
    const WEDGED: &str = "wedged-in-tools";
    const OPERATOR: &str = "parked-for-the-operator";
    let holding = Arc::new(Holding::default());
    let Some(mut world) = World::new(tier, holding_core(Arc::clone(&holding))).await else {
        return;
    };
    world.script(
        WEDGED,
        vec![served::response(vec![served::call(
            "call_0",
            HOLD,
            serde_json::json!({}),
        )])],
    );
    let wedged = world.session(WEDGED, served::spec(16)).await;
    let operator = world.session(OPERATOR, served::spec(16)).await;
    served::assert_answered(OPERATOR, &world.send(&operator, OPERATOR).await);
    let accepted = wedged
        .send(lash::TurnInput::text(WEDGED))
        .await
        .expect("the turn's input is accepted");
    tokio::time::timeout(served::WATCHDOG, holding.entered.notified())
        .await
        .expect("the tool's first run started");
    world.core.shutdown().await.expect("the node goes");
    drop(accepted);
    let run = wedge(&world, WEDGED).await;
    let budget = world.backend.config().settings().activation_loop_budget;
    let activation_loop = lash::SessionParkReason::ActivationLoop {
        failed_activations: budget,
    };
    lash::testing::park_session(
        &world.backend,
        &session_id(OPERATOR),
        activation_loop.clone(),
    )
    .await
    .expect("the operator's session parks");

    world
        .restart("next-boot", holding_core(Arc::clone(&holding)))
        .await;
    let output = settled(&world, WEDGED, &run).await;
    served::assert_answered(WEDGED, &output);
    assert_eq!(output.assistant_message(), Some("done"));

    assert_eq!(
        operator.park_reason().await.expect("the park reads"),
        Some(activation_loop),
        "an activation-loop park waits for the operator through a boot"
    );
    assert!(
        operator
            .redrive("operator")
            .await
            .expect("the redrive commits"),
        "the operator redrove the parked session"
    );
    assert_eq!(operator.park_reason().await.expect("the park reads"), None);
    world.shutdown().await;
}

/// The commits of a cell's call that file its attempts and outcome.
const CALL_COMMITS: [CommitLabel; 4] = [
    CommitLabel::ROUND_START,
    CommitLabel::ROUND_RETRY,
    CommitLabel::ROUND_OUTCOME,
    CommitLabel::TOOL_EFFECT,
];

fn flaky_core(flaky: Arc<Flaky>) -> impl Fn(&lash::Backend) -> lash::LashCoreBuilder {
    move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, sim::untimed_workers()),
        )
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::clone(&flaky) as Arc<dyn lash_core::ToolProvider>,
        )
    }
}

/// A-PARKED-TURN-RESUMES-AT-THE-NEXT-BOOT (FIG-6310) for FIG-6289's ordinal
/// miss: a cell's `Repeatable` call failed its first attempt and its retry
/// answered, its rows filed (admit, start, retry, start, outcome), and the
/// cell's wait was never answered, its session parked `PassLoop` with the
/// live park. The next boot redrives it: the cell's wait is answered from
/// the call's admitted identity with the retry's value, the call is not run
/// a third time, and the turn finishes.
async fn a_session_parked_by_the_ordinal_miss_finishes_its_cell_at_the_next_boot(tier: Tier) {
    const NAME: &str = "parked-by-the-ordinal-miss";
    let flaky = Arc::new(Flaky::default());
    let hook = Arc::new(std::sync::OnceLock::<Arc<DurableCommitHook>>::new());
    let scripts = Arc::new(served::Scripts::default());
    let cell = served::cell("await control.finish(await tools.flaky({}));");
    scripts.register(NAME, vec![cell.clone()]);
    let Some(mut world) = World::layered_with_model(
        tier,
        {
            let hook = Arc::clone(&hook);
            move |stores| {
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_durable_store(move |inner| {
                        let hooked = Arc::new(DurableCommitHook::over(inner));
                        let _ = hook.set(Arc::clone(&hooked));
                        hooked
                    })
                    .into_store_set()
            }
        },
        served::model(Arc::clone(&scripts)),
        flaky_core(Arc::clone(&flaky)),
    )
    .await
    else {
        return;
    };
    world.script(NAME, vec![cell]);
    let hook = Arc::clone(hook.get().expect("the durable store is hooked"));
    // Once the retry answered and its call's commits are filed, the cell's
    // next save is held, as the ordinal miss refused it, and the node goes.
    let actor = ActorKey::session(NAME).expect("a session actor key");
    let (reached, at_save) = tokio::sync::oneshot::channel::<CommitLabel>();
    let (keep, held) = tokio::sync::oneshot::channel::<()>();
    let hold = Arc::new(Mutex::new(Some((reached, held))));
    let answered = Arc::clone(&flaky);
    hook.arm(move |tx, label| {
        let held = (tx.actor() == &actor
            && answered.answered.load(Ordering::SeqCst)
            && !CALL_COMMITS.contains(&label))
        .then(|| hold.lock_recover().take())
        .flatten();
        async move {
            if let Some((reached, held)) = held {
                let _ = reached.send(label);
                let _ = held.await;
            }
            Ok(())
        }
    });
    let session = world.session(NAME, served::spec(16)).await;
    let accepted = session
        .send(lash::TurnInput::text(NAME))
        .await
        .expect("the turn's input is accepted");
    tokio::time::timeout(served::WATCHDOG, at_save)
        .await
        .expect("the cell's save after the retry was reached")
        .expect("the held save reached the hook");
    world.core.shutdown().await.expect("the node goes");
    drop((accepted, keep));
    hook.disarm();
    assert_eq!(flaky.attempts.load(Ordering::SeqCst), 2);
    let run = wedge(&world, NAME).await;

    world
        .restart("next-boot", flaky_core(Arc::clone(&flaky)))
        .await;
    let output = settled(&world, NAME, &run).await;
    served::assert_answered(NAME, &output);
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!("attempt 2")),
        "the cell's wait is answered with the retry's value"
    );
    assert_eq!(
        flaky.attempts.load(Ordering::SeqCst),
        2,
        "the answered call is not run a third time"
    );
    world.shutdown().await;
}

tiered_laws!(
    a_live_surface_install_fault_parks_typed_at_the_budget_and_one_that_clears_completes_once,
    a_session_whose_activation_panics_parks_at_its_budget_and_a_cancel_still_ends_its_turn,
    a_cancel_committed_while_a_session_parks_still_ends_its_turn,
    a_turn_wedged_in_its_tools_phase_by_a_pass_loop_park_finishes_at_the_next_boot,
    a_session_parked_by_the_ordinal_miss_finishes_its_cell_at_the_next_boot
);
