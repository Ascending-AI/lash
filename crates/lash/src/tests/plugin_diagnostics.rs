//! CLAIM-EXPLANATION-MATCHES-PREDICATE and retained attribution (D3/D4).
use super::*;
use crate::plugins::diagnostics::*;
use lash_core::Backend;
use lash_core::durable_port::domain::{
    CANCEL_MAIL, DomainWrite, MailDomainWrite, SESSION_CLOSE_MAIL, SessionCloseWrite,
    TurnCancelRequest, TurnWrite,
};
use lash_core::durable_port::{
    CommitLabel, MailKind, MailTx, NodeId, NodeLease, NodeSpec, Release,
};

fn diagnostic_core(backend: Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_sessions(false)
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .unwrap()
}
fn node_snapshot(lease: &NodeLease) -> NodeServingSnapshot {
    NodeServingSnapshot {
        owner: Some(lease.owner.clone()),
        observed_at: lease.expires_at,
        decodes: lease.decodes.clone(),
        serves: lease.serves.iter().cloned().collect(),
        inventory: None,
    }
}
async fn predicate_law(backend: Backend) {
    let core = diagnostic_core(backend.clone());
    let diagnostics = core.plugin_diagnostics();
    let store = backend.durable();
    for case in [
        "run",
        "unserved",
        "immediate",
        "afterstep",
        "not-due",
        "draining",
        "replaced",
        "unreadable-session",
        "close-mail",
        "unfinished-close",
        "process-cancel",
    ] {
        let format = FormatSet::new(format!("diagnostic-format-{case}"));
        let actor = if case == "process-cancel" {
            ActorKey::process(case).unwrap()
        } else {
            ActorKey::session(case).unwrap()
        };
        let spec = NodeSpec {
            node: NodeId::new(format!("writer-{case}")),
            decodes: vec![format.clone()],
            serves: vec!["unserved".into()],
            ttl_millis: 60000,
        };
        let writer = store.register_node(&spec).await.unwrap();
        let mut mail = MailTx::new();
        mail.create_actor(actor.clone(), format.clone());
        store
            .commit_mail(mail, CommitLabel::TURN_ADMIT)
            .await
            .unwrap();
        let claimed = store
            .claim(&writer, 128)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.actor == actor)
            .unwrap();
        let mut tx = store.begin(&actor, claimed.epoch).await.unwrap();
        tx.set_requires(if case == "run" || case == "not-due" {
            Vec::new()
        } else {
            vec!["unserved".into()]
        });
        if matches!(case, "immediate" | "afterstep" | "unreadable-session") {
            tx.write(DomainWrite::Turn(TurnWrite::Admit {
                session: case.into(),
                run: "cancel-turn".into(),
                admission: lash_core::store::RunAdmissionRecord::Turn {
                    took: lash_core::store::AdmittedTurnRows::Inputs {
                        ids: lash_core::store::AdmittedInputIds::one("input".into()),
                    },
                    trace: None,
                },
                turn_deadline: None,
            }));
        }
        if case == "unfinished-close" {
            tx.write(DomainWrite::SessionClose(SessionCloseWrite::Begin {
                session: case.into(),
            }));
        }
        tx.give_up(if case == "not-due" {
            Release::Waiting {
                next_due: Some(DurableInstant(store.now().await.unwrap().0 + 60000)),
            }
        } else {
            Release::Ready
        });
        store.commit(tx, CommitLabel::TURN_ADMIT).await.unwrap();
        if matches!(case, "immediate" | "afterstep" | "unreadable-session") {
            let mut cancel = MailTx::new();
            cancel.write(MailDomainWrite::RequestTurnCancel(TurnCancelRequest {
                session: case.into(),
                run: "cancel-turn".into(),
                request_id: format!("request-{case}"),
                origin: None,
                reason: None,
                undelivered: crate::TurnCancelUndeliveredInputPolicy::Defer,
                mode: if case == "afterstep" {
                    crate::TurnCancelMode::AfterStep
                } else {
                    crate::TurnCancelMode::Immediate
                },
            }));
            store
                .commit_mail(cancel, CommitLabel::TURN_CANCEL)
                .await
                .unwrap();
        }
        if matches!(case, "close-mail" | "process-cancel") {
            let mut cancel = MailTx::new();
            cancel.append(
                actor.clone(),
                MailKind::new(if case == "close-mail" {
                    SESSION_CLOSE_MAIL
                } else {
                    CANCEL_MAIL
                }),
                "{}",
            );
            store
                .commit_mail(cancel, CommitLabel::TURN_CANCEL)
                .await
                .unwrap();
        }
        let target_spec = NodeSpec {
            node: NodeId::new(format!("target-{case}")),
            decodes: if matches!(case, "unreadable-session" | "process-cancel") {
                Vec::new()
            } else {
                vec![format]
            },
            serves: Vec::new(),
            ttl_millis: 60000,
        };
        let target = store.register_node(&target_spec).await.unwrap();
        let snapshot = node_snapshot(&target);
        if case == "draining" {
            store.mark_draining(&target).await.unwrap();
        }
        if case == "replaced" {
            store.register_node(&target_spec).await.unwrap();
        }
        let explanation = diagnostics
            .explain_actor_claimability(Some(&snapshot), &actor)
            .await
            .unwrap();
        let actual = store.claim(&target, 128).await;
        let actual = match actual {
            Ok(rows) => rows
                .into_iter()
                .find(|row| row.actor == actor)
                .map(|row| row.purpose),
            Err(lash_core::durable_port::DurableError::NodeLeaseLost { .. }) => None,
            Err(error) => panic!("{case}: {error}"),
        };
        let expected = match case {
            "run" => Some(ClaimPurpose::Run),
            "immediate" | "close-mail" | "unfinished-close" | "process-cancel" => {
                Some(ClaimPurpose::CancelOnly)
            }
            _ => None,
        };
        assert_eq!(actual, expected, "actual store predicate: {case}");
        assert_eq!(
            explanation.eligibility,
            expected.map_or(ClaimEligibility::Ineligible, |purpose| {
                ClaimEligibility::Eligible { purpose }
            }),
            "diagnostic predicate: {case}"
        );
        if expected == Some(ClaimPurpose::CancelOnly) {
            assert!(
                explanation
                    .requirements
                    .iter()
                    .any(|requirement| requirement.served == Some(false))
            );
            assert!(explanation.blockers.is_empty());
            assert!(!explanation.run_blockers.is_empty());
        }
        let unknown = diagnostics
            .explain_actor_claimability(None, &actor)
            .await
            .unwrap();
        assert!(matches!(
            unknown.eligibility,
            ClaimEligibility::Unknown { .. }
        ));
        assert!(
            unknown
                .requirements
                .iter()
                .all(|requirement| requirement.served.is_none())
        );
        assert_eq!(explanation.node.unwrap().observed_at, snapshot.observed_at);
        if case == "replaced" {
            assert!(explanation.blockers.contains(&ClaimBlocker::BootReplaced));
        }
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn claim_explanation_matches_predicate_on_sqlite() {
    predicate_law(sqlite_memory_store_backend().await).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run inside a with-service.sh pg gate"]
async fn claim_explanation_matches_predicate_on_postgres() {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    predicate_law(lash_conformance::backend_over(stores)).await;
}

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
        .build(crate::testing::runtime_lease_owner())
        .unwrap();
    // A real isolated owner registry publishes the frozen inventory, and its
    // lifetime ends with the materialization rather than the observer.
    let observed = crate::SessionId::from("materialized-observer");
    let runtime = core
        .session(observed.clone())
        .created()
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

/// Retained expiry replies remain attributable after their owner is removed.
#[tokio::test]
async fn retained_expiry_check_attribution_survives_removal() {
    use lash_core::durable_port::domain::{RunFactKind, RunFactWrite};
    use lash_core::testing::{CallOutcome, MaterialNode, RoundFact, SettledFact};
    use lash_core::tool_run::{AttemptOutcome, MaterialOwner, MaterialRole};
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
    let backend = sqlite_memory_store_backend().await;
    let core = diagnostic_core(backend.clone());
    let store = backend.durable();
    let actor = ActorKey::session("expiry-attribution").unwrap();
    let format = FormatSet::new("expiry-attribution");
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new("expiry-writer"),
            decodes: vec![format.clone()],
            serves: Vec::new(),
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
    let owner = OwnerKey::Turn("expiry-attribution".into(), "recorded-run".into());
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: "expiry-attribution".into(),
        run: "recorded-run".into(),
        admission: lash_core::store::RunAdmissionRecord::Turn {
            took: lash_core::store::AdmittedTurnRows::Inputs {
                ids: lash_core::store::AdmittedInputIds::one("input".into()),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1024 * 1024)));
    let mut decisions = Vec::new();
    for (index, phase) in [
        "tool_args",
        "tool_result",
        "tool_args",
        "tool_result",
        "tool_args",
        "tool_args",
    ]
    .into_iter()
    .enumerate()
    {
        let key = if index % 2 == 0 {
            "tool_args_check:timeout"
        } else {
            "tool_result_check:timeout"
        };
        let callback = crate::plugins::PluginCallbackIdentity {
            owner: "removed-check".into(),
            key: key.into(),
        };
        let expiry = lash_sansio::ControlPhaseExpired {
            phase: phase.into(),
            bound_ms: 10,
            plugin: callback.owner.clone(),
            callback: callback.key.clone(),
        };
        let failure = lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::Internal,
            "expired",
            "expired",
        )
        .with_cause(lash_core::ToolFailureCause::ControlPhaseExpired {
            expiry: Box::new(expiry),
        });
        let prefix = if index % 2 == 0 {
            "tool_args_check"
        } else {
            "tool_result_check"
        };
        let answered = crate::plugins::PluginCallbackIdentity {
            owner: "answered".into(),
            key: format!("{prefix}:allow"),
        };
        let stronger = crate::plugins::PluginCallbackIdentity {
            owner: "stronger".into(),
            key: format!("{prefix}:abort"),
        };
        let evidence = if index % 2 == 0 {
            let mut replies = vec![
                AttributedVerdict {
                    callback: callback.clone(),
                    verdict: crate::plugins::BeforeToolDecision::Deny(failure.clone()),
                },
                AttributedVerdict {
                    callback: answered,
                    verdict: crate::plugins::BeforeToolDecision::Allow,
                },
            ];
            if index >= 2 {
                replies.push(AttributedVerdict {
                    callback: stronger,
                    verdict: crate::plugins::BeforeToolDecision::AbortRun(
                        crate::plugins::PluginAbort::new("stop", "stop"),
                    ),
                });
            }
            serde_json::to_value(CheckRecord::reduce(replies)).unwrap()
        } else {
            let mut replies = vec![
                AttributedVerdict {
                    callback: callback.clone(),
                    verdict: crate::plugins::AfterToolDecision::Deny(failure.clone()),
                },
                AttributedVerdict {
                    callback: answered,
                    verdict: crate::plugins::AfterToolDecision::Allow,
                },
            ];
            if index >= 2 {
                replies.push(AttributedVerdict {
                    callback: stronger,
                    verdict: crate::plugins::AfterToolDecision::AbortRun(
                        crate::plugins::PluginAbort::new("stop", "stop"),
                    ),
                });
            }
            serde_json::to_value(CheckRecord::reduce(replies)).unwrap()
        };
        let evidence = match index {
            4 => serde_json::json!({"ordinary_tool_data": true}),
            5 => {
                serde_json::json!([{"callback": callback, "verdict": {"verdict": "future_representation"}}])
            }
            _ => evidence,
        };
        let raw = lash_core::ToolValue::untrusted_json(evidence);
        let output = if index == 3 {
            let mut cancellation = lash_core::ToolCancellation::runtime("selected cancellation");
            cancellation.raw = Some(raw);
            lash_core::ToolCallOutput::cancelled(cancellation)
        } else {
            let mut selected_failure = if index == 2 || index == 4 {
                lash_core::ToolFailure::runtime(
                    lash_core::ToolFailureClass::Internal,
                    "selected",
                    "selected",
                )
            } else {
                failure
            };
            selected_failure.raw = Some(raw);
            lash_core::ToolCallOutput::failure(selected_failure)
        };
        let call = crate::ToolCallId::fixture(&format!("expiry-{index}"));
        let completed = lash_sansio::sansio::CompletedToolCall {
            call_id: call.clone(),
            provider_call_id: None,
            tool_name: "probe".into(),
            args: serde_json::json!({}),
            model_return: lash_sansio::tool_output::ModelToolReturn::from_output(
                "probe".into(),
                &output,
            ),
            output,
            intent_outcomes: Vec::<lash_core::ToolIntentExecutionOutcome>::new(),
            replay: None,
        };
        let material = MaterialNode::Outcome(
            CallOutcome::of(&completed, None, 1)
                .stage(&mut staging)
                .unwrap(),
        );
        let named = material.material(
            MaterialOwner::Run {
                opener: lash_core::EffectOpener::turn("expiry-attribution", "recorded-run"),
            },
            MaterialRole::AttemptOutput,
        );
        let fact = RoundFact::Outcome {
            start: 1,
            output: SettledFact {
                outcome: AttemptOutcome::Completed(named.reference().clone()),
                material: Some(material),
            },
            state: Vec::new(),
        };
        let record = fact.stage(&mut staging).unwrap();
        let ordinal = Ordinal(u64::try_from(index).unwrap());
        tx.write(DomainWrite::RunFact(RunFactWrite::Append {
            owner: owner.clone(),
            run: RunSeq(1),
            ordinal,
            kind: RunFactKind::XOutcome,
            call: Some(call.clone()),
            record,
        }));
        decisions.push((
            RecordLocator::RunFact {
                owner: owner.clone(),
                run: RunSeq(1),
                ordinal,
            },
            call,
            callback,
        ));
    }
    tx.stage_all(staging);
    store.commit(tx, CommitLabel::TURN_ADMIT).await.unwrap();
    for (index, (record, call, callback)) in decisions.into_iter().enumerate() {
        let reply = Some(if index >= 2 { 1 } else { 0 });
        let selection = if index % 2 == 0 || index == 5 {
            ToolDecisionSelection::BeforeCheck { reply }
        } else {
            ToolDecisionSelection::ResultCheck {
                occurrence: ToolHookOccurrence::Cached,
                reply,
            }
        };
        let attributed = core
            .plugin_diagnostics()
            .attribute(&DecisionRef::Tool {
                record: record.clone(),
                call: call.clone(),
                selection,
            })
            .await
            .unwrap();
        assert!(attributed.installed_now.is_empty());
        if index >= 4 {
            let reason = if index == 4 {
                EvidenceAbsence::AttributionNotRecorded
            } else {
                EvidenceAbsence::UnsupportedRetainedRepresentation
            };
            assert!(
                matches!(attributed.evidence, Evidence::Unavailable { reason: observed } if observed == reason)
            );
            continue;
        }
        match attributed.evidence {
            Evidence::Recorded {
                value:
                    RecordedAttribution::BeforeCheck {
                        selected: Some(reply),
                        ..
                    },
                ..
            } if index % 2 == 0 => assert_eq!(reply.callback, callback),
            Evidence::Partial {
                value:
                    RecordedAttribution::ResultCheck {
                        selected: Some(reply),
                        ..
                    },
                missing,
                ..
            } if index % 2 == 1 => {
                assert_eq!(reply.callback, callback);
                assert!(
                    missing.contains(&EvidenceAbsence::OccurrenceNotRecorded),
                    "occurrence was never retained"
                );
            }
            other => panic!("expiry evidence must be read: {other:?}"),
        }
        let absent = core
            .plugin_diagnostics()
            .attribute(&DecisionRef::Tool {
                record,
                call: crate::ToolCallId::fixture("another-call"),
                selection: ToolDecisionSelection::BeforeCheck { reply: None },
            })
            .await
            .unwrap();
        assert!(
            matches!(absent.evidence, Evidence::Unavailable { .. }),
            "never attribute another call's outcome"
        );
    }
    core.shutdown().await.unwrap();
}
