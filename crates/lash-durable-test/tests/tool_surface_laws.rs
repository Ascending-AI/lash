//! A session's tool surface as its providers' config makes it, through the
//! facade on a served node (R2, S2, S4).
//!
//! Which tools a run is offered is each provider's own configuration: here
//! a [`lash::tools::SelectedToolsPlugin`] whose namespace holds a
//! [`lash::tools::ToolSelection`], changed for one run by that run's
//! options and sticky by the provider's config command. Each law drives the
//! surface with sends and reads what the model was offered and what the
//! provider executed.
//!
//! - RUN-OPTION-CHANGES-ADVERTISEMENT (law 1): a one-run selection changes
//!   what that run is offered; the next run is unchanged.
//! - PROVIDER-READS-RUN-CONFIG (law 2): under a run's selection,
//!   advertisement and execution agree, and runs either side of it see
//!   their own catalog, never a cached one of another config.
//! - COLLISION-REFUSED-NAMED (law 15): two providers advertising one name
//!   refuse the run, naming both tools and both providers; a config change
//!   that keeps the collision is refused the same way and settles, and
//!   reconfiguring one provider's selection admits the next run.
//! - IDLE-RUN-NEVER-STRANDED (law 13, session arm; S7): a session whose
//!   surface offers a provider a node does not serve, with no unsettled
//!   call to it, is claimed and runs on that node; settling one of two
//!   calls to a provider leaves the provider required.
//! - SAME-SURFACE-DEPLOY-RUNS and SURFACE-CHANGE-OUTCOME (law 9, session
//!   arm; S5): a recorded call runs on a node serving the exact recorded
//!   surface, and parks, typed and with no body run, on a node that serves
//!   the same provider key with the tool changed.
//!
//! The host-start laws at the end run a process start on the same served
//! node.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, Mutex};

use lash_core::llm::types::{LlmRequest, LlmRole};
use lash_core::{ToolDefinitionBindingExt as _, ToolId};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG};

#[derive(Clone, Debug)]
struct Spec {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    /// Whether its input is retyped: a structural change of the tool.
    retyped: bool,
}

const fn spec(id: &'static str, name: &'static str, description: &'static str) -> Spec {
    Spec {
        id,
        name,
        description,
        retyped: false,
    }
}

impl Spec {
    /// This tool with its input retyped.
    fn retyped(self) -> Self {
        Self {
            retyped: true,
            ..self
        }
    }

    fn definition(&self) -> lash_core::ToolDefinition {
        let input = if self.retyped {
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "x": { "type": "string" } }
            })
        } else {
            lash_core::ToolDefinition::default_input_schema()
        };
        lash_core::ToolDefinition::raw(
            self.id,
            self.name,
            self.description,
            input,
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_execution_policy(if self.name.ends_with("_held") {
            // A held call stranded by its node's stop runs again on the
            // next node that serves it.
            lash_core::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(2).expect("two attempts"),
                1,
                1,
            )
        } else {
            lash_core::ExecutionPolicy::Once
        })
    }
}

/// A provider of a fixed set of tools that records every call it executes.
/// A tool whose name ends `_held` is repeatable and never answers, unless
/// the provider is `answering`: its body signals `entered` and waits
/// forever.
#[derive(Default)]
struct Surface {
    tools: Vec<Spec>,
    executed: Mutex<Vec<String>>,
    entered: tokio::sync::Notify,
    answering: bool,
}

impl Surface {
    fn new(tools: Vec<Spec>) -> Arc<Self> {
        Arc::new(Self {
            tools,
            ..Self::default()
        })
    }

    /// A provider of `tools` whose held tools answer at once.
    fn answering(tools: Vec<Spec>) -> Arc<Self> {
        Arc::new(Self {
            tools,
            answering: true,
            ..Self::default()
        })
    }

    fn tool(&self, name: &str) -> Option<&Spec> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    fn executed(&self) -> Vec<String> {
        self.executed.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Surface {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok({
            self.tools
                .iter()
                .map(|tool| tool.definition().manifest())
                .collect()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        self.tool(name)
            .map(|tool| Arc::new(tool.definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let Some(tool) = self.tool(call.name()) else {
            return lash_core::ToolOutcome::err_fmt(format_args!(
                "tool `{}` is not live",
                call.name()
            ))
            .into();
        };
        self.executed.lock_recover().push(tool.id.to_owned());
        if tool.name.ends_with("_held") && !self.answering {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "id": tool.id })).into()
    }
}

/// The tool names of every request the model was asked, in order.
#[derive(Default)]
struct Offered {
    requests: Mutex<Vec<Vec<String>>>,
}

impl Offered {
    fn all(&self) -> Vec<Vec<String>> {
        self.requests.lock_recover().clone()
    }

    fn clear(&self) {
        self.requests.lock_recover().clear();
    }
}

/// The input that asks the model to call `tool` once.
fn call_input(tool: &str) -> String {
    format!("call {tool}")
}

/// The model: it records the tools each request offers; asked by
/// [`call_input`] with no answer yet, it calls that tool (each of the
/// `+`-joined tools, in one round), and otherwise answers `done`.
fn model(offered: &Arc<Offered>) -> lash_core::facade_support::ProviderHandle {
    let offered = Arc::clone(offered);
    lash_core::testing::TestProvider::builder()
        .kind("tool-surface-model")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let offered = Arc::clone(&offered);
            async move {
                offered
                    .requests
                    .lock_recover()
                    .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
                let last = request.messages.last().expect("a request has messages");
                let rendered = serde_json::to_string(last).expect("a message encodes");
                if last.role == LlmRole::User
                    && let Some(at) = rendered.find("call ")
                {
                    let tools = rendered[at + 5..]
                        .split('"')
                        .next()
                        .expect("the input names a tool");
                    return Ok(served::response(
                        tools
                            .split('+')
                            .enumerate()
                            .map(|(index, tool)| {
                                let id = if index == 0 {
                                    "surface-call".to_owned()
                                } else {
                                    format!("surface-call-{index}")
                                };
                                served::call(&id, tool, serde_json::json!({}))
                            })
                            .collect(),
                    ));
                }
                Ok(served::text(&request, "done"))
            }
        })
        .build()
        .into_handle()
}

/// A deployment's stores and the law's model.
struct Deployment {
    backend: lash::Backend,
    offered: Arc<Offered>,
    _keep: served::Keep,
}

impl Deployment {
    async fn new(tier: Tier) -> Option<Self> {
        let Some((stores, keep)) = served::stores(tier).await else {
            eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
            return None;
        };
        Some(Self {
            backend: served::backend(stores),
            offered: Arc::default(),
            _keep: keep,
        })
    }

    /// Start a core over the deployment's stores whose tools are `plugins`'.
    fn boot(&self, plugins: Vec<Arc<dyn lash::plugins::PluginFactory>>) -> lash::LashCore {
        plugins
            .into_iter()
            .fold(
                lash::LashCore::standard_builder(self.backend.clone()),
                lash::LashCoreBuilder::plugin,
            )
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .serve_test_llm_profile(model(&self.offered), served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("tool-surface-deployment"),
                lash::persistence::LeaseIncarnationId::new("tool-surface-boot"),
            ))
            .expect("the core builds")
    }
}

fn session_id(name: &str) -> lash::SessionId {
    lash::SessionId::try_from(name.to_owned()).expect("a session id")
}

async fn create(core: &lash::LashCore, name: &str) -> lash::DurableSession {
    core.session(session_id(name))
        .create(lash::SessionCreation::root(served::spec(64)))
        .await
        .expect("the session is created")
}

/// Send `input` to session `name` on `core`, under `selection` for the
/// owner `owner` when given, and await its settled outcome.
async fn outcome(
    core: &lash::LashCore,
    name: &str,
    input: &str,
    selection: Option<(&'static str, lash::tools::ToolSelection)>,
) -> lash::SendOutcome {
    let session = core
        .session(session_id(name))
        .durable()
        .await
        .expect("the session resolves");
    tokio::time::timeout(WATCHDOG, async {
        let mut send = session.send(lash::TurnInput::text(input));
        if let Some((owner, tools)) = selection {
            send = send.options(
                lash::RunOptionsKey::<lash::tools::ToolSelectionConfig>::new(owner),
                lash::tools::ToolSelectionConfig { tools: Some(tools) },
            );
        }
        send.await?.outcome().await
    })
    .await
    .unwrap_or_else(|_| panic!("deadlock watchdog: `{input}` never settled"))
    .expect("the send settles")
}

/// Send `input` like [`outcome`]; the turn answers.
async fn send(
    core: &lash::LashCore,
    name: &str,
    input: &str,
    selection: Option<(&'static str, lash::tools::ToolSelection)>,
) -> lash::TurnOutput {
    match outcome(core, name, input, selection).await {
        lash::SendOutcome::Settled { output, .. } => {
            served::assert_answered(input, &output);
            *output
        }
        other => panic!("`{input}` must settle a turn: {other:?}"),
    }
}

/// Apply `tools` as the sticky selection of the owner `owner` of session
/// `name`, through that provider's config command.
async fn select(
    core: &lash::LashCore,
    name: &str,
    owner: &str,
    tools: lash::tools::ToolSelection,
) -> lash::config::ConfigTransactionOutcome {
    let session = core
        .session(session_id(name))
        .open()
        .await
        .expect("the session opens");
    let config = session.admin().config();
    let revision = config.revision().await.unwrap();
    let written = config
        .apply(
            lash::config::ConfigWrite::new(format!("select-{owner}-{revision}"), revision),
            lash::config::ConfigTransaction::new()
                .then_entry(lash::tools::SetToolSelection { tools }.to(owner)),
        )
        .await
        .expect("the config write is accepted");
    let outcome = written.await_outcome(&config);
    tokio::time::timeout(WATCHDOG, outcome)
        .await
        .unwrap_or_else(|_| panic!("deadlock watchdog: the selection of `{owner}` never settled"))
        .expect("the config write settles")
}

const SURFACE: &str = "surface-tools";

fn alpha() -> Spec {
    spec("tool:surface_alpha", "surface_alpha", "the first tool")
}

fn beta() -> Spec {
    spec("tool:surface_beta", "surface_beta", "the second tool")
}

fn only(tools: &[Spec]) -> lash::tools::ToolSelection {
    lash::tools::ToolSelection::Only(tools.iter().map(|tool| ToolId::from(tool.id)).collect())
}

/// The tool names a request offered, without the protocol's own.
fn host_tools(offered: &[String]) -> Vec<String> {
    let mut names = offered
        .iter()
        .filter(|name| name.starts_with("surface_") || name.starts_with("collide_"))
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// RUN-OPTION-CHANGES-ADVERTISEMENT (law 1; R2, S2): a run that states a
/// selection for the provider is offered that selection alone; the next
/// run, stating none, is offered the provider's recorded selection again.
async fn a_run_option_changes_what_that_run_alone_is_offered(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let surface = Surface::new(vec![alpha(), beta()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        surface as _,
    ))]);
    create(&core, "run-option").await;
    send(
        &core,
        "run-option",
        "narrowed",
        Some((SURFACE, only(&[alpha()]))),
    )
    .await;
    send(&core, "run-option", "unchanged", None).await;
    let offered = deployment.offered.all();
    assert_eq!(offered.len(), 2, "{offered:?}");
    assert_eq!(host_tools(&offered[0]), ["surface_alpha"], "{offered:?}");
    assert_eq!(
        host_tools(&offered[1]),
        ["surface_alpha", "surface_beta"],
        "the next run is offered the recorded selection: {offered:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// PROVIDER-READS-RUN-CONFIG (law 2; S2): the provider reads each run's own
/// config. A run before and a run after the narrowed one are offered every
/// tool; the narrowed run is offered alpha alone, its call to alpha runs,
/// and its call to beta runs no body: what is offered is what executes.
async fn advertisement_and_execution_read_the_same_run_config(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let surface = Surface::new(vec![alpha(), beta()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    create(&core, "run-config").await;
    send(&core, "run-config", "before", None).await;
    let narrowed = Some((SURFACE, only(&[alpha()])));
    send(
        &core,
        "run-config",
        &call_input("surface_alpha"),
        narrowed.clone(),
    )
    .await;
    let before_beta = surface.executed();
    let refused = outcome(&core, "run-config", &call_input("surface_beta"), narrowed).await;
    assert!(
        matches!(refused, lash::SendOutcome::Settled { .. }),
        "a call outside the run's selection is the model's error, not the turn's: {refused:?}"
    );
    send(&core, "run-config", "after", None).await;

    assert_eq!(
        before_beta,
        [alpha().id],
        "the narrowed run's alpha call ran its body"
    );
    assert_eq!(
        surface.executed(),
        [alpha().id],
        "the narrowed run's beta call ran no body"
    );
    let offered = deployment.offered.all();
    let both = ["surface_alpha", "surface_beta"];
    assert_eq!(host_tools(&offered[0]), both, "{offered:?}");
    for request in &offered[1..offered.len() - 1] {
        assert_eq!(host_tools(request), ["surface_alpha"], "{offered:?}");
    }
    assert_eq!(
        host_tools(offered.last().expect("the last run asked")),
        both,
        "the run after sees its own catalog, not the narrowed one: {offered:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

const LEFT: &str = "collide-left";
const RIGHT: &str = "collide-right";

/// COLLISION-REFUSED-NAMED (law 15; S4): two providers that advertise one
/// model-facing name refuse the run, naming both tool ids and both provider
/// keys. A config change that keeps the collision settles refused, typed
/// and naming both, rather than retrying as a fault; the host then
/// reconfigures one provider's selection, and the next run is offered the
/// other's tool and calls it.
async fn a_name_collision_is_refused_naming_both_and_a_selection_admits(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let left = Surface::new(vec![spec("tool:collide_left", "collide_search", "left")]);
    let right = Surface::new(vec![spec("tool:collide_right", "collide_search", "right")]);
    let core = deployment.boot(vec![
        Arc::new(lash::tools::SelectedToolsPlugin::new(
            LEFT,
            Arc::clone(&left) as _,
        )),
        Arc::new(lash::tools::SelectedToolsPlugin::new(
            RIGHT,
            Arc::clone(&right) as _,
        )),
    ]);
    create(&core, "collision").await;
    let refused = outcome(&core, "collision", "collide", None).await;
    let rendered = format!("{refused:?}");
    assert!(
        !matches!(&refused, lash::SendOutcome::Settled { output, .. } if output.is_success()),
        "a colliding surface refuses the run: {rendered}"
    );
    for named in ["tool:collide_left", "tool:collide_right", LEFT, RIGHT] {
        assert!(
            rendered.contains(named),
            "the refusal names `{named}`: {rendered}"
        );
    }
    assert!(
        deployment.offered.all().is_empty(),
        "no model is asked under a colliding surface"
    );

    // A change that keeps the collision is the candidate's typed refusal,
    // naming both tools; it settles and publishes nothing.
    let kept = select(
        &core,
        "collision",
        LEFT,
        lash::tools::ToolSelection::AllExcept(vec![ToolId::from("tool:collide_other")]),
    )
    .await;
    let rendered = format!("{kept:?}");
    assert!(
        matches!(kept, lash::config::ConfigTransactionOutcome::Refused { .. }),
        "a change that keeps the collision is refused: {rendered}"
    );
    for named in ["tool:collide_left", "tool:collide_right"] {
        assert!(
            rendered.contains(named),
            "the refusal names `{named}`: {rendered}"
        );
    }
    let admitted = select(
        &core,
        "collision",
        RIGHT,
        lash::tools::ToolSelection::AllExcept(vec![ToolId::from("tool:collide_right")]),
    )
    .await;
    assert!(
        matches!(
            admitted,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{admitted:?}"
    );
    deployment.offered.clear();
    send(&core, "collision", &call_input("collide_search"), None).await;
    assert_eq!(left.executed(), ["tool:collide_left"]);
    assert!(right.executed().is_empty());
    core.shutdown().await.expect("the core shuts down");
}

const OTHER: &str = "other-tools";

fn quick() -> Spec {
    spec("tool:surface_quick", "surface_quick", "answers at once")
}

fn held() -> Spec {
    spec("tool:surface_held", "surface_held", "never answers")
}

/// The obligation token of the tool provider `owner` declares under
/// `tools`, as a session requires it (S7).
fn provider_token(owner: &str) -> String {
    lash_core::ObligationToken::for_callback(
        lash_core::RegistryKind::Session,
        &lash_core::plugin::ToolProviderHandle::new(
            lash_core::plugin::RegistryKind::Session,
            owner,
            lash_core::tool_provider_key!("tools"),
        )
        .callback_identity(),
    )
}

/// IDLE-RUN-NEVER-STRANDED (law 13, session arm; S7): what a session
/// offers adds no claim requirement. A session offered two providers'
/// tools, which called only one of them, runs its next turn on a node that
/// does not register the other.
async fn a_session_offered_an_unserved_provider_it_never_called_runs(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let surface = Surface::new(vec![alpha()]);
    let other = Surface::new(vec![beta()]);
    let both = deployment.boot(vec![
        Arc::new(lash::tools::SelectedToolsPlugin::new(
            SURFACE,
            Arc::clone(&surface) as _,
        )),
        Arc::new(lash::tools::SelectedToolsPlugin::new(
            OTHER,
            Arc::clone(&other) as _,
        )),
    ]);
    create(&both, "idle-unserved").await;
    send(&both, "idle-unserved", &call_input("surface_alpha"), None).await;
    both.shutdown().await.expect("the core shuts down");
    let offered = deployment.offered.all();
    assert_eq!(
        host_tools(&offered[0]),
        ["surface_alpha", "surface_beta"],
        "the session was offered both providers' tools: {offered:?}"
    );

    let narrow = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    send(&narrow, "idle-unserved", &call_input("surface_alpha"), None).await;
    assert_eq!(surface.executed(), [alpha().id, alpha().id]);
    assert!(other.executed().is_empty());
    narrow.shutdown().await.expect("the core shuts down");
}

/// What the session actor `name` carried as its claim requirements, read
/// by a probe node that serves `serves` and claims it once released.
async fn claimed_requirements(
    backend: &lash::Backend,
    name: &str,
    serves: Vec<String>,
) -> Vec<String> {
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec, Release};
    let store = backend.durable();
    let served = serves.clone();
    let probe = store
        .register_node(&NodeSpec {
            node: NodeId::new("law-requirement-probe"),
            decodes: backend.formats().session_decodes(),
            serves,
            ttl_millis: 15_000,
        })
        .await
        .expect("register the probe node");
    let actor = ActorKey::session(name).expect("a session actor key");
    let snapshot = store.actor(&actor).await.expect("read the actor");
    let claimed = store.claim(&probe, 64).await.expect("claim");
    let mut requires = None;
    for claim in claimed {
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
        .release_node(&probe)
        .await
        .expect("release the probe node");
    requires.unwrap_or_else(|| {
        panic!("the stopped node released `{name}` ready: {snapshot:?}; probe served {served:?}")
    })
}

/// IDLE-RUN-NEVER-STRANDED (law 13, session arm; S7): settling one of two
/// calls to a provider leaves the provider required. A round calls the
/// provider twice; the quick call settles and the held one does not when
/// the node stops, and the session still requires the provider's token.
async fn settling_one_of_two_calls_to_a_provider_leaves_it_required(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let surface = Surface::new(vec![quick(), held()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    create(&core, "one-of-two").await;
    let session = core
        .session(session_id("one-of-two"))
        .durable()
        .await
        .expect("the session resolves");
    let handle = session
        .send(lash::TurnInput::text(call_input(
            "surface_quick+surface_held",
        )))
        .await
        .expect("the send is accepted");
    tokio::time::timeout(WATCHDOG, surface.entered.notified())
        .await
        .expect("the held call starts");
    // The quick call's outcome commits within the group-commit window
    // once its body answers; the held call never answers.
    tokio::time::timeout(WATCHDOG, async {
        while surface.executed().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both bodies ran");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    drop((handle, session));
    let serves = lash::testing::node_serves(&core);
    core.shutdown().await.expect("the core shuts down");
    let mut executed = surface.executed();
    executed.sort();
    assert_eq!(executed, [held().id, quick().id]);
    let requires = claimed_requirements(&deployment.backend, "one-of-two", serves).await;
    assert!(
        requires.contains(&provider_token(SURFACE)),
        "the unsettled held call keeps its provider required: {requires:?}"
    );
}

/// A turn of session `name`, its call to [`held`] in flight, stranded by
/// its node's stop on `deployment`'s stores.
async fn strand_held_call(deployment: &Deployment, name: &str) {
    let surface = Surface::new(vec![quick(), held()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    create(&core, name).await;
    let session = core
        .session(session_id(name))
        .durable()
        .await
        .expect("the session resolves");
    let handle = session
        .send(lash::TurnInput::text(call_input("surface_held")))
        .id(lash::TurnId::parse(name).expect("a turn id"))
        .await
        .expect("the send is accepted");
    tokio::time::timeout(WATCHDOG, surface.entered.notified())
        .await
        .expect("the held call starts");
    drop((handle, session));
    core.shutdown().await.expect("the core shuts down");
}

/// SAME-SURFACE-DEPLOY-RUNS (law 9, session arm; S5, §5a): a node serving
/// the exact surface the run recorded runs the recorded call.
async fn a_recorded_call_runs_on_a_node_serving_the_same_surface(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    strand_held_call(&deployment, "same-surface").await;
    let surface = Surface::answering(vec![quick(), held()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    let session = core
        .session(session_id("same-surface"))
        .durable()
        .await
        .expect("the session resolves");
    let output = tokio::time::timeout(
        WATCHDOG,
        session
            .attach_id(lash::TurnId::parse("same-surface").expect("a turn id"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the resumed turn never settled")
    .expect("the resumed turn settles");
    served::assert_answered("same-surface", &output);
    assert_eq!(
        surface.executed(),
        [held().id],
        "the recorded call ran here"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// SURFACE-CHANGE-OUTCOME (law 9, session arm; S5, §5a): a node serving the
/// same provider key with the recorded tool retyped parks the recorded
/// call, typed, and runs no body. The law asserts this node's outcome
/// alone.
async fn a_recorded_call_parks_on_a_node_serving_a_changed_tool(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    strand_held_call(&deployment, "changed-surface").await;
    let surface = Surface::answering(vec![quick(), held().retyped()]);
    let core = deployment.boot(vec![Arc::new(lash::tools::SelectedToolsPlugin::new(
        SURFACE,
        Arc::clone(&surface) as _,
    ))]);
    let session = core
        .session(session_id("changed-surface"))
        .durable()
        .await
        .expect("the session resolves");
    let park = tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Some(park) = session.park_reason().await.expect("read the park") {
                return park;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("deadlock watchdog: the drifted call never parked");
    let rendered = format!("{park:?}");
    assert!(
        rendered.contains("LashVmCellBindingDrift") && rendered.contains(held().id),
        "the session parks on the drifted call, typed: {rendered}"
    );
    assert!(
        surface.executed().is_empty(),
        "no body ran for the drifted call"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A cell provider that serves its retained match by id, including when
/// the tool is deferred and absent from its advertisement.
struct CellSurface {
    surface: Arc<Surface>,
    deferred: bool,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CellSurface {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok({
            if self.deferred {
                Vec::new()
            } else {
                self.surface
                    .tools
                    .iter()
                    .map(|tool| cell_definition(tool).manifest())
                    .collect()
            }
        })
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_core::ToolManifest> {
        self.surface
            .tools
            .iter()
            .find(|tool| tool.id == id.as_str())
            .map(|tool| cell_definition(tool).manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        self.surface
            .tool(name)
            .map(|tool| Arc::new(cell_definition(tool).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.surface.execute(call).await
    }
}

fn cell_definition(tool: &Spec) -> lash_core::ToolDefinition {
    tool.definition()
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], tool.name))
}

#[derive(Default)]
struct CellResolver {
    resolved: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CellResolver {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::err_fmt("the resolver executes no tool").into()
    }
}

#[async_trait::async_trait]
impl lash::tools::DeferredToolProvider for CellResolver {
    async fn resolve(
        &self,
        _cx: &lash::tools::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, lash::tools::DeferredOutcome> {
        self.resolved
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        paths
            .iter()
            .map(|path| {
                (
                    (*path).to_owned(),
                    if *path == "tools.surface_held" {
                        lash::tools::DeferredOutcome::Resolved {
                            provider: lash::plugins::ToolProviderHandle::new(
                                lash::plugins::RegistryKind::Session,
                                SURFACE,
                                lash::tool_provider_key!("tools"),
                            ),
                            definition: cell_definition(&held()),
                            binding: None,
                        }
                    } else {
                        lash::tools::DeferredOutcome::Unavailable { note: None }
                    },
                )
            })
            .collect()
    }
}

impl Deployment {
    fn boot_cell(
        &self,
        surface: &Arc<Surface>,
        resolver: Option<&Arc<CellResolver>>,
        prepared: Option<Arc<tokio::sync::Notify>>,
    ) -> lash::LashCore {
        let model = lash_core::testing::TestProvider::builder()
            .kind("cell-surface-model")
            .requires_streaming(true)
            .complete(move |request: LlmRequest| {
                let prepared = prepared.clone();
                async move {
                if let Some(prepared) = prepared {
                    prepared.notify_one();
                    std::future::pending::<()>().await;
                }
                let answered = request.messages.iter().any(|message| message.role == LlmRole::Assistant);
                Ok(served::text(&request, if answered { "done" } else {
                    "<typescript>\nconsole.log(await tools.surface_held({}));\n</typescript>"
                }))
                }
            })
            .build().into_handle();
        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        );
        let tools = Arc::new(CellSurface {
            surface: Arc::clone(surface),
            deferred: resolver.is_some(),
        });
        let mut spec = lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), tools);
        if let Some(resolver) = resolver {
            spec = spec.with_deferred_provider(
                lash::tool_provider_key!("deferred"),
                Arc::clone(resolver) as _,
            );
        }
        lash::LashCore::rlm_builder(self.backend.clone(), factory)
            .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
                lash::plugins::PluginDeclaration::initial(SURFACE),
                spec,
            )))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .serve_test_llm_profile(model, served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("cell-surface-deployment"),
                lash::persistence::LeaseIncarnationId::new("cell-surface-boot"),
            ))
            .expect("the cell core builds")
    }
}

async fn cell_starts(deployment: &Deployment, name: &str) -> usize {
    let roots = deployment
        .backend
        .durable()
        .cell_roots(
            &session_id(name),
            &lash::TurnId::parse(name).expect("a turn id"),
        )
        .await
        .expect("read the cell roots");
    let mut starts = 0;
    for root in roots {
        starts += deployment
            .backend
            .durable()
            .run_facts(&root.exec.owner())
            .await
            .expect("read the call ledger")
            .rows
            .iter()
            .filter(|row| row.kind == lash_durable::domain::RunFactKind::XStart)
            .count();
    }
    starts
}

/// SURFACE-CHANGE-OUTCOME (law 9, cell arm; S5, §5a, R-3): an open
/// catalog call or retained deferred match parks on a build that retyped
/// its tool, before another x_start and without entering the new provider.
async fn a_recorded_cell_call_parks_on_a_node_serving_a_changed_tool(tier: Tier) {
    for (deferred, admitted) in [(false, false), (false, true), (true, true)] {
        let Some(deployment) = Deployment::new(tier).await else {
            return;
        };
        let name = if deferred {
            "changed-deferred-cell"
        } else if admitted {
            "changed-catalog-cell"
        } else {
            "changed-new-cell"
        };
        let resolver = deferred.then(|| Arc::new(CellResolver::default()));
        let original = Surface::new(vec![held()]);
        let prepared = (!admitted).then(|| Arc::new(tokio::sync::Notify::new()));
        let core = deployment.boot_cell(&original, resolver.as_ref(), prepared.clone());
        let session = create(&core, name).await;
        let handle = session
            .send(lash::TurnInput::text("call the held tool"))
            .id(lash::TurnId::parse(name).expect("a turn id"))
            .await
            .expect("the send is accepted");
        tokio::time::timeout(WATCHDOG, async {
            tokio::select! {
                () = async {
                    if let Some(prepared) = prepared {
                        prepared.notified().await;
                    } else {
                        original.entered.notified().await;
                    }
                } => (),
                outcome = handle.outcome() => panic!("the original call must start, deferred={deferred}: {outcome:?}"),
            }
        }).await.expect("the original call starts");
        drop(session);
        core.shutdown().await.expect("the original node stops");
        let starts = cell_starts(&deployment, name).await;
        assert_eq!(
            starts,
            usize::from(admitted),
            "only an admitted original call has an x_start"
        );

        let changed = Surface::answering(vec![held().retyped()]);
        let core = deployment.boot_cell(&changed, resolver.as_ref(), None);
        let session = core
            .session(session_id(name))
            .durable()
            .await
            .expect("the session resolves");
        let outcome = tokio::time::timeout(
            WATCHDOG,
            session
                .attach_id(lash::TurnId::parse(name).expect("a turn id"))
                .outcome(),
        )
        .await
        .expect("the resumed cell answers or parks")
        .expect("read the cell outcome");
        assert!(
            matches!(outcome, lash::SendOutcome::Parked { .. }),
            "a changed cell tool must park, deferred={deferred}: {outcome:?}"
        );
        let park = session
            .park_reason()
            .await
            .expect("read the park")
            .expect("the cell is parked");
        let rendered = format!("{park:?}");
        assert!(
            rendered.contains("LashVmCellBindingDrift") && rendered.contains(held().id),
            "the cell parks typed on its retained tool: {rendered}"
        );
        assert!(
            changed.executed().is_empty(),
            "the changed provider never runs"
        );
        assert_eq!(
            cell_starts(&deployment, name).await,
            starts,
            "drift precedes any new x_start"
        );
        if let Some(resolver) = resolver {
            assert_eq!(
                resolver.resolved.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "the deferred match is retained, never resolved again"
            );
        }
        core.shutdown().await.expect("the changed node stops");
    }
}

/// The environment a host start states in these laws: no tools and no
/// components.
fn process_environment() -> lash_core::ProcessEnv {
    lash_core_execution::testing::process_env_with_limit(16)
}

/// A host start observes only the sessions it names: a default start
/// gains no observer edge, and a session named as an observer does (ported from the start half of
/// `session_creation_applies_only_named_process_observers_with_typed_outcomes`).
async fn a_host_start_observes_only_the_sessions_it_names(tier: Tier) {
    let Some(world) = served::World::with_engines(
        tier,
        vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)],
        |backend| lash::LashCore::standard_builder(backend.clone()),
    )
    .await
    else {
        return;
    };
    let parent = world.session("observer-parent", served::spec(8)).await;
    let parent_id = parent.session_id().clone();
    let env_ref = world
        .core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &process_environment())
        .await
        .expect("publish the environment");
    let mut started = std::collections::BTreeMap::new();
    for (label, observers) in [
        ("default-start", Vec::new()),
        ("explicit-start", vec![parent_id.clone()]),
    ] {
        let mut request = lash_core::ProcessStartRequest::engine(
            lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
            serde_json::json!({ "start": label }),
            lash_core::EngineLaunch::new(env_ref.clone(), None),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_host_start_key(label);
        request.observers = observers;
        let receipt = world
            .core
            .processes()
            .start(request, world.core.effect_host())
            .await
            .expect("the host start registers");
        started.insert(label, receipt.process_id);
    }
    let registry = world.backend.process_registry();
    assert!(
        !registry
            .is_observer(&parent_id, &started["default-start"])
            .await
            .expect("read the default start's observers"),
        "a default start gains no observer edge"
    );
    assert!(
        registry
            .is_observer(&parent_id, &started["explicit-start"])
            .await
            .expect("read the explicit start's observers"),
        "the named initial observer receives its edge"
    );
    world.shutdown().await;
}

/// FIG-5412 / ADR 0107: losing the host's acknowledgement does not lose
/// the start's identity. A retained host key fences the request through
/// terminal settlement, and every retry leaves exactly one process.
async fn a_lost_host_start_acknowledgement_recovers_the_retained_process(tier: Tier) {
    let Some(world) = served::World::with_engines(
        tier,
        vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)],
        |backend| lash::LashCore::standard_builder(backend.clone()),
    )
    .await
    else {
        return;
    };
    tokio::time::timeout(WATCHDOG, async {
        let env_ref = world
            .core
            .host_artifacts()
            .publish_process_env(&lash_core::HostArtifactPin::mint(), &process_environment())
            .await
            .expect("publish the environment");
        let request = lash_core::ProcessStartRequest::engine(
            lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
            serde_json::json!({ "work": 1 }),
            lash_core::EngineLaunch::new(env_ref, None),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_host_start_key("lost-host-start-ack");
        // The host loses its acknowledgement; only the database keeps the id.
        let _ = world
            .core
            .processes()
            .start(request.clone(), world.core.effect_host())
            .await
            .expect("the first start commits");
        let filter = lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..Default::default()
        };
        let retained = world
            .core
            .processes()
            .list(
                &filter,
                std::num::NonZeroUsize::new(2).expect("two detects a duplicate start"),
                None,
            )
            .await
            .expect("read the committed process")
            .processes;
        assert_eq!(retained.len(), 1);
        let original_id = retained[0].process_id.clone();

        for terminal in [false, true] {
            if terminal {
                world
                    .core
                    .processes()
                    .cancel(&original_id, world.core.effect_host())
                    .await
                    .expect("end the retained process");
                world
                    .core
                    .processes()
                    .await_output(&original_id)
                    .await
                    .expect("the process settles");
            }
            let retry = world
                .core
                .processes()
                .start(request.clone(), world.core.effect_host())
                .await
                .expect("the same host request recovers its id");
            assert_eq!(retry.process_id, original_id);
            let mut changed = request.clone();
            let lash_core::ProcessStartTarget::Input(lash_core::ProcessInput::Engine {
                payload,
                ..
            }) = &mut changed.input
            else {
                unreachable!("the request starts an engine")
            };
            *payload = serde_json::json!({ "work": 2 });
            let error = world
                .core
                .processes()
                .start(changed, world.core.effect_host())
                .await
                .expect_err("a different request under the retained key conflicts");
            assert!(matches!(
                error,
                lash::EmbedError::Plugin(lash_core::PluginError::StartKeyConflict { start_key })
                    if Some(&start_key) == request.start_key()
            ));
            let processes = world
                .core
                .processes()
                .list(
                    &filter,
                    std::num::NonZeroUsize::new(2).expect("two detects a duplicate start"),
                    None,
                )
                .await
                .expect("read processes after both retries")
                .processes;
            assert_eq!(processes.len(), 1, "no retry creates a second process");
            assert_eq!(processes[0].process_id, original_id);
        }
    })
    .await
    .expect("the host start and retries settle before the watchdog");
    world.shutdown().await;
}

tiered_laws!(
    a_run_option_changes_what_that_run_alone_is_offered,
    advertisement_and_execution_read_the_same_run_config,
    a_name_collision_is_refused_naming_both_and_a_selection_admits,
    a_session_offered_an_unserved_provider_it_never_called_runs,
    settling_one_of_two_calls_to_a_provider_leaves_it_required,
    a_recorded_call_runs_on_a_node_serving_the_same_surface,
    a_recorded_call_parks_on_a_node_serving_a_changed_tool,
    a_recorded_cell_call_parks_on_a_node_serving_a_changed_tool,
    a_host_start_observes_only_the_sessions_it_names,
    a_lost_host_start_acknowledgement_recovers_the_retained_process,
);
