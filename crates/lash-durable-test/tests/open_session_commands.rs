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
#![allow(clippy::expect_used, clippy::unwrap_used)]

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
    config: &lash::admin::SessionConfigAdmin,
    id: &str,
    transaction: lash::config::ConfigTransaction,
) {
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(id, config.revision().await.unwrap()),
            transaction,
        )
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
    let before = kept().await;
    assert_eq!(before.config.generation, seeded(7));
    let resident_before = state.export().await;

    let refused = state
        .open_agent_frame(frame("first-frame"), "reopen-first-frame".to_owned())
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

/// ADR 0101 §4 / FIG-5376: a terminal refusal of the applying commit
/// settles that command with its typed code, discards its head changes,
/// and lets the next command apply without a failed activation.
#[tokio::test]
async fn a_terminally_refused_command_commit_settles_and_the_lane_continues() {
    use lash_core::runtime::durable::session::SessionActivation;
    use lash_durable::{ActorKey, ActorState, CommitLabel, DomainRefusal, DomainWrite};
    use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire};
    use lash_sansio::sync::MutexExt as _;
    use std::sync::Mutex;

    let clock = SimClock::new();
    let stores = Arc::new(sim::memory(Arc::clone(&clock)).await);
    let backend = sim::backend(stores);
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .serve_test_llm_profile(served::model(Arc::default()), served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "command-refusal",
            "boot",
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
        Arc::clone(backend.durable()),
        clock,
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
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
    let refusal = lash_core::StoreError::NodeIdCollision {
        node_id: lash_core::NodeId::try_from("command-node".to_owned()).unwrap(),
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
    let settled = commands.settle(receipt.clone()).await.unwrap();
    assert!(
        matches!(settled, lash_core::runtime::SessionCommandSettlement::Applied {
        outcome: lash_core::runtime::SessionCommandOutcome::Failed { code, message }, ..
    } if code == expected_code && message == expected_message),
        "the settlement keeps the store's typed refusal"
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
}
