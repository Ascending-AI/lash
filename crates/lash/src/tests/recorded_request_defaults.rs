//! A model's request defaults are recorded with a session's model binding
//! (FIG-4374), the response-metadata capture allowlists included (FIG-4397).
//! One engine runs sessions bound under different model keys, each call
//! carrying the defaults its own session recorded, across a redrive and a
//! redeployed registry (FIG-4567).

use super::*;

/// Every request a provider served, in the order it served them.
type Served = Arc<std::sync::Mutex<Vec<LlmRequest>>>;

/// Every request `served` made for `id`, in order.
fn requests_of(served: &Served, id: &str) -> Vec<LlmRequest> {
    served
        .lock_recover()
        .iter()
        .filter(|request| request.session_id().map(crate::SessionId::as_str) == Some(id))
        .cloned()
        .collect()
}

/// The two model keys one deployment registers, each with request defaults
/// of its own.
const KEY_ONE: &str = "recorded-defaults-key-one";
const KEY_TWO: &str = "recorded-defaults-key-two";

/// One session per key.
const ON_KEY_ONE: &str = "recorded-defaults-session-one";
const ON_KEY_TWO: &str = "recorded-defaults-session-two";

/// What the creating deployment registers under [`KEY_ONE`].
fn key_one_metadata() -> lash_core::LlmProfileMetadata {
    let mut metadata = llm_profile_spec(format!("{KEY_ONE}-wire"), None, 200_000)
        .with_request_defaults(lash_core::provider::LlmProfileRequestDefaults {
            response_metadata_headers: vec!["x-key-one-cost".to_string()],
            response_metadata_body_paths: vec!["/key-one/cost".to_string()],
            ..lash_core::provider::LlmProfileRequestDefaults::default()
        });
    metadata.limits.output_tokens =
        crate::OutputTokenLimits::new(None, Some(1111)).expect("valid recorded cap");
    metadata
}

/// What the creating deployment registers under [`KEY_TWO`]: no field agrees
/// with [`key_one_metadata`].
fn key_two_metadata() -> lash_core::LlmProfileMetadata {
    let mut metadata = llm_profile_spec(format!("{KEY_TWO}-wire"), None, 200_000)
        .with_request_defaults(lash_core::provider::LlmProfileRequestDefaults {
            expose_thinking: true,
            cache_retention: crate::provider::CacheRetention::Long,
            response_metadata_headers: vec![
                "x-key-two-cost".to_string(),
                "x-key-two-tier".to_string(),
            ],
            response_metadata_body_paths: vec!["/key-two/cost".to_string()],
        });
    metadata.limits.output_tokens =
        crate::OutputTokenLimits::new(None, Some(2222)).expect("valid recorded cap");
    metadata
}

/// A registry serving [`KEY_ONE`] and [`KEY_TWO`] through `provider`, each
/// under a wire model of its own and the request defaults given for it.
fn two_key_registry(
    provider: &ProviderHandle,
    one: lash_core::LlmProfileMetadata,
    two: lash_core::LlmProfileMetadata,
) -> Arc<lash_core::LlmProfileRegistry> {
    let registry = [(KEY_ONE, one), (KEY_TWO, two)]
        .into_iter()
        .try_fold(
            lash_core::LlmProfileRegistry::new(),
            |registry, (key, mut metadata)| {
                metadata.wire_model = format!("{key}-wire");
                registry.register(
                    key,
                    lash_core::RegisteredLlmProfile::new(metadata, provider.clone()),
                )
            },
        )
        .expect("the registry registers each key once");
    Arc::new(registry)
}

/// Two sessions on one engine run under two model keys whose request
/// defaults differ in every field, the capture allowlists included
/// (FIG-4567). Each session's calls carry the defaults its own binding
/// recorded, and keep carrying them on a redeployed build whose registry
/// serves each key with the other key's defaults. A model call resumed on
/// another node resends the body its admission stored (turn_phases' WIRE
/// law), so a redrive cannot reach the new registry either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_under_two_profile_keys_each_keep_their_request_defaults_across_a_redrive()
-> Result<()> {
    let stores = sqlite_memory_store_set().await;
    let served: Served = Arc::default();
    let provider = {
        let served = Arc::clone(&served);
        crate::testing::TestProvider::builder()
            .kind("recorded-request-defaults")
            .complete(move |request| {
                let served = Arc::clone(&served);
                async move {
                    served.lock_recover().push(request);
                    Ok(text_response("answered"))
                }
            })
            .build()
            .into_handle()
    };
    let deploy = |registry: Arc<lash_core::LlmProfileRegistry>| {
        explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
            stores.clone(),
        )))
        .llm_profiles(registry as Arc<dyn lash_core::LlmProfiles>)
        .build(crate::testing::runtime_lease_owner())
    };
    let recorded = [
        (ON_KEY_ONE, KEY_ONE, key_one_metadata()),
        (ON_KEY_TWO, KEY_TWO, key_two_metadata()),
    ];
    let assert_every_call_carries_its_own = |calls: usize, how: &str| {
        for (id, _, metadata) in &recorded {
            let requests = requests_of(&served, id);
            assert_eq!(requests.len(), calls, "{id} {how}: its model calls");
            for request in requests {
                assert_eq!(
                    request.model.metadata(),
                    metadata,
                    "{id} {how}: every call carries the defaults its session recorded"
                );
            }
        }
    };
    let run = async |core: &LashCore, text: &str| -> Result<()> {
        for (id, _, _) in &recorded {
            core.session(crate::SessionId::parse(*id).expect("nonblank host identity"))
                .durable()
                .await?
                .send(crate::TurnInput::text(text))
                .output()
                .await?;
        }
        Ok(())
    };

    let creating = deploy(two_key_registry(
        &provider,
        key_one_metadata(),
        key_two_metadata(),
    ))?;
    for (id, key, _) in &recorded {
        creating
            .session(crate::SessionId::parse(*id).expect("nonblank host identity"))
            .create(crate::SessionCreation::root(
                mock_session_spec().model(*key),
            ))
            .await?;
    }
    run(&creating, "under the creating registry").await?;
    assert_every_call_carries_its_own(1, "under the creating registry");
    creating.shutdown().await?;

    // The redeployed build serves each key with the other key's defaults.
    let redeployed = deploy(two_key_registry(
        &provider,
        key_two_metadata(),
        key_one_metadata(),
    ))?;
    run(&redeployed, "under the redeployed registry").await?;
    run(&redeployed, "again under the redeployed registry").await?;
    assert_every_call_carries_its_own(3, "under the redeployed registry");
    redeployed.shutdown().await?;
    Ok(())
}
