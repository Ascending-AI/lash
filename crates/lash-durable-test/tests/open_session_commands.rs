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

use std::sync::Arc;

use lash_core::SessionCommitStore as _;
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
