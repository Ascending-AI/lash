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
        .filter(|request| request.scope.session_id.as_str() == id)
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

/// The core's models across a redeploy on one engine: the creating
/// deployment's registry until [`redeploy`](Self::redeploy), the redeployed
/// one's after. A crash listener redeploys between a run's dead attempt and
/// its redrive, as a restarted worker comes back with the new build's
/// registry.
struct RedeployableLlmProfiles {
    creating: Arc<lash_core::LlmProfileRegistry>,
    redeployed: Arc<lash_core::LlmProfileRegistry>,
    is_redeployed: std::sync::atomic::AtomicBool,
}

impl RedeployableLlmProfiles {
    fn redeploy(&self) {
        self.is_redeployed.store(true, Ordering::SeqCst);
    }

    fn current(&self) -> &lash_core::LlmProfileRegistry {
        if self.is_redeployed.load(Ordering::SeqCst) {
            &self.redeployed
        } else {
            &self.creating
        }
    }
}

impl lash_core::LlmProfiles for RedeployableLlmProfiles {
    fn snapshot(
        &self,
        key: &lash_core::LlmProfileKey,
    ) -> std::result::Result<lash_core::RecordedLlmProfile, lash_core::LlmProfileUnavailable> {
        self.current().snapshot(key)
    }

    fn bind(
        &self,
        recorded: &lash_core::RecordedLlmProfile,
    ) -> std::result::Result<ProviderHandle, lash_core::LlmProfileUnavailable> {
        self.current().bind(recorded)
    }
}

/// Two sessions on one engine run under two model keys whose request
/// defaults differ in every field, the capture allowlists included
/// (FIG-4567). Each session's calls carry the defaults its own binding
/// recorded, and keep carrying them across a redrive: a run whose attempt
/// dies under its model call is redriven after the deployment's registry
/// changed to serve each key with the other key's defaults, and makes the
/// call again with the defaults its session recorded. So does every run the
/// session runs afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_under_two_profile_keys_each_keep_their_request_defaults_across_a_redrive()
-> Result<()> {
    let double = restate_double(0x4567_0001).await;
    let served: Served = Arc::default();
    // The provider kills the attempt that makes a call it is told to: the
    // run's handler dies before the call's result is journaled, so its
    // redrive makes the call again.
    let dies_under_its_call = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = {
        let served = Arc::clone(&served);
        let dies_under_its_call = Arc::clone(&dies_under_its_call);
        let double = double.clone();
        crate::testing::TestProvider::builder()
            .kind("recorded-request-defaults")
            .complete(move |request| {
                let served = Arc::clone(&served);
                if dies_under_its_call.swap(false, Ordering::SeqCst) {
                    double.crash_run_execution(lash_restate_test::CrashPoint::BeforeRunResult {
                        name: None,
                    });
                }
                async move {
                    served.lock_recover().push(request);
                    Ok(text_response("answered"))
                }
            })
            .build()
            .into_handle()
    };
    let models = Arc::new(RedeployableLlmProfiles {
        creating: two_key_registry(&provider, key_one_metadata(), key_two_metadata()),
        // The redeployed build serves each key with the other key's defaults.
        redeployed: two_key_registry(&provider, key_two_metadata(), key_one_metadata()),
        is_redeployed: std::sync::atomic::AtomicBool::new(false),
    });
    assert!(
        double.server().on_crash({
            let models = Arc::clone(&models);
            lash_restate_test::CrashCount::new().listener_with(move |_| models.redeploy())
        }),
        "the double takes the law's crash listener"
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .llm_profiles(Arc::clone(&models) as Arc<dyn lash_core::LlmProfiles>)
        .build(crate::testing::runtime_lease_owner())?;
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
    for (id, key, _) in &recorded {
        core.session(*id)
            .create(crate::SessionCreation {
                spec: mock_session_spec().model(*key),
                parent: None,
            })
            .await?;
    }
    for (id, _, _) in &recorded {
        core.session(*id)
            .durable()
            .await?
            .send(TurnInput::text("under the creating registry"))
            .output()
            .await?;
    }
    assert_every_call_carries_its_own(1, "under the creating registry");
    // Each session's next run dies under its model call and is redriven.
    // The first death redeploys the registry, so the first session's redrive
    // and both of the second session's attempts run under the redeployed one.
    for (redriven, (id, _, _)) in recorded.iter().enumerate() {
        dies_under_its_call.store(true, Ordering::SeqCst);
        core.session(*id)
            .durable()
            .await?
            .send(TurnInput::text("dies under its call and is redriven"))
            .output()
            .await?;
        assert_eq!(
            double.server().stats().crashes,
            redriven as u64 + 1,
            "{id}: the run's first attempt died under its model call"
        );
        assert_eq!(
            requests_of(&served, id).len(),
            3,
            "{id}: the dead attempt made the call and the redrive made it again"
        );
    }
    assert_every_call_carries_its_own(3, "across the redrive");
    for (id, _, _) in &recorded {
        core.session(*id)
            .durable()
            .await?
            .send(TurnInput::text("under the redeployed registry"))
            .output()
            .await?;
    }
    assert_every_call_carries_its_own(4, "under the redeployed registry");
    Ok(())
}
