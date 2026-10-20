//! CLAIM-EXPLANATION-MATCHES-PREDICATE and retained attribution (D3/D4).
use super::*;
use crate::plugins::diagnostics::*;
use lash_core::durable_port::domain::{DomainWrite, TurnWrite};
use lash_core::durable_port::{CommitLabel, MailTx, NodeId, NodeSpec};

/// RECORDED-ATTRIBUTION-SURVIVES-REMOVAL: the binding is from a retained
/// admission, even when no installed provider owns its recorded address.
/// Check selections explicitly report that this format never recorded them.
#[tokio::test]
async fn recorded_attribution_survives_removal() {
    use lash_core::durable_port::domain::{RunFactKind, RunFactWrite};
    use lash_core::testing::{AdmitMember, PinnedMember, RoundFact};
    use lash_core::tool_run::{MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole};
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
    let backend = sqlite_memory_store_backend().await;
    let factory = Arc::new(StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("removed-plugin"),
        crate::plugins::PluginSpec::new()
            .with_tool_provider(crate::tool_provider_key!("removed"), Arc::new(AppTools)),
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_sessions(false)
        .plugin(factory)
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::tests::host::runtime_lease_owner())
        .unwrap();
    // A real isolated owner registry publishes the frozen inventory, and its
    // lifetime ends with the materialization rather than the observer.
    let observed = crate::SessionId::from("materialized-observer");
    let runtime = created_session(&core, observed.clone())
        .await
        .open_runtime()
        .await
        .unwrap();
    let plugins = runtime.assembled_plugin_session().clone();
    plugins.materialize().unwrap();
    let scope = InventoryScope::Owner(crate::RuntimeOwner::Session(observed));
    assert!(
        matches!(core.plugin_diagnostics().installed(&scope), InstalledObservation::Present(inventory) if inventory.scope == scope)
    );
    drop(plugins);
    drop(runtime);
    assert!(matches!(
        core.plugin_diagnostics().installed(&scope),
        InstalledObservation::NotMaterialized { .. }
    ));
    let store = backend.durable();
    let actor = ActorKey::session("retained-attribution").unwrap();
    let format = FormatSet::new("retained-attribution");
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new("retained-writer"),
            decodes: vec![format.clone()],
            serves: Vec::new(),
            held: Default::default(),
            ttl_millis: 60000,
        })
        .await
        .unwrap();
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), format);
    store
        .commit_mail(mail, CommitLabel::TURN_ADMIT)
        .await
        .unwrap();
    let epoch = store.claim(&writer, 1).await.unwrap()[0].epoch;
    let owner = OwnerKey::Turn("retained-attribution".into(), "recorded-run".into());
    let callback =
        crate::plugins::PluginCallbackIdentity::tool_provider("removed-plugin", "removed");
    let binding = AdmittedBinding {
        executable: callback.clone(),
        preparation: callback.clone(),
        presentation: PresentationBinding {
            presenter: None,
            steps: vec![crate::plugins::PluginCallbackIdentity {
                owner: "removed-plugin".into(),
                key: "presentation_step:details".into(),
            }],
        },
    };
    let call = crate::ToolCallId::fixture("recorded-call");
    let request = MaterialPayload::new(
        MaterialOwner::Run {
            opener: lash_core::EffectOpener::turn("retained-attribution", "recorded-run"),
        },
        MaterialRole::PreparedRequest,
        "{}".into(),
    )
    .reference(MaterialLocation::JournalLocal)
    .unwrap();
    let fact = RoundFact::Admit {
        members: vec![AdmitMember::Pinned(Box::new(PinnedMember {
            call: call.clone(),
            tool: "removed-tool".into(),
            request,
            policy: lash_core::ExecutionPolicy::Once,
            limit_expires_at_ms: 60000,
            limit_max_slice_ms: 1000,
            wait_id: None,
            trace: None,
            declaration: None,
            binding: Some(binding.clone()),
        }))],
    };
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1024 * 1024)));
    let record = fact.stage(&mut staging).unwrap();
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: "retained-attribution".into(),
        run: "recorded-run".into(),
        admission: lash_core::store::RunAdmissionRecord::Turn {
            took: lash_core::store::AdmittedTurnRows::Inputs {
                ids: lash_core::store::AdmittedInputIds::one("input".into()),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    tx.set_requires(
        binding
            .callbacks()
            .map(|callback| {
                crate::plugins::ObligationToken::for_callback(
                    crate::plugins::RegistryKind::Session,
                    callback,
                )
                .to_string()
            })
            .collect(),
    );
    tx.stage_all(staging)
        .write(DomainWrite::RunFact(RunFactWrite::Append {
            owner: owner.clone(),
            run: RunSeq(1),
            ordinal: Ordinal(0),
            kind: RunFactKind::Admit,
            call: None,
            record,
        }));
    store.commit(tx, CommitLabel::TURN_ADMIT).await.unwrap();
    let bounded = store
        .run_facts_within(
            &owner,
            Some(RunSeq(1)),
            0,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(1024 * 1024),
        )
        .await;
    assert!(matches!(
        bounded,
        Err(lash_core::durable_port::DurableError::Graph(
            lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                actual: 1,
                limit: 0,
                ..
            }
        ))
    ));
    let source = RecordLocator::RunFact {
        owner,
        run: RunSeq(1),
        ordinal: Ordinal(0),
    };
    let diagnostics = core.plugin_diagnostics();
    let decision = DecisionRef::Tool {
        record: source.clone(),
        call: call.clone(),
        selection: ToolDecisionSelection::Executable,
    };
    let revision = store.actor(&actor).await.unwrap().unwrap().revision;
    let page = diagnostics
        .obligation_evidence(
            &"retained-attribution".into(),
            revision,
            0,
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(page.evidence, Evidence::Recorded { value, .. } if value.len() == 1));
    assert_eq!(page.next, Some(1));
    let page = diagnostics
        .obligation_evidence(
            &"retained-attribution".into(),
            revision,
            1,
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(page.evidence, Evidence::Recorded { value, .. } if value.len() == 1));
    assert_eq!(page.next, None);
    let stale = diagnostics
        .obligation_evidence(
            &"retained-attribution".into(),
            StateRevision(revision.0 + 1),
            0,
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        stale.evidence,
        Evidence::Unavailable {
            reason: EvidenceAbsence::ObservationChanged
        }
    ));
    let before = diagnostics.attribute(&decision).await.unwrap();
    assert_eq!(before.installed_now.len(), 1);
    core.shutdown().await.unwrap();
    let removed = diagnostic_core(backend.clone());
    let diagnostics = removed.plugin_diagnostics();
    let attributed = diagnostics.attribute(&decision).await.unwrap();
    assert!(attributed.installed_now.is_empty());
    assert!(
        matches!(attributed.evidence, Evidence::Recorded { value: RecordedAttribution::Binding { binding: recorded, selected }, .. } if recorded == binding && selected == callback)
    );
    let checks = diagnostics
        .attribute(&DecisionRef::Tool {
            record: source,
            call,
            selection: ToolDecisionSelection::BeforeCheck { reply: None },
        })
        .await
        .unwrap();
    assert!(matches!(
        checks.evidence,
        Evidence::Unavailable {
            reason: EvidenceAbsence::AttributionNotRecorded
        }
    ));
    let absent = diagnostics
        .attribute(&DecisionRef::Callback {
            record: RecordLocator::TurnPhase {
                session: "retained-attribution".into(),
                run: "released-run".into(),
            },
            ordinal: 0,
        })
        .await
        .unwrap();
    assert!(matches!(
        absent.evidence,
        Evidence::Unavailable {
            reason: EvidenceAbsence::NotRetainedOrNeverRecorded
        }
    ));
    removed.shutdown().await.unwrap();
}
