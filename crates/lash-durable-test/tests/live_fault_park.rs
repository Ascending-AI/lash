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

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::llm::types::LlmRequest;

use served::{Tier, World};

const PLUGIN: &str = "live-fault-catalog";
const FAULT: &str = "park-fault: the contributor's store did not answer";

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

impl lash::plugins::PluginDefinition for FaultingCatalog {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for FaultingCatalog {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for FaultingCatalog {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let faults = Arc::clone(&self.faults);
        reg.tool_catalog().contribute(
            lash::hook_key!("faulting-catalog"),
            Arc::new(move |_| {
                faults.run()?;
                Ok(Default::default())
            }),
        )
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
        .result
        .cancellation()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.result.outcome));
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

tiered_laws!(
    a_live_surface_install_fault_parks_typed_at_the_budget_and_one_that_clears_completes_once,
    a_session_whose_activation_panics_parks_at_its_budget_and_a_cancel_still_ends_its_turn
);
