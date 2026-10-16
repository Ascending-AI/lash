//! Finalization is an advisory notification identifying committed facts,
//! readable on any node without following the executing node's live stream.

use super::*;
use crate::TurnInput;

/// FIG-6546: an observer receives the committed identity and outcome kind.
/// Its failure cannot undo the commit, and another node reads the exact
/// turn's transcript and tool-call ledger after a later turn advanced the head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalization_identifies_a_committed_turn_readable_on_another_node() -> Result<()> {
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let release_observer = Arc::new(tokio::sync::Semaphore::new(0));
    let observer_gate = Arc::clone(&release_observer);
    let observer = Arc::new(StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("finalized-identity"),
        crate::plugins::PluginSpec::new().with_runtime_event(
            crate::hook_key!("turn-finalized"),
            Arc::new(move |event| {
                let observed = observed.clone();
                let release = Arc::clone(&observer_gate);
                Box::pin(async move {
                    if let crate::plugins::PluginLifecycleEvent::TurnFinalized(turn) = event {
                        release
                            .acquire()
                            .await
                            .expect("the observer gate stays open")
                            .forget();
                        observed
                            .send(turn)
                            .expect("the law receives the notification");
                        return Err(crate::plugins::PluginError::Session(
                            "observer unavailable".into(),
                        ));
                    }
                    Ok(())
                })
            }),
        ),
    ));
    let prior = Arc::new(StdMutex::new(None::<crate::plugins::TurnFinalized>));
    let (plugin_read, mut plugin_reads) = tokio::sync::mpsc::unbounded_channel();
    let reader = Arc::new(StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("committed-reader"),
        crate::plugins::PluginSpec::new().with_before_turn(
            crate::hook_key!("read-prior-turn"),
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
                        Ok(crate::plugins::TurnContributions::default())
                    })
                }
            }),
        ),
    ));
    let sends = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
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
                                tool_name: "app_lookup".into(),
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
        .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
        .plugin(observer.clone())
        .plugin(reader.clone())
        .build(crate::testing::runtime_lease_owner())
    };
    let core = deploy(true)?;
    let session_id = crate::SessionId::from("finalized-identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    // The host awaits the outcome only; it does not follow its own send.
    let answer = session
        .send(TurnInput::text("look up app state"))
        .await?
        .outcome()
        .await?;
    assert_eq!(answer.status(), crate::TurnStatus::Answered);
    release_observer.add_permits(1);
    let first = observations
        .recv()
        .await
        .expect("the observer ran after commit");
    assert_eq!(&first.session_id, &session_id);
    assert_eq!(
        first.outcome,
        crate::persistence::TurnCommitOutcome::Completed
    );
    session
        .send(TurnInput::text("a later turn"))
        .await?
        .outcome()
        .await?;
    release_observer.add_permits(1);
    let later = observations.recv().await.expect("the later observer ran");
    assert_ne!(first.turn_id, later.turn_id);
    core.shutdown().await?;

    let other = deploy(true)?;
    let durable = other.session(session_id).durable().await?;
    let read = durable
        .committed_turn(&first.turn_id)
        .await?
        .expect("the turn committed");
    assert_eq!(&read.turn.turn_id, &first.turn_id);
    assert_eq!(read.turn.outcome, first.outcome);
    let texts = read
        .turn
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            crate::transcript::TranscriptItem::Message(message) => Some(&message.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            crate::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"reply-1"));
    assert!(!texts.contains(&"reply-2"));
    let [call] = read.tool_calls.as_slice() else {
        panic!("the committed turn holds one tool call")
    };
    assert_eq!(call.tool, "app_lookup");
    assert_eq!(call.args, serde_json::json!({}));
    assert!(call.output.is_success());
    assert_eq!(
        sends.load(Ordering::SeqCst),
        3,
        "the read invokes no provider"
    );
    assert!(
        durable
            .committed_turn(&crate::TurnId::fixture("unknown-turn"))
            .await?
            .is_none()
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
        crate::plugins::PluginDeclaration::initial("restore-address"),
        crate::plugins::PluginSpec::new().with_runtime_event(
            crate::hook_key!("restore"),
            Arc::new(move |event| {
                let observed = observed.clone();
                Box::pin(async move {
                    if let crate::plugins::PluginLifecycleEvent::SessionRestored {
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
            owner: lash_core::RuntimeOwner::Session(unpersisted.clone()),
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
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        lash_core::ExecutionBudgets::recommended(),
        lash_core::runtime::DeltaCoalescing::recommended(),
        lash_core::facade_support::DataRetentionConfig::standard(),
    );
    let runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            crate::testing::runtime_lease_owner(),
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
            .build(crate::testing::runtime_lease_owner())
    };
    let core = deploy(true)?;
    let source = SessionId::from("restore-durable");
    let durable = core
        .session(source.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
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
            crate::ForkRequest {
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
