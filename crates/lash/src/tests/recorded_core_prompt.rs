//! The core prompt and a model's request defaults are recorded session
//! config (FIG-4397): a session records its core's prompt layer at creation,
//! beside its own prompt, and the model binding it was created with, request
//! defaults included (FIG-4374). It keeps all of them on every worker. One
//! engine runs sessions created with different prompts, each as created, and
//! a redeployed core with another prompt and other defaults for the same
//! model key reaches none of them.

use super::*;

/// The first deployment's core prompt: what every session it creates records.
const CORE_PROMPT_V1: &str = "CORE PROMPT OF THE CREATING DEPLOYMENT";
/// The redeployed core's prompt: no session created before it may render it.
const CORE_PROMPT_V2: &str = "CORE PROMPT OF THE REDEPLOYED CORE";

const ALPHA: &str = "recorded-core-prompt-alpha";
const BETA: &str = "recorded-core-prompt-beta";
/// Created with the creating core's defaults and no prompt of its own.
const GAMMA: &str = "recorded-core-prompt-gamma";

const ALPHA_PROMPT: &str = "ALPHA SESSION PROMPT";
const BETA_PROMPT: &str = "BETA SESSION PROMPT";

/// The request defaults the first deployment registers for the model: every
/// session it creates records them with its model binding.
fn creating_core_defaults() -> lash_core::provider::ModelRequestDefaults {
    lash_core::provider::ModelRequestDefaults {
        max_output_tokens: Some(3_333),
        response_metadata_headers: vec!["x-creator-cost".to_string()],
        response_metadata_body_paths: vec!["/creator/cost".to_string()],
        ..lash_core::provider::ModelRequestDefaults::default()
    }
}

/// The request defaults the redeployed core registers for the same model
/// key: no session created before it may be served with them.
fn redeployed_core_defaults() -> lash_core::provider::ModelRequestDefaults {
    lash_core::provider::ModelRequestDefaults {
        expose_thinking: true,
        max_output_tokens: Some(7_777),
        cache_retention: crate::provider::CacheRetention::Long,
        response_metadata_headers: vec!["x-redeployed-cost".to_string()],
        response_metadata_body_paths: vec!["/redeployed".to_string()],
    }
}

/// Every request a provider served, in the order it served them.
type Served = Arc<std::sync::Mutex<Vec<LlmRequest>>>;

/// A model that answers every call and keeps each request it served.
fn capturing_provider(served: &Served) -> ProviderHandle {
    let served = Arc::clone(served);
    crate::testing::TestProvider::builder()
        .kind("recorded-core-prompt")
        .complete(move |request| {
            let served = Arc::clone(&served);
            async move {
                served.lock_recover().push(request);
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

/// A core over `backend` whose creation defaults are `core_prompt` and a
/// model registered with `request_defaults`.
fn core_with_defaults(
    backend: lash_core::Backend,
    served: &Served,
    core_prompt: &str,
    request_defaults: lash_core::provider::ModelRequestDefaults,
) -> Result<LashCore> {
    use crate::PromptLayerSink as _;
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .instructions(core_prompt)
    .serve_test_model(
        capturing_provider(served),
        mock_model_spec().with_request_defaults(request_defaults),
    )
    .build(crate::testing::runtime_lease_owner())
}

/// Create `id` stating `prompt` as its own.
async fn create_with_prompt(core: &LashCore, id: &str, prompt: &str) -> Result<()> {
    use crate::PromptLayerSink as _;
    core.session(id)
        .create(crate::SessionCreation::default().instructions(prompt))
        .await?;
    Ok(())
}

/// Create ALPHA and BETA with prompts of their own and GAMMA with none.
async fn create_all(creator: &LashCore) -> Result<()> {
    create_with_prompt(creator, ALPHA, ALPHA_PROMPT).await?;
    create_with_prompt(creator, BETA, BETA_PROMPT).await?;
    creator
        .session(GAMMA)
        .create(crate::SessionCreation::default())
        .await?;
    Ok(())
}

/// Each session and its own prompt, when it stated one.
const RECORDED: [(&str, Option<&str>); 3] = [
    (ALPHA, Some(ALPHA_PROMPT)),
    (BETA, Some(BETA_PROMPT)),
    (GAMMA, None),
];

/// The last request `served` made for `id`.
fn last_request_of(served: &Served, id: &str) -> LlmRequest {
    served
        .lock_recover()
        .iter()
        .rev()
        .find(|request| request.scope.session_id.as_str() == id)
        .cloned()
        .unwrap_or_else(|| panic!("{id}: the provider served no request"))
}

/// What a session's served request must show: its own prompt (when it
/// stated one) and no other session's, the creating core's prompt beneath
/// it, never the redeployed core's, and the request defaults its model
/// binding recorded.
fn assert_served_as_recorded(request: &LlmRequest, id: &str, own_prompt: Option<&str>, how: &str) {
    let instructions = request.instructions.as_deref().unwrap_or_default();
    for (_, prompt) in RECORDED {
        let Some(prompt) = prompt else { continue };
        assert_eq!(
            instructions.contains(prompt),
            own_prompt == Some(prompt),
            "{id} {how}: renders its own prompt and no other session's: {instructions}"
        );
    }
    assert!(
        instructions.contains(CORE_PROMPT_V1),
        "{id} {how}: renders the core prompt it recorded at creation: {instructions}"
    );
    assert!(
        !instructions.contains(CORE_PROMPT_V2),
        "{id} {how}: a redeployed core's prompt never reaches it: {instructions}"
    );
    assert_eq!(
        request.request_defaults,
        creating_core_defaults(),
        "{id} {how}: its calls carry the request defaults its model binding recorded"
    );
}

/// One engine runs sessions created with different prompts: each root, on a
/// host open and on the engine's own reopen, is served the prompt its session
/// recorded over the core prompt it recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_is_served_the_prompts_it_was_created_with() -> Result<()> {
    let served: Served = Arc::default();
    let core = core_with_defaults(
        double_backend().await,
        &served,
        CORE_PROMPT_V1,
        creating_core_defaults(),
    )?;
    create_all(&core).await?;
    for (id, own) in RECORDED {
        core.session(id)
            .open()
            .await?
            .send(TurnInput::text("on a host open"))
            .output()
            .await?;
        assert_served_as_recorded(&last_request_of(&served, id), id, own, "on a host open");
        core.session(id)
            .durable()
            .await?
            .send(TurnInput::text("on the engine's reopen"))
            .output()
            .await?;
        assert_served_as_recorded(
            &last_request_of(&served, id),
            id,
            own,
            "on the engine's reopen",
        );
    }
    Ok(())
}

/// Sessions created on one engine with different prompts keep them, the
/// core prompt they recorded and their model's request defaults across an
/// engine restart: the `first` deployment goes away and a new one over its
/// stores runs a core with another prompt and other defaults for the same
/// model key. Each root, driven on the engine's own reopen and on a host
/// open, is served exactly as its session recorded.
async fn sessions_keep_their_prompts_and_request_defaults_across_a_restart(
    first: lash_restate_test::RestateTestBackend,
    restart: impl FnOnce(
        lash_restate_test::RestateTestBackend,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = lash_restate_test::RestateTestBackend>>,
    >,
) -> Result<()> {
    let served: Served = Arc::default();
    {
        let creator = core_with_defaults(
            first.lash_backend(),
            &served,
            CORE_PROMPT_V1,
            creating_core_defaults(),
        )?;
        create_all(&creator).await?;
    }
    let second = restart(first).await;
    let redeployed = core_with_defaults(
        second.lash_backend(),
        &served,
        CORE_PROMPT_V2,
        redeployed_core_defaults(),
    )?;
    for (id, own) in RECORDED {
        redeployed
            .session(id)
            .durable()
            .await?
            .send(TurnInput::text("after the restart, on the engine's reopen"))
            .output()
            .await?;
        assert_served_as_recorded(
            &last_request_of(&served, id),
            id,
            own,
            "on the engine's reopen after the restart",
        );
        let opened = redeployed.session(id).open().await?;
        opened
            .send(TurnInput::text("after the restart, on a host open"))
            .output()
            .await?;
        assert_served_as_recorded(
            &last_request_of(&served, id),
            id,
            own,
            "on a host open after the restart",
        );
    }
    Ok(())
}

fn redeploy_with_seed(
    seed: u64,
) -> impl FnOnce(
    lash_restate_test::RestateTestBackend,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = lash_restate_test::RestateTestBackend>>,
> {
    move |double| Box::pin(redeploy(double, seed))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_keep_their_prompts_and_request_defaults_across_a_restart_on_sqlite_memory()
-> Result<()> {
    let first = lash_restate_test::backend(0x4397_0001, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the first deployment over SQLite memory");
    sessions_keep_their_prompts_and_request_defaults_across_a_restart(
        first,
        redeploy_with_seed(0x4397_0002),
    )
    .await
}

/// Every await suspends and every resumption replays the root from its
/// journal: the replayed roots are served as their sessions recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_keep_their_prompts_and_request_defaults_across_a_restart_under_always_replay()
-> Result<()> {
    let first = lash_restate_test::backend(
        0x4397_0003,
        lash_restate_test::ServerConfig::default().always_replay(true),
    )
    .await
    .expect("build the first always-replay deployment over SQLite memory");
    sessions_keep_their_prompts_and_request_defaults_across_a_restart(first, |double| {
        Box::pin(async move {
            let stores = Arc::clone(double.engine_stores());
            drop(double);
            lash_restate_test::backend_with(
                0x4397_0004,
                lash_restate_test::ServerConfig::default().always_replay(true),
                move |_| stores,
            )
            .await
            .expect("redeploy the always-replay double over the same stores")
        })
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_keep_their_prompts_and_request_defaults_across_a_restart_on_sqlite_file()
-> Result<()> {
    let dir = tempfile::tempdir().expect("SQLite file store directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(dir.path())
            .await
            .expect("open the SQLite file store set"),
    );
    let first = lash_restate_test::backend_with(
        0x4397_0005,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("build the first deployment over a SQLite file");
    sessions_keep_their_prompts_and_request_defaults_across_a_restart(
        first,
        redeploy_with_seed(0x4397_0006),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_keep_their_prompts_and_request_defaults_across_a_restart_on_postgres()
-> Result<()> {
    let Some((stores, _held)) = postgres_store_set().await else {
        return Ok(());
    };
    let first = lash_restate_test::backend_with(
        0x4397_0007,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("build the first deployment over PostgreSQL");
    sessions_keep_their_prompts_and_request_defaults_across_a_restart(
        first,
        redeploy_with_seed(0x4397_0008),
    )
    .await
}
