use super::*;
use std::sync::Mutex as StdMutex;

pub const PARENT: &str = "session-turn-parent";
pub const CHILD: &str = "session-turn-child";

/// A detached `SessionTurn` start whose child session `CHILD` runs one turn
/// on the profile `model`, its metadata recorded in the start.
pub fn request(model: &str) -> lash_execution_contracts::process::ProcessStartRequest {
    started(model, false)
}

/// A detached `SessionTurn` start of [`request`]'s shape that names its
/// profile by key only when `keyed`: the deployment that creates the child
/// resolves the key's metadata.
pub fn started(model: &str, keyed: bool) -> lash_execution_contracts::process::ProcessStartRequest {
    let metadata = lash_core::LlmProfileMetadata::builder(model)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata");
    let recorded = lash::LlmProfileConfig::new(lash::RecordedLlmProfile::mint(
        lash::LlmProfileKey::new(model),
        metadata,
    ));
    let mut create_request = lash_execution_contracts::session_types::SessionCreateRequest::child(
        PARENT,
        lash_core::SessionPolicy {
            model: (!keyed).then_some(recorded),
            ..lash_core::SessionPolicy::new(
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
                lash::NoProgressBudget::bounded(12),
            )
        },
        lash_core::PluginOptions::default(),
    )
    .with_session_id(lash_kernel_value::identity::SessionId::fixture(
        CHILD.to_string(),
    ));
    if keyed {
        create_request.model = Some(lash::LlmProfileKey::new(model));
    }
    lash_execution_contracts::process::ProcessStartRequest::session_turn(
        "child-turn",
        create_request,
        lash::TurnInput::text("do the child's work"),
        lash_execution_contracts::process::SessionTurnOutcome::Turn,
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    )
}

/// What the model does on the child's turn.
#[derive(Clone)]
pub enum ChildModel {
    /// It answers.
    Answers,
    /// It panics.
    Panics,
    /// It opens the gate and never answers.
    Holds(Arc<tokio::sync::Notify>),
}

/// A core over `backend` whose model answers the parent's turns and does
/// what `child` says on the child's, serving each profile of `served`. The
/// first deployment over `backend` creates the parent's session.
pub async fn deploy(
    backend: &lash_execution_contracts::backend::Backend,
    child: ChildModel,
    served: &[lash_core::LlmProfileMetadata],
    first: bool,
) -> Result<LashCore> {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("session-turn-child")
        .complete(move |request| {
            let on_child = (request
                .session_id()
                .map(lash_kernel_value::identity::SessionId::as_str)
                == Some(CHILD))
            .then(|| child.clone());
            async move {
                match on_child {
                    None => {}
                    Some(ChildModel::Answers) => return Ok(text_response("child answered")),
                    Some(ChildModel::Panics) => panic!("child turn payload only"),
                    Some(ChildModel::Holds(entered)) => {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                }
                Ok(text_response("parent lives"))
            }
        })
        .build()
        .into_handle();
    let mut builder = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()));
    for metadata in served {
        builder = builder.serve_test_llm_profile(provider.clone(), metadata.clone());
    }
    let core = builder.build(runtime_lease_owner())?;
    if first {
        core.session(
            lash_kernel_value::identity::SessionId::parse(PARENT).expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    }
    Ok(core)
}

/// FIG-5547: observation links the session the process actually ran, even
/// when its creator left the child id for the process to derive. The
/// recorded request keeps the creator's optional id unchanged.
pub async fn observed_child_session_matches_execution(named: bool) -> Result<()> {
    let sessions = Arc::new(StdMutex::new(Vec::new()));
    let ran_in = Arc::clone(&sessions);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("observed-child-session")
        .complete(move |request| {
            ran_in.lock_recover().push(
                request
                    .session_id()
                    .expect("the child's model call names its session")
                    .clone(),
            );
            async { Ok(text_response("child answered")) }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(runtime_lease_owner())?;
    let mut request = request("mock-model");
    let lash_execution_contracts::process::ProcessStartTarget::Input(
        lash_execution_contracts::process::ProcessInput::SessionTurn { create_request, .. },
    ) = &mut request.input
    else {
        panic!("the fixture starts a child session turn");
    };
    let stated_id =
        named.then(|| lash_kernel_value::identity::SessionId::fixture(CHILD.to_owned()));
    create_request.session_id = stated_id.clone();
    let started = core.processes().start(request, core.effect_host()).await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the child's turn settles")?;
    assert!(
        matches!(&output, lash_execution_contracts::process::ProcessAwaitOutput::Settled { output }
            if output.is_success()),
        "the child ran successfully: {output:?}"
    );
    let observed = core
        .processes()
        .get(&started.process_id)
        .await?
        .expect("the completed process remains observable");
    let lash_execution_contracts::process::ProcessInput::SessionTurn { create_request, .. } =
        &observed.input
    else {
        panic!("the observed process is a session turn");
    };
    assert_eq!(create_request.session_id, stated_id);
    let ran_in = sessions.lock_recover().clone();
    assert_eq!(ran_in.len(), 1, "the child ran one model call");
    if let Some(stated_id) = stated_id {
        assert_eq!(ran_in[0], stated_id, "a named child keeps its id");
    }
    core.shutdown().await?;
    assert_eq!(
        observed.child_session_id.as_ref(),
        Some(&ran_in[0]),
        "observation names the session the child actually ran"
    );
    Ok(())
}
