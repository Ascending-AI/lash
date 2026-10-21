//! A host's commands on an open store-backed session, through the facade:
//! each is a session command the core's node applies over the session's
//! committed head and commits at a turn boundary (FIG-4202).
//!
//! - **Config composition (FIG-1875):** a merging config command an open
//!   session applies resolves over the head another writer committed since,
//!   not over what the open session last read.
//! - **Historical refusal:** an open that names a frame the session already
//!   left is refused `HistoricalAgentFrameSwitchUnsupported` and changes
//!   nothing: the committed head keeps its config, its current frame and its
//!   leaf, and the open session's resident state keeps the config the host
//!   changed before the refusal.
//! - **Refused commit (FIG-5376, FIG-5391):** a terminally refused applying
//!   commit settles its command with the recorded refusal, typed cause
//!   included, on every tier, and the lane goes on.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;

use served::{Tier, World};

/// Generation options that state only `seed`.
fn seeded(seed: i64) -> lash_core::GenerationOptions {
    lash_core::GenerationOptions {
        seed: Some(seed),
        ..lash_core::GenerationOptions::default()
    }
}

/// The request that opens the frame `material` keys.
fn frame(material: &str) -> lash::OpenAgentFrameRequest {
    lash::OpenAgentFrameRequest::new(
        lash::FrameKey::from_caller_material(material).expect("non-empty frame material"),
        lash_core::AgentFrameReason::new("test"),
    )
}

/// What a refused open must leave: the committed head's config, current
/// frame and leaf.
#[derive(Debug, PartialEq)]
struct Kept {
    config: lash_core::PersistedSessionConfig,
    current_frame: Option<lash_core::FrameNodeId>,
    leaf: Option<lash_core::NodeId>,
}

/// Apply `transaction` through `config` against the session's current
/// revision; it applies.
async fn apply(
    config: &lash::config::SessionConfigAdmin,
    id: &str,
    transaction: lash::config::ConfigTransaction,
) {
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(id, config.revision().await.unwrap()),
            transaction,
        )
        .await
        .unwrap()
        .await_outcome(config)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
}

/// A merging generation command keeps what the head records, which another
/// open session committed after this one opened, and adds its own.
async fn a_merging_config_command_composes_with_the_head_another_writer_committed(tier: Tier) {
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await
    else {
        return;
    };
    let durable = world.session("config-composition", served::spec(8)).await;
    let id = durable.session_id().clone();
    let stale = world.core.session(id.clone()).open().await.unwrap();
    let writer = world.core.session(id.clone()).open().await.unwrap();
    apply(
        &writer.admin().config(),
        "other-writer-seed",
        lash::config::ConfigTransaction::of(lash::config::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(seeded(7)),
        }),
    )
    .await;
    drop(writer);
    apply(
        &stale.admin().config(),
        "merged-cap",
        lash::config::ConfigTransaction::of(lash::config::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    output_token_cap: std::num::NonZeroUsize::new(64),
                    ..lash_core::GenerationOptions::default()
                },
            ),
        }),
    )
    .await;
    let composed = lash_core::GenerationOptions {
        output_token_cap: std::num::NonZeroUsize::new(64),
        ..seeded(7)
    };
    let head = world
        .backend
        .stores()
        .session_store_factory()
        .load_session_head_meta(&id)
        .await
        .unwrap()
        .expect("the session has a head");
    assert_eq!(
        head.config.generation, composed,
        "the merge resolves over the head the other writer committed"
    );
    assert_eq!(
        stale.admin().state().export().await.policy.generation,
        composed,
        "the open session reads the composed config"
    );
    drop(stale);
    world.shutdown().await;
}

/// An open naming a frame the session left is refused typed, and the
/// committed head and the resident state keep the config the host changed
/// before it, the current frame and the leaf.
async fn opening_a_historical_frame_is_refused_and_keeps_the_changed_config(tier: Tier) {
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await
    else {
        return;
    };
    let durable = world.session("historical-frame", served::spec(8)).await;
    let id = durable.session_id().clone();
    let live = world.core.session(id.clone()).open().await.unwrap();
    let state = live.admin().state();
    for material in ["first-frame", "second-frame"] {
        let opened = state
            .open_agent_frame(frame(material), format!("open-{material}"))
            .await
            .unwrap()
            .settle_with(
                &live.admin().commands(),
                lash::testing::admin_fixture_outcome,
            )
            .await
            .unwrap();
        assert!(opened.opened, "{material} opens: {opened:?}");
    }
    apply(
        &live.admin().config(),
        "changed-generation",
        lash::config::ConfigTransaction::of(lash::config::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(seeded(7)),
        }),
    )
    .await;
    let catalog = world.backend.stores().session_store_factory();
    let kept = || {
        let catalog = Arc::clone(&catalog);
        let id = id.clone();
        async move {
            let head = catalog
                .load_session_head_meta(&id)
                .await
                .unwrap()
                .expect("the session has a head");
            Kept {
                config: head.config,
                current_frame: head.current_frame_node_id,
                leaf: head.leaf_node_id,
            }
        }
    };
    // Applying config settles before its notification is delivered and
    // retired by a separate head commit. Complete that setup before taking
    // the refusal's baseline, retaining full config equality below.
    let before = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let head = kept().await;
            if head.config.undelivered_change.is_none() {
                break head;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the engine retires the delivered config change");
    assert_eq!(before.config.generation, seeded(7));
    let resident_before = state.export().await;

    let refused = state
        .open_agent_frame(frame("first-frame"), "reopen-first-frame".to_owned())
        .await
        .expect("the historical open is submitted")
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect_err("the session left the first frame");
    let lash::EmbedError::Runtime(error) = refused else {
        panic!("the refusal is the runtime's: {refused:?}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert_eq!(kept().await, before, "the refusal commits nothing");
    let resident = state.export().await;
    assert_eq!(
        resident.policy.generation,
        seeded(7),
        "the resident state keeps the changed config"
    );
    assert_eq!(
        resident.current_frame_node_id, resident_before.current_frame_node_id,
        "the resident state keeps its current frame"
    );
    drop(live);
    world.shutdown().await;
}

tiered_laws!(
    a_merging_config_command_composes_with_the_head_another_writer_committed,
    opening_a_historical_frame_is_refused_and_keeps_the_changed_config,
);

/// The dialect and server of `tier`, or `None` for a PostgreSQL leg the
/// run was handed no server for.
fn dialect_of(tier: Tier) -> Option<(dialect::Dialect, Option<String>)> {
    match tier {
        Tier::SqliteMemory => Some((dialect::Dialect::SqliteMemory, None)),
        Tier::SqliteFile => Some((dialect::Dialect::SqliteFile, None)),
        Tier::Postgres => match dialect::postgres_url() {
            Some(url) => Some((dialect::Dialect::Postgres, Some(url))),
            None => {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                None
            }
        },
    }
}

/// ADR 0101 §4 / FIG-5376 / FIG-5391: a terminal refusal of the applying
/// commit settles that command with its recorded refusal, code and typed
/// cause, discards its head changes, and lets the next command apply
/// without a failed activation. The refusal is one the store mints with a
/// cause, an ended artifact referrer, so a settlement that kept only the
/// code and message answers `ended_referrer()` with nothing.
async fn a_terminally_refused_command_commit_settles_with_its_cause_and_the_lane_continues(
    tier: Tier,
) {
    use lash_core::runtime::durable::session::SessionActivation;
    use lash_durable::{ActorKey, ActorState, CommitLabel, DomainRefusal, DomainWrite};
    use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire};
    use lash_protocol_types::sync::MutexExt as _;
    use std::sync::Mutex;

    let Some((dialect, postgres_url)) = dialect_of(tier) else {
        return;
    };
    let clock = SimClock::new();
    let keep = Mutex::new(Vec::new());
    let (stores, database) =
        dialect::open(dialect, postgres_url.as_deref(), Arc::clone(&clock), &keep).await;
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .serve_test_llm_profile(served::model(Arc::default()), served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("command-refusal"),
            lash::persistence::LeaseIncarnationId::new("boot"),
        ))
        .unwrap();
    let id = lash::SessionId::try_from("command-refusal".to_owned()).unwrap();
    let durable = core
        .session(id.clone())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .unwrap();
    let live = core.session(id.clone()).open().await.unwrap();
    let commands = live.admin().commands();
    let script = Script::new();
    let nodes = SimNodes::new(
        database,
        clock,
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 1,
        },
        Arc::new(SessionActivation::new(
            backend.clone(),
            lash::testing::session_turn_services(&core),
            Arc::new(Tripwire::default()),
        )),
    );
    nodes.start("owner");
    let actor = ActorKey::session(id.as_str()).unwrap();
    let initial = commands
        .submit(
            lash_core::facade_support::SessionCommand::OpenAgentFrame {
                request: Box::new(frame("initial-frame")),
            },
            "initial-frame",
        )
        .await
        .unwrap();
    let catalog = backend.stores().session_store_factory();
    for _ in 0..100 {
        nodes.step().await;
        if catalog
            .queued_work_batch_completion(&id, initial.batch_id.as_str())
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
    }
    assert!(
        catalog
            .queued_work_batch_completion(&id, initial.batch_id.as_str())
            .await
            .unwrap()
            .is_some()
    );
    let before = catalog.load_session_head_meta(&id).await.unwrap().unwrap();
    let receipt = commands
        .submit(
            lash_core::facade_support::SessionCommand::OpenAgentFrame {
                request: Box::new(frame("refused-frame")),
            },
            "refused-frame",
        )
        .await
        .unwrap();
    let referrer = lash_core_store::artifact_referrer::ArtifactReferrer::Session(id.clone());
    let refusal = lash_core::StoreError::ArtifactReferrerEnded {
        referrer: referrer.clone(),
    };
    let expected_code = refusal.runtime_code();
    let expected_message = refusal.to_string();
    let seen = Arc::new(Mutex::new(0));
    let refused_attempts = Arc::clone(&seen);
    nodes.script().refuse_commits(move |label, writes| {
        if label != CommitLabel::SESSION_COMMAND {
            return None;
        }
        writes.iter().find_map(|write| {
            let DomainWrite::SessionCommit(write) = write else {
                return None;
            };
            let commit = lash_core::store::decode_session_commit(&write.commit_json).unwrap();
            if !commit.command_outcomes.values().any(|outcome| {
                matches!(
                    outcome,
                    lash_core::runtime::SessionCommandOutcome::OpenAgentFrame { .. }
                )
            }) {
                return None;
            }
            *refused_attempts.lock_recover() += 1;
            Some(DomainRefusal::SessionCommitRefused {
                session: write.session.clone(),
                code: refusal.runtime_code(),
                cause: refusal.runtime_cause(),
                reason: refusal.to_string(),
            })
        })
    });
    for _ in 0..100 {
        nodes.step().await;
        nodes.quiesce().await;
        let state = nodes.database().actor(&actor).await.unwrap().unwrap().state;
        if matches!(state, ActorState::Idle | ActorState::Parked) {
            break;
        }
    }
    let completion = catalog
        .queued_work_batch_completion(&id, receipt.batch_id.as_str())
        .await
        .unwrap();
    assert!(
        completion.is_some(),
        "the refused command never settled; applying attempts: {}",
        *seen.lock_recover()
    );
    let recorded = completion
        .unwrap()
        .command_outcomes
        .remove(&receipt.batch_id)
        .expect("the applying commit records the command's outcome");
    let settled = commands.settle(receipt.clone()).await.unwrap();
    let lash_core::runtime::SessionCommandSettlement::Applied {
        outcome: lash_core::runtime::SessionCommandOutcome::Failed { refusal },
        ..
    } = settled
    else {
        panic!("the refused command settles failed: {settled:?}");
    };
    assert!(
        matches!(
            &recorded,
            lash_core::runtime::SessionCommandOutcome::Failed { refusal: stored }
                if *stored == refusal
        ),
        "the settlement answers the recorded outcome: {recorded:?}"
    );
    assert_eq!(
        refusal.code, expected_code,
        "the settlement keeps the store's code"
    );
    assert_eq!(refusal.message, expected_message);
    let answered = lash_core::RuntimeError::from(refusal);
    assert_eq!(
        answered.ended_referrer(),
        Some(&referrer),
        "the settlement keeps the store's typed cause"
    );
    assert_eq!(
        *seen.lock_recover(),
        1,
        "a terminal refusal never retries the applying commit"
    );
    assert_eq!(
        nodes.database().actor(&actor).await.unwrap().unwrap().state,
        ActorState::Idle,
        "the refusal settles without parking the command run"
    );
    let after = catalog.load_session_head_meta(&id).await.unwrap().unwrap();
    assert_eq!(
        after.current_frame_node_id, before.current_frame_node_id,
        "the refused frame never opens"
    );
    assert_eq!(
        after.leaf_node_id, before.leaf_node_id,
        "the refused head changes are discarded"
    );
    assert_eq!(
        nodes
            .script()
            .trace()
            .iter()
            .filter(|write| matches!(write.stored, Stored::Refused(_)))
            .count(),
        1
    );

    let next = commands
        .submit(
            lash_core::facade_support::SessionCommand::OpenAgentFrame {
                request: Box::new(frame("next-frame")),
            },
            "next-frame",
        )
        .await
        .unwrap();
    // The same fault would refuse another frame open: stop injecting after
    // proving that the first command's applying commit was attempted once.
    nodes.script().refuse_commits(|_, _| None);
    for _ in 0..100 {
        nodes.step().await;
        nodes.quiesce().await;
        if catalog
            .queued_work_batch_completion(&id, next.batch_id.as_str())
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
    }
    assert!(
        catalog
            .queued_work_batch_completion(&id, next.batch_id.as_str())
            .await
            .unwrap()
            .is_some(),
        "the next command settles"
    );
    assert!(matches!(
        commands.settle(next).await.unwrap(),
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::OpenAgentFrame { .. },
            ..
        }
    ));
    assert!(
        matches!(
            commands.settle(receipt).await.unwrap(),
            lash_core::runtime::SessionCommandSettlement::Applied {
                outcome: lash_core::runtime::SessionCommandOutcome::Failed { .. },
                ..
            }
        ),
        "reattaching keeps the first refusal"
    );
    nodes.kill("owner");
    core.shutdown().await.unwrap();
    drop(durable);
    drop(keep);
}

#[tokio::test]
async fn a_terminally_refused_command_commit_settles_with_its_cause_on_sqlite_memory() {
    a_terminally_refused_command_commit_settles_with_its_cause_and_the_lane_continues(
        Tier::SqliteMemory,
    )
    .await;
}

#[tokio::test]
async fn a_terminally_refused_command_commit_settles_with_its_cause_on_sqlite_file() {
    a_terminally_refused_command_commit_settles_with_its_cause_and_the_lane_continues(
        Tier::SqliteFile,
    )
    .await;
}

#[tokio::test]
async fn a_terminally_refused_command_commit_settles_with_its_cause_on_postgres() {
    a_terminally_refused_command_commit_settles_with_its_cause_and_the_lane_continues(
        Tier::Postgres,
    )
    .await;
}

async fn owner_service_runtime(
    world: &World,
    store: lash_core::store::SessionStore,
) -> lash_core::facade_support::LashRuntime {
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .unwrap()
    .unwrap()
    .state;
    let env = lash_core::facade_support::RuntimeEnvironment::builder(
        lash_core::testing::runtime_helpers::test_runtime_host_config(&world.backend),
    )
    .with_plugin_host(Arc::new(lash_core::plugin::PluginHost::new(
        vec![Arc::new(lash::plugins::StandardProtocolPluginFactory::new())],
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(Arc::new(lash_core::facade_support::SystemClock)),
    )))
    .build();
    lash_core::facade_support::LashRuntime::from_environment(
        &env,
        state.effective_policy().clone(),
        state,
        Some(store),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .unwrap()
}

async fn settle_service_append(
    live: &lash::LashSession,
    result: lash::plugins::SessionGraphAppendResult,
) -> lash_core::AppendSessionNodesOutcome {
    match result {
        lash::plugins::SessionGraphAppendResult::Applied { outcome } => outcome,
        lash::plugins::SessionGraphAppendResult::Pending { receipt } => {
            let settlement = live.admin().commands().settle(receipt).await.unwrap();
            let lash_core::runtime::SessionCommandSettlement::Applied {
                outcome: lash_core::runtime::SessionCommandOutcome::AppendSessionNodes { outcome },
                ..
            } = settlement
            else {
                panic!("{settlement:?}");
            };
            outcome
        }
    }
}

/// LO-OWNER-ONLY: an outside service accepts a command while its owner is
/// blocked in a model callback, rather than writing the claimed head itself.
async fn lo_owner_only_service_append(tier: Tier) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let model = lash_core::testing::TestProvider::builder()
        .kind("owner-only")
        .requires_streaming(true)
        .complete({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move |request| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    entered.notify_one();
                    release.acquire().await.unwrap().forget();
                    Ok(served::text(&request, "done"))
                }
            }
        })
        .build()
        .into_handle();
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await
    else {
        return;
    };
    let durable = world.session("owner-only", served::spec(8)).await;
    let id = durable.session_id().clone();
    let catalog = world.backend.session_store_factory();
    let recording = Arc::new(
        lash_core::testing::runtime_helpers::RecordingStore::over_session(
            catalog.clone(),
            id.clone(),
        ),
    );
    let store = lash_core::store::SessionStore::new(recording.clone(), id.clone()).unwrap();
    let mut runtime = owner_service_runtime(&world, store.clone()).await;
    let service = runtime.session_graph_service().unwrap();
    let running = tokio::spawn(async move {
        durable
            .send(lash::TurnInput::text("hold-owner"))
            .output()
            .await
            .unwrap()
    });
    tokio::time::timeout(served::WATCHDOG, entered.notified())
        .await
        .unwrap();
    let request = lash_core::AppendSessionNodesRequest {
        operation_id: "outside-append".to_owned(),
        requires_ancestor_node_id: None,
        nodes: vec![lash_core::SessionAppendNode::plugin(
            "owner-only",
            serde_json::json!({"note": 1}),
        )],
    };
    let actor = lash_durable::ActorKey::session(id.as_str()).unwrap();
    assert_eq!(
        world
            .backend
            .durable()
            .actor(&actor)
            .await
            .unwrap()
            .unwrap()
            .state,
        lash_durable::ActorState::Owned
    );
    let before = store.load_session_head_meta().await.unwrap().unwrap();
    let accepted = tokio::time::timeout(
        served::WATCHDOG,
        service.append_session_nodes(&id, request.clone()),
    )
    .await
    .expect("acceptance must not await the blocked owner");
    let queued = store.list_queued_work().await.unwrap();
    assert_eq!(
        queued.len(),
        1,
        "the service must enqueue one command: {accepted:?}"
    );
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .unwrap()
            .unwrap()
            .head_revision,
        before.head_revision,
        "acceptance cannot move the owner's head"
    );
    let accepted = accepted.unwrap();
    let lash::plugins::SessionGraphAppendResult::Pending { receipt } = &accepted else {
        panic!("a blocked owner leaves a pending receipt: {accepted:?}");
    };
    assert_eq!(&receipt.batch_id, &queued[0].batch_id);
    let retry = service
        .append_session_nodes(&id, request.clone())
        .await
        .unwrap();
    assert_eq!(retry, accepted);
    assert_eq!(store.list_queued_work().await.unwrap().len(), 1);
    let mut changed = request.clone();
    changed.nodes.push(lash_core::SessionAppendNode::plugin(
        "owner-only",
        serde_json::json!({"note": 2}),
    ));
    assert!(matches!(
        service.append_session_nodes(&id, changed).await,
        Err(
            lash_execution_contracts::plugin_error::PluginError::AppendOperationIdentityConflict { .. }
        )
    ));
    // No append-induced background refresh is owed on the originating runtime.
    let reloads = recording.load_session_head_meta_count();
    runtime.await_background_work().await.unwrap();
    assert_eq!(
        recording.load_session_head_meta_count() - reloads,
        0,
        "own_head_reload=0: no bypass-induced refresh"
    );
    release.add_permits(1);
    served::assert_answered(
        "held owner",
        &tokio::time::timeout(served::WATCHDOG, running)
            .await
            .unwrap()
            .unwrap(),
    );
    let live = world.core.session(id.clone()).open().await.unwrap();
    let receipt = lash::SessionCommandReceipt {
        session_id: id.clone(),
        batch_id: queued[0].batch_id.clone(),
        source_key: lash_core::facade_support::SessionCommand::AppendSessionNodes {
            request: Box::new(request.clone()),
        }
        .source_key(&request.operation_id),
    };
    let settled = live.admin().commands().settle(receipt).await.unwrap();
    let lash_core::runtime::SessionCommandSettlement::Applied {
        outcome: lash_core::runtime::SessionCommandOutcome::AppendSessionNodes { outcome },
        ..
    } = settled
    else {
        panic!("the owner applies the append once: {settled:?}");
    };
    let lash_core::AppendSessionNodesOutcome::Appended {
        node_ids,
        leaf_node_id,
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(node_ids.len(), 1);
    assert_eq!(leaf_node_id.as_ref(), node_ids.last());
    assert_eq!(
        service
            .append_session_nodes(&id, request.clone())
            .await
            .unwrap(),
        lash::plugins::SessionGraphAppendResult::Applied {
            outcome: outcome.clone()
        }
    );
    assert!(store.list_queued_work().await.unwrap().is_empty());
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .unwrap()
    .unwrap()
    .state;
    assert_eq!(
        state
            .session_graph
            .nodes
            .iter()
            .filter(|node| matches!(&node.payload,
        lash_core::SessionNodePayload::Plugin { plugin_type, .. } if plugin_type == "owner-only"))
            .count(),
        1
    );
    // A derivation naming an ancestor survives an intervening append.
    let ancestor = node_ids[0].clone();
    let mut advanced = request.clone();
    advanced.operation_id = "advanced-ancestor".to_owned();
    advanced.requires_ancestor_node_id = Some(ancestor.clone());
    let advance = live
        .admin()
        .state()
        .append_session_nodes(advanced)
        .await
        .unwrap()
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .unwrap();
    let lash_core::AppendSessionNodesOutcome::Appended {
        node_ids: advanced_ids,
        ..
    } = advance
    else {
        panic!("{advance:?}");
    };
    let mut derived = request.clone();
    derived.operation_id = "service-derived".to_owned();
    derived.requires_ancestor_node_id = Some(ancestor.clone());
    assert_eq!(
        world
            .backend
            .durable()
            .actor(&actor)
            .await
            .unwrap()
            .unwrap()
            .state,
        lash_durable::ActorState::Owned,
        "the idle session remains claimed"
    );
    let derived = settle_service_append(
        &live,
        service.append_session_nodes(&id, derived).await.unwrap(),
    )
    .await;
    let lash_core::AppendSessionNodesOutcome::Appended {
        node_ids: derived_ids,
        ..
    } = derived
    else {
        panic!("{derived:?}");
    };
    let read = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .unwrap()
    .unwrap()
    .state;
    let derived_node = read
        .session_graph
        .nodes
        .iter()
        .find(|node| node.node_id == derived_ids[0])
        .unwrap();
    assert_eq!(derived_node.parent_node_id.as_ref(), advanced_ids.last());
    // Rewind a new session to the ancestor: the descendant still exists in
    // shared history, but cannot be the new session's derivation base.
    let branch_id = lash::SessionId::try_from("owner-only-branch".to_owned()).unwrap();
    let revision = catalog
        .revisions(&id)
        .await
        .unwrap()
        .into_iter()
        .find(|revision| revision.leaf_node_id.as_ref() == Some(&ancestor))
        .unwrap()
        .head_revision;
    let head = store.load_session_head_meta().await.unwrap().unwrap();
    catalog
        .fork_session(&lash_core::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: branch_id.clone(),
            source_session_id: id.clone(),
            head_revision: revision,
            relation: lash_core::SessionRelation::Root,
            config: head.config,
            retention: lash_core::Retention::UntilGc,
        })
        .await
        .unwrap();
    let branch_store =
        lash_core::store::SessionStore::new(catalog.clone(), branch_id.clone()).unwrap();
    let branch_runtime = owner_service_runtime(&world, branch_store.clone()).await;
    let branch_service = branch_runtime.session_graph_service().unwrap();
    let branch = world.core.session(branch_id.clone()).open().await.unwrap();
    let mut abandoned = request.clone();
    abandoned.operation_id = "abandoned-base".to_owned();
    abandoned.requires_ancestor_node_id = advanced_ids.first().cloned();
    let before = branch_store
        .load_session_head_meta()
        .await
        .unwrap()
        .unwrap()
        .leaf_node_id;
    let stale = settle_service_append(
        &branch,
        branch_service
            .append_session_nodes(&branch_id, abandoned)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        stale,
        lash_core::AppendSessionNodesOutcome::StaleBranch {
            required_node_id: advanced_ids[0].clone()
        }
    );
    assert_eq!(
        branch_store
            .load_session_head_meta()
            .await
            .unwrap()
            .unwrap()
            .leaf_node_id,
        before
    );
    drop(branch);
    drop(branch_runtime);
    // Only the removed append bypass is counted here. Ordinary command,
    // idle and terminal loads remain part of the engine's normal path.
    release.add_permits(1);
    let next = world.core.session(id.clone()).durable().await.unwrap();
    let reloads = recording.load_session_head_meta_count();
    served::assert_answered(
        "the next owner turn",
        &world.send(&next, "after-append").await,
    );
    runtime.await_background_work().await.unwrap();
    assert_eq!(
        recording.load_session_head_meta_count() - reloads,
        0,
        "the next turn owes no append-bypass head refresh"
    );
    eprintln!(
        "LO-OWNER-ONLY service: one accepted command, one application, stable retry, typed identity refusal, ancestor/abandoned branch, append bypass reloads=0"
    );
    // Direct writes belong only to an unclaimed session, and advance its
    // actor revision atomically (the lo-writers half of the cutover).
    let direct = world.session("owner-only-direct", served::spec(8)).await;
    let direct_id = direct.session_id();
    let direct_store =
        lash_core::store::SessionStore::new(catalog.clone(), direct_id.clone()).unwrap();
    let direct_actor = lash_durable::ActorKey::session(direct_id.as_str()).unwrap();
    let previous = world.backend.durable().actor(&direct_actor).await.unwrap();
    assert!(
        previous
            .as_ref()
            .is_none_or(|actor| actor.state != lash_durable::ActorState::Owned)
    );
    let revision = previous.map_or(0, |actor| actor.revision.0);
    let mut state = lash_core::store::load_session_window_state(
        &direct_store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .unwrap()
    .unwrap()
    .state;
    let commit = lash_core::store::append_request_commit_for_testing(
        &mut state,
        "unclaimed-direct",
        &request.nodes,
        None,
    )
    .unwrap();
    let receipt = direct_store.commit_runtime_state(commit).await.unwrap();
    assert_eq!(
        receipt.realized_node_timestamps.last().unwrap().node_id,
        receipt.committed_leaf_node_id.unwrap()
    );
    let after = world.backend.durable().actor(&direct_actor).await.unwrap();
    assert_eq!(
        after.map_or(0, |actor| actor.revision.0),
        revision + 1,
        "unclaimed direct append must advance actor revision atomically"
    );

    drop(live);
    drop(runtime);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lo_owner_only_service_append_on_sqlite_memory() {
    Box::pin(lo_owner_only_service_append(Tier::SqliteMemory)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lo_owner_only_service_append_on_sqlite_file() {
    Box::pin(lo_owner_only_service_append(Tier::SqliteFile)).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PostgreSQL"]
async fn lo_owner_only_service_append_on_postgres_when_configured() {
    let _ = lash_postgres_store::testing::required_database_url();
    Box::pin(lo_owner_only_service_append(Tier::Postgres)).await;
}
