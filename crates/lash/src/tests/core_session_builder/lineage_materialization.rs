use super::*;

const LINEAGE: &str = "lineage-roots";

/// An owner that records, as its namespace, whether the session it resolved
/// for was a root (FIG-4379): the lineage an owner sees is the creator's, once.
#[derive(Default)]
struct LineageRoots {
    resolved: Arc<StdMutex<Vec<bool>>>,
}

/// The `lineage` namespace: whether the session was created a root.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct LineageConfig {
    root: bool,
}

/// The `lineage` owner refuses nothing.
#[derive(serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
enum LineageRefusal {}

impl std::fmt::Display for LineageRefusal {
    fn fmt(&self, _formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

struct LineageOwner {
    resolved: Arc<StdMutex<Vec<bool>>>,
}

impl lash_core::ConfigOwner for LineageOwner {
    type Create = LineageConfig;
    type Recorded = LineageConfig;
    type Refusal = LineageRefusal;
    type RunOptions = lash_core::NoRunOptions;

    fn implementation(&self) -> &str {
        "lineage:1"
    }

    fn create(
        &self,
        _input: Option<LineageConfig>,
        facts: lash_core::CreationFacts<'_, LineageConfig>,
    ) -> std::result::Result<Option<LineageConfig>, LineageRefusal> {
        self.resolved.lock_recover().push(facts.is_root_session);
        Ok(Some(LineageConfig {
            root: facts.is_root_session,
        }))
    }

    fn validate(
        &self,
        _value: &LineageConfig,
        _base: Option<&LineageConfig>,
        _facts: &lash_core::CandidateFacts<'_>,
    ) -> std::result::Result<(), LineageRefusal> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &Self::Recorded,
        _options: Self::RunOptions,
    ) -> std::result::Result<Self::Recorded, Self::Refusal> {
        Ok(recorded.clone())
    }
}

struct LineageRootsPlugin;

impl lash_core::facade_support::SessionPlugin for LineageRootsPlugin {
    fn id(&self) -> &'static str {
        LINEAGE
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::facade_support::PluginFactory for LineageRoots {
    fn id(&self) -> &'static str {
        LINEAGE
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(LineageRootsPlugin))
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> std::result::Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(LineageOwner {
            resolved: Arc::clone(&self.resolved),
        })
    }
}

/// An ordinary child is classified from its recorded lineage once, when it is
/// created; the facade and engine opens deliver that recorded classification
/// and never ask the owner again.
#[tokio::test]
async fn ordinary_child_is_not_root_under_facade_and_engine_opens() -> Result<()> {
    let owner = Arc::new(LineageRoots::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .plugin(owner.clone())
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::from("ordinary-lineage-child");
    let durable = core
        .session(id.clone())
        .create(crate::SessionCreation {
            spec: mock_session_spec(),
            parent: Some("ordinary-lineage-parent".into()),
        })
        .await?;
    assert_eq!(*owner.resolved.lock_recover(), vec![false]);
    let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
    let state = crate::session::load_state_from_store(&id, &store).await?;
    assert!(state.authority.subagent.is_none());
    assert_eq!(
        state.authority.plugin_config.get(LINEAGE),
        Some(&serde_json::json!({ "root": false }))
    );

    let session = core.session(id).open().await?;
    assert_eq!(session.parent_session_id(), Some("ordinary-lineage-parent"));
    drop(session);
    durable
        .send(TurnInput::text("open through the engine"))
        .output()
        .await?;
    assert_eq!(
        *owner.resolved.lock_recover(),
        vec![false],
        "opens deliver the recorded config and never re-resolve it"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_creation_defaults_from_lineage_and_opens_preserve_recorded_formats() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let double = restate_double(0x4252).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(
            crate::testing::TestProvider::builder()
                .kind("lineage-defaults")
                .complete(|_| async {
                    Ok(text_response(
                        "<typescript>\nfinish(\"answered\");\n</typescript>",
                    ))
                })
                .build()
                .into_handle(),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    for engine in [false, true] {
        for (case, parent, stated, expected) in [
            (
                "child-unstated",
                Some("lineage-parent"),
                None,
                RlmFinalAnswerFormat::RawFinalValue,
            ),
            ("root-unstated", None, None, RlmFinalAnswerFormat::Markdown),
            (
                "child-stated",
                Some("lineage-parent"),
                Some(RlmFinalAnswerFormat::Markdown),
                RlmFinalAnswerFormat::Markdown,
            ),
            (
                "root-stated",
                None,
                Some(RlmFinalAnswerFormat::RawFinalValue),
                RlmFinalAnswerFormat::RawFinalValue,
            ),
        ] {
            let id = SessionId::from(format!("{case}-engine-{engine}"));
            let plugin_options = match stated {
                Some(ref format) => lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(format.clone()),
                        ..Default::default()
                    },
                )?,
                None => lash_core::PluginOptions::default(),
            };
            let durable = core
                .session(id.clone())
                .create(crate::SessionCreation {
                    parent: parent.map(Into::into),
                    spec: mock_session_spec().plugin_options(plugin_options),
                })
                .await?;
            let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
            let state = crate::session::load_state_from_store(&id, &store).await?;
            assert!(state.authority.subagent.is_none());
            assert_eq!(
                lash_protocol_rlm::rlm_session_config(&state.effective_protocol_turn_options())
                    .expect("recorded RLM config")
                    .final_answer_format,
                Some(expected.clone()),
                "creation records the lineage default: {case}"
            );
            if engine {
                durable
                    .send(TurnInput::text("resolve the format through the engine"))
                    .output()
                    .await?;
            }
            let session = retry_when_claim_frees(|| core.session(id.clone()).open()).await?;
            assert_eq!(session.parent_session_id(), parent);
            assert_eq!(
                session
                    .rlm_config()
                    .expect("recorded RLM options")
                    .final_answer_format,
                Some(expected.clone()),
                "{case}, engine={engine}"
            );
        }
    }
    Ok(())
}
