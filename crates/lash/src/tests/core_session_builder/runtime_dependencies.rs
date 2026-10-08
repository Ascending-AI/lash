use super::*;
use crate::tests::harness::llm_profile_spec;
use lash_core::{
    ProcessEventLogTestSupport as _, ProcessLifecycle as _, ProcessObserverRegistry as _,
    ProcessRegistrar as _, ProcessRetention as _,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

// =============================================================================
// Runtime dependencies come from one backend
// =============================================================================
//
/// `LashCore` is not `Debug`, so `Result::expect_err` is unavailable; this
/// extracts the build error or panics with the given message.
fn expect_build_error<T>(result: std::result::Result<T, EmbedError>, message: &str) -> EmbedError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(err) => err,
    }
}

/// FIG-3633: the RLM protocol keeps its Lashlang artifacts in the backend it
/// was built over, so a core over any other backend refuses it at build. A
/// core's plugin set is the only one its sessions and workers run
/// (FIG-4396). Otherwise a
/// resumed session would look for its modules in a substrate that never held
/// them, and the core's artifact cleanup would sweep a store nobody wrote.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_core_refuses_an_rlm_factory_built_over_another_backend() -> Result<()> {
    let artifacts = sqlite_memory_store_backend().await;
    let core_backend = sqlite_memory_store_backend().await;
    // Precondition: two memory store sets are two substrates.
    assert_ne!(
        artifacts.binding_identity(),
        core_backend.binding_identity(),
        "two memory store sets must name two substrates"
    );
    let build = |factory_backend: &lash_core::Backend| {
        LashCore::rlm_builder(core_backend.clone(), rlm_factory(factory_backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(crate::DataRetention::standard())
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(crate::testing::runtime_lease_owner())
    };

    // Control: the same factory over the core's own backend builds.
    build(&core_backend)?;

    let error = expect_build_error(
        build(&artifacts),
        "an RLM factory over another backend must be refused",
    );
    match error {
        EmbedError::PluginBackendMismatch {
            plugin_id,
            plugin_backend,
            backend,
        } => {
            assert_eq!(plugin_id, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
            assert_eq!(plugin_backend, artifacts.binding_identity().to_string());
            assert_eq!(backend, core_backend.binding_identity().to_string());
        }
        other => panic!("expected PluginBackendMismatch, got {other}"),
    }

    Ok(())
}

/// The backend's process registry stamps an event from the backend's clock:
/// the one clock the core and every store share.
#[tokio::test]
async fn the_backend_process_registry_stamps_from_the_backend_clock() {
    const NOW_MS: u64 = 4_200_000;
    let clock = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let core = LashCore::standard_builder(store_backend_with_clock(clock).await)
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .build(crate::testing::runtime_lease_owner())
        .expect("build core over a clocked memory backend");
    let registry = core.process_registry();
    let builder_clock_process_id = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register clock-wiring process")
        .id;
    let wait = lash_core::WaitState {
        since_ms: NOW_MS,
        kind: lash_core::WaitKind::Call {
            call_id: lash_core::ToolCallId::fixture("builder-clock-call"),
            tool_id: lash_core::ToolId::from("builder-clock-tool"),
        },
    };
    let runner = lash_core::ProcessExecutionWriteAuthority::invocation(
        builder_clock_process_id.clone(),
        "builder-clock-runner",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &builder_clock_process_id,
            runner
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &runner,
        )
        .await
        .expect("record the clock-wiring execution start");
    let appended = registry
        .append_event_with_authority(
            &builder_clock_process_id,
            lash_core::ProcessEventAppendRequest::wait_entered(&builder_clock_process_id, &wait),
            &runner,
        )
        .await
        .expect("append clock-wiring lifecycle event");

    assert_eq!(appended.event.occurred_at, NOW_MS);
}

#[tokio::test]
async fn fork_distinguishes_collected_revision_from_unknown_and_deleted_sources() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let unknown_error = core
        .fork_at(
            &SessionId::from("unknown-source"),
            lash_core::Target::Revision(0),
            crate::ForkRequest {
                session_id: ("unknown-fork-branch").into(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: ("unknown-source").into(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await
        .expect_err("a session the store never held has no revision to fork");
    assert!(matches!(
        unknown_error,
        EmbedError::Store(lash_core::StoreError::SessionNotFound { session_id })
            if session_id == "unknown-source"
    ));

    let source_profile = Some(recorded_llm_profile(llm_profile_spec(
        "orphaned-source-model",
        None,
        200_000,
    )));
    let source_policy = lash_core::SessionPolicy {
        model: source_profile,
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    };
    let source_request = lash_core::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("orphaned-fork-source"),
        relation: lash_core::SessionRelation::Root,
        config: lash_core::PersistedSessionConfig::from_policy(
            &source_policy.clone(),
            lash_core::SessionToolAccess::ambient(),
        ),
        head: lash_core::SessionCreationHead::Config,
        retention: lash_core::Retention::UntilGc,
    };
    let source = lash_core::runtime::admit_session_view(&factory, &source_request)
        .await
        .expect("create source that will be deleted");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: source_request.session_id.clone(),
        policy: source_policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    source_state.ensure_agent_frame_initialized();
    let retained_revision = source
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
        ))
        .await
        .expect("commit orphaned source frame")
        .head_revision;
    let retained_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("orphaned source leaf");
    let retained = lash_core::Target::Revision(retained_revision);
    let fork_request = |branch: &str| crate::ForkRequest {
        session_id: branch.parse().unwrap(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: ("orphaned-fork-source").into(),
            source_node_id: None,
        },
        observed_processes: Vec::new(),
    };

    let forked = core
        .fork_at(
            &source_request.session_id,
            retained.clone(),
            fork_request("orphaned-fork-branch"),
        )
        .await
        .expect("a retained revision resolves the policy its frame captured");
    assert_eq!(forked.leaf_node_id, Some(retained_node_id));
    assert_eq!(
        (forked.source_session_id, forked.head_revision),
        (source_request.session_id.clone(), retained_revision),
        "the receipt names the forked state"
    );
    let branch =
        lash_core::runtime::live_session_view(&factory, &SessionId::from("orphaned-fork-branch"))
            .await
            .expect("open the fork")
            .expect("the fork exists");
    let branch_config = branch
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await?
        .expect("the fork has a head")
        .config;
    assert_eq!(
        branch_config
            .model
            .as_ref()
            .map(|model| model.model.wire_model()),
        Some("orphaned-source-model"),
        "the forked revision's frame carries its model identity"
    );

    let pending_error = core
        .fork_at(
            &source_request.session_id,
            lash_core::Target::Revision(retained_revision + 1),
            fork_request("pending-fork-branch"),
        )
        .await
        .expect_err("a revision past the head names no state yet");
    assert!(matches!(
        pending_error,
        EmbedError::Store(lash_core::StoreError::ForkTargetPending { .. })
    ));

    // A pin is deleted with its session: it does not make the deleted
    // source forkable.
    factory
        .pin(&source_request.session_id, &retained)
        .await
        .expect("pin the retained revision");
    factory
        .delete_session(&source_request.session_id)
        .await
        .expect("delete pinned source session");
    let deleted_error = core
        .fork_at(
            &source_request.session_id,
            retained,
            fork_request("deleted-fork-branch"),
        )
        .await
        .expect_err("a deleted session has no revision to fork");
    assert!(matches!(
        deleted_error,
        EmbedError::Store(lash_core::StoreError::SessionDeleted { session_id })
            if session_id == "orphaned-fork-source"
    ));
    Ok(())
}

#[tokio::test]
async fn fork_observer_selection_is_recoverable_and_selective() -> Result<()> {
    // The fault decorator's read hook injects the transient observer failure
    // below, over the backend's own registry.
    let faults = Arc::new(std::sync::OnceLock::new());
    let installed = Arc::clone(&faults);
    let backend = lash_conformance::backend_over(
        lash_core::testing::runtime_helpers::LayeredStores::over(sqlite_memory_store_set().await)
            .map_process_registry(|inner| {
                let registry = Arc::new(lash_core::testing::ProcessRegistryFaults::new(inner));
                let _ = installed.set(Arc::clone(&registry));
                registry as Arc<dyn lash_core::ProcessRegistry>
            })
            .into_store_set(),
    );
    let registry = Arc::clone(faults.get().expect("the stores decorated their registry"));
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let source_profile = Some(recorded_llm_profile(llm_profile_spec(
        "fork-source-model",
        None,
        200_000,
    )));
    let policy = lash_core::SessionPolicy {
        // The host and the branch point agree on the provider: a durable
        // pin is a fact, so a host naming a different one is refused at
        // open rather than silently discarded (FIG-1558).
        model: source_profile,
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    };
    let source_store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-observer-source"),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(
                &policy.clone(),
                lash_core::SessionToolAccess::ambient(),
            ),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::Retention::UntilGc,
        },
    )
    .await
    .expect("create fork observer source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: SessionId::from("fork-observer-source"),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    source_state.ensure_agent_frame_initialized();
    let fork_revision = source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
        ))
        .await
        .expect("commit fork observer source")
        .head_revision;

    let fork_visible_process_id = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register fork-visible process")
        .id;
    registry
        .add_observer(
            &SessionId::from("fork-observer-source"),
            &fork_visible_process_id,
            lash_core::ProcessObserverBy::host("fork-test-source"),
        )
        .await
        .expect("observe source process");

    let fork_receipt = core
        .fork_at(
            &SessionId::from("fork-observer-source"),
            lash_core::Target::Revision(fork_revision),
            crate::ForkRequest {
                session_id: ("fork-observer-branch").into(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: ("fork-observer-source").into(),
                    source_node_id: None,
                },
                observed_processes: vec![fork_visible_process_id.clone()],
            },
        )
        .await?;
    assert_eq!(fork_receipt.observed_processes.len(), 1);
    assert_eq!(
        fork_receipt.observed_processes[0].process_id,
        fork_visible_process_id
    );
    let branch_store =
        lash_core::runtime::live_session_view(&factory, &SessionId::from("fork-observer-branch"))
            .await
            .expect("open branch store")
            .expect("branch store exists");
    let branch_read = branch_store
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await
        .expect("load branch config")
        .expect("branch head exists");
    assert_eq!(
        branch_read
            .config
            .model
            .as_ref()
            .map(|model| model.model.wire_model()),
        Some("fork-source-model")
    );

    let inherited = registry
        .list_observed_by(
            &SessionId::from("fork-observer-branch"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list inherited observations");
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].id, fork_visible_process_id);

    registry.set_process_read_error(Some(lash_core::PluginError::Session(
        "transient fork observer registry failure".to_string(),
    )));
    core.fork_at(
        &SessionId::from("fork-observer-source"),
        lash_core::Target::Revision(fork_revision),
        crate::ForkRequest {
            session_id: ("fork-transient-branch").into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("fork-observer-source").into(),
                source_node_id: None,
            },
            observed_processes: inherited.iter().map(|record| record.id.clone()).collect(),
        },
    )
    .await
    .expect("transient observer registry failure must not fail fork_at");
    registry.set_process_read_error(None);
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-transient-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list transient-failure branch observations")
            .is_empty(),
        "a transiently unavailable process must not gain a fork observer edge"
    );
    let transient_branch_store =
        lash_core::runtime::live_session_view(&factory, &SessionId::from("fork-transient-branch"))
            .await
            .expect("open transient-failure branch store")
            .expect("transient-failure branch store exists");
    let transient_meta = transient_branch_store
        .load_session_meta()
        .await
        .expect("load transient-failure fork metadata")
        .expect("transient-failure fork metadata exists");
    assert!(
        transient_meta.pending_observer_intents
            == vec![
                lash_core::facade_support::SessionObserverIntent::host_requested(
                    fork_visible_process_id.clone()
                )
            ],
        "fork_at must retain transiently unavailable observer intents"
    );

    let published_meta = branch_store
        .load_session_meta()
        .await
        .expect("load published fork metadata")
        .expect("published fork metadata exists");
    assert!(matches!(
        published_meta.relation,
        lash_core::SessionRelation::Fork { .. }
    ));
    assert!(
        published_meta.pending_observer_intents.is_empty(),
        "successfully published observers must be consumed from the recovery intent"
    );

    let mut recovery_meta = published_meta;
    recovery_meta.pending_observer_intents.push(
        lash_core::facade_support::SessionObserverIntent::host_requested(
            fork_visible_process_id.clone(),
        ),
    );
    let fork_pruned_process_id = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register process that will be pruned during fork publication")
        .id;
    let pruned_terminal = registry
        .complete_process(
            &fork_pruned_process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(&fork_pruned_process_id),
        )
        .await
        .expect("complete inherited process before recovery");
    registry
        .prune_terminal_processes(
            pruned_terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune inherited process before recovery");
    recovery_meta.pending_observer_intents.push(
        lash_core::facade_support::SessionObserverIntent::host_requested(
            fork_pruned_process_id.clone(),
        ),
    );
    branch_store
        .settle_observer_intents(recovery_meta.pending_observer_intents)
        .await
        .expect("simulate a crash before observer intent consumption");
    registry
        .remove_observer(
            &SessionId::from("fork-observer-branch"),
            &fork_visible_process_id,
            lash_core::ProcessObserverBy::host("fork-test-crash"),
        )
        .await
        .expect("remove the partially published observer");
    core.session(crate::SessionId::parse("fork-observer-branch").expect("nonblank host identity"))
        .created()
        .await
        .open_with_state(lash_core::RuntimeSessionState {
            session_id: SessionId::from("fork-observer-branch"),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy {
                model: branch_read.config.model.clone(),
                ..lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                )
            })
        })
        .await?;
    assert_eq!(
        registry
            .list_observed_by(
                &SessionId::from("fork-observer-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list recovered fork observations")
            .len(),
        1,
        "opening a durable fork must reconcile an unconsumed observer intent"
    );
    let recovered_meta = branch_store
        .load_session_meta()
        .await
        .expect("load recovered fork metadata")
        .expect("recovered fork metadata exists");
    assert!(
        recovered_meta.pending_observer_intents.is_empty(),
        "recovery must consume the intent after idempotent publication"
    );
    registry
        .remove_observer(
            &SessionId::from("fork-observer-branch"),
            &fork_visible_process_id,
            lash_core::ProcessObserverBy::host("fork-test-revoke"),
        )
        .await
        .expect("deliberately remove the recovered observer");
    core.session(crate::SessionId::parse("fork-observer-branch").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-observer-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list observations after deliberate removal")
            .is_empty(),
        "a later deliberate observer removal must remain removed"
    );

    let fork_selective_process_id = registry
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ),
            &[SessionId::from("fork-observer-source")],
        )
        .await
        .expect("register second observed process")
        .id;
    core.fork_at(
        &SessionId::from("fork-observer-source"),
        lash_core::Target::Revision(fork_revision),
        crate::ForkRequest {
            session_id: ("fork-only-branch").into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("fork-observer-source").into(),
                source_node_id: None,
            },
            observed_processes: vec![fork_selective_process_id.clone()],
        },
    )
    .await?;
    let only = registry
        .list_observed_by(
            &SessionId::from("fork-only-branch"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list Only selector result");
    assert_eq!(
        only.iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec![fork_selective_process_id.as_str()]
    );
    let event_count_before = registry
        .full_event_window(&fork_selective_process_id, 0)
        .await
        .expect("read observer audit before duplicate apply")
        .len();
    registry
        .add_observer(
            &SessionId::from("fork-only-branch"),
            &fork_selective_process_id,
            lash_core::ProcessObserverBy::host("observer-test"),
        )
        .await
        .expect("reapply fork observer");
    assert_eq!(
        registry
            .full_event_window(&fork_selective_process_id, 0)
            .await
            .expect("read observer audit after duplicate apply")
            .len(),
        event_count_before,
        "double-apply must be an event-log no-op"
    );

    core.fork_at(
        &SessionId::from("fork-observer-source"),
        lash_core::Target::Revision(fork_revision),
        crate::ForkRequest {
            session_id: ("fork-none-branch").into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("fork-observer-source").into(),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await?;
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-none-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list None selector result")
            .is_empty()
    );
    Ok(())
}

async fn duplicate_only_fork_intents_are_canonical(
    case: &str,
    backend: lash_core::Backend,
) -> Result<()> {
    let source_session_id = SessionId::fixture(format!("duplicate-only-source-{case}"));
    let branch_session_id = SessionId::fixture(format!("duplicate-only-branch-{case}"));
    let factory = backend.session_store_factory();
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let policy = lash_core::SessionPolicy {
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    };
    let source_store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: source_session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(
                &policy.clone(),
                lash_core::SessionToolAccess::ambient(),
            ),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::Retention::UntilGc,
        },
    )
    .await?;
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: source_session_id.clone(),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    source_state.ensure_agent_frame_initialized();
    let fork_revision = source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
        ))
        .await?
        .head_revision;

    let process_id = registry
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ),
            std::slice::from_ref(&source_session_id),
        )
        .await?
        .id;

    let receipt = core
        .fork_at(
            &source_session_id,
            lash_core::Target::Revision(fork_revision),
            crate::ForkRequest {
                session_id: (&branch_session_id).into(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: (&source_session_id).into(),
                    source_node_id: None,
                },
                observed_processes: vec![process_id.clone(), process_id.clone()],
            },
        )
        .await?;

    assert_eq!(
        receipt.observed_processes.len(),
        1,
        "duplicate host selection must persist and settle one observer intent"
    );
    assert_eq!(receipt.observed_processes[0].process_id, process_id);
    assert_eq!(
        registry
            .list_observed_by(
                &branch_session_id,
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await?
            .len(),
        1,
        "the fork must expose one observer edge"
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_only_fork_intents_are_canonical_on_sqlite_memory() -> Result<()> {
    duplicate_only_fork_intents_are_canonical("memory", sqlite_memory_store_backend().await).await
}

#[tokio::test]
async fn session_create_observer_intent_replays_idempotently_on_open() -> Result<()> {
    let session_id = "session-create-observer-recovery";
    let backend = sqlite_memory_store_backend().await;
    let factory = backend.session_store_factory();
    let registry = backend.process_registry();
    let process_id = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await?
        .id;
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: vec![
                lash_core::facade_support::SessionObserverIntent::host_requested(
                    process_id.clone(),
                ),
            ],
            session_id: SessionId::fixture(session_id.to_string()),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(
                &lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                        crate::NoProgressBudget::bounded(12),
                    )
                },
                lash_core::SessionToolAccess::ambient(),
            ),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::Retention::UntilGc,
        },
    )
    .await?;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;

    assert!(
        !registry
            .is_observer(&SessionId::from(session_id), &process_id)
            .await?,
        "the fixture must preserve the real crash gap before publication"
    );
    core.session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert!(
        registry
            .is_observer(&SessionId::from(session_id), &process_id)
            .await?,
        "open must publish the observer edge left pending by a create crash"
    );
    let observer_event_count = registry
        .full_event_window(&process_id, 0)
        .await?
        .into_iter()
        .filter(|event| event.fact.kind() == lash_core::ProcessEventKind::ObserverAdded)
        .count();
    assert_eq!(
        observer_event_count, 1,
        "recovery must publish the missing observer edge exactly once"
    );
    core.session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert_eq!(
        registry
            .full_event_window(&process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.fact.kind() == lash_core::ProcessEventKind::ObserverAdded)
            .count(),
        observer_event_count,
        "recovery after edge publication must be idempotent"
    );
    assert!(matches!(
        store
            .load_session_meta()
            .await?
            .expect("session metadata")
            .relation,
        lash_core::SessionRelation::Root
    ));

    registry
        .remove_observer(
            &SessionId::from(session_id),
            &process_id,
            lash_core::ProcessObserverBy::host("post-recovery-removal"),
        )
        .await?;
    core.session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert!(
        !registry
            .is_observer(&SessionId::from(session_id), &process_id)
            .await?,
        "consumed create intent must not recreate a deliberately removed edge"
    );
    Ok(())
}

#[tokio::test]
async fn session_observer_intents_settle_in_one_pass_before_open_returns() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;

    for (case, simulate_crash_between_layers) in [("fresh", false), ("crash-resume", true)] {
        let session_id = SessionId::fixture(format!("nested-observer-intent-{case}"));
        let mut registered = Vec::new();
        for _ in 0..2 {
            let process_id = registry
                .register_process(lash_core::testing::held_engine_registration(
                    serde_json::Value::Null,
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                ))
                .await?
                .id;
            registered.push(process_id);
        }
        let [create_process_id, fork_process_id] =
            <[ProcessId; 2]>::try_from(registered).expect("two registered processes");
        let store = lash_core::runtime::admit_session_view(
            &factory,
            &lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: vec![
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        create_process_id.clone(),
                    ),
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        fork_process_id.clone(),
                    ),
                ],
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: SessionId::fixture(format!("nested-source-{case}")),
                    source_node_id: Some(lash_core::NodeId::fixture(format!(
                        "nested-source-node-{case}"
                    ))),
                },
                config: lash_core::PersistedSessionConfig::from_policy(
                    &lash_core::SessionPolicy {
                        model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                        ..lash_core::SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                            lash_core::MaxToolCalls::new(1024),
                            crate::NoProgressBudget::bounded(12),
                        )
                    },
                    lash_core::SessionToolAccess::ambient(),
                ),
                head: lash_core::SessionCreationHead::Config,
                retention: lash_core::Retention::UntilGc,
            },
        )
        .await?;

        if simulate_crash_between_layers {
            registry
                .add_observer(
                    &session_id,
                    &create_process_id,
                    lash_core::ProcessObserverBy::host(format!("session-create:{session_id}")),
                )
                .await
                .expect("simulate outer publication before a crash between layers");
            assert!(
                !registry.is_observer(&session_id, &fork_process_id).await?,
                "the crash fixture must leave the inner fork layer unpublished"
            );
        }

        core.session(session_id.clone())
            .created()
            .await
            .open()
            .await?;

        assert!(
            registry
                .is_observer(&session_id, &create_process_id)
                .await?,
            "open must settle the outer session-create observer intent"
        );
        assert!(
            registry.is_observer(&session_id, &fork_process_id).await?,
            "open must settle the inner fork observer intent"
        );
        let meta = store
            .load_session_meta()
            .await?
            .expect("attributed session metadata");
        assert!(
            matches!(meta.relation, lash_core::SessionRelation::Fork { .. }),
            "open must preserve the base fork relation, got {:?}",
            meta.relation
        );
        assert!(meta.pending_observer_intents.is_empty());

        let create_events = registry
            .full_event_window(&create_process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.fact.kind() == lash_core::ProcessEventKind::ObserverAdded)
            .collect::<Vec<_>>();
        assert_eq!(create_events.len(), 1);
        assert_eq!(
            create_events[0].fact.payload()["by"],
            serde_json::json!({
                "kind": "host",
                "operation_id": format!("session-create:{session_id}")
            })
        );
        let fork_events = registry
            .full_event_window(&fork_process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.fact.kind() == lash_core::ProcessEventKind::ObserverAdded)
            .collect::<Vec<_>>();
        assert_eq!(fork_events.len(), 1);
        assert_eq!(
            fork_events[0].fact.payload()["by"],
            serde_json::json!({"kind": "host", "operation_id": format!("session-create:{session_id}")})
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_fork_runs_under_its_branch_points_generation_not_what_the_host_passes() -> Result<()> {
    // A fork copies its fork point's recorded config in full (FIG-4594): the
    // sampling the branch point ran with is the branch's, as its model is.
    // What the host passes to the sessions it creates reaches neither the
    // fork nor a reopen of it.
    let host_generation = lash_core::GenerationOptions {
        output_token_cap: std::num::NonZeroUsize::new(4_096),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        parallel_tool_calls: None,
        projection_provenance: Default::default(),
    };
    let backend = sqlite_memory_store_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session(
        crate::SessionId::parse("generation-fork-host-default").expect("nonblank host identity"),
    )
    .create(crate::SessionCreation::root(
        crate::plugins::SessionToolAccess::ambient(),
        mock_session_spec().generation(host_generation.clone()),
    ))
    .await?;

    let source_profile = Some(recorded_llm_profile(llm_profile_spec(
        "fork-source-model",
        None,
        200_000,
    )));
    let source_policy = lash_core::SessionPolicy {
        model: source_profile,
        // The branch point ran with sampling of its own: the branch's.
        generation: lash_core::GenerationOptions {
            seed: Some(9),
            ..Default::default()
        },
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )
    };
    let source_store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("generation-fork-source"),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(
                &source_policy.clone(),
                lash_core::SessionToolAccess::ambient(),
            ),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::Retention::UntilGc,
        },
    )
    .await
    .expect("create fork source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: SessionId::from("generation-fork-source"),
        policy: source_policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    source_state.ensure_agent_frame_initialized();
    let fork_revision = source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
        ))
        .await
        .expect("commit fork source")
        .head_revision;

    core.fork_at(
        &SessionId::from("generation-fork-source"),
        lash_core::Target::Revision(fork_revision),
        crate::ForkRequest {
            session_id: ("generation-fork-branch").into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("generation-fork-source").into(),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await?;

    let branch = core
        .session(crate::SessionId::parse("generation-fork-branch").expect("nonblank host identity"))
        .open()
        .await?;
    let branch_state = branch.admin().state().persist_current().await?;
    assert_ne!(branch_state.policy.generation, host_generation);
    assert_eq!(
        branch_state.policy.generation.seed,
        Some(9),
        "a branch runs the generation its fork point recorded"
    );
    assert_eq!(
        branch_state.policy.wire_model(),
        Some("fork-source-model"),
        "the branch still records the model that produced the history it continues"
    );
    Ok(())
}
