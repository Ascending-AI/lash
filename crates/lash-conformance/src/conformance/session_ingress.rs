//! The session ingress's store laws (ADR 0101 §5 and ADR 0105 §2, as
//! amended): the one per-session sequence both admission tables draw from,
//! and the drive-epoch seal that fences every drive's claims and commits.

use std::sync::Arc;

use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_core::store::{AdmissionId, DriveEpochSeal, DriveEpochStore};
use lash_sansio::SessionId;

/// The session every ingress law runs in, on a fresh fixture per law.
pub const SESSION_INGRESS_SESSION_ID: &str = "session-ingress";

/// The two handles an ingress law drives: the runtime store bound to
/// [`SESSION_INGRESS_SESSION_ID`], which owns the admission tables, and the
/// drive-epoch store over the same database.
#[derive(Clone)]
pub struct SessionIngressHandles {
    pub runtime: Arc<dyn crate::RuntimeStore>,
    pub ingress: Arc<dyn DriveEpochStore>,
}

/// The store-creation request the fixture opens [`SESSION_INGRESS_SESSION_ID`]
/// with.
#[must_use]
pub fn session_ingress_session_request() -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    }
}

fn session() -> SessionId {
    SessionId::from(SESSION_INGRESS_SESSION_ID)
}

fn wake_delivery(process: &str, sequence: u64, text: &str) -> crate::ProcessWakeDelivery {
    crate::ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("{process}-wake-{sequence}"),
        target_session_id: session(),
        process_id: crate::ProcessId::fixture(process),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session(SESSION_INGRESS_SESSION_ID),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: crate::ProcessId::fixture(process),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    }
}

/// The start marker the store-level laws seal under: one execution per
/// admission (ADR 0105 L-S8).
fn root_start() -> lash_core::store::RootStartNonce {
    lash_core::store::RootStartNonce::new("conformance-root-start")
}

/// Inputs, commands and wakes share the session's allocation counter: the
/// two admission tables are one ingress with one order.
pub async fn every_ingress_producer_shares_the_session_sequence(handles: SessionIngressHandles) {
    let first = handles
        .runtime
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("first input"),
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue input: {error}"));
    let command = handles
        .runtime
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "command".into(),
            },
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue command: {error}"));
    let wake = handles
        .runtime
        .enqueue_queued_work(crate::runtime::process_wake_batch_draft(wake_delivery(
            "waking-process",
            1,
            "wake",
        )))
        .await
        .unwrap_or_else(|error| panic!("enqueue wake: {error}"));
    let input = handles
        .runtime
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("second input"),
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue the second input: {error}"));
    let next = handles
        .runtime
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            session(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "second command".into(),
            },
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue the second command: {error}"));
    assert_eq!(
        [
            first.enqueue_seq,
            command.enqueue_seq,
            wake.enqueue_seq,
            input.enqueue_seq,
            next.enqueue_seq
        ],
        [1, 2, 3, 4, 5]
    );
    let unrelated = handles
        .runtime
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            SessionId::from("another-session"),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("unrelated"),
        ))
        .await
        .unwrap_or_else(|error| panic!("enqueue unrelated input: {error}"));
    assert_eq!(unrelated.enqueue_seq, 1, "sessions allocate independently");
}

/// Two admissions sealing the same observed epoch at once are serialized by
/// the store: exactly one raises the epoch, and the other is superseded.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_seals_serialize(handles: SessionIngressHandles) {
    let observed = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch")
        .epoch;
    let spawn_seal = |admission: &'static str| {
        let handles = handles.clone();
        tokio::spawn(async move {
            handles
                .ingress
                .seal_drive_epoch(
                    &session(),
                    &AdmissionId::new(admission),
                    observed,
                    &root_start(),
                )
                .await
                .expect("seal a drive epoch")
        })
    };
    let (left, right) = (spawn_seal("concurrent-a"), spawn_seal("concurrent-b"));
    let outcomes = [
        left.await.expect("join the first seal"),
        right.await.expect("join the second seal"),
    ];
    let winners = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            DriveEpochSeal::Sealed(fence) => Some(fence.clone()),
            DriveEpochSeal::Superseded { .. } | DriveEpochSeal::ExecutionLost => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1, "exactly one seal wins: {outcomes:?}");
    assert_eq!(winners[0].epoch(), observed + 1);
    assert!(
        outcomes.contains(&DriveEpochSeal::Superseded {
            epoch: observed + 1
        }),
        "the other seal is superseded at the winner's epoch: {outcomes:?}"
    );
    let stored = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(stored.epoch, observed + 1);
    assert_eq!(stored.admission.as_ref(), Some(winners[0].admission()));
}

/// The drive-epoch seal is a compare-and-set on the session's `session_meta`
/// row, idempotent per admission: a retried seal answers the fence it already
/// raised, and a seal from a stale observation is superseded without writing.
/// The seal stores the start marker of the execution that sealed it: the same
/// admission sealed under another marker, a fresh execution of a root that
/// already started, is `ExecutionLost` and writes nothing (L-S8).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_drive_epoch_seal_is_idempotent_per_admission(handles: SessionIngressHandles) {
    let seal_at = |admission: &'static str, observed: u64| {
        let handles = handles.clone();
        async move {
            handles
                .ingress
                .seal_drive_epoch(
                    &session(),
                    &AdmissionId::new(admission),
                    observed,
                    &root_start(),
                )
                .await
                .expect("seal a drive epoch")
        }
    };
    let start = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    let observed = start.epoch;
    let DriveEpochSeal::Sealed(first) = seal_at("seal-a", observed).await else {
        panic!("a seal at the stored epoch is granted");
    };
    assert_eq!(first.epoch(), observed + 1);
    assert_eq!(first.admission(), &AdmissionId::new("seal-a"));
    assert_eq!(
        seal_at("seal-a", observed).await,
        DriveEpochSeal::Sealed(first.clone()),
        "a retried seal answers the same fence"
    );
    assert_eq!(
        handles
            .ingress
            .seal_drive_epoch(
                &session(),
                &AdmissionId::new("seal-a"),
                observed,
                &lash_core::store::RootStartNonce::new("another execution"),
            )
            .await
            .expect("seal a drive epoch"),
        DriveEpochSeal::ExecutionLost,
        "the sealed admission under another start marker is a lost execution"
    );
    let sealed = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(
        sealed.epoch,
        first.epoch(),
        "a lost execution writes nothing"
    );
    assert_eq!(sealed.root_start, Some(root_start()));
    assert_eq!(
        seal_at("seal-b", observed).await,
        DriveEpochSeal::Superseded {
            epoch: first.epoch()
        },
        "another admission at the old observation is superseded"
    );
    assert_eq!(
        seal_at("seal-a", first.epoch()).await,
        DriveEpochSeal::Sealed(first.clone()),
        "a retry that re-read the epoch it raised answers the same fence without raising"
    );
    let DriveEpochSeal::Sealed(second) = seal_at("seal-b", first.epoch()).await else {
        panic!("a seal at the new epoch is granted");
    };
    assert_eq!(second.epoch(), first.epoch() + 1);
    assert_eq!(
        seal_at("seal-a", observed).await,
        DriveEpochSeal::Superseded {
            epoch: second.epoch()
        },
        "a retry after a later seal no longer answers"
    );
    let stored = handles
        .ingress
        .drive_epoch(&session())
        .await
        .expect("read the drive epoch");
    assert_eq!(stored.epoch, second.epoch());
    assert_eq!(stored.admission, Some(AdmissionId::new("seal-b")));
}
