use super::*;
use crate::tests::host::ingress::*;
use lash_execution_contracts::process::ProcessRegistry;
use lash_protocol_types::ProcessId;

/// FIG-3070: a start committed before its submission outcome is retained
/// reports the store's coalescing verdict when ingress realizes it again.
#[tokio::test]
async fn a_store_coalesced_start_reports_replayed_through_ingress() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let (core, registry) = ingress_engine_core(backend.clone()).await?;
    let scope = lash_core::ExecutionScope::turn(SESSION, SCOPE);
    let ingress = core.tool_intents(SessionId::from(SESSION), scope.clone())?;
    let key = ingress.key("store-coalesced-start", 0).expect("key");
    let intent = engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"}));
    let lash_execution_contracts::tool_intent::ToolIntent::StartProcess(start) = &intent else {
        panic!("start intent");
    };
    // Model the crash interval: the start exists, but ingress has no recorded
    // submission outcome to answer without invoking its local executor.
    let request = start.into_request(key.identity());
    let env_store = backend.process_env_store();
    let starter = scope.journal_identity().expect("valid turn scope");
    let started = lash_core::runtime::register_process_start(
        &lash_core::runtime::ProcessStartStores {
            tracing: Some(&core.env.core.tracing),
            registry: registry.as_ref(),
            env_store: Some(&env_store),
            engines: &core.host_process_engines,
            session_catalog: Some(core.store_factory.as_ref()),
            session_turn_admission: None,
            executor: "coalesced ingress law",
            starter: &starter,
        },
        request.into_registration(),
        &[],
    )
    .await
    .map_err(lash_execution_contracts::plugin_error::PluginError::from)?;
    assert_eq!(
        started.realization(),
        lash_execution_contracts::process::StoreRealization::Realized
    );
    let coalesced = ingress.submit(key, intent).await;
    assert!(
        matches!(
            &coalesced,
            crate::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
        ),
        "the local observer must report the store's verdict: {coalesced:?}"
    );
    assert_eq!(started_process_id(&coalesced), started.record.id);
    assert_eq!(registered_process_count(&registry).await?, 1);
    let fresh = ingress
        .submit(
            ingress.key("fresh-start", 0).expect("fresh key"),
            engine_start_intent(INGRESS_ENGINE_KIND, serde_json::json!({"program": "known"})),
        )
        .await;
    assert!(
        matches!(
            fresh,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                replayed: false,
                ..
            }
        ),
        "a fresh start must realize: {fresh:?}"
    );
    core.shutdown().await?;
    Ok(())
}

/// FIG-1838: host ingress and session-owned recorded-intent execution admit an
/// equivalent start to one identity: it never changes with the route used to
/// start it.
#[tokio::test]
async fn equivalent_recorded_start_has_the_same_identity_across_routes() -> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let payload = serde_json::json!({"program": "environment-sensitive"});

    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, "host-ingress-route"),
    )?;
    let ingress_key = ingress
        .key("environment-sensitive-host", 0)
        .expect("a host submission handle");
    let host_outcome = ingress
        .submit(
            ingress_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            host_outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed { .. },
                ..
            }
        ),
        "host ingress must admit the environment-sensitive engine"
    );
    let ingress_identity = registry
        .get_process(&started_process_id(&host_outcome))
        .await?
        .expect("host ingress registers a process")
        .identity;

    let session = created_session(
        &core,
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let effect_host = session.effect_host();
    let scoped = effect_host.scoped(lash_core::AdmittedScope::turn(
        SESSION,
        "session-recorded-intent-route",
    ))?;
    let processes = {
        let writer = session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let intents =
        lash_execution_contracts::tool_intent::ToolIntents::v3(vec![engine_start_intent(
            INGRESS_ENGINE_KIND,
            payload,
        )]);
    let realization = lash_core::testing::execute_tool_intents_with_services(
        scoped,
        processes,
        &SessionId::from(SESSION),
        &lash_core::ToolCallId::fixture("environment-sensitive-session"),
        &intents,
    )
    .await
    .map_err(lash_execution_contracts::plugin_error::PluginError::from)?;
    let outcomes = &realization.receipt.outcomes;
    let [
        lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed {
            realized:
                lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(result),
            ..
        },
    ] = outcomes.as_slice()
    else {
        panic!("session recorded-intent route must execute: {outcomes:?}")
    };
    // The session route stages its start for the outcome commit of the call
    // that declares it: the identity is the one that commit registers.
    let [staged] = realization.store_local.as_slice() else {
        panic!(
            "the session route stages one start: {:?}",
            realization.store_local
        )
    };
    let staged = lash_core::testing::staged_start_record(staged)?
        .expect("the session route stages a process start");
    assert_eq!(staged.id, result.process_id);
    let session_identity = staged.identity;

    assert_eq!(ingress_identity, session_identity);
    let expected = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": {"program": "environment-sensitive"},
        }),
        [],
    )
    .expect("the payload produces a canonical definition");
    assert_eq!(
        ingress_identity.definition_id,
        Some(expected.id()),
        "the shared identity is the payload's admission"
    );
    Ok(())
}

/// A start owned by `session` that stays live until the law cancels it.
fn engine_start_for(
    session: &SessionId,
    payload: serde_json::Value,
) -> lash_execution_contracts::tool_intent::ToolIntent {
    lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
        lash_execution_contracts::tool_intent::StartProcessIntent {
            owner: crate::RuntimeOwner::Session(session.clone()),
            declaration: lash_execution_contracts::testing::held_engine_declaration(
                payload,
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
        },
    ))
}

/// A cancel of `process` owned by `session`.
fn cancel_for(
    session: &SessionId,
    process: &ProcessId,
) -> lash_execution_contracts::tool_intent::ToolIntent {
    lash_execution_contracts::tool_intent::ToolIntent::CancelProcess(
        lash_execution_contracts::tool_intent::CancelProcessIntent {
            owner: crate::RuntimeOwner::Session(session.clone()),
            process_id: process.clone(),
        },
    )
}

/// What the ledger holds under `key`: the first writer's row, read by a
/// resubmission the ledger answers without realizing anything.
async fn ledger_row(
    registry: &Arc<dyn ProcessRegistry>,
    key: &crate::tools::ToolIntentIngressKey,
) -> Result<lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord> {
    let probe = lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord::new(
        key.identity().clone(),
        cancel_for(
            &SessionId::fixture("ledger-probe-session"),
            &ProcessId::fixture("ledger-probe"),
        ),
    )
    .expect("the probe hashes");
    match registry.admit_tool_intent_submission(probe).await? {
        lash_execution_contracts::tool_intent::ToolIntentSubmissionAdmission::Existing(row) => {
            Ok(*row)
        }
        other => panic!("the ledger answers the first writer: {other:?}"),
    }
}

/// Whether `row` still holds its first payload.
fn payload_retained(
    row: &lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord,
) -> bool {
    match &row.intent {
        crate::tools::ToolIntentSubmissionPayload::Retained(_) => true,
        crate::tools::ToolIntentSubmissionPayload::Expired => false,
    }
}

/// The first outcome a submission answered.
fn first_outcome(
    outcome: &crate::tools::ToolIntentIngressOutcome,
) -> lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome {
    match outcome {
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome,
            replayed: false,
        } => outcome.clone(),
        other => panic!("the first submission is admitted: {other:?}"),
    }
}

/// `outcome` is the replay of `first`.
fn assert_replays(
    outcome: &crate::tools::ToolIntentIngressOutcome,
    first: &lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome,
    what: &str,
) {
    assert_eq!(
        outcome,
        &crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: first.clone(),
            replayed: true,
        },
        "{what} replays the first outcome"
    );
}

/// SETTLED-TOOL-INTENT-PAYLOAD-RETENTION (FIG-6612): a settled host
/// submission's payload follows its turn's revision under the session's
/// retention, and its replay envelope does not.
///
/// Under each policy a turn's submissions settle (an executed start, an
/// executed cancel, a refusal), one stays pending, a pinned later turn
/// settles one and a session operation settles an uncorrelated one. Once
/// the session moves past the first turn's unpinned revision, the first
/// turn's settled payloads are typed expired while every other payload is
/// retained; identical replays answer the first outcomes, a changed cancel
/// and a kind mismatch still refuse, a changed start still answers the
/// process its trusted key minted, the refusal stays the first settlement,
/// and nothing realizes again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settled_tool_intent_payloads_follow_session_retention() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(
            crate::tests::host::send::scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .build(crate::tests::host::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_execution_contracts::testing::process_env_fixture(),
        )
        .await?;
    let law = PayloadRetentionLaw {
        core,
        registry,
        release,
        calls,
    };
    for (name, retention) in [
        ("head-only", lash_core::Retention::HeadOnly),
        (
            "last-turn",
            lash_core::Retention::LastTurns(std::num::NonZeroU32::MIN),
        ),
        ("until-gc", lash_core::Retention::UntilGc),
    ] {
        Box::pin(law.under(name, retention)).await?;
    }
    law.core.shutdown().await?;
    Ok(())
}

/// The core a payload retention law runs its sessions on: its provider holds
/// a turn sent [`crate::tests::host::send::HELD`] until `release` is notified.
struct PayloadRetentionLaw {
    core: LashCore,
    registry: Arc<dyn ProcessRegistry>,
    release: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}

impl PayloadRetentionLaw {
    async fn under(&self, name: &str, retention: lash_core::Retention) -> Result<()> {
        let Self {
            core,
            registry,
            release,
            calls,
        } = self;
        let session_id = crate::SessionId::parse(format!("intent-retention-{name}"))
            .expect("nonblank host identity");
        let run = |label: &str| {
            crate::TurnId::parse(format!("{name}-{label}")).expect("nonblank host identity")
        };
        let session = created_session(core, session_id.clone())
            .await
            .open()
            .await?;
        session.set_retention(retention).await?;

        // The first turn's ledger, written while the turn runs: a large
        // executed start, an executed cancel, a refusal, and a claim nothing
        // realizes.
        let called = calls.load(Ordering::SeqCst);
        let held = session
            .send(crate::TurnInput::text(crate::tests::host::send::HELD))
            .id(run("first"))
            .await?;
        crate::tests::host::send::reaches(calls, called + 1, "the first turn runs").await;
        let first = core.tool_intents(
            session_id.clone(),
            lash_core::ExecutionScope::turn(session_id.clone(), run("first")),
        )?;
        let declaration = serde_json::json!({"program": "known", "body": "x".repeat(16 * 1024)});
        let start_key = first.key("start", 0).expect("key");
        let started = first_outcome(
            &first
                .submit(
                    start_key.clone(),
                    engine_start_for(&session_id, declaration.clone()),
                )
                .await,
        );
        let process = match &started {
            lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed {
                realized: lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(handle),
                ..
            } => handle.process_id.clone(),
            other => panic!("the start executes: {other:?}"),
        };
        let cancel_key = first.key("cancel", 0).expect("key");
        let cancelled = first_outcome(
            &first
                .submit(cancel_key.clone(), cancel_for(&session_id, &process))
                .await,
        );
        assert!(
            matches!(
                cancelled,
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed { .. }
            ),
            "the cancel executes: {cancelled:?}"
        );
        let missing = ProcessId::fixture(&format!("{name}-never-registered"));
        let refused_key = first.key("refused", 0).expect("key");
        let refused = first_outcome(
            &first
                .submit(refused_key.clone(), cancel_for(&session_id, &missing))
                .await,
        );
        assert!(
            matches!(
                refused,
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Refused { .. }
            ),
            "a cancel of no process is refused: {refused:?}"
        );
        let pending_key = first.key("pending", 0).expect("key");
        let pending = lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord::new(
            pending_key.identity().clone(),
            cancel_for(&session_id, &missing),
        )
        .expect("the claim hashes")
        .owned_by_run(run("first"));
        assert!(matches!(
            registry.admit_tool_intent_submission(pending).await?,
            lash_execution_contracts::tool_intent::ToolIntentSubmissionAdmission::Admitted
        ));
        let refused_row = ledger_row(registry, &refused_key).await?;
        release.notify_one();
        held.output().await?;

        // A session operation's submission names no turn.
        let operation = core.tool_intents(
            session_id.clone(),
            lash_core::ExecutionScope::session_operation(session_id.clone(), format!("{name}-op")),
        )?;
        let uncorrelated_key = operation.key("uncorrelated", 0).expect("key");
        first_outcome(
            &operation
                .submit(uncorrelated_key.clone(), cancel_for(&session_id, &missing))
                .await,
        );

        // A pinned later turn settles one more.
        session
            .send(crate::TurnInput::text("second"))
            .id(run("second"))
            .output()
            .await?;
        session.pin(lash_core::Target::Turn(run("second"))).await?;
        let second = core.tool_intents(
            session_id.clone(),
            lash_core::ExecutionScope::turn(session_id.clone(), run("second")),
        )?;
        let pinned_key = second.key("pinned", 0).expect("key");
        first_outcome(
            &second
                .submit(pinned_key.clone(), cancel_for(&session_id, &missing))
                .await,
        );
        session
            .send(crate::TurnInput::text("third"))
            .id(run("third"))
            .output()
            .await?;
        if retention == lash_core::Retention::UntilGc {
            assert!(
                payload_retained(&ledger_row(registry, &start_key).await?),
                "until_gc keeps the first turn's payload until the host collects"
            );
            core.store_factory
                .gc_unreachable()
                .await
                .expect("the host collection completes");
        }
        let processes = registered_process_count(registry).await?;

        for (key, outcome, kind, what) in [
            (
                &start_key,
                &started,
                lash_core::ToolIntentKind::StartProcess,
                "the executed start",
            ),
            (
                &cancel_key,
                &cancelled,
                lash_core::ToolIntentKind::CancelProcess,
                "the executed cancel",
            ),
            (
                &refused_key,
                &refused,
                lash_core::ToolIntentKind::CancelProcess,
                "the refusal",
            ),
        ] {
            let row = ledger_row(registry, key).await?;
            assert!(
                !payload_retained(&row),
                "{name}: {what}'s payload is expired with its turn's revision"
            );
            assert_eq!(
                (row.kind(), row.execution_outcome().as_ref()),
                (kind, Some(outcome)),
                "{name}: {what} keeps its kind and first outcome"
            );
        }
        for (key, what) in [
            (&pending_key, "a pending claim"),
            (&pinned_key, "a pinned turn's submission"),
            (&uncorrelated_key, "an uncorrelated submission"),
        ] {
            assert!(
                payload_retained(&ledger_row(registry, key).await?),
                "{name}: {what} keeps its payload"
            );
        }

        // The envelope answers every replay exactly as before.
        assert_replays(
            &first
                .submit(
                    start_key.clone(),
                    engine_start_for(&session_id, declaration.clone()),
                )
                .await,
            &started,
            "an identical start",
        );
        assert_replays(
            &first
                .submit(
                    start_key.clone(),
                    engine_start_for(&session_id, serde_json::json!({"program": "known"})),
                )
                .await,
            &started,
            "a changed start under its trusted key",
        );
        assert_replays(
            &first
                .submit(cancel_key.clone(), cancel_for(&session_id, &process))
                .await,
            &cancelled,
            "an identical cancel",
        );
        assert_eq!(
            first
                .submit(cancel_key.clone(), cancel_for(&session_id, &missing))
                .await,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                    kind: lash_core::ToolIntentKind::CancelProcess,
                },
            },
            "{name}: a changed cancel still refuses"
        );
        assert_eq!(
            first
                .submit(
                    cancel_key.clone(),
                    engine_start_for(&session_id, declaration.clone()),
                )
                .await,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                    recorded_kind: lash_core::ToolIntentKind::CancelProcess,
                    submitted_kind: lash_core::ToolIntentKind::StartProcess,
                },
            },
            "{name}: a kind mismatch still refuses"
        );
        assert_eq!(
            first
                .submit(refused_key.clone(), cancel_for(&session_id, &missing))
                .await,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: refused.clone(),
                replayed: false,
            },
            "{name}: the refused identity is refused again, never admitted fresh"
        );
        let after = ledger_row(registry, &refused_key).await?;
        assert_eq!(
            (
                after.settlement.as_ref().map(|settlement| settlement.at_ms),
                after.execution_outcome()
            ),
            (
                refused_row
                    .settlement
                    .as_ref()
                    .map(|settlement| settlement.at_ms),
                refused_row.execution_outcome()
            ),
            "{name}: the first refusal stays the settlement"
        );
        assert_eq!(
            registered_process_count(registry).await?,
            processes,
            "{name}: no replay realizes again"
        );
        Ok(())
    }
}
