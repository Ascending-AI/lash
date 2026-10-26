//! Finalization is an advisory notification identifying committed facts,
//! readable on any node without following the executing node's live stream.

use super::*;
use lash::TurnInput;
use lash_core::durable_port::DurableReads as _;

/// FIG-6546: an observer receives the committed identity and outcome kind.
/// Its failure cannot undo the commit, and another node reads the exact
/// turn's transcript and tool-call ledger after a later turn advanced the head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalization_identifies_a_committed_turn_readable_on_another_node() -> Result<()> {
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let (config_sent, mut config_seen) = tokio::sync::mpsc::unbounded_channel();
    let release_observer = Arc::new(tokio::sync::Semaphore::new(0));
    let observer_gate = Arc::clone(&release_observer);
    let observer = Arc::new(StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("finalized-identity"),
        lash::plugins::PluginSpec::new().with_runtime_event(
            lash::hook_key!("turn-finalized"),
            Arc::new(move |event| {
                let observed = observed.clone();
                let config_sent = config_sent.clone();
                let release = Arc::clone(&observer_gate);
                Box::pin(async move {
                    if let lash::plugins::PluginLifecycleEvent::SessionConfigChanged(ctx) = event {
                        config_sent.send(ctx).expect("config address retained");
                        return Ok(());
                    }
                    if let lash::plugins::PluginLifecycleEvent::TurnFinalized(turn) = event {
                        release
                            .acquire()
                            .await
                            .expect("the observer gate stays open")
                            .forget();
                        observed
                            .send(turn)
                            .expect("the law receives the notification");
                        return Err(lash::plugins::PluginError::Session(
                            "observer unavailable".into(),
                        ));
                    }
                    Ok(())
                })
            }),
        ),
    ));
    let prior = Arc::new(StdMutex::new(None::<lash::plugins::TurnFinalized>));
    let (plugin_read, mut plugin_reads) = tokio::sync::mpsc::unbounded_channel();
    let reader = Arc::new(StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("committed-reader"),
        lash::plugins::PluginSpec::new().with_before_turn(
            lash::hook_key!("read-prior-turn"),
            Arc::new({
                let prior = Arc::clone(&prior);
                move |ctx| {
                    let prior = prior.lock_recover().clone();
                    let plugin_read = plugin_read.clone();
                    Box::pin(async move {
                        if let Some(prior) = prior {
                            let read = ctx
                                .sessions
                                .committed_turn(&prior.session_id, &prior.turn_id)
                                .await
                                .expect("the plugin reads committed facts");
                            plugin_read
                                .send(read)
                                .expect("the law receives the plugin read");
                        }
                        Ok(lash::plugins::TurnContributions::default())
                    })
                }
            }),
        ),
    ));
    let sends = Arc::new(AtomicUsize::new(0));
    let model = lash::testing::TestProvider::builder()
        .kind("finalized-identity")
        .complete({
            let sends = Arc::clone(&sends);
            move |_| {
                let call = sends.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "lookup".into(),
                                tool_name: "tools__app_lookup".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }],
                            ..Default::default()
                        })
                    } else {
                        Ok(text_response(&format!("reply-{call}")))
                    }
                }
            }
        })
        .build()
        .into_handle();
    let stores = sqlite_memory_store_set().await;
    let deploy = |serves| {
        explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
            stores.clone(),
        )))
        .serve_sessions(serves)
        .serve_test_llm_profile(model.clone(), mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
        .plugin(observer.clone())
        .plugin(reader.clone())
        .build(lash_facade_tests::runtime_lease_owner())
    };
    let core = deploy(true)?;
    let session_id = lash_kernel_value::identity::SessionId::from("finalized-identity");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(session_id.clone()).open().await?;
    session.set_retention(lash::Retention::HeadOnly).await?;
    // Capture the real published call ids, rather than reconstructing them.
    let answer = session
        .send(TurnInput::text("look up app state"))
        .await?
        .output()
        .await?;
    assert_eq!(answer.status(), lash::TurnStatus::Answered);
    let call_ids = answer
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ModelCallRecorded { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids.len(), 2);
    release_observer.add_permits(1);
    let first = observations
        .recv()
        .await
        .expect("the observer ran after commit");
    assert_eq!(&first.session_id, &session_id);
    assert_eq!(
        first.outcome,
        lash::persistence::TurnCommitOutcome::Completed
    );
    let retained = session
        .durable()
        .committed_turn(&first.turn_id)
        .await?
        .expect("retained turn");
    let model_facts = session
        .durable()
        .read_turn_model_calls(&first.turn_id)
        .await?;
    assert_eq!(
        model_facts
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>(),
        call_ids
    );
    let run = lash_vocabulary::identity::RunId::parse(first.turn_id.as_str())
        .expect("the event names a nonblank run");
    let window = serde_json::to_value(
        session
            .durable()
            .read_run(&run)
            .await?
            .expect("window")
            .to_snapshot(),
    )?;
    let target = lash::Target::Turn(first.turn_id.clone());
    session.pin(target.clone()).await?;
    let prompt = lash::prompt::PromptCallKey {
        session: session_id.clone(),
        call: lash::prompt::ModelCallId::Turn {
            run: first.turn_id.clone(),
            ordinal: 1,
        },
    };
    let admin = session.admin().config();
    let mut applied_revisions = Vec::new();
    for limit in [7, 6] {
        let revision = admin.revision().await?;
        let outcome = admin
            .apply(
                lash::config::ConfigWrite::new(format!("limit-{limit}"), revision),
                lash::config::ConfigTransaction::of(lash::config::SetMaxToolCalls {
                    max_tool_calls: lash::MaxToolCalls::new(limit),
                }),
            )
            .await?
            .await_outcome(&admin)
            .await?;
        let lash::config::ConfigTransactionOutcome::Applied { revision, .. } = outcome else {
            panic!("the config applied: {outcome:?}");
        };
        applied_revisions.push(revision);
    }
    let config = config_seen.recv().await.expect("first config notification");
    let config_revision = config.revision;
    let config_fields = config.changed.clone();
    let current_config = config_seen
        .recv()
        .await
        .expect("second config notification");
    assert_eq!(
        [config_revision, current_config.revision],
        applied_revisions.as_slice()
    );
    assert_eq!(
        config_fields,
        [lash::plugins::SessionConfigField::MaxToolCalls].into()
    );
    assert_eq!(
        current_config
            .sessions
            .snapshot_current()
            .await?
            .policy
            .max_tool_calls
            .get(),
        6
    );
    assert_eq!(
        config
            .sessions
            .snapshot_current()
            .await?
            .policy
            .max_tool_calls
            .get(),
        7
    );
    let audit = stores.durable_store().prompt_snapshot(&prompt).await?;
    assert!(audit.is_some());
    session
        .send(TurnInput::text("a later turn"))
        .await?
        .outcome()
        .await?;
    release_observer.add_permits(1);
    let later = observations.recv().await.expect("the later observer ran");
    assert_ne!(first.turn_id, later.turn_id);
    let later_run = lash_vocabulary::identity::RunId::parse(later.turn_id.as_str())
        .expect("a finalization names a run");
    session
        .pin(lash::Target::Turn(later.turn_id.clone()))
        .await?;
    let later_window = serde_json::to_value(
        session
            .durable()
            .read_run(&later_run)
            .await?
            .expect("later window")
            .to_snapshot(),
    )?;
    let later_prompt = lash::prompt::PromptCallKey {
        session: first.session_id.clone(),
        call: lash::prompt::ModelCallId::Turn {
            run: later.turn_id.clone(),
            ordinal: 1,
        },
    };
    let later_audit = stores
        .durable_store()
        .prompt_snapshot(&later_prompt)
        .await?;
    assert!(later_audit.is_some());
    assert_eq!(
        serde_json::to_value(
            session
                .durable()
                .read_run(&run)
                .await?
                .expect("pinned window")
                .to_snapshot()
        )?,
        window
    );
    assert_eq!(
        stores.durable_store().prompt_snapshot(&prompt).await?,
        audit
    );
    session.unpin(target).await?;
    // A newer revision releases only the old window/audit; compact facts survive.
    session
        .send(TurnInput::text("advance after unpin"))
        .output()
        .await?;
    release_observer.add_permits(1);
    observations.recv().await.expect("third finalization");
    stores
        .session_store_factory()
        .gc_unreachable()
        .await
        .expect("collection completes");
    core.shutdown().await?;

    let other = deploy(true)?;
    let durable = other.session(session_id).durable().await?;
    let read = durable
        .committed_turn(&first.turn_id)
        .await?
        .expect("the turn committed");
    assert_eq!(
        read, retained,
        "the captured address still resolves exact transcript and tools"
    );
    assert_eq!(
        durable.read_turn_model_calls(&first.turn_id).await?,
        model_facts
    );
    let addresses = call_ids
        .iter()
        .map(|call_id| lash::turn::TurnModelCallRef {
            turn_id: first.turn_id.clone(),
            call_id: call_id.clone(),
        })
        .collect::<Vec<_>>();
    let expected_calls = model_facts
        .iter()
        .cloned()
        .map(|record| lash::turn::ModelCallRead::Settled {
            record: Box::new(record),
        })
        .collect::<Vec<_>>();
    assert_eq!(durable.read_model_calls(&addresses).await?, expected_calls);
    let terminal = durable.run(run.clone()).outcome().await?;
    assert_eq!(
        terminal.output().map(|output| &output.outcome),
        Some(&answer.outcome)
    );
    assert_eq!(
        serde_json::to_value(
            durable
                .read_run(&later_run)
                .await?
                .expect("the other pin keeps only its own window")
                .to_snapshot()
        )?,
        later_window
    );
    assert_eq!(
        stores
            .durable_store()
            .prompt_snapshot(&later_prompt)
            .await?,
        later_audit
    );
    assert!(matches!(
        durable.read_run(&run).await,
        Err(EmbedError::Store(
            lash_core::StoreError::RunWindowReleased { session_id, run: released, .. }
        )) if session_id == first.session_id && released == first.turn_id
    ));
    assert!(matches!(
        stores.durable_store().prompt_snapshot(&prompt).await,
        Err(lash_core::durable_port::DurableError::Domain(
            lash_core::durable_port::domain::DomainRefusal::PromptAuditExpired { call }
        )) if call == prompt
    ));
    assert_eq!(&read.turn.turn_id, &first.turn_id);
    assert_eq!(read.turn.outcome, first.outcome);
    let texts = read
        .turn
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            lash::transcript::TranscriptItem::Message(message) => Some(&message.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            lash::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"reply-1"));
    assert!(!texts.contains(&"reply-2"));
    let [call] = read.tool_calls.as_slice() else {
        panic!("the committed turn holds one tool call")
    };
    assert_eq!(call.tool, "tools.app_lookup");
    assert_eq!(call.args, serde_json::json!({}));
    assert!(call.output.is_success());
    assert_eq!(
        sends.load(Ordering::SeqCst),
        4,
        "the read invokes no provider"
    );
    assert!(
        durable
            .committed_turn(&lash_vocabulary::identity::TurnId::fixture("unknown-turn"))
            .await?
            .is_none()
    );
    assert_eq!(config.revision, config_revision);
    assert_eq!(config.changed, config_fields);
    assert!(
        config
            .changed
            .contains(&lash::plugins::SessionConfigField::MaxToolCalls)
    );
    assert_eq!(
        config
            .sessions
            .snapshot_current()
            .await?
            .policy
            .max_tool_calls
            .get(),
        7
    );
    *prior.lock_recover() = Some(first);
    durable
        .send(TurnInput::text("read the first turn in a plugin"))
        .await?
        .outcome()
        .await?;
    release_observer.add_permits(1);
    observations
        .recv()
        .await
        .expect("the second node's observer ran");
    let plugin_read = plugin_reads
        .recv()
        .await
        .expect("the second node's plugin read the commit");
    assert_eq!(
        plugin_read,
        Some(read),
        "plugins and hosts read the same committed facts"
    );
    other.shutdown().await?;
    Ok(())
}

/// RESTORE-ADDRESS: absence is unpersisted state; a durable fork at revision
/// zero is still addressable, and a reopen identifies the store's actual head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_notifications_identify_reopened_and_revision_zero_heads() -> Result<()> {
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let observer = Arc::new(StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("restore-address"),
        lash::plugins::PluginSpec::new().with_runtime_event(
            lash::hook_key!("restore"),
            Arc::new(move |event| {
                let observed = observed.clone();
                Box::pin(async move {
                    if let lash::plugins::PluginLifecycleEvent::SessionRestored {
                        session_id,
                        revision,
                    } = event
                    {
                        observed
                            .send((session_id, revision))
                            .expect("the law receives restores");
                    }
                    Ok(())
                })
            }),
        ),
    ));
    let backend = sqlite_memory_store_backend().await;
    let store = backend.session_store_factory();
    let unpersisted = SessionId::from("restore-unpersisted");
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(observer.clone());
    let plugins = lash_core::facade_support::PluginHost::new(
        factories,
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(Arc::new(lash_core::facade_support::SystemClock)),
    )
    .build_session(lash_core::plugin::PluginSessionRequest::creation(
        unpersisted.clone(),
        lash_core::plugin::SessionAuthorityContext::fixture(),
    ))
    .expect("storeless plugin session");
    // A restored runtime has accepted plugin state even when no checkpoint
    // has committed. Fresh assembly without this snapshot stays lazy.
    let transition = plugins.host().transition_plugins(
        lash_core::plugin::PluginTransitionRequest {
            id: lash_core::plugin::PluginTransitionId(
                lash_core::EffectAddress::new(
                    lash_core::ExecutionScope::turn(unpersisted.clone(), "restore-fixture"),
                    "plugin-transition",
                )
                .expect("fixture transition address"),
            ),
            owner: lash_vocabulary::identity::RuntimeOwner::Session(unpersisted.clone()),
            base: lash_core::plugin::PluginTransitionBase::Session {
                head: lash_core::store::SessionHeadRef {
                    generation: 0,
                    revision: 0,
                    leaf: None,
                    checkpoint: None,
                },
            },
            target: lash_core::store::plugin_writers::PluginAdmission::from_plugins(
                plugins
                    .host()
                    .factories()
                    .iter()
                    .map(|factory| lash_core::store::plugin_writers::AdmittedPlugin {
                        plugin: factory.id().into(),
                        writer: factory.plugin_declaration().format_version,
                    })
                    .collect(),
            ),
        },
        &plugins.export_state(),
        &Default::default(),
    );
    plugins
        .adopt_plugin_transition(&transition)
        .expect("native plugin transition");
    let mut state = lash_core::RuntimeSessionState {
        session_id: unpersisted.clone(),
        ..lash_core::RuntimeSessionState::fixture(lash_core::testing::standard_test_policy())
    };
    state
        .capture_plugin_states(plugins.as_ref(), lash_core::store::FleetFormat::current())
        .expect("unpersisted native plugin snapshot");
    assert!(state.checkpoint_ref.is_none());
    let host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash::CommitBudget::default(),
        lash::QueuedWorkBatchingConfig::new(1),
        lash_core::ExecutionBudgets::recommended(),
        lash_core::runtime::DeltaCoalescing::recommended(),
        lash_core::facade_support::DataRetentionConfig::standard(),
    )
    .expect("the fixture ingress limit fits its commit budget");
    let runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_facade_tests::runtime_lease_owner(),
        )
        .with_initial_state(state)
        .with_plugin_session(plugins)
        .build(),
    )
    .await
    .expect("storeless runtime");
    assert_eq!(
        Some(
            observations
                .try_recv()
                .expect("restore finished before constructor returned")
        ),
        Some((unpersisted, None))
    );
    drop(runtime);

    let deploy = |serves| {
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_sessions(serves)
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(observer.clone())
            .build(lash_facade_tests::runtime_lease_owner())
    };
    let core = deploy(true)?;
    let source = SessionId::from("restore-durable");
    let durable = core
        .session(source.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    durable
        .send(TurnInput::text("commit a head"))
        .output()
        .await?;
    core.shutdown().await?;
    while observations.try_recv().is_ok() {}
    let head = store
        .load_session_head_meta(&source)
        .await?
        .expect("committed head");
    let reopened = deploy(true)?;
    let session = reopened.session(source.clone()).open().await?;
    // Opening a handle is lazy; the engine restores the runtime when it
    // admits this turn. Its notification names the head before that turn.
    session
        .send(TurnInput::text("materialize the reopened runtime"))
        .output()
        .await?;
    assert_eq!(
        Some(
            observations
                .try_recv()
                .expect("restore finished before constructor returned")
        ),
        Some((
            source.clone(),
            Some(lash_core::SessionRevision::new(head.head_revision))
        ))
    );
    drop(session);
    while observations.try_recv().is_ok() {}
    let source_head = store
        .load_session_head_meta(&source)
        .await?
        .expect("fork source head");
    let fork_id = SessionId::from("restore-fork");
    reopened
        .fork_at(
            &source,
            lash_core::Target::Revision(source_head.head_revision),
            lash::ForkRequest {
                session_id: fork_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: source.clone(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await?;
    let fork_head = store
        .load_session_head_meta(&fork_id)
        .await?
        .expect("fork head");
    assert_eq!(fork_head.head_revision, 0);
    assert!(fork_head.checkpoint_ref.is_some());
    let fork = reopened.session(fork_id.clone()).open().await?;
    fork.send(TurnInput::text("materialize the revision-zero fork"))
        .output()
        .await?;
    assert_eq!(
        Some(
            observations
                .try_recv()
                .expect("restore finished before constructor returned")
        ),
        Some((fork_id, Some(lash_core::SessionRevision::new(0))))
    );
    assert_eq!(
        fork.policy_snapshot().max_tool_calls,
        durable
            .read()
            .await?
            .expect("source read")
            .policy()
            .max_tool_calls
    );
    drop(fork);
    reopened.shutdown().await?;
    Ok(())
}
