//! OWED-IS-NOT-INSTALLED (FIG-6089; law 13's session arm across two
//! activations, FIG-6075 R-2): a session requires a provider only while a
//! call it recorded to that provider is unsettled, whatever the claim that
//! settles the call carried. A turn's call to provider K parks and the
//! session is released requiring K, not the installed J it never called:
//! a node whose registrations lack K, serving all it installs, does not
//! claim it. A later claim settles K's call and the turn calls J, which
//! parks: the release requires J and no longer K, so a node that does not
//! install K claims the session to run J's call.

use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt as _;

use super::served::{self, Tier, WATCHDOG};

/// The input that starts the law's turn.
const TURN: &str = "settlement-turn";
/// The turn's id, which a later deployment attaches to.
const RUN: &str = "settlement-run";
const SESSION: &str = "settlement-session";
/// The tool provider K serves under the key `tools`.
const K_PROVIDER: &str = "settlement-k";
const K_TOOL: &str = "settlement_k";
/// The tool provider J serves under the key `tools`.
const J_PROVIDER: &str = "settlement-j";
const J_TOOL: &str = "settlement_j";

fn definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
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

/// A provider of one deferring tool, `name`: its call parks, and the key
/// of the completion wait it parks on is kept for the law to resolve.
struct Deferring {
    name: &'static str,
    key: Mutex<Option<String>>,
    parked: tokio::sync::Notify,
}

impl Deferring {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            key: Mutex::new(None),
            parked: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Deferring {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![definition(self.name).manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.name).then(|| Arc::new(definition(self.name).contract()))
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

/// The token a session requires while a call to `owner`'s provider is
/// unsettled.
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
    .to_string()
}

/// The core over `backend` that installs both providers.
fn deployment(
    backend: &lash::Backend,
    k: &Arc<Deferring>,
    j: &Arc<Deferring>,
) -> lash::LashCoreBuilder {
    [(K_PROVIDER, k), (J_PROVIDER, j)].into_iter().fold(
        lash::LashCore::standard_builder(backend.clone()),
        |builder, (owner, provider)| {
            builder.plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial(owner),
                lash_core::facade_support::PluginSpec::new().with_tool_provider(
                    lash_core::tool_provider_key!("tools"),
                    Arc::clone(provider) as _,
                ),
            )))
        },
    )
}

/// The claim requirements session `name` carries, as a probe node serving
/// `serves` reads them when it claims the session; `None` when it cannot
/// claim it. A claimed session is released ready, untouched.
async fn probe(backend: &lash::Backend, name: &str, serves: Vec<String>) -> Option<Vec<String>> {
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec, Release};
    let store = backend.durable();
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("settlement-law-probe"),
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

pub(super) async fn a_settled_call_leaves_its_provider_unrequired_on_a_later_claim(tier: Tier) {
    let k = Deferring::new(K_TOOL);
    let j = Deferring::new(J_TOOL);
    let (built_k, built_j) = (Arc::clone(&k), Arc::clone(&j));
    let Some(mut world) =
        served::World::new(tier, move |backend| deployment(backend, &built_k, &built_j)).await
    else {
        return;
    };
    world.script(
        TURN,
        vec![
            served::response(vec![served::call(
                "settlement-k-call",
                K_TOOL,
                serde_json::json!({}),
            )]),
            served::response(vec![served::call(
                "settlement-j-call",
                J_TOOL,
                serde_json::json!({}),
            )]),
        ],
    );
    let session = world.session(SESSION, served::spec(8)).await;
    let handle = session
        .send(lash::TurnInput::text(TURN))
        .id(lash::TurnId::parse(RUN).expect("a turn id"))
        .await
        .expect("the turn is accepted");
    tokio::time::timeout(WATCHDOG, k.parked.notified())
        .await
        .expect("the call to K parks");
    // The park commits with the member's outcome window; the actor is
    // released waiting on the call's completion.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    drop((handle, session));
    let serves = lash::testing::node_serves(&world.core);
    world.core.shutdown().await.expect("the first core stops");
    let key = k.key.lock_recover().clone().expect("K's park key");
    lash_core::waits::resolve_host(
        &world.backend,
        &key,
        lash_core::Resolution::Ok(serde_json::json!({ "answered": "k" })),
    )
    .await
    .expect("K's wait resolves");
    let (k_token, j_token) = (provider_token(K_PROVIDER), provider_token(J_PROVIDER));
    assert!(
        serves.contains(&k_token) && serves.contains(&j_token),
        "the deployment served both providers: {serves:?}"
    );
    let without_k: Vec<String> = serves
        .iter()
        .filter(|token| **token != k_token)
        .cloned()
        .collect();
    assert_eq!(
        probe(&world.backend, SESSION, without_k.clone()).await,
        None,
        "a node whose registrations lack K, serving all it installs, does not claim K's unsettled call"
    );
    let requires = probe(&world.backend, SESSION, serves)
        .await
        .expect("a node serving K claims the session");
    assert!(
        requires.contains(&k_token) && !requires.contains(&j_token),
        "the unsettled call requires K, and installed J adds nothing: {requires:?}"
    );

    // A second claim settles K's call, and the turn calls J, which parks.
    let (built_k, built_j) = (Arc::clone(&k), Arc::clone(&j));
    world
        .restart("settlement-second-boot", move |backend| {
            deployment(backend, &built_k, &built_j)
        })
        .await;
    tokio::time::timeout(WATCHDOG, j.parked.notified())
        .await
        .expect("the call to J parks after K's settled");
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    world.core.shutdown().await.expect("the second core stops");

    // J's completion arrives, so the session is due to run J's call.
    let key = j.key.lock_recover().clone().expect("J's park key");
    lash_core::waits::resolve_host(
        &world.backend,
        &key,
        lash_core::Resolution::Ok(serde_json::json!({ "answered": "j" })),
    )
    .await
    .expect("J's wait resolves");
    let requires = probe(&world.backend, SESSION, without_k)
        .await
        .expect("a node without K claims the session: K's call settled");
    assert!(
        requires.contains(&j_token),
        "J's unsettled call keeps J required: {requires:?}"
    );
    assert!(
        !requires.contains(&k_token),
        "K's settled call no longer requires K: {requires:?}"
    );
    world.shutdown().await;
}
