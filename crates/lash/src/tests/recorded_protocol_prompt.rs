//! The system prompt is recorded protocol config (FIG-4589). A session
//! records its protocol's prompt when it is created, from the spec its
//! creator states: a core keeps no default (FIG-4594), so a host's default
//! prompt is a `SessionSpec` value it keeps and passes. A prompt command
//! changes it for the roots after it; and a run's options cannot state it.
//! So:
//!
//! - a session created from one deployment's default spec is served that
//!   prompt on every later root, redrives included, whatever default spec the
//!   deployment reopening it passes to the sessions it creates, and keeps the
//!   request defaults its model binding recorded beside it (FIG-4374,
//!   FIG-4567);
//! - a prompt command reaches the next root and never the running one;
//! - a child created before its parent's prompt changed keeps the prompt it
//!   recorded;
//! - a run-options payload that carries a prompt is refused, typed.

use super::*;

/// The prompt of the first deployment's default spec: what a session it
/// creates from that spec records.
const DEFAULTS_A: &str = "INTRO OF THE CREATING DEPLOYMENT'S DEFAULTS";
/// The prompt of the later deployment's default spec: no session created
/// before it may be served it.
const DEFAULTS_B: &str = "INTRO OF THE REDEPLOYED DEFAULTS";
/// A prompt a config command states.
const COMMANDED: &str = "INTRO A CONFIG COMMAND STATED";
/// A prompt a session's own spec states.
const OWN: &str = "INTRO THE SESSION'S OWN SPEC STATED";

/// The request defaults the first deployment registers for the model: every
/// session it creates records them with its model binding.
fn creating_core_metadata() -> lash_core::LlmProfileMetadata {
    let mut metadata = mock_llm_profile_spec().with_request_defaults(
        lash_core::provider::LlmProfileRequestDefaults {
            response_metadata_headers: vec!["x-creator-cost".to_string()],
            response_metadata_body_paths: vec!["/creator/cost".to_string()],
            ..lash_core::provider::LlmProfileRequestDefaults::default()
        },
    );
    metadata.limits.output_tokens =
        crate::OutputTokenLimits::new(None, Some(3333)).expect("valid recorded cap");
    metadata
}

/// The request defaults the redeployed core registers for the same model
/// key: no session created before it may be served with them.
fn redeployed_core_metadata() -> lash_core::LlmProfileMetadata {
    let mut metadata = mock_llm_profile_spec().with_request_defaults(
        lash_core::provider::LlmProfileRequestDefaults {
            expose_thinking: true,
            cache_retention: crate::provider::CacheRetention::Long,
            response_metadata_headers: vec!["x-redeployed-cost".to_string()],
            response_metadata_body_paths: vec!["/redeployed".to_string()],
        },
    );
    metadata.limits.output_tokens =
        crate::OutputTokenLimits::new(None, Some(7777)).expect("valid recorded cap");
    metadata
}

/// Every request a provider served, in the order it served them.
type Served = Arc<std::sync::Mutex<Vec<LlmRequest>>>;

/// What the laws' model does on a call, beside keeping its request.
#[derive(Default)]
struct Script {
    /// The deployment whose next root handler dies under its model call.
    dies_on: std::sync::Mutex<Option<lash_restate_test::RestateTestBackend>>,
    /// Set, the next call answers with a tool call, so its root makes a
    /// second call.
    calls_a_tool: std::sync::atomic::AtomicBool,
    /// Set, the next call tells the law it is in flight and waits to be
    /// released.
    holds: std::sync::atomic::AtomicBool,
    in_flight: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// A model that keeps each request it served and follows `script`.
fn scripted_provider(served: &Served, script: &Arc<Script>) -> ProviderHandle {
    let served = Arc::clone(served);
    let script = Arc::clone(script);
    crate::testing::TestProvider::builder()
        .kind("recorded-protocol-prompt")
        .complete(move |request| {
            let served = Arc::clone(&served);
            let script = Arc::clone(&script);
            if let Some(double) = script.dies_on.lock_recover().take() {
                // The root's handler dies before this call's result is
                // journaled, so its redrive makes the call again.
                double.crash_turn_drive(lash_restate_test::CrashPoint::BeforeRunResult {
                    name: None,
                });
            }
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

/// A standard prompt whose intro is `intro`.
fn prompt(intro: &str) -> crate::standard::StandardPrompt {
    crate::standard::StandardPrompt {
        intro: Some(intro.to_string()),
        ..Default::default()
    }
}

/// A spec stating the standard prompt [`prompt`] gives `intro`.
fn spec_stating(intro: &str) -> Result<lash_core::facade_support::SessionSpec> {
    mock_session_spec()
        .plugin(
            crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
            crate::standard::StandardTurnOptions {
                prompt: Some(prompt(intro)),
                render: None,
            },
        )
        .map_err(EmbedError::ProtocolTurnOptions)
}

/// A core over `backend` whose model is registered with `metadata`.
/// The deployment's default spec is its host's: [`spec_stating`] the
/// deployment's default intro, passed to each creation.
fn core_with_metadata(
    backend: lash_core::Backend,
    provider: ProviderHandle,
    metadata: lash_core::LlmProfileMetadata,
) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .tools(Arc::new(AppTools))
        .serve_test_llm_profile(provider, metadata)
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

/// `request` carries the intro `expected` once, and no other intro these
/// laws state.
fn assert_intro(request: &LlmRequest, expected: &str, how: &str) {
    let instructions = request.instructions.as_deref().unwrap_or_default();
    for intro in [DEFAULTS_A, DEFAULTS_B, COMMANDED, OWN] {
        assert_eq!(
            instructions.matches(intro).count(),
            usize::from(intro == expected),
            "{how}: the request states `{expected}` once and no other intro: {instructions}"
        );
    }
}

/// Where a law's deployment keeps its sessions.
#[derive(Clone, Copy)]
enum Stores {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// A Restate double over `stores`, with what must outlive it.
async fn deployment(
    stores: Stores,
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> (
    lash_restate_test::RestateTestBackend,
    Box<dyn std::any::Any>,
) {
    match stores {
        Stores::SqliteMemory => (
            lash_restate_test::backend(seed, config)
                .await
                .expect("build the deployment over SQLite memory"),
            Box::new(()),
        ),
        Stores::SqliteFile => {
            let dir = tempfile::tempdir().expect("SQLite file store directory");
            let stores = Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(dir.path())
                    .await
                    .expect("open the SQLite file store set"),
            );
            (
                lash_restate_test::backend_with(seed, config, move |_| stores)
                    .await
                    .expect("build the deployment over a SQLite file"),
                Box::new(dir),
            )
        }
        Stores::Postgres => {
            let (stores, held) = postgres_store_set()
                .await
                .expect("a selected PostgreSQL law has its service");
            (
                lash_restate_test::backend_with(seed, config, move |_| stores)
                    .await
                    .expect("build the deployment over PostgreSQL"),
                held,
            )
        }
    }
}

/// How long a root sent after a restart may take to answer. A root the
/// restarted deployment cannot run never answers: its attempts fail until
/// the server pauses it. The bound turns that hang into a failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

/// A session keeps the prompt and the request defaults it was created under
/// (acceptance law a). It is created from the host's default spec A, and with a
/// prompt of its own, and runs a root, so it restarts with history and the
/// cancellation binding its first root recorded (FIG-4567). Then the
/// deployment's process goes away and a new one serves the same stores and
/// the same Restate state with defaults B and other request defaults for the
/// same model key. Each root after the restart, on the engine's reopen and on
/// a host open, is served as its session recorded. So is a root whose first
/// attempt dies under its model call: its redrive makes the call again with
/// the recorded prompt. A session the new deployment creates records B.
async fn a_session_created_under_defaults_a_reopens_and_redrives_under_a(
    stores: Stores,
    seed: u64,
    config: lash_restate_test::ServerConfig,
    crash_redrive: bool,
) -> Result<()> {
    const DEFAULTED: &str = "recorded-prompt-defaulted";
    const STATED: &str = "recorded-prompt-stated";
    const CREATED_LATER: &str = "recorded-prompt-created-later";
    let recorded = [(DEFAULTED, DEFAULTS_A), (STATED, OWN)];
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let assert_served_as_recorded = |id: &str, intro: &str, calls: usize, how: &str| {
        let requests = requests_of(&served, id);
        assert_eq!(requests.len(), calls, "{id} {how}: its model calls");
        for request in &requests {
            assert_intro(request, intro, &format!("{id} {how}"));
            assert_eq!(
                request.model.metadata(),
                &creating_core_metadata(),
                "{id} {how}: its calls carry the request defaults its model binding recorded"
            );
        }
    };

    let (first, _held) = deployment(stores, seed, config).await;
    {
        let creator = core_with_metadata(
            first.lash_backend(),
            scripted_provider(&served, &script),
            creating_core_metadata(),
        )?;
        creator
            .session(DEFAULTED)
            .create(crate::SessionCreation::root(spec_stating(DEFAULTS_A)?))
            .await?;
        creator
            .session(STATED)
            .create(crate::SessionCreation {
                spec: spec_stating(OWN)?,
                parent: None,
            })
            .await?;
        for (id, intro) in recorded {
            creator
                .session(id)
                .durable()
                .await?
                .send(TurnInput::text("before the restart"))
                .output()
                .await?;
            assert_served_as_recorded(id, intro, 1, "before the restart");
            first
                .settle_session_drive(&lash_core::SessionId::from(id))
                .await;
        }
    }
    let second = redeploy(first).await;
    let redeployed = core_with_metadata(
        second.lash_backend(),
        scripted_provider(&served, &script),
        redeployed_core_metadata(),
    )?;
    for (id, intro) in recorded {
        tokio::time::timeout(
            ANSWERS_WITHIN,
            redeployed
                .session(id)
                .durable()
                .await?
                .send(TurnInput::text("after the restart, on the engine's reopen"))
                .output(),
        )
        .await
        .unwrap_or_else(|_| panic!("{id}: a root sent after the restart answers"))?;
        assert_served_as_recorded(id, intro, 2, "on the engine's reopen after the restart");

        if crash_redrive {
            // The next root's first attempt dies under its model call, and its
            // redrive makes the call again.
            let crashes = second.server().stats().crashes;
            *script.dies_on.lock_recover() = Some(second.clone());
            tokio::time::timeout(
                ANSWERS_WITHIN,
                redeployed
                    .session(id)
                    .durable()
                    .await?
                    .send(TurnInput::text("dies under its call and is redriven"))
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{id}: the redriven root answers"))?;
            assert_eq!(
                second.server().stats().crashes,
                crashes + 1,
                "{id}: the root's first attempt died under its model call"
            );
            assert_served_as_recorded(id, intro, 4, "across the redrive");
            second
                .settle_session_drive(&lash_core::SessionId::from(id))
                .await;
        }
        let opened = retry_when_claim_frees(|| redeployed.session(id).open()).await?;
        opened
            .send(TurnInput::text("after the restart, on a host open"))
            .output()
            .await?;
        assert_served_as_recorded(
            id,
            intro,
            if crash_redrive { 5 } else { 3 },
            "on a host open after the restart",
        );
    }

    // The redeployed host's default spec is what a session it creates
    // records.
    redeployed
        .session(CREATED_LATER)
        .create(crate::SessionCreation::root(spec_stating(DEFAULTS_B)?))
        .await?;
    redeployed
        .session(CREATED_LATER)
        .durable()
        .await?
        .send(TurnInput::text("created after the restart"))
        .output()
        .await?;
    let created_later = requests_of(&served, CREATED_LATER);
    assert_eq!(created_later.len(), 1);
    assert_intro(&created_later[0], DEFAULTS_B, "a session created later");
    assert_eq!(
        created_later[0].model.metadata(),
        &redeployed_core_metadata()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_under_defaults_a_reopens_and_redrives_under_a_on_sqlite_memory()
-> Result<()> {
    a_session_created_under_defaults_a_reopens_and_redrives_under_a(
        Stores::SqliteMemory,
        0x4589_0001,
        lash_restate_test::ServerConfig::default(),
        true,
    )
    .await
}

/// Every await suspends and every resumption replays the root from its
/// journal, on the restarted deployment as on the first: the replayed roots
/// are served as their sessions recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_under_defaults_a_reopens_and_redrives_under_a_under_always_replay()
-> Result<()> {
    a_session_created_under_defaults_a_reopens_and_redrives_under_a(
        Stores::SqliteMemory,
        0x4589_0003,
        lash_restate_test::ServerConfig::default().always_replay(true),
        // Every await already replays the root from its journal here, so
        // the law injects no crash of its own.
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_under_defaults_a_reopens_and_redrives_under_a_on_sqlite_file()
-> Result<()> {
    a_session_created_under_defaults_a_reopens_and_redrives_under_a(
        Stores::SqliteFile,
        0x4589_0005,
        lash_restate_test::ServerConfig::default(),
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_session_created_under_defaults_a_reopens_and_redrives_under_a_on_postgres() -> Result<()>
{
    a_session_created_under_defaults_a_reopens_and_redrives_under_a(
        Stores::Postgres,
        0x4589_0007,
        lash_restate_test::ServerConfig::default(),
        true,
    )
    .await
}

/// A prompt command applies to the next root, not the admitted one
/// (acceptance law b). A root makes its first model call under prompt A and
/// is held there while a prompt command is submitted; released, it calls a
/// tool and makes a second model call, which still carries A, however far
/// the command got meanwhile. The next root carries the commanded prompt.
async fn a_prompt_command_reaches_the_next_root_and_not_the_running_one(
    stores: Stores,
    seed: u64,
) -> Result<()> {
    const ID: &str = "recorded-prompt-command";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let (double, _held) =
        deployment(stores, seed, lash_restate_test::ServerConfig::default()).await;
    let core = core_with_metadata(
        double.lash_backend(),
        scripted_provider(&served, &script),
        creating_core_metadata(),
    )?;
    core.session(ID)
        .create(crate::SessionCreation::root(spec_stating(DEFAULTS_A)?))
        .await?;
    let session = core.session(ID).open().await?;

    script.holds.store(true, Ordering::SeqCst);
    script.calls_a_tool.store(true, Ordering::SeqCst);
    let command = async {
        script.in_flight.notified().await;
        let config = session.admin().config();
        let mut configure = std::pin::pin!(config.configure(crate::config::ConfigTransaction::of(
            crate::standard::SetStandardPrompt {
                prompt: prompt(COMMANDED),
            }
        )));
        // Give the command time to settle while the root is mid-call, then
        // let the root go on whether or not it has.
        let settled = tokio::select! {
            outcome = &mut configure => Some(outcome),
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => None,
        };
        script.release.notify_one();
        match settled {
            Some(outcome) => outcome,
            None => configure.await,
        }
    };
    let (output, commanded) =
        tokio::join!(session.send(TurnInput::text("first")).output(), command);
    output?;
    commanded?;

    let running = requests_of(&served, ID);
    assert_eq!(
        running.len(),
        2,
        "the running root called the model, a tool, and the model again"
    );
    for request in &running {
        assert_intro(request, DEFAULTS_A, "the root admitted before the command");
    }

    session.send(TurnInput::text("second")).output().await?;
    let requests = requests_of(&served, ID);
    assert_eq!(requests.len(), 3);
    assert_intro(&requests[2], COMMANDED, "the root after the command");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_command_reaches_the_next_root_and_not_the_running_one_on_sqlite() -> Result<()> {
    a_prompt_command_reaches_the_next_root_and_not_the_running_one(
        Stores::SqliteMemory,
        0x4589_0011,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_prompt_command_reaches_the_next_root_and_not_the_running_one_on_postgres() -> Result<()>
{
    a_prompt_command_reaches_the_next_root_and_not_the_running_one(Stores::Postgres, 0x4589_0013)
        .await
}

/// A child created before its parent's prompt changed keeps the prompt it
/// recorded (acceptance law c). A child a parent creates records its
/// namespaces through its plugins' owners with the parent's recorded config
/// beneath it: the standard owner copies the parent's prompt, whatever the
/// creating host's defaults are. A prompt command on the parent afterwards
/// reaches the parent's next root and never the child's.
async fn a_child_created_before_its_parents_prompt_changed_keeps_its_own(
    stores: Stores,
    seed: u64,
) -> Result<()> {
    const PARENT: &str = "recorded-prompt-parent";
    const CHILD: &str = "recorded-prompt-child";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let (double, _held) =
        deployment(stores, seed, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let core = core_with_metadata(
        backend.clone(),
        scripted_provider(&served, &script),
        creating_core_metadata(),
    )?;
    core.session(PARENT)
        .create(crate::SessionCreation {
            spec: spec_stating(OWN)?,
            parent: None,
        })
        .await?;

    // The child, created as the runtime creates a parent's child: every
    // owner creates its namespace over the parent's recorded one.
    let sessions = backend.session_store_factory();
    let parent_config = lash_core::store::SessionStore::new(
        Arc::clone(&sessions) as Arc<dyn lash_core::RuntimeStore>,
        lash_core::SessionId::from(PARENT),
    )
    .expect("the parent's session view")
    .load_session_head_meta()
    .await
    .expect("load the parent's head")
    .expect("the parent has a head")
    .config;
    let mut child_config = parent_config.clone();
    child_config.plugin_config = lash_core::facade_support::PluginHost::new(vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )
        as Arc<dyn PluginFactory>])
    .resolve_creation_plugin_config(
        Some(crate::standard::STANDARD_PROTOCOL_PLUGIN_ID),
        &lash_core::PluginOptions::default(),
        Some(&parent_config.plugin_config),
        false,
        &lash_core::store::plugin_writers::PluginAdmission::default(),
    )
    .expect("the child's owners create its namespaces");
    lash_core::runtime::admit_session_view(
        &sessions,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: lash_core::SessionId::from(CHILD),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: lash_core::SessionId::from(PARENT),
                caused_by: None,
            },
            config: child_config,
            head: lash_core::SessionCreationHead::Config,
        },
    )
    .await
    .expect("create the child session");

    let parent = core.session(PARENT).open().await?;
    parent
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::standard::SetStandardPrompt {
                prompt: prompt(COMMANDED),
            },
        ))
        .await?;
    parent
        .send(TurnInput::text("after the change"))
        .output()
        .await?;
    let child = core.session(CHILD).open().await?;
    child
        .send(TurnInput::text("after the parent's change"))
        .output()
        .await?;
    Box::pin(child.close()).await?;
    retry_when_claim_frees(|| core.session(CHILD).durable())
        .await?
        .send(TurnInput::text("on the engine's reopen"))
        .output()
        .await?;

    let parents = requests_of(&served, PARENT);
    assert_eq!(parents.len(), 1);
    assert_intro(&parents[0], COMMANDED, "the parent after its command");
    let children = requests_of(&served, CHILD);
    assert_eq!(children.len(), 2);
    for request in &children {
        assert_intro(
            request,
            OWN,
            "the child keeps the prompt it copied from its parent at creation",
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_created_before_its_parents_prompt_changed_keeps_its_own_on_sqlite() -> Result<()> {
    a_child_created_before_its_parents_prompt_changed_keeps_its_own(
        Stores::SqliteMemory,
        0x4589_0021,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_child_created_before_its_parents_prompt_changed_keeps_its_own_on_postgres() -> Result<()>
{
    a_child_created_before_its_parents_prompt_changed_keeps_its_own(Stores::Postgres, 0x4589_0023)
        .await
}

/// The typed cause of a run the standard owner refused for options that are
/// not its run options (FIG-4652).
fn assert_not_run_options(refused: &crate::EmbedError, what: &str) {
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
    assert_eq!(
        refusal.owner,
        crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
        "{what}"
    );
    assert_eq!(refusal.at, lash_core::RefusalSite::Candidate, "{what}");
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
    assert_eq!(
        refusal.owner_refusal::<crate::standard::StandardConfigRefusal>(),
        None,
        "{what}: the framework's reason is not the owner's refusal"
    );
}

/// A run states only the standard owner's run options (FIG-4589 acceptance
/// law e, FIG-4652). A payload that carries the session's prompt or its
/// behaviour is refused, typed: the root ends `RunShapeRefused` with the
/// owner and the unreadable run options as its cause, makes no model call,
/// and leaves the recorded namespace as it was. That holds even when the
/// payload restates the very value the session recorded.
async fn a_run_options_prompt_is_refused(stores: Stores, seed: u64) -> Result<()> {
    const ID: &str = "recorded-prompt-run-options";
    let served: Served = Arc::default();
    let script = Arc::new(Script::default());
    let (double, _held) =
        deployment(stores, seed, lash_restate_test::ServerConfig::default()).await;
    let core = core_with_metadata(
        double.lash_backend(),
        scripted_provider(&served, &script),
        creating_core_metadata(),
    )?;
    core.session(ID)
        .create(crate::SessionCreation::root(spec_stating(DEFAULTS_A)?))
        .await?;
    let session = core.session(ID).open().await?;
    let recorded = session.read_view().protocol_turn_options().payload.clone();
    let recorded_behaviour = recorded
        .get("behaviour")
        .cloned()
        .expect("the session recorded its behaviour");

    for (what, stated) in [
        (
            "another prompt",
            serde_json::json!({ "prompt": prompt(COMMANDED) }),
        ),
        (
            "the recorded prompt",
            serde_json::json!({ "prompt": prompt(DEFAULTS_A) }),
        ),
        (
            "the recorded behaviour",
            serde_json::json!({ "behaviour": recorded_behaviour }),
        ),
    ] {
        let refused = session
            .send(TurnInput::text("options this run cannot state"))
            .protocol_turn_options(lash_core::ProtocolTurnOptions::from_payload(stated))
            .output()
            .await
            .expect_err(what);
        assert_not_run_options(&refused, what);
    }
    assert!(
        requests_of(&served, ID).is_empty(),
        "a refused run reaches no model"
    );
    assert_eq!(
        session.read_view().protocol_turn_options().payload,
        recorded,
        "a refused run leaves the recorded namespace as it was"
    );

    session
        .send(TurnInput::text("a run that states its render options"))
        .protocol_turn_options(lash_core::ProtocolTurnOptions::typed(
            crate::standard::StandardRunOptions::default(),
        )?)
        .output()
        .await?;
    let requests = requests_of(&served, ID);
    assert_eq!(requests.len(), 1);
    assert_intro(&requests[0], DEFAULTS_A, "the root after the refusals");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_options_prompt_is_refused_on_sqlite() -> Result<()> {
    a_run_options_prompt_is_refused(Stores::SqliteMemory, 0x4589_0031).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_run_options_prompt_is_refused_on_postgres() -> Result<()> {
    a_run_options_prompt_is_refused(Stores::Postgres, 0x4589_0033).await
}

/// The RLM owner refuses a run-options prompt the same way.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_rlm_run_options_prompt_is_refused() -> Result<()> {
    const ID: &str = "recorded-prompt-rlm-run-options";
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_llm_profile(
            recording_request_provider(Arc::clone(&seen)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    core.session(ID)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(ID).open().await?;
    let refused = session
        .send(TurnInput::text("a prompt for this run"))
        .protocol_turn_options(lash_core::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "prompt": crate::rlm::RlmPrompt::default() }),
        ))
        .output()
        .await
        .expect_err("a run cannot state the session's prompt");
    let crate::EmbedError::Runtime(error) = &refused else {
        panic!("the refusal is the run's: {refused:?}");
    };
    assert_eq!(error.code, lash_core::RuntimeErrorCode::RunShapeRefused);
    let Some(lash_core::RunShapeRefusal::Owner { refusal }) = error.run_shape_refusal() else {
        panic!("the refusal carries its owner's typed cause: {error:?}");
    };
    assert_eq!(refusal.owner, crate::rlm::RLM_PROTOCOL_PLUGIN_ID);
    assert!(
        matches!(
            refusal.reason,
            lash_core::ConfigRefusalReason::Unreadable {
                role: lash_core::ConfigValueRole::RunOptions,
                ..
            }
        ),
        "the prompt is no RLM run option: {refusal:?}"
    );
    assert!(
        seen.lock_recover().is_empty(),
        "a refused run reaches no model"
    );
    Ok(())
}
