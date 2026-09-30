use super::*;

#[derive(Default)]
struct MaterializationRoots {
    roots: StdMutex<Vec<bool>>,
}

impl lash_core::plugin::ProtocolSessionPlugin for MaterializationRoots {
    fn configure_runtime_on_materialize(
        &self,
        _runtime: lash_core::plugin::ProtocolRuntimeContext<'_>,
        materialization: lash_core::plugin::ProtocolSessionMaterialization<'_>,
    ) -> std::result::Result<(), lash_core::SessionError> {
        self.roots
            .lock_recover()
            .push(materialization.is_root_session);
        Ok(())
    }
}

#[tokio::test]
async fn ordinary_child_is_not_root_under_facade_and_engine_opens() -> Result<()> {
    let protocol = Arc::new(MaterializationRoots::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .protocol_plugin(
        lash_core::testing::test_standard_protocol_factory_with_runtime_state(
            protocol.clone(),
            None,
        ),
    )
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let id = SessionId::from("ordinary-lineage-child");
    let durable = core
        .session(id.clone())
        .create(crate::SessionCreation {
            parent: Some("ordinary-lineage-parent".into()),
            ..Default::default()
        })
        .await?;
    protocol.roots.lock_recover().clear();
    let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
    let state = crate::session::load_state_from_store(&id, &core.policy, &store).await?;
    assert!(state.authority.subagent.is_none());

    let session = core.session(id).open().await?;
    assert_eq!(session.parent_session_id(), Some("ordinary-lineage-parent"));
    assert_eq!(*protocol.roots.lock_recover(), vec![false]);
    drop(session);
    durable
        .send(TurnInput::text("open through the engine"))
        .output()
        .await?;
    assert_eq!(
        *protocol.roots.lock_recover(),
        vec![false, false],
        "both openers must classify the ordinary child from recorded lineage"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_openers_default_from_lineage_and_preserve_recorded_formats() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let double = restate_double(0x4252).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("lineage-defaults")
                .complete(|_| async {
                    Ok(text_response(
                        "<typescript>\nfinish(\"answered\");\n</typescript>",
                    ))
                })
                .build()
                .into_handle(),
        )
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    for engine in [false, true] {
        for (case, parent, recorded, expected) in [
            (
                "child-empty",
                Some("lineage-parent"),
                None,
                RlmFinalAnswerFormat::RawFinalValue,
            ),
            ("root-empty", None, None, RlmFinalAnswerFormat::Markdown),
            (
                "child-recorded",
                Some("lineage-parent"),
                Some(RlmFinalAnswerFormat::Markdown),
                RlmFinalAnswerFormat::Markdown,
            ),
            (
                "root-recorded",
                None,
                Some(RlmFinalAnswerFormat::RawFinalValue),
                RlmFinalAnswerFormat::RawFinalValue,
            ),
        ] {
            let id = SessionId::from(format!("{case}-engine-{engine}"));
            let mut policy = core.policy.clone();
            policy.session_id = Some(id.clone());
            let mut config = lash_core::PersistedSessionConfig::from(&policy);
            let empty_options = recorded.is_none();
            if let Some(format) = recorded {
                config.protocol_turn_options = Some(lash_core::ProtocolTurnOptions::typed(
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(format),
                        ..Default::default()
                    },
                )?);
            }
            // Admit the exact empty-options case without facade creation,
            // which normally resolves and records RLM defaults beforehand.
            let store = lash_core::runtime::admit_session_view(
                &core.store_factory,
                &lash_core::SessionStoreCreateRequest {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: id.clone(),
                    relation: parent
                        .map(|parent| lash_core::SessionRelation::Child {
                            parent_session_id: parent.into(),
                            caused_by: None,
                        })
                        .unwrap_or_default(),
                    config,
                    head: lash_core::SessionCreationHead::Config,
                },
            )
            .await?;
            let state = crate::session::load_state_from_store(&id, &policy, &store).await?;
            assert!(state.authority.subagent.is_none());
            assert_eq!(state.protocol_turn_options.is_empty(), empty_options);
            if engine {
                core.session(id.clone())
                    .durable()
                    .await?
                    .send(TurnInput::text("resolve the format through the engine"))
                    .output()
                    .await?;
            }
            let session = retry_when_claim_frees(|| core.session(id.clone()).open()).await?;
            assert_eq!(session.parent_session_id(), parent);
            assert_eq!(
                session
                    .rlm_config()
                    .expect("materialized RLM options")
                    .final_answer_format,
                Some(expected),
                "{case}, engine={engine}"
            );
        }
    }
    Ok(())
}
