//! A session's prompt is recorded config (FIG-4589): since FIG-5257 the
//! protocols contribute keyed prompt sections, and the session's recorded
//! prompt plan places them. A session records its plan when it is created,
//! from the creation its host states: a core keeps no default (FIG-4594),
//! so a host's default plan is a value it keeps and passes. A plan command
//! changes it for the runs after it; and a run's options cannot state a
//! prompt. So:
//!
//! - a session created under one deployment's plan is served that plan on
//!   every later run, whatever plan the deployment reopening it passes to
//!   the sessions it creates;
//! - a plan command reaches the next run and never the running one;
//! - a run-options payload that carries a prompt is refused, typed.
//!
//! A child records exactly its own creation's plan whatever its parent's
//! (FIG-5296): `core_node_processes.rs` in lash-durable-test holds that law.

use super::*;
use crate::prompt::{PromptPlacement, PromptPlan, PromptSectionId, PromptSectionKey};

/// The section the first deployment's plan places.
const DEFAULTS_A: &str = "INTRO OF THE CREATING DEPLOYMENT'S PLAN";
/// The section the later deployment's plan places: no session created
/// before it may be served it.
const DEFAULTS_B: &str = "INTRO OF THE REDEPLOYED PLAN";
/// The section a config command's plan places.
const COMMANDED: &str = "INTRO A CONFIG COMMAND PLACED";
/// Every section the plugin registers, by key, with its text.
const SECTIONS: [(&str, &str); 3] = [
    ("defaults-a", DEFAULTS_A),
    ("defaults-b", DEFAULTS_B),
    ("commanded", COMMANDED),
];
/// The plugin that registers the sections.
const PLUGIN: &str = "recorded-prompt-sections";

fn section(key: &str) -> PromptSectionId {
    PromptSectionId::new(
        PLUGIN,
        PromptSectionKey::new(key).expect("valid section key"),
    )
}

/// The plan that places the section of `key` in the initial instructions;
/// every other section keeps its default, `Excluded`.
fn placing(key: &str) -> PromptPlan {
    PromptPlan {
        placements: vec![crate::prompt::PromptSectionPlacement {
            section: section(key),
            placement: PromptPlacement::InitialInstructions,
        }],
        ..PromptPlan::default()
    }
}

/// Registers every [`SECTIONS`] entry, excluded unless a plan places it.
#[derive(Clone)]
struct Sections;

impl lash_core::plugin::PluginDefinition for Sections {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(PLUGIN)
    }
}

impl PluginFactory for Sections {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::facade_support::SessionPlugin for Sections {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        for (key, text) in SECTIONS {
            reg.prompt().section(
                crate::plugins::PromptSectionSpec::new(
                    PromptSectionKey::new(key).expect("valid section key"),
                    PromptPlacement::Excluded,
                ),
                Arc::new(move |_: &crate::plugins::PromptInput<'_>| {
                    Ok::<_, crate::plugins::PromptRenderError>(crate::plugins::SectionText::text(
                        text,
                    ))
                }),
            )?;
        }
        Ok(())
    }
}

/// Every request a provider served, in the order it served them.
type Served = Arc<std::sync::Mutex<Vec<LlmRequest>>>;

/// What the scripted model does next.
#[derive(Default)]
struct Script {
    /// The next call signals [`Self::in_flight`] and holds until
    /// [`Self::release`].
    holds: std::sync::atomic::AtomicBool,
    in_flight: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// The next call answers with an `app_lookup` call.
    calls_a_tool: std::sync::atomic::AtomicBool,
}

/// A model that keeps each request it served and follows `script`.
fn scripted_provider(served: &Served, script: &Arc<Script>) -> ProviderHandle {
    let served = Arc::clone(served);
    let script = Arc::clone(script);
    crate::testing::TestProvider::builder()
        .kind("recorded-prompt")
        .complete(move |request| {
            let served = Arc::clone(&served);
            let script = Arc::clone(&script);
            async move {
                served.lock_recover().push(request);
                if script.holds.swap(false, Ordering::SeqCst) {
                    script.in_flight.notify_one();
                    script.release.notified().await;
                }
                if script.calls_a_tool.swap(false, Ordering::SeqCst) {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "call-1".to_string(),
                            tool_name: "app_lookup".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    });
                }
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

/// A core over `backend` with the sections and `app_lookup` installed.
fn core_over(backend: lash_core::Backend, provider: ProviderHandle) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .tools(Arc::new(AppTools))
        .plugin(Arc::new(Sections))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
}

/// Every request `served` made for `id`, in order.
fn requests_of(served: &Served, id: &str) -> Vec<LlmRequest> {
    served
        .lock_recover()
        .iter()
        .filter(|request| request.scope.session_id.as_str() == id)
        .cloned()
        .collect()
}

/// `request` carries the section `expected` once, and no other section
/// these laws place.
fn assert_intro(request: &LlmRequest, expected: &str, how: &str) {
    let instructions = request.instructions.as_deref().unwrap_or_default();
    for (_, intro) in SECTIONS {
        assert_eq!(
            instructions.matches(intro).count(),
            usize::from(intro == expected),
            "{how}: the request states `{expected}` once and no other section: {instructions}"
        );
    }
}

/// Create `id` under the plan placing `key`.
async fn create_placing(core: &LashCore, id: &str, key: &str) -> Result<()> {
    core.session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()).with_prompt_plan(placing(key)))
        .await?;
    Ok(())
}

/// Send `id` `text` through its durable session and wait for its answer.
async fn run(core: &LashCore, id: &str, text: &str) -> Result<()> {
    core.session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .durable()
        .await?
        .send(crate::TurnInput::text(text))
        .output()
        .await?;
    Ok(())
}

/// A session created under deployment A's plan is served it on every run.
/// Then A's process goes away and a new one serves the same stores, its
/// host passing plan B to the sessions it creates. Each run after the
/// restart, on the engine's open and on a host open, is served as its
/// session recorded; a session the new deployment creates records B.
async fn a_session_created_under_defaults_a_reopens_under_a(
    stores: Arc<dyn lash_core::StoreSet>,
) -> Result<()> {
    const ID: &str = "recorded-prompt-defaults";
    const LATER: &str = "recorded-prompt-created-later";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let first = core_over(
        lash_conformance::backend_over(Arc::clone(&stores)),
        scripted_provider(&served, &script),
    )?;
    create_placing(&first, ID, "defaults-a").await?;
    run(&first, ID, "under the creating deployment").await?;
    first.shutdown().await?;

    let redeployed = core_over(
        lash_conformance::backend_over(stores),
        scripted_provider(&served, &script),
    )?;
    run(&redeployed, ID, "on the engine's open after the restart").await?;
    redeployed
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?
        .send(crate::TurnInput::text("on a host open after the restart"))
        .output()
        .await?;
    let requests = requests_of(&served, ID);
    assert_eq!(requests.len(), 3, "one model call per run");
    for (index, request) in requests.iter().enumerate() {
        assert_intro(request, DEFAULTS_A, &format!("run {index} of the session"));
    }

    create_placing(&redeployed, LATER, "defaults-b").await?;
    run(&redeployed, LATER, "under the redeployed plan").await?;
    let later = requests_of(&served, LATER);
    assert_eq!(later.len(), 1);
    assert_intro(
        &later[0],
        DEFAULTS_B,
        "a session the new deployment created",
    );
    redeployed.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_under_defaults_a_reopens_under_a_on_sqlite_memory() -> Result<()> {
    a_session_created_under_defaults_a_reopens_under_a(sqlite_memory_store_set().await).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_under_defaults_a_reopens_under_a_on_sqlite_file() -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(directory.path().join("lash.db"))
            .await
            .expect("open SQLite store set"),
    );
    a_session_created_under_defaults_a_reopens_under_a(stores).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_session_created_under_defaults_a_reopens_under_a_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    a_session_created_under_defaults_a_reopens_under_a(stores).await
}

/// A plan command applies to the next run, not the admitted one. A run
/// makes its first model call under plan A and is held there while a plan
/// command is submitted; released, it calls a tool and makes a second model
/// call, which still carries A, however far the command got meanwhile. The
/// next run carries the commanded plan.
async fn a_prompt_command_reaches_the_next_run_and_not_the_running_one(
    stores: Arc<dyn lash_core::StoreSet>,
) -> Result<()> {
    const ID: &str = "recorded-prompt-command";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let core = core_over(
        lash_conformance::backend_over(stores),
        scripted_provider(&served, &script),
    )?;
    create_placing(&core, ID, "defaults-a").await?;
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?;

    script.holds.store(true, Ordering::SeqCst);
    script.calls_a_tool.store(true, Ordering::SeqCst);
    let command = async {
        script.in_flight.notified().await;
        let config = session.admin().config();
        let revision = config.revision().await?;
        let mut apply = std::pin::pin!(config.apply(
            crate::config::ConfigWrite::new("commanded-plan", revision),
            crate::config::ConfigTransaction::of(crate::config::SetPromptPlan {
                plan: placing("commanded"),
            }),
        ));
        // Give the command time to settle while the run is mid-call, then
        // let the run go on whether or not it has.
        let settled = tokio::select! {
            outcome = &mut apply => Some(outcome),
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => None,
        };
        script.release.notify_one();
        match settled {
            Some(outcome) => outcome,
            None => apply.await,
        }
    };
    let (output, commanded) = tokio::join!(
        session.send(crate::TurnInput::text("first")).output(),
        command
    );
    output?;
    assert!(
        matches!(
            commanded?,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "the plan command applies"
    );

    let running = requests_of(&served, ID);
    assert_eq!(
        running.len(),
        2,
        "the running run called the model, a tool, and the model again"
    );
    for request in &running {
        assert_intro(request, DEFAULTS_A, "the run admitted before the command");
    }

    session
        .send(crate::TurnInput::text("second"))
        .output()
        .await?;
    let requests = requests_of(&served, ID);
    assert_eq!(requests.len(), 3);
    assert_intro(&requests[2], COMMANDED, "the run after the command");
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_command_reaches_the_next_run_and_not_the_running_one_on_sqlite() -> Result<()> {
    a_prompt_command_reaches_the_next_run_and_not_the_running_one(sqlite_memory_store_set().await)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_prompt_command_reaches_the_next_run_and_not_the_running_one_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    a_prompt_command_reaches_the_next_run_and_not_the_running_one(stores).await
}

/// The typed cause of a run `owner` refused for options that are not its
/// run options (FIG-4652).
fn assert_not_run_options(refused: &crate::EmbedError, owner: &str, what: &str) {
    let crate::EmbedError::Runtime(error) = refused else {
        panic!("{what}: the refusal is the run's: {refused:?}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RunShapeRefused,
        "{what}"
    );
    let Some(lash_core::RunShapeRefusal::Owner { refusal }) = error.run_shape_refusal() else {
        panic!("{what}: the refusal carries its owner's typed cause: {error:?}");
    };
    assert_eq!(refusal.owner, owner, "{what}");
    assert!(
        matches!(
            refusal.reason,
            lash_core::ConfigRefusalReason::Unreadable {
                role: lash_core::ConfigValueRole::RunOptions,
                ..
            }
        ),
        "{what}: {refusal:?}"
    );
}

/// A run states only the standard owner's run options (FIG-4589 acceptance
/// law e, FIG-4652). A payload that carries a prompt is refused, typed: the
/// run ends `RunShapeRefused` with the owner and the unreadable run options
/// as its cause, makes no model call, and leaves the recorded config as it
/// was. A run stating its render options runs under the recorded plan.
async fn a_run_options_prompt_is_refused(stores: Arc<dyn lash_core::StoreSet>) -> Result<()> {
    const ID: &str = "recorded-prompt-run-options";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let core = core_over(
        lash_conformance::backend_over(stores),
        scripted_provider(&served, &script),
    )?;
    create_placing(&core, ID, "defaults-a").await?;
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?;
    let revision = session.admin().config().revision().await?;

    for (what, stated) in [
        (
            "a prompt",
            serde_json::json!({ "prompt": { "intro": COMMANDED } }),
        ),
        (
            "a prompt plan",
            serde_json::json!({ "prompt_plan": placing("commanded") }),
        ),
    ] {
        let refused = session
            .send(crate::TurnInput::text("options this run cannot state"))
            .protocol_turn_options(lash_core::ProtocolTurnOptions::from_payload(stated))
            .output()
            .await
            .expect_err(what);
        assert_not_run_options(&refused, crate::standard::STANDARD_PROTOCOL_PLUGIN_ID, what);
    }
    assert!(
        requests_of(&served, ID).is_empty(),
        "a refused run reaches no model"
    );
    assert_eq!(
        session.admin().config().revision().await?,
        revision,
        "a refused run leaves the recorded config as it was"
    );

    session
        .send(crate::TurnInput::text(
            "a run that states its render options",
        ))
        .protocol_turn_options(lash_core::ProtocolTurnOptions::typed(
            crate::standard::StandardRunOptions::default(),
        )?)
        .output()
        .await?;
    let requests = requests_of(&served, ID);
    assert_eq!(requests.len(), 1);
    assert_intro(&requests[0], DEFAULTS_A, "the run after the refusals");
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_options_prompt_is_refused_on_sqlite() -> Result<()> {
    a_run_options_prompt_is_refused(sqlite_memory_store_set().await).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_run_options_prompt_is_refused_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    a_run_options_prompt_is_refused(stores).await
}

/// The RLM owner refuses a run-options prompt the same way.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_rlm_run_options_prompt_is_refused() -> Result<()> {
    const ID: &str = "recorded-prompt-rlm-run-options";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let backend = sqlite_memory_store_backend().await;
    let factory = rlm_factory(&backend);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(scripted_provider(&served, &script), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?;
    let refused = session
        .send(crate::TurnInput::text("a prompt for this run"))
        .protocol_turn_options(lash_core::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "prompt": { "intro": COMMANDED } }),
        ))
        .output()
        .await
        .expect_err("a run cannot state the session's prompt");
    assert_not_run_options(
        &refused,
        crate::rlm::RLM_PROTOCOL_PLUGIN_ID,
        "the prompt is no RLM run option",
    );
    assert!(
        requests_of(&served, ID).is_empty(),
        "a refused run reaches no model"
    );
    core.shutdown().await?;
    Ok(())
}
