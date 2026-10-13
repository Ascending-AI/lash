//! OBLIGATIONS-COVER-CALLBACKS (law 19; §5a): a parked member's recorded
//! presentation plan is a claim obligation. A call parks under a plan that
//! names a presentation step of a plugin with no tool provider; while it is
//! parked the session requires that step's token, so a node that does not
//! register the step never claims it to run. The node that does presents
//! the call's final answer under the recorded plan, not the steps it has
//! registered since, and once the presentation and its turn commit the
//! session requires the step no longer.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt as _;

use super::served::{self, Tier, WATCHDOG};

/// The input that starts the law's turn.
const TURN: &str = "obligation-turn";
/// The turn's id, which a later deployment attaches to.
const RUN: &str = "obligation-run";
const DEFERRED: &str = "obligation_deferred";
/// The plugin whose presentation step the parked call's plan records. It
/// registers no tool provider.
const PRESENTER: &str = "obligation-law-presenter";
const STEP: &str = "obligation-law-present";
/// The plugin whose step a later deployment adds.
const LATE_PRESENTER: &str = "obligation-law-late";
const PRESENTED: &str = "presented-under-the-recorded-plan";
const LATE: &str = "presented-by-a-later-step";

fn definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{DEFERRED}"),
        DEFERRED,
        "Answers once its completion wait resolves.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(
        lash_core::ToolDeclaration::deferring(),
        Some(lash_core::ParkBound::Within(
            std::time::Duration::from_secs(120),
        )),
    )
    .expect("a deferring tool declares its park bound")
}

/// A provider of one deferring tool: its call parks, and the key of the
/// completion wait it parks on is kept for the law to resolve.
#[derive(Default)]
struct Deferring {
    key: Mutex<Option<String>>,
    parked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Deferring {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == DEFERRED).then(|| Arc::new(definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let key = call
            .context
            .completion_key()
            .expect("a deferring call's round pins its completion wait");
        *self.key.lock_recover() = Some(key.as_str().to_owned());
        self.parked.notify_one();
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new())
    }
}

/// A plugin, `owner`, whose one presentation step appends `text` to every
/// call's model-facing return; it registers no tool provider.
fn presenter(owner: &'static str, text: &'static str) -> Arc<dyn lash::plugins::PluginFactory> {
    let step: lash_core::plugin::ToolPresentationStep = Arc::new(move |input| {
        Box::pin(async move {
            let mut presented = input.previous;
            presented
                .parts
                .push(lash_core::facade_support::ModelToolReturnPart::text(text));
            Ok(presented)
        })
    });
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(owner),
        lash_core::facade_support::PluginSpec::new()
            .with_presentation_step(lash_core::hook_key!("obligation-law-present"), step),
    ))
}

/// The obligation token of [`PRESENTER`]'s step, as a session requires it.
fn step_token() -> String {
    lash_core::ObligationToken::for_callback(
        lash_core::RegistryKind::Session,
        &lash_core::plugin::PluginCallbackIdentity {
            owner: PRESENTER.to_owned(),
            key: format!("presentation_step:{STEP}"),
        },
    )
    .to_string()
}

/// The claim requirements session `name` carries, as a probe node serving
/// `serves` reads them when it claims the session; `None` when it cannot
/// claim it. A claimed session is released ready, untouched.
async fn probe(backend: &lash::Backend, name: &str, serves: Vec<String>) -> Option<Vec<String>> {
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec, Release};
    let store = backend.durable();
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("obligation-law-probe"),
            decodes: backend.formats().session_decodes(),
            serves,
            ttl_millis: 15_000,
        })
        .await
        .expect("register the probe node");
    let actor = ActorKey::session(name).expect("a session actor key");
    let mut requires = None;
    for claim in store.claim(&node, 64).await.expect("claim") {
        if claim.actor == actor {
            requires = Some(claim.requires.clone());
        }
        let mut tx = store
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("open the probe's transaction");
        tx.give_up(Release::Ready);
        store
            .commit(tx, CommitLabel::new("law.probe_requires"))
            .await
            .expect("release the probed actor");
    }
    store
        .release_node(&node)
        .await
        .expect("release the probe node");
    requires
}

/// The core over `backend` that serves the law's tool and `plugins`.
fn deployment(
    backend: &lash::Backend,
    tools: &Arc<Deferring>,
    plugins: Vec<Arc<dyn lash::plugins::PluginFactory>>,
) -> lash::LashCoreBuilder {
    plugins.into_iter().fold(
        lash::LashCore::standard_builder(backend.clone())
            .tools(lash::tool_provider_key!("tools"), Arc::clone(tools) as _),
        lash::LashCoreBuilder::plugin,
    )
}

pub(super) async fn a_parked_members_recorded_presentation_plan_is_required_until_it_presents(
    tier: Tier,
) {
    let tools = Arc::new(Deferring::default());
    let built = Arc::clone(&tools);
    let Some(mut world) = served::World::new(tier, move |backend| {
        deployment(backend, &built, vec![presenter(PRESENTER, PRESENTED)])
    })
    .await
    else {
        return;
    };
    world.script(
        TURN,
        vec![served::response(vec![served::call(
            "obligation-call",
            DEFERRED,
            serde_json::json!({}),
        )])],
    );
    let session = world.session("obligation-session", served::spec(8)).await;
    let handle = session
        .send(lash::TurnInput::text(TURN))
        .id(lash::TurnId::parse(RUN).expect("a turn id"))
        .await
        .expect("the turn is accepted");
    tokio::time::timeout(WATCHDOG, tools.parked.notified())
        .await
        .expect("the call parks");
    // The park commits with the member's outcome window; the actor is
    // released waiting on the call's completion.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let serves = lash::testing::node_serves(&world.core);
    drop((handle, session));
    world.core.shutdown().await.expect("the parking core stops");

    let key = tools.key.lock_recover().clone().expect("the park's key");
    lash_core::waits::resolve_host(
        &world.backend,
        &key,
        lash_core::Resolution::Ok(serde_json::json!({ "answered": "late" })),
    )
    .await
    .expect("the parked call's wait resolves");
    assert!(
        serves.contains(&step_token()),
        "the parking node served the step"
    );
    let without_step = serves
        .iter()
        .filter(|token| **token != step_token())
        .cloned()
        .collect();
    assert_eq!(
        probe(&world.backend, "obligation-session", without_step).await,
        None,
        "a node without the recorded step does not claim the parked member's session"
    );
    let requires = probe(&world.backend, "obligation-session", serves.clone())
        .await
        .expect("a node serving the step claims the session");
    assert!(
        requires.contains(&step_token()),
        "the parked member's recorded plan is required: {requires:?}"
    );

    let built = Arc::clone(&tools);
    world
        .restart("obligation-presenting-boot", move |backend| {
            deployment(
                backend,
                &built,
                vec![
                    presenter(PRESENTER, PRESENTED),
                    presenter(LATE_PRESENTER, LATE),
                ],
            )
        })
        .await;
    let session = world
        .core
        .session(lash::SessionId::try_from("obligation-session".to_owned()).expect("an id"))
        .durable()
        .await
        .expect("the session resolves");
    let output = tokio::time::timeout(
        WATCHDOG,
        session
            .attach_id(lash::TurnId::parse(RUN).expect("a turn id"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the resumed turn never settled")
    .expect("the resumed turn settles");
    served::assert_answered(TURN, &output);
    let requests = world.requests(TURN);
    let presented = requests.last().expect("the model saw the presented answer");
    assert!(
        presented.contains(PRESENTED),
        "the recorded step presented the answer: {presented}"
    );
    assert!(
        !presented.contains(LATE),
        "a step registered after the park is not in the recorded plan: {presented}"
    );
    let serves = lash::testing::node_serves(&world.core);
    world
        .core
        .shutdown()
        .await
        .expect("the presenting core stops");
    let without_step = serves
        .into_iter()
        .filter(|token| *token != step_token())
        .collect();
    let requires = probe(&world.backend, "obligation-session", without_step)
        .await
        .expect("a node without the step claims the session once it presented");
    assert!(
        !requires.contains(&step_token()),
        "a presented member requires its plan no longer: {requires:?}"
    );
    world.shutdown().await;
}
