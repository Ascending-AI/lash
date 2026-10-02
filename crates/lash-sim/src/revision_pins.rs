//! FIG-4731 on the simulator's engine: an input pinned in its acceptance
//! keeps the turn that applied it through the next turn and a host
//! collection, and a fork of the input is that turn's committed state.

use std::sync::Arc;

use crate::backend::DiscardedTurnActivity;
use crate::provider::ScriptedLlmHttpTransport;
use crate::runtime_providers::{
    OPENAI_COMPATIBLE, runtime_provider_components, runtime_script_for_text,
};

/// What a retained revision publishes: the leaf and checkpoint a fork of it
/// reads back.
fn published(revision: &lash::RetainedRevision) -> (Option<String>, Option<String>) {
    (
        revision.leaf_node_id.as_ref().map(ToString::to_string),
        revision
            .checkpoint_ref
            .as_ref()
            .map(|checkpoint| checkpoint.as_str().to_owned()),
    )
}

#[tokio::test]
async fn a_turn_pinned_at_send_forks_after_collection_on_the_sim_engine() {
    let scripts = ["first reply", "second reply", "third reply"]
        .into_iter()
        .map(|text| runtime_script_for_text(OPENAI_COMPATIBLE, text))
        .collect::<Result<Vec<_>, _>>()
        .expect("runtime scripts");
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts(scripts).expect("valid runtime provider scripts"),
    );
    let (provider, model, _) =
        runtime_provider_components(OPENAI_COMPATIBLE, &transport).expect("runtime provider");
    let engine = crate::backend::SimEngine::new(0x5eed_4731)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let stores = backend.session_store_factory();
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(provider, model.clone())
        .build(crate::sim_process_owner())
        .expect("runtime core");
    let session = crate::open_created_session(model.wire_model.clone(), &core, "sim-revision-pins")
        .await
        .expect("session");

    engine
        .run_text_turn(&session, "sim-pin-turn-1", "first question")
        .await
        .expect("first turn handler")
        .expect("first turn");
    let first = session
        .revisions()
        .await
        .expect("list revisions")
        .pop()
        .expect("the first turn published a head");
    engine
        .run_turn(
            &session,
            "sim-pin-turn-2",
            Arc::new(DiscardedTurnActivity),
            Arc::new(|session: &lash::LashSession| {
                Ok(session.send(lash::TurnInput::text("second question")).pin())
            }),
        )
        .await
        .expect("second turn handler")
        .expect("second turn");
    let pinned = session
        .revisions()
        .await
        .expect("list revisions")
        .pop()
        .expect("the second turn published a head");
    let [target @ lash::Target::Input(_)] = pinned.pinned_by.as_slice() else {
        panic!("the send pinned its input: {pinned:?}");
    };
    engine
        .run_text_turn(&session, "sim-pin-turn-3", "third question")
        .await
        .expect("third turn handler")
        .expect("third turn");

    stores
        .gc_unreachable()
        .await
        .expect("the host collection completes");
    let retained = session.revisions().await.expect("list revisions");
    assert_eq!(
        retained
            .iter()
            .map(|revision| (revision.head_revision, revision.head))
            .collect::<Vec<_>>(),
        vec![
            (pinned.head_revision, false),
            (
                retained.last().expect("the head is retained").head_revision,
                true
            )
        ],
        "the collection keeps the pinned turn and the head"
    );

    let source = session.session_id();
    let fork = |target: lash::Target, branch: &'static str| {
        core.fork_at(
            &source,
            target,
            lash::ForkRequest {
                session_id: branch.into(),
                relation: lash::persistence::SessionRelation::Fork {
                    source_session_id: source.clone(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
    };
    fork(target.clone(), "sim-revision-pins-fork")
        .await
        .expect("the pinned input forks after the collection");
    let branch = core
        .session("sim-revision-pins-fork")
        .open()
        .await
        .expect("open the fork");
    let forked = branch.revisions().await.expect("list the fork's revisions");
    assert_eq!(
        forked.iter().map(published).collect::<Vec<_>>(),
        vec![published(&pinned)],
        "the fork's head is the pinned turn's committed state"
    );

    let refused = fork(
        lash::Target::Revision(first.head_revision),
        "sim-revision-pins-collected",
    )
    .await
    .expect_err("the collection released the unpinned first turn");
    assert!(
        matches!(
            &refused,
            lash::EmbedError::Store(lash::persistence::StoreError::ForkTargetPruned { target, .. })
                if *target == lash::Target::Revision(first.head_revision)
        ),
        "{refused:?}"
    );
}
