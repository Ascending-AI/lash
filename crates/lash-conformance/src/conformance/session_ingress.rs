//! The session ingress's store laws (ADR 0101 §5): the one per-session
//! sequence both admission tables draw from, and the reserved source keys.

use std::sync::Arc;
use std::{future::Future, pin::Pin};

use lash_sansio::SessionId;

/// The session every ingress law runs in, on a fresh fixture per law.
pub const SESSION_INGRESS_SESSION_ID: &str = "session-ingress";

/// What an ingress law executes: the runtime store bound to
/// [`SESSION_INGRESS_SESSION_ID`], which owns the admission tables, and a
/// probe of its allocations.
#[derive(Clone)]
pub struct SessionIngressHandles {
    pub runtime: Arc<dyn crate::RuntimeStore>,
    pub admission_snapshot: IngressAdmissionProbe,
}

/// Admission allocations observed directly on the fixture's database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressAdmissionSnapshot {
    pub inputs: i64,
    pub batches: i64,
    pub run_specs: i64,
    pub sequence: i64,
}

/// A read of admission allocations on the backend under test.
pub type IngressAdmissionProbe =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = IngressAdmissionSnapshot> + Send>> + Send + Sync>;

fn require_reserved_refusal<T: std::fmt::Debug>(
    result: Result<T, crate::StoreError>,
    kind: &str,
    key: &str,
) {
    assert!(
        matches!(&result, Err(crate::StoreError::IngressReservedSourceKey {
            session_id, kind: refused_kind, source_key,
        }) if *session_id == session() && *refused_kind == kind && source_key == key),
        "{kind} under {key} must be refused with its typed identity: {result:?}"
    );
}

/// ADR 0101 §8: each reserved prefix belongs to its ingress kind. Refusals
/// allocate nothing, including when a mixed input batch already staged a
/// valid member. A refused input cannot occupy a command's key.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn ingress_reserved_source_keys_are_refused_before_admission(
    handles: SessionIngressHandles,
) {
    let store = &handles.runtime;
    let snapshot = &handles.admission_snapshot;
    let empty = snapshot().await;
    let command = crate::SessionCommand::RefreshToolCatalog {
        reason: "refresh".into(),
    };
    let spec = crate::RunSpec::overrides(crate::RunOverrides {
        model: Some(crate::LlmProfileKey::new("reserved-key-refusal")),
        ..crate::RunOverrides::default()
    });
    let input = |key: &str| {
        crate::PendingTurnInputDraft::new(
            session(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("host input"),
        )
        .with_source_key(key)
        .with_run_spec(spec.clone())
    };
    let command_key = command.source_key("reserved-key-command");
    for key in [&command_key, "command:", "command:future:namespace"] {
        require_reserved_refusal(
            store.enqueue_pending_turn_input(input(key)).await,
            "input",
            key,
        );
        let batch =
            crate::PendingTurnInputBatch::new(session(), vec![input(key)]).expect("one input");
        require_reserved_refusal(store.admit_pending_turn_inputs(batch).await, "input", key);
        assert_eq!(snapshot().await, empty, "a refused input allocated nothing");
        for position in 0..3 {
            let mut drafts = ["host:before", "host:middle", "host:after"]
                .map(input)
                .to_vec();
            drafts[position] = input(key);
            let batch = crate::PendingTurnInputBatch::new(session(), drafts).expect("mixed batch");
            require_reserved_refusal(
                store.enqueue_pending_turn_inputs(batch.clone()).await,
                "input",
                key,
            );
            require_reserved_refusal(store.admit_pending_turn_inputs(batch).await, "input", key);
            assert_eq!(
                snapshot().await,
                empty,
                "the whole mixed request rolls back at position {position}"
            );
        }
    }
    let accepted_command = store
        .enqueue_queued_work(
            crate::QueuedWorkBatchDraft::new(
                session(),
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                command,
            )
            .with_source_key(command_key),
        )
        .await
        .expect("valid command key");
    assert_eq!(accepted_command.enqueue_seq, 1);
    let accepted = store
        .enqueue_pending_turn_inputs(
            crate::PendingTurnInputBatch::new(
                session(),
                [
                    "host:ordinary",
                    "commandish:ordinary",
                    "processish:ordinary",
                ]
                .map(input)
                .to_vec(),
            )
            .expect("valid host batch"),
        )
        .await
        .expect("ordinary host keys");
    assert_eq!(
        accepted
            .iter()
            .map(|row| row.enqueue_seq)
            .collect::<Vec<_>>(),
        [2, 3, 4]
    );
    assert_eq!(
        snapshot().await,
        IngressAdmissionSnapshot {
            inputs: 3,
            batches: 1,
            run_specs: 1,
            sequence: 4,
        }
    );
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
        config: crate::PersistedSessionConfig::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: crate::SessionCreationHead::Config,
        retention: crate::Retention::UntilGc,
    }
}

fn session() -> SessionId {
    SessionId::from(SESSION_INGRESS_SESSION_ID)
}

/// Inputs and commands share the session's allocation counter: the
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
            input.enqueue_seq,
            next.enqueue_seq
        ],
        [1, 2, 3, 4]
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

/// FIG-6333: a forsake and an admission of one source key are decided under
/// the session's write authority, so exactly one of them holds the key: the
/// admission is accepted and the forsake answers its input, or the tombstone
/// is written and the admission is refused, storing nothing. Each round
/// races the two on separate tasks; the key's status and a second forsake
/// agree with the winner.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn a_forsake_and_an_admission_of_one_key_have_exactly_one_winner(
    handles: SessionIngressHandles,
) {
    const ROUNDS: usize = 24;
    let before = (handles.admission_snapshot)().await;
    let mut accepted = 0;
    for round in 0..ROUNDS {
        let key = format!("forsake-race-{round}");
        let input_id = crate::PendingTurnInputDraft::keyed_input_id(&session(), &key);
        let draft = crate::PendingTurnInputDraft::new(
            session(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("raced input"),
        )
        .with_source_key(key.clone())
        .with_input_id(input_id.clone());
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let forsake = tokio::spawn({
            let (store, start, key) = (
                Arc::clone(&handles.runtime),
                Arc::clone(&start),
                key.clone(),
            );
            async move {
                start.wait().await;
                store.forsake_turn_input(&session(), &key).await
            }
        });
        let admit = tokio::spawn({
            let (store, start) = (Arc::clone(&handles.runtime), Arc::clone(&start));
            async move {
                start.wait().await;
                store.enqueue_pending_turn_input(draft).await
            }
        });
        let forsaking = forsake
            .await
            .expect("the forsake task completes")
            .expect("a forsake of a live session's key is answered");
        let admission = admit.await.expect("the admission task completes");
        let send_won = match (&forsaking, &admission) {
            (crate::TurnInputForsaking::Held { input_id: held }, Ok(row)) => {
                assert_eq!(
                    *held, row.input_id,
                    "round {round}: the forsake answers the input"
                );
                true
            }
            (
                crate::TurnInputForsaking::Forsaken { input_id: forsaken },
                Err(crate::StoreError::TurnInputForsaken {
                    input_id: refused,
                    source_key,
                    ..
                }),
            ) => {
                assert_eq!(forsaken, refused, "round {round}");
                assert_eq!(*source_key, key, "round {round}");
                false
            }
            (forsaking, admission) => panic!(
                "round {round}: exactly one of a forsake and an admission wins: \
                 {forsaking:?} / {admission:?}"
            ),
        };
        accepted += i64::from(send_won);
        let status = handles
            .runtime
            .input_status(&session(), &input_id)
            .await
            .expect("read the raced key's status");
        assert_eq!(status.forsaken, !send_won, "round {round}: {status:?}");
        assert_eq!(status.open, send_won, "round {round}: {status:?}");
        assert_eq!(
            handles
                .runtime
                .forsake_turn_input(&session(), &key)
                .await
                .expect("forsake the raced key again"),
            forsaking,
            "round {round}: the verdict is final"
        );
    }
    let after = (handles.admission_snapshot)().await;
    assert_eq!(
        after.inputs - before.inputs,
        accepted,
        "a refused admission stores no row"
    );
}
