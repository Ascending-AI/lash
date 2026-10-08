//! The lash core's node serves processes (FIG-5216).
//!
//! Each law builds one deployment the way a host does: a durable backend
//! over a store set, and a lash core over it whose own node serves the
//! backend's session and process actors. Work enters only through the
//! facade: a session's `send()`, or the core's process API. Nothing drives
//! a turn or advances a process by hand.
//!
//! - **spawn_agent:** a parent turn's model calls `spawn_agent`; the child
//!   is a `SessionTurn` process whose turn its own session actor runs, and
//!   the parent's call answers the child's reply, once.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmStreamEvent, LlmUsage, StreamBlockIdentity,
};
use lash_core::{LlmOutputPart, ProcessEventLogTestSupport as _};
use lash_core_execution::StoreSet;
use lash_sansio::SessionId;

const MODEL: &str = "core-node-model";

/// The law `$law(tier)` on SQLite in memory, a SQLite file and PostgreSQL;
/// the PostgreSQL leg runs when the run is handed a server.
macro_rules! on_every_tier {
    ($law:ident) => {
        mod $law {
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn on_sqlite_memory() {
                super::$law(super::Tier::SqliteMemory).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn on_sqlite_file() {
                super::$law(super::Tier::SqliteFile).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn on_postgres() {
                if super::postgres_url().is_none() {
                    return;
                }
                super::$law(super::Tier::Postgres).await;
            }
        }
    };
}

/// Where a law's database lives.
#[derive(Clone, Copy, Debug)]
enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The PostgreSQL server a PostgreSQL leg runs on, or `None` when the run
/// was handed none and the leg is skipped.
fn postgres_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// A law's deployment: its backend, its core, and what must outlive them.
struct Deployment {
    backend: lash::Backend,
    core: lash::LashCore,
    _keep: Vec<Box<dyn std::any::Any + Send>>,
}

/// A fresh store set of `tier`, on the wall clock, and what must outlive
/// it.
async fn stores(tier: Tier) -> (Arc<dyn StoreSet>, Vec<Box<dyn std::any::Any + Send>>) {
    match tier {
        Tier::SqliteMemory => (
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("an in-memory store set opens"),
            ),
            Vec::new(),
        ),
        Tier::SqliteFile => {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(dir.path().join("lash.db"))
                .await
                .expect("a file store set opens");
            (Arc::new(stores), vec![Box::new(dir)])
        }
        Tier::Postgres => {
            let url = postgres_url().expect("a PostgreSQL URL");
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .expect("the isolated database opens");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
            );
            (Arc::new(stores), vec![Box::new(isolated)])
        }
    }
}

/// A deployment over a fresh database of `tier`: a backend with `engines`,
/// and a standard core `configure` builds over it.
async fn deploy(
    tier: Tier,
    engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
    configure: impl FnOnce(lash::LashCoreBuilder) -> lash::LashCoreBuilder,
) -> Deployment {
    deploy_with(tier, engines, |backend| {
        configure(lash::LashCore::standard_builder(backend.clone()))
    })
    .await
}

/// [`deploy`], the core `build` builds over the backend.
async fn deploy_with(
    tier: Tier,
    engines: Vec<Arc<dyn lash_core::ProcessEngine>>,
    build: impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder,
) -> Deployment {
    let (stores, keep) = stores(tier).await;
    let backend = engines
        .into_iter()
        .fold(
            lash::durable::DurableBackendBuilder::new(stores),
            lash::durable::DurableBackendBuilder::process_engine,
        )
        .build()
        .expect("the backend assembles");
    let core = build(&backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "core-node-deployment",
            "core-node-boot",
        ))
        .expect("the core builds");
    Deployment {
        backend,
        core,
        _keep: keep,
    }
}

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

fn spec() -> lash::SessionSpec {
    lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(16),
    )
}

/// A text answer, streamed as one delta.
fn text(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Delta {
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        });
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// One native tool call.
fn call(id: &str, tool: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: id.to_owned(),
            tool_name: tool.to_owned(),
            input_json: input.to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

/// The tool results a request carries, rendered.
fn results(request: &LlmRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            lash_core::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                Some(format!("{content:?}"))
            }
            _ => None,
        })
        .collect()
}

/// A model whose answer to each request `respond` decides, counting the
/// requests whose transcript holds each marker.
fn scripted(
    respond: impl Fn(&LlmRequest, &str) -> LlmResponse + Send + Sync + 'static,
) -> ProviderHandle {
    let respond = Arc::new(respond);
    lash_core::testing::TestProvider::builder()
        .kind("core-node-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let respond = Arc::clone(&respond);
            async move {
                let transcript = format!("{:?}", request.messages);
                Ok(respond(&request, &transcript))
            }
        })
        .build()
        .into_handle()
}

/// The answer a session's turn settled with, waited for through the
/// facade.
async fn settle(core: &lash::LashCore, session: &str, input: &str) -> lash::TurnOutput {
    let session = core
        .session(SessionId::try_from(session.to_owned()).unwrap())
        .create(lash::SessionCreation::root(spec()))
        .await
        .expect("the session is created");
    tokio::time::timeout(
        Duration::from_secs(60),
        session.send(lash::TurnInput::text(input)).output(),
    )
    .await
    .expect("the turn settles within a minute")
    .expect("the turn settles")
}

// --- spawn_agent ------------------------------------------------------------

const PARENT_INPUT: &str = "core-node law: spawn the child";
const CHILD_TASK: &str = "core-node child: answer your literal";
const CHILD_REPLY: &str = "the core-node child's literal";
const PARENT_DONE: &str = "the parent saw its child";

/// The host's delegation tool (`examples/delegation`): children are created
/// from [`spec`], stated explicitly, and live until their starter ends.
fn delegation() -> delegation::DelegationPluginFactory {
    delegation::DelegationPluginFactory::new(spec(), lash_core::lifetime::starter)
}

/// A parent turn sent to the core calls `spawn_agent`: the core's node runs
/// the child's `SessionTurn` process and its child turn, the child's model
/// is asked once, and the parent's call answers the child's reply, which
/// the parent's next step reads once.
async fn spawn_agent_child_runs_and_answers_its_parent(tier: Tier) {
    let child_calls = Arc::new(AtomicUsize::new(0));
    let parent_saw = Arc::new(Mutex::new(Vec::new()));
    let model = {
        let child_calls = Arc::clone(&child_calls);
        let parent_saw = Arc::clone(&parent_saw);
        scripted(move |request, transcript| {
            if transcript.contains(CHILD_TASK) && !transcript.contains(PARENT_INPUT) {
                child_calls.fetch_add(1, Ordering::SeqCst);
                return text(request, CHILD_REPLY);
            }
            let seen = results(request);
            if seen.is_empty() {
                return call(
                    "core-node-spawn",
                    "spawn_agent",
                    serde_json::json!({ "task": CHILD_TASK }),
                );
            }
            *parent_saw.lock().unwrap() = seen;
            text(request, PARENT_DONE)
        })
    };
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder
            .serve_test_llm_profile(model, metadata())
            .plugin(Arc::new(delegation()))
    })
    .await;
    let output = settle(&deployment.core, "core-node-parent", PARENT_INPUT).await;
    assert!(
        format!("{output:?}").contains(PARENT_DONE),
        "the parent's turn ends on its own answer: {output:?}"
    );
    assert_eq!(
        child_calls.load(Ordering::SeqCst),
        1,
        "the child's model is asked once"
    );
    let saw = parent_saw.lock().unwrap().clone();
    assert_eq!(saw.len(), 1, "the parent reads one call's answer: {saw:?}");
    assert!(
        saw[0].contains(CHILD_REPLY),
        "the parent's call answers the child's reply: {saw:?}"
    );
    drop(deployment.backend);
}

on_every_tier!(spawn_agent_child_runs_and_answers_its_parent);

// FIG-5295: only catalog forks inherit config; other children start with
// the prompt plan their creator supplies, or the neutral default.
#[derive(Clone, Copy)]
enum PromptChild {
    Spawn,
    Fork,
    Related,
}

async fn set_prompt_plan(
    core: &lash::LashCore,
    session_id: &SessionId,
    id: &str,
    plan: lash::prompt::PromptPlan,
) {
    let session = core.session(session_id.clone()).open().await.unwrap();
    let config = session.admin().config();
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(id, config.revision().await.unwrap()),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan }),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
}

fn parent_prompt_plan() -> lash::prompt::PromptPlan {
    use lash::prompt::{
        PromptPlacement, PromptPlan, PromptSectionId, PromptSectionKey, PromptSectionPlacement,
    };
    let section =
        |key| PromptSectionId::new("standard_protocol", PromptSectionKey::new(key).unwrap());
    PromptPlan {
        order: vec![section("guidance"), section("execution")],
        placements: vec![PromptSectionPlacement {
            section: section("intro"),
            placement: PromptPlacement::CurrentContext,
        }],
        limits: lash::prompt::PromptLimits {
            max_sections: std::num::NonZeroU32::new(16).unwrap(),
            max_wrappers: std::num::NonZeroU32::new(16).unwrap(),
            max_section_bytes: std::num::NonZeroU32::new(16_384).unwrap(),
            max_total_bytes: std::num::NonZeroU32::new(65_536).unwrap(),
            render_budget_ms: std::num::NonZeroU32::new(1_000).unwrap(),
        },
    }
}

async fn assert_child_creation_plan(
    tier: Tier,
    kind: PromptChild,
    explicit: Option<lash::prompt::PromptPlan>,
) {
    const CHILD_INPUT: &str = "prompt-plan child call";
    const INTRO: &str = "You are an assistant operating the lash harness.";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = {
        let requests = Arc::clone(&requests);
        scripted(move |request, transcript| {
            if (transcript.contains(CHILD_TASK) && !transcript.contains(PARENT_INPUT))
                || transcript.contains(CHILD_INPUT)
            {
                requests
                    .lock()
                    .unwrap()
                    .push((request.instructions.clone(), transcript.to_owned()));
                return text(request, CHILD_REPLY);
            }
            if transcript.contains(PARENT_INPUT) && results(request).is_empty() {
                return call(
                    "prompt-plan-spawn",
                    "spawn_agent",
                    serde_json::json!({ "task": CHILD_TASK }),
                );
            }
            text(request, PARENT_DONE)
        })
    };
    // A tool creator opts into a plan by stating it on its ordinary create
    // request; without one the child takes the neutral default.
    let plugin = Arc::new(match explicit.clone() {
        Some(plan) => delegation().with_child_prompt_plan(plan),
        None => delegation(),
    });
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder
            .serve_test_llm_profile(model, metadata())
            .plugin(plugin)
    })
    .await;
    let parent_id = SessionId::from("prompt-plan-parent");
    let parent = deployment
        .core
        .session(parent_id.clone())
        .create(lash::SessionCreation::root(spec()))
        .await
        .unwrap();
    let plan = parent_prompt_plan();
    set_prompt_plan(&deployment.core, &parent_id, "parent-plan", plan.clone()).await;
    let catalog = deployment.backend.stores().session_store_factory();
    let parent_head =
        lash_core::SessionCommitStore::load_session_head_meta(catalog.as_ref(), &parent_id)
            .await
            .unwrap()
            .unwrap();
    let child_id = SessionId::from("prompt-plan-child");
    let child_id = match kind {
        PromptChild::Spawn => {
            let output = parent
                .send(lash::TurnInput::text(PARENT_INPUT))
                .output()
                .await
                .unwrap();
            assert!(output.is_success(), "{output:?}");
            let children = catalog
                .list_sessions(&lash_core::SessionListFilter {
                    relation: Some(lash_core::SessionRelationKind::Child),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(children.len(), 1);
            children[0].session_id.clone()
        }
        PromptChild::Fork => {
            deployment
                .core
                .fork_at(
                    &parent_id,
                    lash::Target::Revision(parent_head.head_revision),
                    lash::ForkRequest {
                        session_id: child_id.clone(),
                        relation: lash_core::SessionRelation::Fork {
                            source_session_id: parent_id.clone(),
                            source_node_id: None,
                        },
                        observed_processes: Vec::new(),
                    },
                )
                .await
                .unwrap();
            let fork_head =
                lash_core::SessionCommitStore::load_session_head_meta(catalog.as_ref(), &child_id)
                    .await
                    .unwrap()
                    .unwrap();
            // Config revision counts writes in this session; a clone starts
            // its own counter while retaining every configuration value.
            let mut expected_config = parent_head.config.clone();
            expected_config.config_revision = 0;
            assert_eq!(
                fork_head.config, expected_config,
                "a fork copies every retained config value"
            );
            child_id
        }
        PromptChild::Related => {
            let mut creation = lash::SessionCreation::child_of(parent_id.clone(), spec());
            if let Some(plan) = explicit.clone() {
                creation = creation.with_prompt_plan(plan);
            }
            deployment
                .core
                .session(child_id.clone())
                .create(creation)
                .await
                .unwrap();
            child_id
        }
    };
    let expected = if matches!(kind, PromptChild::Fork) {
        plan.clone()
    } else {
        explicit.unwrap_or_default()
    };
    let head = lash_core::SessionCommitStore::load_session_head_meta(catalog.as_ref(), &child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        head.config.prompt_plan, expected,
        "only a fork inherits the parent's plan"
    );
    let child = deployment
        .core
        .session(child_id.clone())
        .durable()
        .await
        .unwrap();
    if !matches!(kind, PromptChild::Spawn) {
        let output = child
            .send(lash::TurnInput::text(CHILD_INPUT))
            .output()
            .await
            .unwrap();
        assert!(output.is_success(), "{output:?}");
    }
    {
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the child's first model call is observed"
        );
        let (instructions, context) = &requests[0];
        let instructions = instructions.as_deref().unwrap();
        if expected.placements == plan.placements {
            assert!(instructions.starts_with("## Guidance"));
            assert!(!instructions.contains(INTRO));
            assert!(context.contains(INTRO));
        } else {
            assert!(
                instructions.starts_with(INTRO),
                "the child uses default section ordering and placement: {instructions}"
            );
        }
    }
    set_prompt_plan(
        &deployment.core,
        &child_id,
        "child-plan",
        lash::prompt::PromptPlan::default(),
    )
    .await;
    let parent_head =
        lash_core::SessionCommitStore::load_session_head_meta(catalog.as_ref(), &parent_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        parent_head.config.prompt_plan, plan,
        "configuring a child leaves its parent intact"
    );
}

async fn a_fork_starts_with_its_parents_full_configuration(tier: Tier) {
    assert_child_creation_plan(tier, PromptChild::Fork, None).await;
}

async fn spawned_and_related_children_start_with_the_default_prompt_plan(tier: Tier) {
    assert_child_creation_plan(tier, PromptChild::Spawn, None).await;
    assert_child_creation_plan(tier, PromptChild::Related, None).await;
}

async fn a_child_uses_exactly_its_explicit_prompt_plan(tier: Tier) {
    let mut plan = parent_prompt_plan();
    // Distinct from both the parent's plan and the neutral default, including
    // a creator-selected limit: neither may overwrite any part of it.
    plan.limits.max_sections = std::num::NonZeroU32::new(32).unwrap();
    assert_child_creation_plan(tier, PromptChild::Spawn, Some(plan.clone())).await;
    assert_child_creation_plan(tier, PromptChild::Related, Some(plan)).await;
}

// FIG-5296 (ADR 0134): a parent link is lineage for display and audit only.

/// A child created with a parent link records and runs exactly what an
/// unlinked session created from the same explicit input does: the same
/// config head and the same first model request, whatever the parent's own
/// config. Deleting the parent leaves the child session untouched: it keeps
/// its head and still runs a turn.
async fn a_linked_child_behaves_like_an_unlinked_session_and_outlives_its_parent(tier: Tier) {
    const INPUT: &str = "lineage law: answer your literal";
    let instructions = Arc::new(Mutex::new(Vec::new()));
    let model = {
        let instructions = Arc::clone(&instructions);
        scripted(move |request, _| {
            instructions.lock().unwrap().push((
                request.session_id().to_string(),
                request.instructions.clone(),
            ));
            text(request, CHILD_REPLY)
        })
    };
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder.serve_test_llm_profile(model, metadata())
    })
    .await;
    let catalog = deployment.backend.stores().session_store_factory();
    let parent_id = SessionId::from("lineage-parent");
    deployment
        .core
        .session(parent_id.clone())
        .create(lash::SessionCreation::root(spec()))
        .await
        .unwrap();
    // The parent's own config differs from the neutral default.
    set_prompt_plan(
        &deployment.core,
        &parent_id,
        "parent-plan",
        parent_prompt_plan(),
    )
    .await;
    let linked = SessionId::from("lineage-linked-child");
    let unlinked = SessionId::from("lineage-unlinked");
    for (id, creation) in [
        (
            linked.clone(),
            lash::SessionCreation::child_of(parent_id.clone(), spec()),
        ),
        (unlinked.clone(), lash::SessionCreation::root(spec())),
    ] {
        deployment.core.session(id).create(creation).await.unwrap();
    }
    let head = |id: SessionId| {
        let catalog = Arc::clone(&catalog);
        async move {
            lash_core::SessionCommitStore::load_session_head_meta(catalog.as_ref(), &id)
                .await
                .unwrap()
                .unwrap()
                .config
        }
    };
    let linked_head = head(linked.clone()).await;
    assert_eq!(
        linked_head,
        head(unlinked.clone()).await,
        "the parent link adds nothing to the child's recorded config"
    );
    let first_request = |id: &SessionId| {
        instructions
            .lock()
            .unwrap()
            .iter()
            .find(|(session, _)| session == id.as_str())
            .map(|(_, instructions)| instructions.clone())
            .expect("the session's model was asked")
    };
    for id in [&linked, &unlinked] {
        let output = deployment
            .core
            .session(id.clone())
            .durable()
            .await
            .unwrap()
            .send(lash::TurnInput::text(INPUT))
            .output()
            .await
            .unwrap();
        assert!(output.is_success(), "{output:?}");
    }
    assert_eq!(
        first_request(&linked),
        first_request(&unlinked),
        "the child's first model request is the unlinked session's"
    );

    let administration = deployment.core.session_administration().await;
    lash::LashCore::delete_session(administration.delete_context(&parent_id).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !matches!(
            catalog.lookup_session(&parent_id).await.unwrap(),
            lash_core::store::SessionLookup::Deleted
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the parent's deletion completes within a minute");
    assert!(
        matches!(
            catalog.lookup_session(&linked).await.unwrap(),
            lash_core::store::SessionLookup::Live(_)
        ),
        "deleting the parent leaves the child session"
    );
    let mut kept = linked_head.clone();
    let after = head(linked.clone()).await;
    kept.config_revision = after.config_revision;
    assert_eq!(after, kept, "deleting the parent leaves the child's config");
    let output = deployment
        .core
        .session(linked.clone())
        .durable()
        .await
        .unwrap()
        .send(lash::TurnInput::text(INPUT))
        .output()
        .await
        .unwrap();
    assert!(output.is_success(), "the child still runs: {output:?}");
    drop(deployment.backend);
}

on_every_tier!(a_linked_child_behaves_like_an_unlinked_session_and_outlives_its_parent);
on_every_tier!(a_child_uses_exactly_its_explicit_prompt_plan);
on_every_tier!(a_fork_starts_with_its_parents_full_configuration);
on_every_tier!(spawned_and_related_children_start_with_the_default_prompt_plan);

// --- a child's own turns ----------------------------------------------------

/// The bytes a child's tool writes.
const CHILD_BYTES: [u8; 4] = [4, 2, 4, 2];

/// Writes [`CHILD_BYTES`] through its call's attachment client and answers
/// the stored attachment, typed, so the turn's commit holds it.
struct WriteAttachment;

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for WriteAttachment {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let reference = match call
            .context
            .attachments()
            .put(
                CHILD_BYTES.to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("image/png").unwrap(),
                    Some(lash_core::AttachmentTypeMetadata::image(Some(2), Some(2))),
                    Some("child.png".to_owned()),
                ),
            )
            .await
        {
            Ok(reference) => reference,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
        ))
        .into()
    }
}

/// The host tool `write_attachment`.
fn write_attachment_tool() -> Arc<dyn lash_core::ToolProvider> {
    use lash_core::ToolDefinitionBindingExt as _;
    let definition = lash_core::ToolDefinition::raw(
        "write_attachment",
        "write_attachment",
        "Writes a small image attachment.",
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        serde_json::json!({}),
    )
    .expect("the attachment tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], "write_attachment"));
    Arc::new(lash::tools::StaticToolProvider::new(
        vec![definition],
        WriteAttachment,
    ))
}

/// A linked child's turn writes its attachment for itself: the child
/// session holds the reference its committed turn wrote, and its parent
/// holds none.
async fn a_linked_childs_attachment_is_held_by_the_child_alone(tier: Tier) {
    let model = scripted(|request, _| {
        if results(request).is_empty() {
            call(
                "child-attachment-call",
                "write_attachment",
                serde_json::json!({}),
            )
        } else {
            text(request, CHILD_REPLY)
        }
    });
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder
            .serve_test_llm_profile(model, metadata())
            .tools(write_attachment_tool())
    })
    .await;
    let parent = SessionId::from("attachment-parent");
    let child = SessionId::from("attachment-child");
    for (id, creation) in [
        (parent.clone(), lash::SessionCreation::root(spec())),
        (
            child.clone(),
            lash::SessionCreation::child_of(parent.clone(), spec()),
        ),
    ] {
        deployment.core.session(id).create(creation).await.unwrap();
    }
    let output = deployment
        .core
        .session(child.clone())
        .durable()
        .await
        .unwrap()
        .send(lash::TurnInput::text("write the attachment"))
        .output()
        .await
        .unwrap();
    assert!(output.is_success(), "{output:?}");
    let referrers = deployment
        .backend
        .attachment_referrers()
        .attachment_referrers(&lash_core::attachments::content_id(&CHILD_BYTES))
        .await
        .unwrap();
    assert!(
        referrers.contains(&lash_core::ArtifactReferrer::Session(child)),
        "the child holds the attachment its turn wrote: {referrers:?}"
    );
    assert!(
        !referrers.contains(&lash_core::ArtifactReferrer::Session(parent)),
        "the parent holds no reference to its child's attachment: {referrers:?}"
    );
    drop(deployment.backend);
}

// The PostgreSQL store set these laws deploy keeps no attachment bytes.
mod a_linked_childs_attachment_is_held_by_the_child_alone {
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn on_sqlite_memory() {
        super::a_linked_childs_attachment_is_held_by_the_child_alone(super::Tier::SqliteMemory)
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn on_sqlite_file() {
        super::a_linked_childs_attachment_is_held_by_the_child_alone(super::Tier::SqliteFile).await;
    }
}

/// The usage a model call reports.
fn usage(input: i64, output: i64, cache_read: i64, reasoning: i64) -> LlmUsage {
    LlmUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: reasoning,
    }
}

/// `response`, reporting `usage`.
fn reporting(response: LlmResponse, usage: &LlmUsage) -> LlmResponse {
    LlmResponse {
        usage: usage.clone(),
        ..response
    }
}

/// The usage each model call of `output`'s turn reported, in call order.
fn call_usage(output: &lash::TurnOutput) -> Vec<LlmUsage> {
    output
        .result
        .llm_calls
        .iter()
        .flat_map(|call| &call.attempts)
        .filter_map(|attempt| attempt.usage.clone())
        .collect()
}

/// Every session owns its usage: a parent turn whose `spawn_agent` child
/// ran inside it records only its own model calls' usage, and a linked
/// child's turn records its own, a cache-read-only call's included, on its
/// own turn and on no other.
async fn a_childs_model_usage_stays_on_the_childs_own_turn(tier: Tier) {
    const CACHED_CHILD_INPUT: &str = "usage law: answer from the cache";
    let parent_spawns = usage(11, 3, 0, 0);
    let parent_answers = usage(5, 1, 0, 0);
    let cached_child = usage(0, 0, 9, 0);
    let model = {
        let spawned_child = usage(7, 2, 4, 1);
        let (parent_spawns, parent_answers, cached_child) = (
            parent_spawns.clone(),
            parent_answers.clone(),
            cached_child.clone(),
        );
        scripted(move |request, transcript| {
            if transcript.contains(CHILD_TASK) && !transcript.contains(PARENT_INPUT) {
                return reporting(text(request, CHILD_REPLY), &spawned_child);
            }
            if transcript.contains(CACHED_CHILD_INPUT) {
                return reporting(text(request, CHILD_REPLY), &cached_child);
            }
            if results(request).is_empty() {
                return reporting(
                    call(
                        "usage-spawn",
                        "spawn_agent",
                        serde_json::json!({ "task": CHILD_TASK }),
                    ),
                    &parent_spawns,
                );
            }
            reporting(text(request, PARENT_DONE), &parent_answers)
        })
    };
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder
            .serve_test_llm_profile(model, metadata())
            .plugin(Arc::new(delegation()))
    })
    .await;
    let parent = settle(&deployment.core, "usage-parent", PARENT_INPUT).await;
    assert!(
        format!("{parent:?}").contains(PARENT_DONE),
        "the parent's turn ends on its own answer: {parent:?}"
    );
    assert_eq!(
        call_usage(&parent),
        vec![parent_spawns, parent_answers],
        "the parent records its own two calls and none of its child's"
    );
    assert_eq!(
        parent.result.usage,
        lash_core::TokenUsage {
            input_tokens: 16,
            output_tokens: 4,
            ..lash_core::TokenUsage::default()
        },
        "the parent report excludes its child's usage"
    );
    let child = SessionId::from("usage-linked-child");
    deployment
        .core
        .session(child.clone())
        .create(lash::SessionCreation::child_of(
            SessionId::from("usage-parent"),
            spec(),
        ))
        .await
        .unwrap();
    let cached = deployment
        .core
        .session(child)
        .durable()
        .await
        .unwrap()
        .send(lash::TurnInput::text(CACHED_CHILD_INPUT))
        .output()
        .await
        .unwrap();
    assert!(cached.is_success(), "{cached:?}");
    assert_eq!(
        call_usage(&cached),
        vec![cached_child],
        "the child's cache-read-only call is its own turn's"
    );
    assert_eq!(
        cached.result.usage,
        lash_core::TokenUsage {
            cache_read_input_tokens: 9,
            ..lash_core::TokenUsage::default()
        },
        "the linked child report includes its own cache-only usage"
    );
    drop(deployment.backend);
}

on_every_tier!(a_childs_model_usage_stays_on_the_childs_own_turn);

/// A turn's report carries the sum of the usage its own model calls
/// reported.
async fn a_turn_report_sums_its_own_model_calls_usage(tier: Tier) {
    let reported = usage(11, 3, 0, 0);
    let model = {
        let reported = reported.clone();
        scripted(move |request, _| reporting(text(request, CHILD_REPLY), &reported))
    };
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder.serve_test_llm_profile(model, metadata())
    })
    .await;
    let output = settle(&deployment.core, "report-usage", "report your usage").await;
    assert_eq!(call_usage(&output), vec![reported], "{output:?}");
    assert_eq!(
        output.result.usage,
        lash_core::TokenUsage {
            input_tokens: 11,
            output_tokens: 3,
            ..lash_core::TokenUsage::default()
        },
        "the report sums its own call's usage"
    );
    drop(deployment.backend);
}

/// A tool round makes two model calls, and the report sums every usage bucket.
async fn a_turn_report_sums_two_model_calls_usage(tier: Tier) {
    let first = LlmUsage {
        cache_write_input_tokens: 2,
        ..usage(11, 3, 7, 1)
    };
    let second = LlmUsage {
        cache_write_input_tokens: 4,
        ..usage(5, 2, 9, 1)
    };
    let model = {
        let (first, second) = (first.clone(), second.clone());
        scripted(move |request, _| {
            if results(request).is_empty() {
                reporting(
                    call(
                        "report-usage-tool",
                        "write_attachment",
                        serde_json::json!({}),
                    ),
                    &first,
                )
            } else {
                reporting(text(request, CHILD_REPLY), &second)
            }
        })
    };
    let deployment = deploy(tier, Vec::new(), |builder| {
        builder
            .serve_test_llm_profile(model, metadata())
            .tools(write_attachment_tool())
    })
    .await;
    let output = settle(&deployment.core, "report-two-calls", "write the attachment").await;
    assert_eq!(call_usage(&output), vec![first, second], "{output:?}");
    assert_eq!(
        output.result.usage,
        lash_core::TokenUsage {
            input_tokens: 16,
            output_tokens: 5,
            cache_read_input_tokens: 16,
            cache_write_input_tokens: 6,
            reasoning_output_tokens: 2,
        },
        "the report sums both calls, including cache and reasoning tokens"
    );
    drop(deployment.backend);
}

mod sqlite_memory_report_usage {
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_report_sums_its_own_model_calls_usage() {
        super::a_turn_report_sums_its_own_model_calls_usage(super::Tier::SqliteMemory).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_report_sums_two_model_calls_usage() {
        super::a_turn_report_sums_two_model_calls_usage(super::Tier::SqliteMemory).await;
    }
}

// --- host engines -----------------------------------------------------------

/// How a scripted engine answers each event: its state is a JSON value it
/// rewrites in place.
type Advance = fn(&mut serde_json::Value, lash_core::EngineEvent) -> lash_core::EngineAction;

/// A host process engine of `kind` whose transitions `advance` scripts.
struct ScriptEngine {
    kind: &'static str,
    advance: Advance,
}

fn infra(error: impl std::fmt::Display) -> lash_core::ProcessInfraError {
    lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
}

/// The terminal that carries `value` as the process's answer.
fn answer(value: serde_json::Value) -> lash_core::EngineAction {
    lash_core::EngineAction::Terminal(lash_core::ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::success(value),
    ))
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for ScriptEngine {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: self.kind.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        let mut script = if state.bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&state.bytes).map_err(infra)?
        };
        let action = (self.advance)(&mut script, event);
        Ok((
            lash_core::EngineState {
                format: self.state_format(),
                bytes: serde_json::to_vec(&script).map_err(infra)?,
            },
            action,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(format!(
            "a scripted engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionRefusal> {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// A cancel answered at once: what every scripted engine does with one.
fn cancelled(origin: lash_sansio::CancelOrigin) -> lash_core::EngineAction {
    lash_core::EngineAction::Terminal(lash_core::ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::cancelled(
            lash_core::ToolCancellation::runtime("the scripted engine answered its cancel")
                .with_origin(origin),
        ),
    ))
}

/// What a settled step answered: a tool step's output value, an engine
/// step's payload, or its outcome when it names no material.
fn settled(outcome: &lash_core::SettledOutput) -> serde_json::Value {
    let Some(payload) = outcome.payload() else {
        return format!("{:?}", outcome.outcome()).into();
    };
    match serde_json::from_str::<lash_core::ToolCallOutput>(payload) {
        Ok(output) => output.value_for_projection(),
        Err(_) => serde_json::from_str(payload).unwrap_or_else(|_| payload.into()),
    }
}

/// The host tool a tool step runs.
const WRITE_TOOL: &str = "core_node_write";

/// The outside world: what the write tool wrote, per call. It outlives
/// every node, as the world outside a deployment does.
#[derive(Debug, Default)]
struct World {
    writes: Mutex<Vec<(lash_core::ToolCallId, serde_json::Value)>>,
}

struct Write {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for Write {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .writes
            .lock()
            .unwrap()
            .push((call.context.call_id().clone(), call.args.clone()));
        lash_core::ToolOutcome::ok(serde_json::json!({ "wrote": call.args })).into()
    }
}

/// The `Once` host tool that writes to `world`.
fn write_tool(world: &Arc<World>) -> Arc<dyn lash_core::ToolProvider> {
    use lash_core::ToolDefinitionBindingExt as _;
    let definition = lash_core::ToolDefinition::raw(
        WRITE_TOOL,
        WRITE_TOOL,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the write tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], WRITE_TOOL));
    Arc::new(lash::tools::StaticToolProvider::new(
        vec![definition],
        Write {
            world: Arc::clone(world),
        },
    ))
}

/// The environment every host start in these laws captures: the core's
/// plugins at their defaults, and the standard protocol's builtin renderer
/// at its defaults, which its tools' outputs are rendered by.
fn environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
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

/// A dangling environment is a typed admission refusal, before a process
/// or its runnable actor exists. Repeating the same start refuses identically
/// (FIG-5341).
async fn a_start_with_a_missing_environment_is_refused_at_admission(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let registry = stores.process_registry();
    let env_store = stores.process_env_store();
    let spec = environment();
    let env_ref = spec.stable_ref().expect("the environment's reference");
    let key = lash_core::StartKey::for_host("missing-environment-law");
    let registration = lash_core_execution::testing::held_engine_registration(
        serde_json::Value::Null,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(key.clone()))
    .with_execution_env_ref(Some(env_ref.clone()));
    let mut first = None;
    for source_ends_after_prepare in [false, true] {
        let pin = lash_core_execution::testing::host_pin_claim_for_testing();
        if source_ends_after_prepare {
            lash_core::publish_process_execution_env(env_store.as_ref(), &pin, &spec)
                .await
                .expect("the reference exists during preparation");
        }
        let prepared = registry
            .prepare_process_registration(registration.clone(), &[])
            .await
            .expect("preparation is read-only");
        let process = prepared.process_id().clone();
        let anchor = prepared.trace().anchor().clone();
        if source_ends_after_prepare {
            env_store
                .end_process_env_referrer(&lash_core::ResolvedArtifactCleanup {
                    referrer: pin.referrer(),
                    carries: Vec::new(),
                })
                .await
                .expect("the source ends before admission commits");
        }
        let error = registry
            .commit_process_registration(prepared, anchor)
            .await
            .expect_err("a reference with no stored bytes is refused at admission");
        let lash_core::PluginError::Runtime(runtime) = &error else {
            panic!("the refusal keeps its typed runtime error: {error:?}");
        };
        assert_eq!(runtime.code, lash_core::RuntimeErrorCode::ArtifactMissing);
        assert_eq!(error.class(), lash_core::PluginErrorClass::Terminal);
        assert!(error.to_string().contains(env_ref.as_str()));
        let encoded = serde_json::to_value(runtime).expect("typed refusal encodes");
        if let Some(first) = &first {
            assert_eq!(&encoded, first, "a repeat has the same typed refusal");
        } else {
            first = Some(encoded);
        }
        assert!(
            registry
                .get_process_by_start_key(&key)
                .await
                .expect("read the start")
                .is_none(),
            "a refused start writes no process"
        );
        let actor = lash_durable::ActorKey::process(process.as_str()).expect("the actor key");
        assert!(
            stores
                .durable_store()
                .actor(&actor)
                .await
                .expect("read the actor")
                .is_none(),
            "a refused start writes no runnable actor"
        );
    }
}

on_every_tier!(a_start_with_a_missing_environment_is_refused_at_admission);

/// Admission holds the environment under the process record in its own
/// transaction, so ending the source pin cannot strand an admitted start
/// (FIG-5341).
async fn an_admitted_process_keeps_its_environment_after_the_source_pin_ends(tier: Tier) {
    let (stores, _keep) = stores(tier).await;
    let registry = stores.process_registry();
    let env_store = stores.process_env_store();
    let pin = lash_core_execution::testing::host_pin_claim_for_testing();
    let spec = environment();
    let env_ref = lash_core::publish_process_execution_env(env_store.as_ref(), &pin, &spec)
        .await
        .expect("publish the source environment");
    let registration = lash_core_execution::testing::held_engine_registration(
        serde_json::Value::Null,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(
        "admitted-environment-law",
    )))
    .with_execution_env_ref(Some(env_ref.clone()));
    let admitted = registry
        .register_process(registration.clone())
        .await
        .expect("admit the start");
    env_store
        .end_process_env_referrer(&lash_core::ResolvedArtifactCleanup {
            referrer: pin.referrer(),
            carries: Vec::new(),
        })
        .await
        .expect("end the source pin");
    assert_eq!(
        lash_core::runtime::load_process_execution_env(env_store.as_ref(), &env_ref)
            .await
            .expect("the admitted process still resolves its environment"),
        spec
    );
    assert_eq!(
        registry
            .register_process(registration)
            .await
            .expect("repeat the retained start")
            .id,
        admitted.id
    );
}

on_every_tier!(an_admitted_process_keeps_its_environment_after_the_source_pin_ends);

/// Start a detached process of engine `kind` on `payload` through the
/// core's process API, under the captured [`environment`].
async fn start(
    core: &lash::LashCore,
    kind: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessId {
    start_as(core, kind, payload, |request| request).await
}

/// [`start`], its request as `shape` makes it.
async fn start_as(
    core: &lash::LashCore,
    kind: &str,
    payload: serde_json::Value,
    shape: impl FnOnce(lash_core::ProcessStartRequest) -> lash_core::ProcessStartRequest,
) -> lash_core::ProcessId {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: kind.to_owned(),
            payload,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_env_ref(env_ref);
    core.processes()
        .start(shape(request), core.effect_host())
        .await
        .expect("the process starts")
        .process_id
}

/// The answer `process` ended with, waited for through the core's process
/// API.
async fn ended(core: &lash::LashCore, process: &lash_core::ProcessId) -> lash_core::ToolCallOutput {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        core.processes().await_output(process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    match output {
        lash_core::ProcessAwaitOutput::Settled { output } => *output,
        other => panic!("the process ended without an answer: {other:?}"),
    }
}

/// The value a successful answer carries.
fn success(output: &lash_core::ToolCallOutput) -> serde_json::Value {
    assert!(output.is_success(), "the process failed: {output:?}");
    output.value_for_projection()
}

/// The tool engine: it runs the write tool once on its start's `x` (a
/// trigger start carries its subscription's payload as `args`), and ends
/// with what the step answered.
const TOOL_ENGINE: &str = "core-node-tool-engine";

fn tool_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { payload } => {
            let x = payload
                .pointer("/x")
                .or_else(|| payload.pointer("/args/x"))
                .cloned()
                .unwrap_or_default();
            lash_core::EngineAction::Steps(vec![lash_core::StepRequest::Tool {
                language_execution: None,
                step: lash_core::StepName("write".to_owned()),
                tool: lash_core::ToolId::new(WRITE_TOOL),
                input: serde_json::json!({ "x": x }),
                site: None,
            }])
        }
        lash_core::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// A host engine started through the core's process API runs its Steps
/// action on the core's node: the catalog tool it names runs once, under
/// its `Once` policy, and the engine reads the tool's output as the step's
/// answer.
async fn host_engine_step_runs_its_catalog_tool_once(tier: Tier) {
    let world = Arc::new(World::default());
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: TOOL_ENGINE,
            advance: tool_engine_advance,
        })],
        |builder| builder.tools(write_tool(&world)),
    )
    .await;
    let process = start(&deployment.core, TOOL_ENGINE, serde_json::json!({ "x": 7 })).await;
    let answer = success(&ended(&deployment.core, &process).await);
    let writes = world.writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 1, "the tool ran once: {writes:?}");
    assert_eq!(writes[0].1, serde_json::json!({ "x": 7 }));
    assert_eq!(
        answer,
        serde_json::json!({ "step": { "wrote": { "x": 7 } } }),
        "the engine read the tool's output"
    );
}

on_every_tier!(host_engine_step_runs_its_catalog_tool_once);

// --- engine steps -----------------------------------------------------------

/// The engine-step engine: it runs its own `double` body once on its start
/// payload, and ends with what the body answered.
fn engine_step_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { payload } => {
            lash_core::EngineAction::Steps(vec![lash_core::StepRequest::Engine {
                step: lash_core::StepName("double".to_owned()),
                kind: lash_core::EngineStepKind::new(DOUBLE),
                input: payload,
            }])
        }
        lash_core::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// The engine body a plugin's engine declares.
const DOUBLE: &str = "double";

/// A plugin-contributed engine that declares the `double` body.
const PLUGIN_ENGINE: &str = "core-node-plugin-engine";

/// A host engine registered on the backend, which declares no body.
const BARE_ENGINE: &str = "core-node-bare-engine";

/// The `double` body: it answers twice its input, counting its runs.
struct Double {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for Double {
    fn kinds(&self) -> Vec<lash_core::EngineStepKind> {
        vec![lash_core::EngineStepKind::new(DOUBLE)]
    }

    fn execution(&self, _kind: &lash_core::EngineStepKind) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    async fn run(
        &self,
        run: lash_core::EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> lash_core::SettledOutput {
        use lash_core::tool_run::{MaterialOwner, MaterialRole};
        self.runs.fetch_add(1, Ordering::SeqCst);
        let text = (run.input.as_i64().unwrap_or_default() * 2).to_string();
        lash_core::SettledOutput::Completed(lash_core::Material::journal_local(
            MaterialOwner::Process {
                process_id: run.process,
            },
            MaterialRole::AttemptOutput,
            text,
        ))
    }
}

/// A plugin that contributes [`PLUGIN_ENGINE`] with its `double` body, as a
/// host's engine plugin does.
struct EnginePlugin {
    runs: Arc<AtomicUsize>,
}

struct NoSessionPlugin;

impl lash_core::plugin::SessionPlugin for NoSessionPlugin {
    fn id(&self) -> &'static str {
        "core-node-engine-plugin"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for EnginePlugin {
    fn id(&self) -> &'static str {
        "core-node-engine-plugin"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![
            lash_core::ProcessEngineRegistration::accepting(Arc::new(ScriptEngine {
                kind: PLUGIN_ENGINE,
                advance: engine_step_advance,
            }))
            .with_engine_steps(Arc::new(Double {
                runs: Arc::clone(&self.runs),
            })),
        ])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for EnginePlugin {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("core-node-engine-plugin")
    }
}

/// An engine a plugin contributes advances on the core's node as a host's
/// does, and its engine step runs through the `EngineSteps` its
/// registration declares, once.
async fn plugin_engine_step_runs_through_its_registration(tier: Tier) {
    let runs = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(EnginePlugin {
        runs: Arc::clone(&runs),
    });
    let deployment = deploy(tier, Vec::new(), |builder| builder.plugin(plugin)).await;
    let process = start(&deployment.core, PLUGIN_ENGINE, serde_json::json!(21)).await;
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "step": 42 }));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the body ran once");
}

on_every_tier!(plugin_engine_step_runs_through_its_registration);

/// An engine step of an engine whose registration declares no engine
/// steps is refused before its admission: the process ends `Failed` with
/// the typed refusal, and no step was admitted or run.
async fn engine_step_without_the_capability_is_refused_before_admission(tier: Tier) {
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: BARE_ENGINE,
            advance: engine_step_advance,
        })],
        |builder| builder,
    )
    .await;
    let process = start(&deployment.core, BARE_ENGINE, serde_json::json!(21)).await;
    let output = ended(&deployment.core, &process).await;
    assert!(!output.is_success(), "the process failed: {output:?}");
    let refusal = lash_core::EngineStepRefusal::NoEngineSteps {
        engine: BARE_ENGINE.to_owned(),
    }
    .to_string();
    assert!(
        format!("{output:?}").contains(&refusal),
        "the process ends with the typed refusal `{refusal}`: {output:?}"
    );
    let rows = deployment
        .backend
        .durable()
        .run_records(&lash_durable::domain::OwnerKey::Process(process))
        .await
        .expect("the process's run records are read");
    assert!(rows.is_empty(), "no step was admitted: {rows:?}");
}

on_every_tier!(engine_step_without_the_capability_is_refused_before_admission);

// --- signals ----------------------------------------------------------------

/// The signal engine: it waits for a signal and ends with its payload.
const SIGNAL_ENGINE: &str = "core-node-signal-engine";

fn signal_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Signal(signal) => {
            answer(serde_json::json!({ "signal": signal.payload }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// The event type of the signal named `name`, which a process that takes
/// it declares.
fn signal_type(name: &str) -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: format!("signal.{name}"),
        payload_schema: lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "object" }))
            .expect("the signal's schema"),
        semantics: Default::default(),
    }
}

/// Wait until `process`'s actor is released waiting, with nothing to run.
async fn parked(backend: &lash::Backend, process: &lash_core::ProcessId) {
    let actor = lash_durable::ActorKey::process(process.as_str()).expect("a process actor key");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let snapshot = backend
                .durable()
                .actor(&actor)
                .await
                .expect("the actor is read");
            if snapshot.is_some_and(|snapshot| snapshot.state == lash_durable::ActorState::Waiting)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process parks within a minute");
}

/// A host's signal through the core's process API wakes a process parked
/// waiting for it: its engine reads the signal and ends with its payload.
async fn host_signal_wakes_a_parked_process(tier: Tier) {
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: SIGNAL_ENGINE,
            advance: signal_engine_advance,
        })],
        |builder| builder,
    )
    .await;
    let process = start_as(
        &deployment.core,
        SIGNAL_ENGINE,
        serde_json::Value::Null,
        |request| request.with_event_types([signal_type("go")]),
    )
    .await;
    parked(&deployment.backend, &process).await;
    let identity = lash_core::ProcessSignalIdentity::new(process.clone(), "go", "core-node-signal")
        .expect("a signal identity");
    deployment
        .core
        .processes()
        .signal(
            lash_core::ProcessSignal::new(identity, serde_json::json!({ "go": true })),
            deployment.core.effect_host(),
        )
        .await
        .expect("the signal is delivered");
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "signal": { "go": true } }));
}

on_every_tier!(host_signal_wakes_a_parked_process);

/// A host start is its own operation (ADR 0113 §3.3): its run preparation
/// holds what it names under its own staging, `Start(key)`, until its
/// registration carries it onto the process record. It never holds through
/// the core's host context, a scope every host start shares and that has
/// settled as soon as the cleanup relay reads it. So after one start's
/// source pin is released and the relay has run, its process still
/// resolves its environment, and the next host start prepares and
/// registers (FIG-5384).
async fn a_host_starts_preparation_is_held_by_its_own_start(tier: Tier) {
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: SIGNAL_ENGINE,
            advance: signal_engine_advance,
        })],
        |builder| builder,
    )
    .await;
    let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
        &deployment.backend,
        lash_core::ProcessEngineRegistry::default(),
    );
    let env_store = deployment.backend.process_env_store();
    let artifacts = deployment.core.host_artifacts();
    let mut started = Vec::new();
    for run in 0..2 {
        let pin = lash_core::HostArtifactPin::mint();
        let env_ref = artifacts
            .publish_process_env(&pin, &environment())
            .await
            .expect("the environment is published");
        let request = lash_core::ProcessStartRequest::new(
            lash_core::ProcessInput::Engine {
                kind: SIGNAL_ENGINE.to_owned(),
                payload: serde_json::json!({ "run": run }),
            },
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_env_ref(env_ref.clone());
        let receipt = deployment
            .core
            .processes()
            .start(request, deployment.core.effect_host())
            .await;
        artifacts.release(pin).await.expect("the pin is released");
        let process = receipt
            .unwrap_or_else(|error| panic!("host start {run} prepares and registers: {error}"))
            .process_id;
        // Every cleanup the start and the release armed is due a day on.
        let later = lash_core::testing::TestClock::new(
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("the wall clock is past the epoch")
                    .as_millis(),
            )
            .expect("milliseconds fit")
                + 86_400_000,
        );
        for _ in 0..2 {
            lash_core::runtime::obligations::relay::relay_due(
                &relay,
                &later,
                std::num::NonZeroUsize::new(256).expect("a page"),
            )
            .await
            .expect("the cleanup relay's due pass");
        }
        assert_eq!(
            lash_core::runtime::load_process_execution_env(env_store.as_ref(), &env_ref)
                .await
                .expect("the started process still resolves its environment"),
            environment()
        );
        started.push(process);
    }
    assert_ne!(
        started[0], started[1],
        "each start registered its own process"
    );
}

on_every_tier!(a_host_starts_preparation_is_held_by_its_own_start);

/// The tool whose call declares a `SignalProcess` intent.
const SIGNAL_TOOL: &str = "core_node_signal";

/// A tool whose call answers at once and declares one `SignalProcess`
/// intent: the signal `resume` to `process`, from `session`.
struct SignalIntent {
    session: SessionId,
    process: Arc<Mutex<Option<lash_core::ProcessId>>>,
    calls: Arc<AtomicUsize>,
}

fn signal_intent_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{SIGNAL_TOOL}"),
        SIGNAL_TOOL,
        "Signal a parked process through the public intent path.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("the signal tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(
        lash_core::ToolDeclaration::default()
            .with_intents([lash_core::ToolIntentKind::SignalProcess]),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SignalIntent {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![signal_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SIGNAL_TOOL).then(|| Arc::new(signal_intent_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let process = self
            .process
            .lock()
            .unwrap()
            .clone()
            .expect("the target is started before the turn");
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "signalled": true })),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::SignalProcess(
                lash_core::SignalProcessIntent {
                    owner: lash_core::RuntimeOwner::Session(self.session.clone()),
                    process_id: process,
                    signal_name: "resume".to_owned(),
                    payload: serde_json::json!({ "tier": "durable" }),
                },
            )]),
        )
    }
}

const SIGNALLER: &str = "core-node-signaller";

/// A tool's `SignalProcess` intent, from a turn sent to the core, wakes the
/// process it names, parked on its process actor: the signal is the
/// process's mail, its engine reads it once and ends with its payload, and
/// its event log holds the one signal.
async fn public_signal_intent_wakes_parked_process(tier: Tier) {
    let session = SessionId::try_from(SIGNALLER.to_owned()).unwrap();
    let target = Arc::new(Mutex::new(None));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model = {
        let model_calls = Arc::clone(&model_calls);
        scripted(
            move |request, _| match model_calls.fetch_add(1, Ordering::SeqCst) {
                0 => call("core-node-signal-call", SIGNAL_TOOL, serde_json::json!({})),
                1 => text(request, "signal delivered"),
                index => panic!("an unexpected model call {index}"),
            },
        )
    };
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(SignalIntent {
        session: session.clone(),
        process: Arc::clone(&target),
        calls: Arc::clone(&tool_calls),
    });
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: SIGNAL_ENGINE,
            advance: signal_engine_advance,
        })],
        |builder| {
            builder
                .serve_test_llm_profile(model, metadata())
                .tools(tools)
        },
    )
    .await;
    let process = start_as(
        &deployment.core,
        SIGNAL_ENGINE,
        serde_json::Value::Null,
        |request| {
            request
                .with_event_types([signal_type("resume")])
                .with_observers([session.clone()])
        },
    )
    .await;
    parked(&deployment.backend, &process).await;
    *target.lock().unwrap() = Some(process.clone());
    settle(&deployment.core, SIGNALLER, "signal the parked process").await;
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "the tool ran once");
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        2,
        "the model was asked twice"
    );
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(
        answer,
        serde_json::json!({ "signal": { "tier": "durable" } })
    );
    let signals = deployment
        .backend
        .process_registry()
        .full_event_window(&process, 0)
        .await
        .expect("the process's events are read")
        .into_iter()
        .filter(|event| event.event_type == "signal.resume")
        .count();
    assert_eq!(signals, 1, "the process received one signal");
}

on_every_tier!(public_signal_intent_wakes_parked_process);

// --- triggers ---------------------------------------------------------------

const TRIGGER_SOURCE: &str = "core-node.event";
const TRIGGER_KEY: &str = "core-node-source";

/// An occurrence a host emits through the core's trigger API starts the
/// process its subscription names, and the core's node runs it: its tool
/// step writes once and its terminal carries the tool's answer.
async fn trigger_started_process_runs_on_the_core_node(tier: Tier) {
    let world = Arc::new(World::default());
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: TOOL_ENGINE,
            advance: tool_engine_advance,
        })],
        |builder| builder.tools(write_tool(&world)),
    )
    .await;
    let core = &deployment.core;
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    deployment
        .backend
        .trigger_store()
        .execute_command(
            "core-node-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("core-node").expect("a host scope"),
                actor: lash_core::ProcessOriginator::host_scoped("core-node"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "core-node/write",
                    env_ref,
                    TRIGGER_SOURCE,
                    TRIGGER_KEY,
                    lash_core::ProcessInput::Engine {
                        kind: TOOL_ENGINE.to_owned(),
                        payload: serde_json::json!({ "x": 5 }),
                    },
                    lash_core::ProcessIdentity::labelled(TOOL_ENGINE, Some("write")),
                )
                .with_payload_schema(lash_sansio::JsonSchema::any()),
            },
        )
        .await
        .expect("the subscription is registered")
        .expect("the subscription is admitted");
    let report = core
        .triggers()
        .emit(
            lash_core::TriggerOccurrenceRequest::new(
                TRIGGER_SOURCE,
                TRIGGER_KEY,
                serde_json::json!({ "fired": true }),
                "core-node-occurrence",
            ),
            core.effect_host(),
        )
        .await
        .expect("the occurrence is emitted");
    let started = report.started_process_ids();
    assert_eq!(
        started.len(),
        1,
        "the occurrence started one process: {report:?}"
    );
    let answer = success(&ended(core, &started[0]).await);
    assert_eq!(
        answer,
        serde_json::json!({ "step": { "wrote": { "x": 5 } } })
    );
    let writes = world.writes.lock().unwrap().clone();
    assert_eq!(
        writes.len(),
        1,
        "the started process's tool ran once: {writes:?}"
    );
}

on_every_tier!(trigger_started_process_runs_on_the_core_node);

// --- lashlang ---------------------------------------------------------------

/// The lashlang program a law's process runs: two sleeps, then its answer.
const LASHLANG_SOURCE: &str = "process worker() -> str { sleep(5); sleep(7); finish \"done\" }";

/// An RLM core over `backend`: its protocol plugin contributes the lashlang
/// engine and its `vm_run` body.
fn rlm_core(backend: &lash::Backend) -> lash::LashCoreBuilder {
    use lash::rlm::Dialect as _;
    lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            backend,
        )
        .with_worker_service(lash::rlm::TypescriptDialect.worker_service()),
    )
}

/// Publish the worker module under a host pin, and answer the start payload
/// of its `worker` process.
async fn worker_payload(backend: &lash::Backend) -> serde_json::Value {
    use lashlang::testing::ast_builders as b;
    lashlang_payload(
        backend,
        LASHLANG_SOURCE,
        b::process_returning(
            "worker",
            Vec::new(),
            lashlang::TypeExpr::Str,
            b::block(vec![
                b::sleep_for(b::num(5.0)),
                b::sleep_for(b::num(7.0)),
                b::finish(b::string("done")),
            ]),
        ),
    )
    .await
}

/// Compile the module of the one process `declaration` declares from
/// `source`, publish it under a host pin, and answer that process's start
/// payload.
async fn lashlang_payload(
    backend: &lash::Backend,
    source: &str,
    declaration: lashlang::Declaration,
) -> serde_json::Value {
    use lashlang::testing::ast_builders as b;
    let lashlang::Declaration::Process(process) = &declaration else {
        panic!("the module declares a process");
    };
    let name = process.name.to_string();
    let environment = lash_lashlang_runtime::LashlangSurface::default()
        .for_process_registry(true)
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the host environment");
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        program: b::module(vec![declaration], Vec::new()),
        environment: &environment,
    })
    .expect("the process module compiles");
    lashlang::LashlangArtifacts::of_backend(backend)
        .publish_module_artifact(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &output.artifact,
        )
        .await
        .expect("the process module publishes");
    serde_json::to_value(lash_lashlang_runtime::LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref(&name)
            .expect("the process export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: name,
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}

/// A lashlang process started through the core's process API runs on the
/// core's node: its VM runs only in its `vm_run` engine steps, from one
/// committed snapshot to the next across its sleeps, to its terminal.
async fn lashlang_process_runs_to_its_terminal(tier: Tier) {
    let deployment = deploy_with(tier, Vec::new(), rlm_core).await;
    let payload = worker_payload(&deployment.backend).await;
    let process = start(
        &deployment.core,
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        payload,
    )
    .await;
    let output = ended(&deployment.core, &process).await;
    assert_eq!(success(&output), serde_json::json!("done"));
    let steps = deployment
        .backend
        .durable()
        .run_records(&lash_durable::domain::OwnerKey::Process(process))
        .await
        .expect("the process's run records are read");
    assert!(
        steps
            .iter()
            .any(|row| format!("{row:?}").contains(lash_lashlang_runtime::engine::VM_RUN_STEP)),
        "the VM ran in its vm_run steps: {steps:?}"
    );
}

on_every_tier!(lashlang_process_runs_to_its_terminal);

/// A lashlang process waiting on its signal parks idle on its process
/// actor; a host's signal through the core's process API is the process's
/// mail, and its VM resumes with the signal's payload as the wait's value
/// and ends with it.
async fn lashlang_signal_wait_resolves_with_the_hosts_signal(tier: Tier) {
    use lashlang::testing::ast_builders as b;
    let deployment = deploy_with(tier, Vec::new(), rlm_core).await;
    let payload = lashlang_payload(
        &deployment.backend,
        "process listen() signals { ready: any } { let payload = wait_signal(ready); finish payload }",
        b::process_with_signals(
            "listen",
            Vec::new(),
            vec![b::signal("ready", lashlang::TypeExpr::Any)],
            b::block(vec![
                b::assign("payload", b::wait_signal("ready")),
                b::finish(b::var("payload")),
            ]),
        ),
    )
    .await;
    let process = start_as(
        &deployment.core,
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        payload,
        |request| request.with_event_types([signal_type("ready")]),
    )
    .await;
    parked(&deployment.backend, &process).await;
    let identity =
        lash_core::ProcessSignalIdentity::new(process.clone(), "ready", "lashlang-signal")
            .expect("a signal identity");
    deployment
        .core
        .processes()
        .signal(
            lash_core::ProcessSignal::new(identity, serde_json::json!({ "received": true })),
            deployment.core.effect_host(),
        )
        .await
        .expect("the signal is delivered");
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "received": true }));
}

on_every_tier!(lashlang_signal_wait_resolves_with_the_hosts_signal);

// --- isolated tools ---------------------------------------------------------

/// The engine an isolated call binds: it ends with the payload it started
/// on.
const ISOLATED_ENGINE: &str = "core-node-isolated-engine";
const ISOLATED_TOOL: &str = "iso_run";

fn isolated_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { payload } => {
            answer(serde_json::json!({ "isolated": payload }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// A native tool declared isolated, which its provider binds to
/// [`ISOLATED_ENGINE`] when `bound`; its ordinary body counts its runs.
struct IsolatedTools {
    bound: bool,
    executions: AtomicUsize,
}

fn isolated_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::ToolDefinition::raw(
        "tool:iso_run",
        ISOLATED_TOOL,
        "Runs in an isolated process.",
        serde_json::json!({
            "type": "object",
            "properties": { "label": { "type": "string" } },
            "required": ["label"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the isolated tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["iso"], "run"))
    .with_declaration(lash_core::ToolDeclaration {
        isolated: true,
        ..lash_core::ToolDeclaration::default()
    })
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for IsolatedTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![isolated_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == ISOLATED_TOOL).then(|| Arc::new(isolated_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!({ "ordinary": true })).into()
    }

    fn isolated_process(
        &self,
        call: lash_core::IsolatedProcessRequest<'_>,
    ) -> Option<lash_core::IsolatedProcessBinding> {
        self.bound.then(|| lash_core::IsolatedProcessBinding {
            engine: ISOLATED_ENGINE.to_owned(),
            payload: call.args.clone(),
        })
    }
}

/// An RLM turn whose cell calls the isolated tool: the deployment, its
/// tools, the turn's output and every request its model saw.
async fn isolated_turn(
    tier: Tier,
    bound: bool,
) -> (
    Deployment,
    Arc<IsolatedTools>,
    lash::TurnOutput,
    Vec<String>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let model = {
        let seen = Arc::clone(&seen);
        scripted(move |request, transcript| {
            let mut seen = seen.lock().unwrap();
            seen.push(transcript.to_owned());
            let source = if seen.len() == 1 {
                "const started = await iso.run({label: \"isolated-law\"});\nfinish(started);"
            } else {
                "finish(\"asked again\");"
            };
            text(request, &format!("<typescript>\n{source}\n</typescript>"))
        })
    };
    let tools = Arc::new(IsolatedTools {
        bound,
        executions: AtomicUsize::new(0),
    });
    let deployment = deploy_with(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: ISOLATED_ENGINE,
            advance: isolated_engine_advance,
        })],
        {
            let tools = Arc::clone(&tools);
            move |backend| {
                rlm_core(backend)
                    .serve_test_llm_profile(model, metadata())
                    .tools(tools)
            }
        },
    )
    .await;
    let output = settle(&deployment.core, "isolated-call", "call the isolated tool").await;
    let seen = seen.lock().unwrap().clone();
    (deployment, tools, output, seen)
}

/// The processes of the isolated engine the registry holds.
async fn isolated_processes(backend: &lash::Backend) -> Vec<lash_core::ProcessRecord> {
    backend
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("the registry lists its processes")
        .into_iter()
        .filter(|record| {
            matches!(
                record.input.as_ref(),
                lash_core::ProcessInput::Engine { kind, .. } if kind == ISOLATED_ENGINE
            )
        })
        .collect()
}

/// L08/D04: an RLM cell's call of a tool declared isolated, which its
/// provider binds to a registered engine, starts exactly one lash process
/// under an isolated start key, before any body runs: no ordinary body
/// runs, the engine runs the process, and the call answers its descriptor.
async fn an_isolated_rlm_tool_starts_one_process_and_answers_its_descriptor(tier: Tier) {
    let (deployment, tools, output, _) = isolated_turn(tier, true).await;
    assert!(output.is_success(), "{output:?}");
    let descriptor: lash_core::tool_dispatch::IsolatedProcessDescriptor = serde_json::from_value(
        output
            .final_value()
            .cloned()
            .unwrap_or_else(|| panic!("the cell finishes with the call's answer: {output:?}")),
    )
    .unwrap_or_else(|error| panic!("the call answers its descriptor ({error}): {output:?}"));
    assert!(
        descriptor
            .start_key
            .as_str()
            .starts_with("process-start-key:v1:isolated:"),
        "{:?}",
        descriptor.start_key
    );
    let processes = isolated_processes(&deployment.backend).await;
    assert_eq!(processes.len(), 1, "one lash process: {processes:#?}");
    assert_eq!(processes[0].id, descriptor.process_id);
    assert_eq!(processes[0].start_key.as_ref(), Some(&descriptor.start_key));
    assert_eq!(
        tools.executions.load(Ordering::SeqCst),
        0,
        "no ordinary body"
    );
    assert_eq!(
        success(&ended(&deployment.core, &descriptor.process_id).await),
        serde_json::json!({ "isolated": { "label": "isolated-law" } }),
        "the engine ran the one process on the call's arguments"
    );
}

/// L08/D04: an isolated tool its provider binds to no engine is refused at
/// admission: the cell's call answers the typed refusal, and no body,
/// process or engine run starts.
async fn an_unbound_isolated_tool_is_refused_typed_before_any_body(tier: Tier) {
    let (deployment, tools, output, seen) = isolated_turn(tier, false).await;
    assert!(output.is_success(), "{output:?}");
    assert_eq!(seen.len(), 2, "the model saw the refused cell: {seen:#?}");
    assert!(
        seen[1].contains(
            "was refused at admission: the tool is declared isolated and no process \
             implementation is bound to it"
        ),
        "the cell's call answered the unsupported-isolation refusal: {}",
        seen[1]
    );
    assert_eq!(
        tools.executions.load(Ordering::SeqCst),
        0,
        "no ordinary body"
    );
    assert!(
        isolated_processes(&deployment.backend).await.is_empty(),
        "no process"
    );
}

on_every_tier!(an_isolated_rlm_tool_starts_one_process_and_answers_its_descriptor);
on_every_tier!(an_unbound_isolated_tool_is_refused_typed_before_any_body);

#[path = "core_node_processes/per_process_surface.rs"]
mod per_process_surface;

#[path = "core_node_processes/process_endstates.rs"]
mod process_endstates;
