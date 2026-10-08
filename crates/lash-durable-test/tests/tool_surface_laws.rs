//! A session's tool surface across turns, host edits and core restarts,
//! through the facade on a served node (FIG-5310, ported from the deleted
//! laws of lash-core's `runtime/tests/tool_surface_lifecycle.rs` and
//! `runtime/tests/tool_restore_report.rs`).
//!
//! A session's capabilities exist only inside a run: each law drives the
//! surface with sends and host admin commands, reads what the model was
//! offered, and reads the tool state the durable head recorded. A restart is
//! a new core over the same stores, with the live tool source the law gives
//! it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, Mutex};

use lash_core::ToolId;
use lash_core::facade_support::ToolStateFacadeOps as _;
use lash_core::llm::types::{LlmRequest, LlmRole};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG};

#[derive(Clone, Debug)]
struct Spec {
    id: &'static str,
    name: &'static str,
    description: &'static str,
}

const fn spec(id: &'static str, name: &'static str, description: &'static str) -> Spec {
    Spec {
        id,
        name,
        description,
    }
}

impl Spec {
    fn definition(&self) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            self.id,
            self.name,
            self.description,
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
    }
}

/// A tool source whose advertised set a law changes between turns, and
/// that records every call it executes.
#[derive(Default)]
struct Surface {
    tools: Mutex<Vec<Spec>>,
    executed: Mutex<Vec<String>>,
}

impl Surface {
    fn new(tools: Vec<Spec>) -> Arc<Self> {
        Arc::new(Self {
            tools: Mutex::new(tools),
            executed: Mutex::default(),
        })
    }

    fn replace(&self, tools: Vec<Spec>) {
        *self.tools.lock_recover() = tools;
    }

    fn tool(&self, name: &str) -> Option<Spec> {
        self.tools
            .lock_recover()
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    fn executed(&self) -> Vec<String> {
        self.executed.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Surface {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.tools
            .lock_recover()
            .iter()
            .map(|tool| tool.definition().manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
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
        lash_core::ToolOutcome::ok(serde_json::json!({ "id": tool.id })).into()
    }
}

/// The tool names of every request the model was asked, in order.
#[derive(Default)]
struct Offered {
    requests: Mutex<Vec<Vec<String>>>,
}

impl Offered {
    fn last(&self) -> Vec<String> {
        self.requests
            .lock_recover()
            .last()
            .cloned()
            .expect("the model was asked")
    }
}

/// The input that asks the model to call `tool` once.
fn call_input(tool: &str) -> String {
    format!("call {tool}")
}

/// The model: it records the tools each request offers; asked by
/// [`call_input`] with no answer yet, it calls that tool, and otherwise
/// answers `done`.
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
                    let tool = rendered[at + 5..]
                        .split('"')
                        .next()
                        .expect("the input names a tool");
                    return Ok(served::response(vec![served::call(
                        "surface-call",
                        tool,
                        serde_json::json!({}),
                    )]));
                }
                Ok(served::text(&request, "done"))
            }
        })
        .build()
        .into_handle()
}

/// A deployment's stores and the law's model, outliving each core a law
/// starts over them.
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

    /// Start a core over the deployment's stores whose live tool source is
    /// `tools` and whose runs hold `policy`. Every core is the one worker
    /// identity: a parked session resumes under the owner that parked it.
    fn boot(
        &self,
        tools: Arc<dyn lash_core::ToolProvider>,
        policy: lash_core::ToolSourcePolicy,
    ) -> lash::LashCore {
        lash::LashCore::standard_builder(self.backend.clone())
            .tools(tools)
            .tool_source_policy(policy)
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .serve_test_llm_profile(model(&self.offered), served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "tool-surface-deployment",
                "tool-surface-boot",
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

async fn open(core: &lash::LashCore, name: &str) -> lash::LashSession {
    core.session(session_id(name))
        .open()
        .await
        .expect("the session opens")
}

/// Send `input` to session `name` on `core` and await its settled outcome.
async fn outcome(core: &lash::LashCore, name: &str, input: &str) -> lash::SendOutcome {
    let session = core
        .session(session_id(name))
        .durable()
        .await
        .expect("the session resolves");
    tokio::time::timeout(WATCHDOG, async {
        session
            .send(lash::TurnInput::text(input))
            .await?
            .outcome()
            .await
    })
    .await
    .unwrap_or_else(|_| panic!("deadlock watchdog: `{input}` never settled"))
    .expect("the send settles")
}

/// Send `input` to session `name` on `core`; the turn answers.
async fn send(core: &lash::LashCore, name: &str, input: &str) -> lash::TurnOutput {
    match outcome(core, name, input).await {
        lash::SendOutcome::Settled { output, .. } => {
            served::assert_answered(input, &output);
            *output
        }
        other => panic!("`{input}` must settle a turn: {other:?}"),
    }
}

/// The tool state session `name`'s durable head recorded.
async fn recorded(core: &lash::LashCore, name: &str) -> lash_core::ToolState {
    open(core, name)
        .await
        .admin()
        .tools()
        .state()
        .await
        .expect("the tool state reads")
        .recorded()
        .cloned()
        .expect("a run recorded the session's tool state")
}

fn entry<'a>(
    state: &'a lash_core::ToolState,
    id: &str,
) -> &'a lash_core::facade_support::ToolStateEntry {
    state
        .get(&ToolId::from(id))
        .unwrap_or_else(|| panic!("the state names `{id}`: {state:?}"))
}

/// Apply `access` to session `name` through its config command.
async fn set_tool_access(core: &lash::LashCore, name: &str, access: lash_core::SessionToolAccess) {
    let session = open(core, name).await;
    let config = session.admin().config();
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(
                format!("tool-access-{name}-{}", config.revision().await.unwrap()),
                config.revision().await.unwrap(),
            ),
            lash::config::ConfigTransaction::of(lash::config::SetToolAccess { access }),
        )
        .await
        .expect("the config write settles");
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
}

fn hiding(name: &str) -> lash_core::SessionToolAccess {
    lash_core::SessionToolAccess::ambient()
        .with_hidden_tools([name])
        .expect("valid hidden tool")
}

const TOLERATE: lash_core::ToolSourcePolicy = lash_core::ToolSourcePolicy::Tolerate;
const REQUIRE: lash_core::ToolSourcePolicy = lash_core::ToolSourcePolicy::Require;

/// A session's tool-access config narrows the next model request, and a
/// later write widens it again.
async fn tool_access_setter_changes_the_next_model_request_in_both_directions(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let tool = spec(
        "tool:mutable_authority",
        "mutable_authority",
        "visible when the session authority permits it",
    );
    let core = deployment.boot(Surface::new(vec![tool.clone()]), TOLERATE);
    create(&core, "mutable-authority").await;

    set_tool_access(&core, "mutable-authority", hiding(tool.name)).await;
    send(&core, "mutable-authority", "observe the narrowed surface").await;
    assert!(
        !deployment.offered.last().contains(&tool.name.to_owned()),
        "the request after the narrowing hides the tool: {:?}",
        deployment.offered.last()
    );

    set_tool_access(
        &core,
        "mutable-authority",
        lash_core::SessionToolAccess::ambient(),
    )
    .await;
    send(&core, "mutable-authority", "observe the widened surface").await;
    assert!(
        deployment.offered.last().contains(&tool.name.to_owned()),
        "the request after the widening offers the tool: {:?}",
        deployment.offered.last()
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A tool-access write survives a park and a resume on a restarted core:
/// the resumed session's runs still hide the tool, and a tool the source
/// advertises only after the restart is hidden by the recorded authority
/// too when it names it.
async fn updated_tool_access_survives_park_and_resume(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let visible = spec(
        "tool:authority_visible",
        "authority_visible",
        "present before and after the restart",
    );
    let hidden = spec(
        "tool:authority_hidden",
        "authority_hidden",
        "appears only after the restart",
    );
    let surface = Surface::new(vec![visible.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "parked-authority").await;
    set_tool_access(&core, "parked-authority", hiding(hidden.name)).await;
    send(&core, "parked-authority", "before the park").await;
    let parked = Box::pin(open(&core, "parked-authority").await.park())
        .await
        .expect("the session parks");
    core.shutdown().await.expect("the core shuts down");

    surface.replace(vec![visible.clone(), hidden.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    let resumed = core.resume(parked).await.expect("the session resumes");
    assert_eq!(
        resumed.session_id(),
        &session_id("parked-authority"),
        "the resumed session keeps its store-bound id"
    );
    send(&core, "parked-authority", "after the resume").await;
    let offered = deployment.offered.last();
    assert!(offered.contains(&visible.name.to_owned()), "{offered:?}");
    assert!(
        !offered.contains(&hidden.name.to_owned()),
        "a newly live tool is still hidden by the recorded authority after the restart: {offered:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A restarted core whose source advertises a new tool discovers it, keeps
/// the host's opt-out of the old one, bumps the generation once, and an
/// unchanged source on the next turn restores it exactly, with no second
/// bump.
async fn cold_resume_discovers_curated_live_surface_and_persists_it_without_flapping(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let original = spec(
        "tool:original",
        "original",
        "present in the first recorded surface",
    );
    let discovered = spec(
        "tool:discovered",
        "discovered",
        "advertised while the core is down",
    );
    let surface = Surface::new(vec![original.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "live-surface").await;
    send(&core, "live-surface", "record the first surface").await;
    open(&core, "live-surface")
        .await
        .admin()
        .tools()
        .set_membership(original.id, false)
        .await
        .expect("the host opts the original out");
    let curated = recorded(&core, "live-surface").await;
    assert!(!entry(&curated, original.id).is_member());
    core.shutdown().await.expect("the core shuts down");

    surface.replace(vec![original.clone(), discovered.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    send(&core, "live-surface", &call_input(discovered.name)).await;
    let offered = deployment.offered.last();
    assert!(offered.contains(&discovered.name.to_owned()), "{offered:?}");
    assert!(!offered.contains(&original.name.to_owned()), "{offered:?}");
    assert_eq!(
        surface.executed(),
        [discovered.id],
        "the discovered tool executes by its id"
    );
    let rebuilt = recorded(&core, "live-surface").await;
    assert_eq!(rebuilt.generation(), curated.generation() + 1);
    assert!(
        !entry(&rebuilt, original.id).is_member(),
        "the recorded opt-out stays attached to the original id"
    );
    assert!(
        entry(&rebuilt, discovered.id).is_member(),
        "a newly advertised id defaults to catalog membership"
    );
    core.shutdown().await.expect("the core shuts down");

    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    send(&core, "live-surface", "an unchanged source").await;
    assert_eq!(
        serde_json::to_value(recorded(&core, "live-surface").await).unwrap(),
        serde_json::to_value(&rebuilt).unwrap(),
        "an unchanged live surface restores exactly without another generation bump"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A tool the session's authority hides is never offered and never runs,
/// though the source advertises it and the recorded state keeps it a
/// member: authority is not host curation.
async fn hidden_tool_stays_denied_without_becoming_curation(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let visible = spec("tool:cold_visible", "cold_visible", "visible throughout");
    let hidden = spec("tool:cold_hidden", "cold_hidden", "hidden by authority");
    let surface = Surface::new(vec![visible.clone(), hidden.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "hidden-tool").await;
    set_tool_access(&core, "hidden-tool", hiding(hidden.name)).await;
    send(&core, "hidden-tool", &call_input(hidden.name)).await;
    let offered = deployment.offered.last();
    assert!(offered.contains(&visible.name.to_owned()), "{offered:?}");
    assert!(!offered.contains(&hidden.name.to_owned()), "{offered:?}");
    assert!(
        surface.executed().is_empty(),
        "a call to the hidden tool is refused before it runs"
    );
    assert!(
        entry(&recorded(&core, "hidden-tool").await, hidden.id).is_member(),
        "authority hiding must not become recorded curation"
    );
    core.shutdown().await.expect("the core shuts down");

    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    send(&core, "hidden-tool", &call_input(hidden.name)).await;
    assert!(!deployment.offered.last().contains(&hidden.name.to_owned()));
    assert!(
        surface.executed().is_empty(),
        "the hidden tool stays denied across a restart"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A recorded tool whose source is gone is kept as an orphaned non-member;
/// the same id coming back rebinds it with its curation; a new id reusing
/// the name supersedes it, with no duplicate model-facing name.
async fn orphan_lifecycle_rebinds_by_id_and_supersedes_same_name_without_duplicates(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let original = spec(
        "tool:orphan-original",
        "orphaned_name",
        "recorded original manifest",
    );
    let rebound = spec(
        original.id,
        original.name,
        "fresh live manifest after the source recovers",
    );
    let replacement = spec(
        "tool:orphan-replacement",
        original.name,
        "a different id reusing the orphaned name",
    );
    let surface = Surface::new(vec![original.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "orphans").await;
    send(&core, "orphans", "record the original").await;
    open(&core, "orphans")
        .await
        .admin()
        .tools()
        .set_membership(original.id, false)
        .await
        .expect("the host opts the original out");

    surface.replace(Vec::new());
    send(&core, "orphans", "the source is gone").await;
    let orphaned = recorded(&core, "orphans").await;
    assert!(entry(&orphaned, original.id).is_orphaned());
    assert!(!entry(&orphaned, original.id).is_member());
    assert!(
        !deployment
            .offered
            .last()
            .contains(&original.name.to_owned())
    );

    surface.replace(vec![rebound.clone()]);
    send(&core, "orphans", "the source is back").await;
    let rebound_state = recorded(&core, "orphans").await;
    let rebound_entry = entry(&rebound_state, original.id);
    assert!(!rebound_entry.is_orphaned());
    assert!(
        !rebound_entry.is_member(),
        "a same-id rebind keeps the host's opt-out"
    );
    assert_eq!(rebound_entry.manifest().description, rebound.description);

    surface.replace(Vec::new());
    send(&core, "orphans", "orphaned again").await;
    surface.replace(vec![replacement.clone()]);
    send(&core, "orphans", &call_input(replacement.name)).await;
    let replaced = recorded(&core, "orphans").await;
    assert!(!replaced.contains(&ToolId::from(original.id)));
    assert!(
        entry(&replaced, replacement.id).is_member(),
        "the old id's opt-out does not transfer to the replacement"
    );
    assert_eq!(
        deployment
            .offered
            .last()
            .iter()
            .filter(|name| *name == replacement.name)
            .count(),
        1,
        "a superseded orphan never yields a duplicate model-facing name"
    );
    assert_eq!(surface.executed(), [replacement.id]);
    core.shutdown().await.expect("the core shuts down");
}

/// The host's whole-snapshot apply is a delta at the recorded generation:
/// it bumps the generation once and changes only what it edits, and a
/// snapshot at a stale generation is refused.
async fn public_apply_tool_state_round_trip_keeps_delta_and_generation_fencing(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let first = spec("tool:apply-first", "apply_first", "first live tool");
    let second = spec("tool:apply-second", "apply_second", "second live tool");
    let surface = Surface::new(vec![first.clone(), second.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "apply-state").await;
    send(&core, "apply-state", "record the surface").await;

    let stale = recorded(&core, "apply-state").await;
    let mut edited = stale.clone();
    edited
        .set_membership(&ToolId::from(first.id), false)
        .expect("edit membership through the public snapshot");
    let session = open(&core, "apply-state").await;
    let applied_generation = session
        .admin()
        .tools()
        .advanced()
        .apply_state(edited)
        .await
        .expect("a generation-matched delta applies");
    assert_eq!(applied_generation, stale.generation() + 1);
    let applied = recorded(&core, "apply-state").await;
    assert!(!entry(&applied, first.id).is_member());
    assert!(
        entry(&applied, second.id).is_member(),
        "apply_state is a delta over the submitted snapshot, not a blanket opt-out"
    );
    send(&core, "apply-state", &call_input(first.name)).await;
    assert!(!deployment.offered.last().contains(&first.name.to_owned()));
    assert!(
        surface.executed().is_empty(),
        "a non-member is refused by id"
    );

    let refused = session
        .admin()
        .tools()
        .advanced()
        .apply_state(stale)
        .await
        .expect_err("a stale generation is fenced");
    let message = refused.to_string();
    assert!(message.contains("generation"), "{message}");
    assert!(
        message.contains(&applied_generation.to_string()),
        "{message}"
    );
    core.shutdown().await.expect("the core shuts down");
}

const ALPHA_ID: &str = "tool:fig3367_alpha";
const ALPHA_NAME: &str = "fig3367_alpha";
const BETA_ID: &str = "tool:fig3367_beta";
const BETA_NAME: &str = "fig3367_beta";
const REPLACEMENT_ID: &str = "tool:fig3367_alpha_v2";

fn alpha() -> Spec {
    spec(ALPHA_ID, ALPHA_NAME, "fixed fig3367 fixture tool")
}

fn beta() -> Spec {
    spec(BETA_ID, BETA_NAME, "fixed fig3367 fixture tool")
}

/// The FIG-3353 sequence, end to end: a commit taken while every tool is
/// orphaned does not record them as non-members for good. The recorded
/// state is read between restarts, with no run in the way.
async fn fig3353_sequence_keeps_curation_across_an_orphaned_commit(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    // Step 1: run with the source and record a deliberate opt-out of beta.
    let core = deployment.boot(Surface::new(vec![alpha(), beta()]), TOLERATE);
    create(&core, "fig3353").await;
    let first = send(&core, "fig3353", "record both tools").await;
    assert!(
        first.tool_restore_report().is_none(),
        "a first run has no recorded tool state to install"
    );
    open(&core, "fig3353")
        .await
        .admin()
        .tools()
        .set_membership(BETA_ID, false)
        .await
        .expect("opt out beta");
    let seeded = recorded(&core, "fig3353").await;
    assert!(entry(&seeded, ALPHA_ID).is_member() && !entry(&seeded, ALPHA_ID).is_orphaned());
    assert!(!entry(&seeded, BETA_ID).member && !entry(&seeded, BETA_ID).is_orphaned());
    core.shutdown().await.expect("the core shuts down");

    // Step 2: a core that does not carry the source. Tolerate, so the run
    // goes on, commits while every tool is orphaned, and reports the loss.
    let core = deployment.boot(Surface::new(Vec::new()), TOLERATE);
    let grantless = send(&core, "fig3353", "commit while orphaned").await;
    let report = grantless
        .tool_restore_report()
        .expect("the run reports its restore to the host");
    assert_eq!(
        report.lost_members,
        vec![ToolId::from(ALPHA_ID)],
        "the curated member is the only lost capability"
    );
    assert_eq!(
        report.parked_opt_outs,
        vec![ToolId::from(BETA_ID)],
        "an unresolved tool the host already opted out of is not loss"
    );
    assert!(report.superseded_identities.is_empty());
    assert_eq!(
        report.generation,
        seeded.generation() + 1,
        "orphaning both entries changed the surface, so the restore bumped once"
    );
    assert!(
        !deployment.offered.last().contains(&ALPHA_NAME.to_owned()),
        "an orphan is not offered while its source is gone"
    );
    let orphaned = recorded(&core, "fig3353").await;
    assert_eq!(orphaned.generation(), seeded.generation() + 1);
    let alpha_entry = entry(&orphaned, ALPHA_ID);
    assert!(alpha_entry.is_orphaned());
    assert!(
        alpha_entry.member,
        "the host's curation bit survives orphaning: membership is derived, never rewritten"
    );
    let beta_entry = entry(&orphaned, BETA_ID);
    assert!(beta_entry.is_orphaned());
    assert!(!beta_entry.member, "and its opt-out is still an opt-out");
    core.shutdown().await.expect("the core shuts down");

    // Step 3: the source returns.
    let core = deployment.boot(Surface::new(vec![alpha(), beta()]), TOLERATE);
    send(&core, "fig3353", "the source is back").await;
    let regranted = recorded(&core, "fig3353").await;
    assert_eq!(
        regranted.generation(),
        seeded.generation() + 2,
        "exactly two restores changed the surface"
    );
    assert!(entry(&regranted, ALPHA_ID).is_member() && !entry(&regranted, ALPHA_ID).is_orphaned());
    assert!(
        !entry(&regranted, BETA_ID).member && !entry(&regranted, BETA_ID).is_orphaned(),
        "the opt-out made before the grantless run is still an opt-out"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A source that replaced a tool with a new id under the same model-facing
/// name is a superseded identity, not a loss, and Require does not refuse
/// the run.
async fn alias_replacement_is_reported_as_superseded_and_never_refuses(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let core = deployment.boot(Surface::new(vec![alpha()]), TOLERATE);
    create(&core, "superseded").await;
    send(&core, "superseded", "record alpha").await;
    core.shutdown().await.expect("the core shuts down");

    let core = deployment.boot(
        Surface::new(vec![spec(REPLACEMENT_ID, ALPHA_NAME, "the replacement")]),
        REQUIRE,
    );
    let replaced = send(&core, "superseded", "run on the replacement").await;
    let report = replaced
        .tool_restore_report()
        .expect("the run reports the superseded identity");
    assert!(report.lost_members.is_empty());
    assert!(report.parked_opt_outs.is_empty());
    let [superseded] = report.superseded_identities.as_slice() else {
        panic!("one identity was superseded: {report:?}");
    };
    assert_eq!(superseded.retired_id, ToolId::from(ALPHA_ID));
    assert_eq!(superseded.live_id, ToolId::from(REPLACEMENT_ID));
    assert_eq!(superseded.name, ALPHA_NAME);
    assert!(
        entry(&recorded(&core, "superseded").await, REPLACEMENT_ID).is_member(),
        "the replacement is a default member"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A session whose recorded tool state names alpha, on a restarted
/// Require core whose source no longer advertises it.
async fn require_core_without_alpha(
    deployment: &Deployment,
    name: &str,
) -> (lash::LashCore, lash_core::ToolState) {
    let core = deployment.boot(Surface::new(vec![alpha()]), TOLERATE);
    create(&core, name).await;
    send(&core, name, "record alpha").await;
    let snapshot = recorded(&core, name).await;
    core.shutdown().await.expect("the core shuts down");
    (deployment.boot(Surface::new(Vec::new()), REQUIRE), snapshot)
}

/// A host's restore onto a session answers its report instead of
/// refusing, whatever the run policy: Require governs a turn run, not a
/// restore the host asked for.
async fn a_host_restore_on_a_require_core_reports_instead_of_refusing(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let (core, snapshot) = require_core_without_alpha(&deployment, "require-restore").await;
    let report = open(&core, "require-restore")
        .await
        .admin()
        .tools()
        .advanced()
        .restore_state(snapshot)
        .await
        .expect("a host restore never refuses, whatever the run policy is");
    assert_eq!(
        report.lost_members,
        vec![ToolId::from(ALPHA_ID)],
        "the caller is handed the loss"
    );
    let restored = recorded(&core, "require-restore").await;
    assert_eq!(restored.generation(), report.generation);
    assert!(entry(&restored, ALPHA_ID).is_orphaned());
    core.shutdown().await.expect("the core shuts down");
}

/// Require is a run policy: a turn run that would lose a recorded member is
/// refused, typed, before it publishes anything (FIG-5134).
async fn a_require_run_that_would_lose_a_member_is_refused_typed(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let (core, _) = require_core_without_alpha(&deployment, "require-run").await;
    let refused = outcome(&core, "require-run", "run without alpha").await;
    let lash::SendOutcome::Refused { refusal, .. } = refused else {
        panic!("a Require run that would lose alpha is refused: {refused:?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::ToolSourcesUnavailable,
        "{refusal:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A fork records its source revision's tool curation and authority and
/// reconciles them over the live source: a tool advertised since the
/// source's turn is discovered and callable, the curated opt-out stays out,
/// the hidden tool stays hidden though its recorded entry stays a member,
/// and the host curating the hidden tool out and back in again never lets
/// it through.
async fn session_fork_discovers_live_tools_and_preserves_curation_and_hidden_policy(tier: Tier) {
    let Some(deployment) = Deployment::new(tier).await else {
        return;
    };
    let curated = spec(
        "tool:curated",
        "curated",
        "the host opts this tool out before the fork",
    );
    let discovered = spec(
        "tool:fork_discovered",
        "fork_discovered",
        "appears between the source's turn and the fork's",
    );
    let hidden = spec(
        "tool:fork_hidden",
        "fork_hidden",
        "never enters the fork's authority",
    );
    let surface = Surface::new(vec![curated.clone()]);
    let core = deployment.boot(Arc::clone(&surface) as _, TOLERATE);
    create(&core, "fork-source").await;
    send(&core, "fork-source", "record the source surface").await;
    open(&core, "fork-source")
        .await
        .admin()
        .tools()
        .set_membership(curated.id, false)
        .await
        .expect("curate the source's tool out");
    set_tool_access(&core, "fork-source", hiding(hidden.name)).await;
    let lash::SendOutcome::Settled { run, .. } =
        outcome(&core, "fork-source", "the turn the fork is taken at").await
    else {
        panic!("the source's turn settles");
    };
    surface.replace(vec![curated.clone(), discovered.clone(), hidden.clone()]);
    core.fork_at(
        &session_id("fork-source"),
        lash_core::Target::Turn(run),
        lash::ForkRequest {
            session_id: session_id("fork-child"),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: session_id("fork-source"),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await
    .expect("fork the source at its turn");

    send(&core, "fork-child", &call_input(discovered.name)).await;
    let offered = deployment.offered.last();
    assert!(offered.contains(&discovered.name.to_owned()), "{offered:?}");
    assert!(!offered.contains(&curated.name.to_owned()), "{offered:?}");
    assert!(!offered.contains(&hidden.name.to_owned()), "{offered:?}");
    assert_eq!(surface.executed(), [discovered.id]);
    let child = recorded(&core, "fork-child").await;
    assert!(
        !entry(&child, curated.id).is_member(),
        "membership curation survives the fork"
    );
    assert!(
        entry(&child, discovered.id).is_member(),
        "the fork reconciles the source's snapshot over the live source"
    );
    assert!(
        entry(&child, hidden.id).is_member(),
        "authority never rewrites ToolId-keyed host curation"
    );

    let tools = open(&core, "fork-child").await.admin().tools();
    tools
        .set_membership(hidden.id, false)
        .await
        .expect("curate the hidden tool out");
    tools
        .set_membership(hidden.id, true)
        .await
        .expect("curate the hidden tool back in");
    assert!(
        entry(&recorded(&core, "fork-child").await, hidden.id).is_member(),
        "authority does not undo set_membership(true)"
    );
    send(&core, "fork-child", "after the re-add").await;
    assert!(
        !deployment.offered.last().contains(&hidden.name.to_owned()),
        "authority still keeps a curated member off the model's surface"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// The execution environment a host start publishes for the held engine.
fn process_environment() -> lash_core_execution::ProcessExecutionEnvSpec {
    let mut environment = lash_core_execution::ProcessExecutionEnvSpec::new(
        lash_core_execution::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(16)),
    );
    environment.render = Some(lash_core::RecordedRender {
        renderer_id: lash::render::ToolOutputRendererSlot::default()
            .0
            .id()
            .to_owned(),
        params: serde_json::to_value(lash::render::ResolvedStandardRenderConfig {
            defaults: lash::render::ToolRenderParams::default(),
            per_tool: std::collections::BTreeMap::new(),
        })
        .expect("the render config encodes"),
    });
    environment
}

/// A host start observes only the sessions it names: the session a start
/// wakes gains no observer edge from being its wake target, and a session
/// the start names as an observer does (ported from the start half of
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
        let mut request = lash_core::ProcessStartRequest::new(
            lash_core_execution::testing::held_engine_input(serde_json::json!({ "start": label })),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_env_ref(env_ref.clone())
        .with_host_start_key(label);
        request.wake_session_id = Some(parent_id.clone());
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
        "the wake target does not imply an observer edge"
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
        let request = lash_core::ProcessStartRequest::new(
            lash_core_execution::testing::held_engine_input(serde_json::json!({ "work": 1 })),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_env_ref(env_ref)
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
            .list(&filter)
            .await
            .expect("read the committed process");
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
            changed.input =
                lash_core_execution::testing::held_engine_input(serde_json::json!({ "work": 2 }))
                    .into();
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
                .list(&filter)
                .await
                .expect("read processes after both retries");
            assert_eq!(processes.len(), 1, "no retry creates a second process");
            assert_eq!(processes[0].process_id, original_id);
        }
    })
    .await
    .expect("the host start and retries settle before the watchdog");
    world.shutdown().await;
}

tiered_laws!(
    tool_access_setter_changes_the_next_model_request_in_both_directions,
    updated_tool_access_survives_park_and_resume,
    cold_resume_discovers_curated_live_surface_and_persists_it_without_flapping,
    hidden_tool_stays_denied_without_becoming_curation,
    orphan_lifecycle_rebinds_by_id_and_supersedes_same_name_without_duplicates,
    public_apply_tool_state_round_trip_keeps_delta_and_generation_fencing,
    fig3353_sequence_keeps_curation_across_an_orphaned_commit,
    alias_replacement_is_reported_as_superseded_and_never_refuses,
    a_host_restore_on_a_require_core_reports_instead_of_refusing,
    a_require_run_that_would_lose_a_member_is_refused_typed,
    a_host_start_observes_only_the_sessions_it_names,
    a_lost_host_start_acknowledgement_recovers_the_retained_process,
    session_fork_discovers_live_tools_and_preserves_curation_and_hidden_policy,
);
