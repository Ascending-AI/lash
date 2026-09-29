//! FIG-4200: who ends a superseded root, and which moved heads still park.
//!
//! - A refused run ends its root only while it still owns it. A run whose
//!   commit met a head another writer moved, and whose fence a later
//!   admission superseded before it wrote the end, leaves the root to that
//!   admission's execution: the store checks the fence in the ending
//!   transaction, so an obsolete executor never ends its successor's root.
//! - A root's recorded head inspection decides a moved head by its
//!   components. A higher revision is ordinary overtaking and ends typed; a
//!   head that is inconsistent with the admission's base (a lower revision,
//!   or the same revision with another leaf or checkpoint) still parks for an
//!   operator, with nothing ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::engine::{DriveAbort, DriveOutcome, RootOutcome};
use lash_core::store::{RootTerminalCause, SessionHeadRef};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::drive_admission::{DriveParts, admitted, on_tier};

/// A drive of the session through its drive loop, answered as it ended.
async fn drive(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    id: &str,
) -> Result<DriveOutcome, DriveAbort> {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(
            async move { lash_core::drive::drive_session(&mut runtime, &scope, &request).await },
        )
    })
    .await
}

/// The root `input` was admitted to.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn root_of(parts: &DriveParts, input: &crate::InputId) -> TurnId {
    parts
        .store
        .root_of_input(&parts.session_id, input)
        .await
        .expect("read the input's root")
        .expect("the input was admitted to a root")
}

/// Whether `input` is still open, admitted to `root`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn held_by(parts: &DriveParts, input: &crate::InputId, root: &TurnId) -> bool {
    parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("read pending input")
        .into_iter()
        .any(|read| {
            read.input.input_id == *input
                && matches!(
                    read.status,
                    crate::PendingTurnInputReadStatus::Admitted { root: ref holder }
                        if holder == root
                )
        })
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn terminal(parts: &DriveParts, root: &TurnId) -> Option<lash_core::store::RootTerminal> {
    parts
        .store
        .root_terminal(&parts.session_id, root)
        .await
        .expect("read the root's terminal")
}

fn superseded_refusal(terminal: Option<&lash_core::store::RootTerminal>) -> bool {
    terminal.is_some_and(|terminal| {
        matches!(
            &terminal.cause,
            RootTerminalCause::Refused { code, .. }
                if *code == crate::RuntimeErrorCode::StoreCommitSuperseded
        )
    })
}

/// A run whose commit meets a head another writer moved is refused
/// `StoreCommitSuperseded`, but a successor admission seals a newer drive
/// epoch before the run writes the end. The obsolete run writes nothing: the
/// root stays unfinished with its input admitted to it. The successor's
/// drive resumes the root, finds the head overtaken, and ends it typed under
/// its own fence; the next input then admits a new root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_obsolete_executor_never_ends_its_successors_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "obsolete-executor", &effect_host, &stores, 8).await;
    let recording = Arc::new(
        lash_core::testing::runtime_helpers::RecordingStore::over_session(
            Arc::clone(&parts.store),
            parts.session_id.clone(),
        ),
    );
    parts.store = Arc::clone(&recording) as Arc<dyn crate::RuntimeStore>;
    // The first model call moves the head through another writer, which
    // commits the session under the law's own policy; every later call
    // answers.
    let policy = parts.initial_state().policy;
    let calls = Arc::new(AtomicUsize::new(0));
    let overtaken = Arc::new(AtomicBool::new(false));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let recording = Arc::clone(&recording);
            let calls = Arc::clone(&calls);
            let overtaken = Arc::clone(&overtaken);
            move |_request| {
                let recording = Arc::clone(&recording);
                let policy = policy.clone();
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let overtake = !overtaken.swap(true, Ordering::SeqCst);
                async move {
                    if overtake {
                        lash_core::testing::runtime_helpers::advance_session_head(
                            &recording,
                            &[],
                            |state| state.policy = policy,
                        )
                        .await;
                    }
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    // Between the obsolete run meeting its refusal and writing its end, a
    // successor admission seals a newer drive epoch.
    let successor_sealed = Arc::new(AtomicBool::new(false));
    recording.before_next_end_refused_root(Arc::new({
        let store = Arc::clone(&parts.store);
        let session_id = parts.session_id.clone();
        let successor_sealed = Arc::clone(&successor_sealed);
        move || {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let successor_sealed = Arc::clone(&successor_sealed);
            Box::pin(async move {
                let stored = store
                    .drive_epoch(&session_id)
                    .await
                    .expect("read the drive epoch");
                let sealed = store
                    .seal_drive_epoch(
                        &session_id,
                        &lash_core::store::AdmissionId::new(format!("{session_id}-successor")),
                        stored.epoch,
                        &lash_core::store::RootStartNonce::new(format!(
                            "{session_id}-successor-start"
                        )),
                    )
                    .await
                    .expect("the successor seals");
                assert!(
                    matches!(sealed, lash_core::store::DriveEpochSeal::Sealed(_)),
                    "{sealed:?}"
                );
                successor_sealed.store(true, Ordering::SeqCst);
            })
        }
    }));

    let input = parts.enqueue("ask", None).await;
    let request = parts.request("obsolete-executor-drive");
    let refused = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                    .await
                    .expect("admit the root"),
            );
            lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted).await
        })
    })
    .await;
    assert!(
        successor_sealed.load(Ordering::SeqCst),
        "the successor sealed"
    );
    match refused {
        Err(DriveAbort::Refused(error)) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            "{error:?}"
        ),
        other => panic!("the obsolete run's commit is refused superseded: {other:?}"),
    }
    let root = root_of(&parts, &input).await;
    assert_eq!(
        terminal(&parts, &root).await,
        None,
        "the obsolete run never ends its successor's root"
    );
    assert_eq!(
        parts
            .store
            .unfinished_root(&parts.session_id)
            .await
            .expect("read the unfinished root")
            .map(|unfinished| unfinished.root),
        Some(root.clone()),
        "the root still holds the session"
    );
    assert!(
        held_by(&parts, &input, &root).await,
        "the input stays admitted to the root"
    );

    // The successor's execution owns the root: it resumes it, finds the head
    // overtaken, and ends it typed under its own fence.
    let successor = drive(&runner, &parts, "obsolete-executor-successor").await;
    match successor {
        Err(DriveAbort::Refused(error)) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            "{error:?}"
        ),
        other => panic!("the successor ends the overtaken root typed: {other:?}"),
    }
    assert!(
        superseded_refusal(terminal(&parts, &root).await.as_ref()),
        "the successor ends the root: {:?}",
        terminal(&parts, &root).await
    );
    assert_eq!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("read the park"),
        None
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the successor made no model call"
    );

    let next = parts.enqueue("ask again", None).await;
    let outcome = drive(&runner, &parts, "obsolete-executor-next")
        .await
        .expect("the next drive runs");
    let next_root = root_of(&parts, &next).await;
    assert_ne!(next_root, root, "the next input heads a new root");
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RootOutcome::Committed { root, .. } if *root == next_root)),
        "{outcome:?}"
    );
}

/// How a law's recorded admission base is inconsistent with the live head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InconsistentHead {
    /// The live head's revision is lower than the base's.
    LowerRevision,
    /// The same revision, with another leaf.
    OtherLeaf,
    /// The same revision, with another checkpoint.
    OtherCheckpoint,
}

/// A root admitted on a base the live head is inconsistent with, `head`,
/// parks when a drive on a fresh journal inspects it: the verdict is
/// `Diverged`, the park is an `EffectReplayDivergence`, nothing ends the
/// root, its input stays admitted to it, and no model call runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn inconsistent_divergence_still_parks(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    head: InconsistentHead,
) {
    let parts = DriveParts::new(
        prefix,
        &format!("inconsistent-{head:?}").to_lowercase(),
        &effect_host,
        &stores,
        8,
    )
    .await;
    let root = TurnId::from(format!("inconsistent-{head:?}-root").to_lowercase());
    let input = parts.enqueue("ask", Some(root.as_str())).await;
    // An earlier execution recorded the root's admission on `base`.
    let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
        &parts.store,
        &parts.session_id,
        "inconsistent-first-execution",
    )
    .await;
    // The head a drive reads live: the store's, or the initial state's for a
    // session that committed nothing yet.
    let state = parts.initial_state();
    let live = match parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the session head")
    {
        Some(head) => SessionHeadRef {
            generation: 0,
            revision: head.head_revision,
            leaf: head.leaf_node_id,
            checkpoint: head.checkpoint_ref,
        },
        None => SessionHeadRef {
            generation: 0,
            revision: state.head_revision,
            leaf: state.session_graph.leaf_node_id.clone(),
            checkpoint: state.checkpoint_ref.clone(),
        },
    };
    let base = match head {
        InconsistentHead::LowerRevision => SessionHeadRef {
            revision: live.revision + 1,
            ..live.clone()
        },
        InconsistentHead::OtherLeaf => SessionHeadRef {
            leaf: Some(crate::NodeId::from("inconsistent-leaf")),
            ..live.clone()
        },
        InconsistentHead::OtherCheckpoint => SessionHeadRef {
            checkpoint: Some(lash_core::store::BlobRef(
                "inconsistent-checkpoint".to_string(),
            )),
            ..live.clone()
        },
    };
    parts
        .store
        .admit_root(&lash_core::store::AdmitRootRequest {
            fence,
            root: root.clone(),
            head: lash_core::store::AdmittedHead::Input(input.clone()),
            max_inputs: 1,
            policy: lash_core::testing::queued_work_admission_policy(1),
            base,
            turn_index: state.turn_index as u64 + 1,
            generation: None,
            admitted_generation: lash_core::engine::BuildGeneration::for_test("conformance-law"),
        })
        .await
        .expect("record the root's admission")
        .expect("the root's admission reaches its head");

    let parked = drive(&runner, &parts, "inconsistent-drive").await;
    match parked {
        Err(DriveAbort::Parked {
            root: parked,
            error,
        }) => {
            assert_eq!(parked, root);
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                "{error:?}"
            );
        }
        other => panic!("an inconsistent head parks the root: {other:?}"),
    }
    let park = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("read the park")
        .expect("the root parked");
    assert_eq!(park.turn_id, root);
    assert!(
        matches!(
            park.reason,
            lash_core::store::ParkReason::EffectReplayDivergence { .. }
        ),
        "{park:?}"
    );
    assert_eq!(terminal(&parts, &root).await, None, "nothing ends the root");
    assert!(
        held_by(&parts, &input, &root).await,
        "the input stays admitted to the parked root"
    );
    assert_eq!(parts.calls(), 0, "the parked root made no model call");
    let blocked = drive(&runner, &parts, "inconsistent-blocked")
        .await
        .expect("the drive stops");
    assert!(
        matches!(blocked.stop, lash_core::engine::DriveStop::Parked(_)),
        "{blocked:?}"
    );
}

/// [`inconsistent_divergence_still_parks`] on a live head below the base.
pub async fn inconsistent_divergence_still_parks_on_a_lower_revision(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::LowerRevision,
    )
    .await;
}

/// [`inconsistent_divergence_still_parks`] on the base's revision with
/// another leaf.
pub async fn inconsistent_divergence_still_parks_on_another_leaf(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::OtherLeaf,
    )
    .await;
}

/// [`inconsistent_divergence_still_parks`] on the base's revision with
/// another checkpoint.
pub async fn inconsistent_divergence_still_parks_on_another_checkpoint(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::OtherCheckpoint,
    )
    .await;
}
