//! The round trips the owner's commits pay on PostgreSQL (FIG-5275,
//! FIG-5936), counted on an isolated database: the acceptance of one
//! input, a turn's admission, `model.start`, a round's outcome, a VM park and a
//! code cell's checkpoint that leases its root (FIG-7585),
//! `turn.commit`, every boundary of a five-tool turn, a session's opening
//! turn and its close, and a commit publishing a candidate beside its pages
//! (FIG-7576).

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use lash_core_store::checkpoint_graph::CommitId;
use lash_core_store::checkpoint_graph::EncodeBudget;
use lash_core_store::checkpoint_graph::ExplicitLimit;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId as GraphNode;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_core_store::store::AdmittedTurnRows;
use lash_core_store::store::RunAdmissionRecord;
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::{
    DomainWrite, ExecKey, ExecRootWrite, ModelPin, Ordinal, OwnerKey, PhaseAdvance, RunFactKind,
    RunFactWrite, RunSeq, SessionMailWrite, SnapshotRev, TurnWrite, UnfinishedPhase,
};
use lash_durable::{
    ActorKey, ActorTx, CommitLabel, DurableReads as _, DurableStore as _, Epoch, FormatSet, MailTx,
    NodeId, NodeSpec, Release,
};
use lash_kernel_value::identity::SessionId;
use lash_kernel_value::tokens::TokenWriter;
use lash_vocabulary::identity::TurnId;

use crate::PostgresStorage;
use crate::observed_sql::ROUND_TRIPS;
use crate::testing::IsolatedDatabase;

/// Count the round trips `work` makes from this task.
async fn round_trips<T>(work: impl std::future::Future<Output = T>) -> (T, u64) {
    ROUND_TRIPS
        .scope(std::cell::Cell::new(0), async {
            let value = work.await;
            (value, ROUND_TRIPS.with(std::cell::Cell::get))
        })
        .await
}

/// An isolated store whose session `name` is in the catalog, with its
/// actor created and claimed; `None` without a database.
struct Owned {
    _database: IsolatedDatabase,
    storage: PostgresStorage,
    session: SessionId,
    actor: ActorKey,
    epoch: Epoch,
}

async fn owned(name: &'static str) -> Option<Owned> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the {name} round-trip budget: database URL is not set");
        return None;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let session = SessionId::from(name);
    lash_core_store::store::SessionCatalogStore::admit_session(
        &storage.store(),
        &lash_core_execution::testing::store_fixtures::root_session_request(&session),
    )
    .await
    .expect("the catalog admits the session");
    let store = storage.durable_store();
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("round-trips"),
            decodes: vec![FormatSet::unstarted_session()],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register");
    let actor = ActorKey::session(session.as_str()).expect("actor key");
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    store
        .commit_mail(wake, CommitLabel::new("law.wake"))
        .await
        .expect("wake the idle actor");
    let epoch = store.claim(&node, 1).await.expect("claim")[0].epoch;
    Some(Owned {
        _database: database,
        storage,
        session,
        actor,
        epoch,
    })
}

/// Stage on `tx` one node of `kind` holding `text`, as a commit's
/// checkpoint or record is staged.
fn staged(tx: &mut ActorTx, kind: NodeKind, text: &str) -> GraphNode {
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut body = TokenWriter::new();
    body.text(text);
    let node = staging
        .encode(kind, TypedBody::logical(&body.into_bytes()))
        .expect("the node stages");
    tx.stage_all(staging);
    node
}

/// Accept input `id` for `owned`'s session, as a producer does.
async fn accept(owned: &Owned, id: &str) -> lash_vocabulary::identity::InputId {
    use lash_core_store::store::TurnInputStore as _;
    use lash_core_store::turn_input_vocabulary::PendingTurnInputBatch;
    let draft = lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
        &owned.session,
        lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
        lash_core_store::turn_input_vocabulary::TurnInput::text("round trips"),
    )
    .with_source_key(id);
    let accepted = owned
        .storage
        .store()
        .enqueue_pending_turn_inputs(PendingTurnInputBatch::one(draft))
        .await
        .expect("accept the input");
    accepted[0].input_id.clone()
}

/// Admit `run` with `input` on `tx`, as the session's drain does.
fn admit(tx: &mut ActorTx, owned: &Owned, run: &TurnId, input: lash_vocabulary::identity::InputId) {
    tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
        session: owned.session.clone(),
        run: run.clone(),
        inputs: vec![input.clone()],
        batches: Vec::new(),
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: owned.session.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(input),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    tx.ack_seen();
}

/// Write `model.start` of model call `call` on `tx`: the call's pin with
/// its checkpoint.
fn model_start(tx: &mut ActorTx, owned: &Owned, run: &TurnId, call: u32) {
    let deadline = tx.opened_at().after_millis(60_000);
    let checkpoint = staged(
        tx,
        NodeKind::NAMESPACE_VALUE,
        &format!("{{\"call\":{call}}}"),
    );
    tx.write(DomainWrite::Turn(TurnWrite::Advance {
        session: owned.session.clone(),
        run: run.clone(),
        phase: PhaseAdvance::Model {
            pin: ModelPin {
                call,
                attempt: 1,
                request_ref: "request".to_owned(),
                deadline,
                stream_from: "stream".to_owned(),
            },
            checkpoint,
        },
        iteration: 0,
    }));
}

/// Assert that `operation` took at most `budget` round trips.
fn within(operation: &str, trips: u64, budget: u64) {
    eprintln!("{operation}: {trips} round trips");
    assert!(
        trips <= budget,
        "{operation} took {trips} round trips, over its budget of {budget}"
    );
}

/// The most round trips the acceptance of one input pays (FIG-5936; 17
/// before it): its guarded transaction and fleet check, the session's
/// write authority taken once with the deletion check, replay and the
/// forsaken tombstone read together (FIG-6345),
/// closing and referrer checks, the allocation, the insert, which returns
/// the row, the session's wake and `COMMIT`.
const ACCEPT_ROUND_TRIPS: u64 = 14;

/// A producer's acceptance of one input stays within its round-trip
/// budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_input_acceptance_stays_within_its_round_trip_budget() {
    let Some(owned) = owned("round-trips-accept").await else {
        return;
    };
    let (_, trips) = round_trips(accept(&owned, "accepted")).await;
    within("accept", trips, ACCEPT_ROUND_TRIPS);
}

/// The most round trips a turn's admission pays from its open (FIG-5936;
/// 12 before it): the open and the one-call commit, which binds the input,
/// admits the run, writes its phase and acknowledges the mailbox.
const ADMIT_ROUND_TRIPS: u64 = 2;

/// A turn's admission of one input stays within its round-trip budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_admission_stays_within_its_round_trip_budget() {
    let Some(owned) = owned("round-trips-admit").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "admitted").await;
    let run = TurnId::from("round-trips-turn");
    let (_, trips) = round_trips(async {
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        admit(&mut tx, &owned, &run, input);
        store
            .commit(tx, CommitLabel::TURN_ADMIT)
            .await
            .expect("admit the turn")
    })
    .await;
    within("turn.admit", trips, ADMIT_ROUND_TRIPS);
    assert!(store.turn(&owned.session).await.expect("read").is_some());
}

/// The most round trips a model call's critical path pays to commit
/// `model.start` (FIG-5275; 14 before it, 8 before FIG-5936): the open,
/// which also reads the turn's cancel and the clock, and the commit.
const MODEL_START_ROUND_TRIPS: u64 = 2;

/// The most round trips `model.start`'s commit pays once the owner is open
/// (FIG-5936; 7 before it): one call, which runs the envelope, reading the
/// turn's cancel, installs the phase's checkpoint node and advances the
/// phase. No checkout pings the server.
const MODEL_START_COMMIT_ROUND_TRIPS: u64 = 1;

/// The owner's path from a model call's first poll to its committed
/// `model.start` stays within its round-trip budget, and its commit within
/// its own, so a read or an envelope statement added to either cannot land
/// unnoticed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_start_commit_stays_within_its_round_trip_budget() {
    let Some(owned) = owned("round-trips-model-start").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "model-start").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    admit(&mut tx, &owned, &run, input);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit the turn");

    let (mut tx, opened) = round_trips(async {
        store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open")
    })
    .await;
    assert!(tx.turn_cancel().is_none(), "no cancel was requested");
    model_start(&mut tx, &owned, &run, 1);
    let (committed, commit_trips) = round_trips(async {
        store
            .commit(tx, CommitLabel::MODEL_START)
            .await
            .expect("commit model.start")
    })
    .await;
    assert!(committed.turn_cancel.is_none(), "no cancel was requested");
    within(
        "model.start",
        opened + commit_trips,
        MODEL_START_ROUND_TRIPS,
    );
    within(
        "model.start's commit",
        commit_trips,
        MODEL_START_COMMIT_ROUND_TRIPS,
    );
    let row = store
        .turn(&owned.session)
        .await
        .expect("read the turn")
        .expect("the turn is open");
    assert!(
        matches!(row.phase, UnfinishedPhase::Model { .. }),
        "model.start committed its pin"
    );
}

/// The most round trips a round's outcome pays from its open (FIG-5936;
/// 10 before it): the open and the one-call commit, which installs its
/// record and appends the fact.
const ROUND_OUTCOME_ROUND_TRIPS: u64 = 2;

/// A round's outcome of one call stays within its round-trip budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_round_outcome_stays_within_its_round_trip_budget() {
    let Some(owned) = owned("round-trips-outcome").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let call =
        lash_protocol_types::ToolCallId::parse(&format!("tc_{}", "ab".repeat(32))).expect("call");
    let (_, trips) = round_trips(async {
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        let record = staged(&mut tx, NodeKind::ROUND_FACT, "outcome");
        tx.write(DomainWrite::RunFact(RunFactWrite::Append {
            owner: OwnerKey::Turn(owned.session.clone(), TurnId::from("round-trips-turn")),
            run: RunSeq(0),
            ordinal: Ordinal(0),
            kind: RunFactKind::XOutcome,
            call: Some(call),
            record,
        }));
        store
            .commit(tx, CommitLabel::ROUND_OUTCOME)
            .await
            .expect("commit the outcome")
    })
    .await;
    within("round.outcome", trips, ROUND_OUTCOME_ROUND_TRIPS);
}

/// The most round trips a VM's park over its earlier park pays from its
/// open (FIG-5936; 9 before it): the open and the one-call commit, which
/// installs its state and replaces the root.
const VM_PARK_ROUND_TRIPS: u64 = 2;

/// A VM park over an earlier park stays within its round-trip budget, and
/// so does a checkpoint park that leases its root and ends its earlier
/// park's lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vm_park_stays_within_its_round_trip_budget() {
    let Some(owned) = owned("round-trips-park").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let exec = ExecKey::Cell(
        owned.session.clone(),
        TurnId::from("round-trips-turn"),
        lash_durable::domain::CellId::new("cell"),
    );
    let park = |tx: &mut ActorTx, state: &str, expected: Option<(u64, GraphNode)>| {
        let root = staged(tx, NodeKind::EXEC_ROOT, state);
        tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
            exec: exec.clone(),
            expected: expected.map(|(rev, _)| SnapshotRev(rev)),
            expected_root: expected.map(|(_, root)| root),
            root,
            commit_id: CommitId::mint(),
        }));
        root
    };
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    let first = park(&mut tx, "first park", None);
    store
        .commit(tx, CommitLabel::CELL_SNAPSHOT)
        .await
        .expect("the first park");
    let (second, trips) = round_trips(async {
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        let root = park(&mut tx, "second park", Some((1, first)));
        store
            .commit(tx, CommitLabel::CELL_SNAPSHOT)
            .await
            .expect("the second park");
        root
    })
    .await;
    within("cell.snapshot", trips, VM_PARK_ROUND_TRIPS);
    // A code cell's checkpoint leases the root it publishes, and the next
    // one ends that lease as it leases its own (FIG-7585): each is still
    // the open and one call, which answers the lease it took.
    let mut expected = (2, second);
    let mut held = None;
    for state in ["leased park", "next leased park"] {
        let ((committed, root), trips) = round_trips(async {
            let mut tx = store
                .begin(&owned.actor, owned.epoch, u64::MAX)
                .await
                .expect("open");
            let root = park(&mut tx, state, Some(expected));
            tx.lease_root(RootOwner::Exec(exec.clone()), root, 60_000);
            if let Some(lease) = held {
                tx.release_root_lease(lease);
            }
            let committed = store
                .commit(tx, CommitLabel::CELL_SNAPSHOT_ADMIT)
                .await
                .expect("the leased park");
            (committed, root)
        })
        .await;
        within("a leased cell.snapshot+admit", trips, VM_PARK_ROUND_TRIPS);
        let [lease] = &committed.root_leases[..] else {
            panic!(
                "the call answers its one lease: {:?}",
                committed.root_leases
            );
        };
        held = Some(lease.id);
        expected = (expected.0 + 1, root);
    }
}

/// The most round trips `turn.commit` pays from its open (FIG-5942; 38
/// before it): the open, which reads the head the commit is planned on,
/// and the one call, which guards every fact of that plan.
const TURN_COMMIT_ROUND_TRIPS: u64 = 2;

/// A turn's commit, its session's head commit with the run's terminal and
/// its scope's end, stays within its round-trip budget: without artifacts,
/// with artifacts its execution and a gated frame read last, and with a
/// later frame of the session still reading them (FIG-7576), which the call
/// vouches for without a decoder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_commit_stays_within_its_round_trip_budget() {
    turn_commit_round_trips(false, false).await;
    turn_commit_round_trips(true, false).await;
    turn_commit_round_trips(true, true).await;
}

async fn turn_commit_round_trips(holds_artifacts: bool, surviving_frame: bool) {
    use lash_core_store::store::SessionHistoryStore as _;
    use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;
    let Some(owned) = owned("round-trips-turn-commit").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "turn-commit").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    admit(&mut tx, &owned, &run, input);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit the turn");
    let journal = lash_protocol_types::ExecutionScope::turn(owned.session.clone(), run.clone())
        .journal_identity()
        .expect("turn journal");
    let artifacts = owned.storage.lash_vm_artifact_store();
    let claim = lash_core_store::artifact_referrer::ReferrerClaim::guarded(
        lash_core_store::artifact_referrer::ReferrerGuard::Journal(journal.clone()),
    );
    if holds_artifacts {
        artifacts
            .publish_module_artifact(&claim, "round-trips-module", b"module")
            .await
            .expect("the execution's artifact");
        let frame = lash_core_store::artifact_referrer::FrameEnvironmentId::new(
            owned.session.clone(),
            lash_core_store::session_identity::FrameNodeId::new("round-trips-old-frame")
                .expect("frame"),
        );
        let frame_claim = lash_core_store::artifact_referrer::ReferrerClaim::guarded(
            lash_core_store::artifact_referrer::ReferrerGuard::Frame {
                frame: frame.clone(),
                creator: journal.clone(),
            },
        );
        artifacts
            .acquire_module_artifact(&frame_claim, "round-trips-module")
            .await
            .expect("the source frame's artifact");
        owned
            .storage
            .artifact_cleanup()
            .arm_cleanup(
                &lash_core_store::artifact_referrer::ArtifactCleanup::ended(
                    lash_core_store::artifact_referrer::ArtifactReferrer::FrameEnvironment(frame),
                    Vec::new(),
                    Some(journal.clone()),
                ),
                1,
            )
            .await
            .expect("the frame waits for the turn to settle");
        if surviving_frame {
            let later = lash_core_store::artifact_referrer::FrameEnvironmentId::new(
                owned.session.clone(),
                lash_core_store::session_identity::FrameNodeId::new("round-trips-new-frame")
                    .expect("frame"),
            );
            artifacts
                .acquire_module_artifact(
                    &lash_core_store::artifact_referrer::ReferrerClaim::guarded(
                        lash_core_store::artifact_referrer::ReferrerGuard::Frame {
                            frame: later,
                            creator: journal.clone(),
                        },
                    ),
                    "round-trips-module",
                )
                .await
                .expect("the session's later frame reads the artifact");
        }
    }
    let factory = owned.storage.store();
    let mut state = lash_core_store::store::window_state(
        factory
            .load_session_window(
                &owned.session,
                lash_core_store::store::WindowSelector::Current,
            )
            .await
            .expect("the session's window")
            .expect("the session's head"),
        lash_core_store::store::FleetFormat::current(),
    )
    .expect("the session's state")
    .state;
    let head = lash_core_store::store::append_request_commit_with_clock_for_testing(
        &mut state,
        "round-trips",
        &[
            lash_protocol_types::session_model::SessionAppendNode::message(
                lash_protocol_types::plugin::PluginMessage::text(
                    lash_protocol_types::session_model::MessageRole::Assistant,
                    "done",
                ),
            ),
        ],
        None,
        &lash_core_ids::clock::SystemClock,
    )
    .expect("the head commit");
    let expected_head = head.expected_head_revision;
    let encoded = lash_core_store::store::encode_session_commit(&head).expect("encode");
    let scope = lash_durable::domain::ScopeKey::Turn(owned.session.clone(), run.clone());
    let (_, trips) = round_trips(async {
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        match encoded.descriptor {
            Some(descriptor) => {
                tx.stage([descriptor]);
            }
            None => {
                if let Some(root) = encoded.execution {
                    tx.require(root);
                }
            }
        }
        tx.write(DomainWrite::SessionCommit(
            lash_durable::domain::SessionCommitWrite {
                session: owned.session.clone(),
                expected_head,
                commit_json: encoded.json,
                execution: encoded.execution,
            },
        ));
        tx.write(DomainWrite::Turn(TurnWrite::Terminal {
            session: owned.session.clone(),
            run: run.clone(),
            cause: Box::new(lash_core_store::store::RunTerminalCause::Committed {
                commit: lash_core_store::store::TurnCommitId::new(run.clone(), 0),
                turn: run.clone(),
                outcome: lash_core_store::store::RunCommittedOutcome::Finished {
                    finish: lash_protocol_types::TurnFinish::AssistantMessage {
                        text: "done".to_owned(),
                    },
                    value_schema: None,
                },
            }),
            head_revision: Some(expected_head.saturating_add(1)),
            transcript: None,
        }));
        tx.write(DomainWrite::Wait(
            lash_durable::domain::WaitWrite::RevokeScope(scope.clone()),
        ));
        tx.write(DomainWrite::Process(
            lash_durable::domain::ProcessWrite::ScopeClosed {
                scope: scope.clone(),
            },
        ));
        tx.give_up(Release::Idle);
        store
            .commit(tx, CommitLabel::TURN_COMMIT)
            .await
            .expect("commit the turn")
    })
    .await;
    within(
        match (holds_artifacts, surviving_frame) {
            (false, _) => "turn.commit without artifact edges",
            (true, false) => "turn.commit with execution and gated-frame edges",
            (true, true) => "turn.commit with a surviving frame reader",
        },
        trips,
        TURN_COMMIT_ROUND_TRIPS,
    );
    assert_eq!(
        artifacts
            .get_module_artifact("round-trips-module")
            .await
            .expect("read artifact")
            .is_some(),
        surviving_frame,
        "the terminal reclaims the execution and gated frame's last-reader bytes, \
         and keeps those a surviving frame reads"
    );
    assert!(
        matches!(
            artifacts
                .publish_module_artifact(&claim, "late-module", b"module")
                .await,
            Err(
                lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { .. }
            )
        ),
        "empty and non-empty executions both keep their publication fence"
    );
    assert!(store.turn(&owned.session).await.expect("read").is_none());
}

/// Real model boundaries include claim requirements and dispatch records,
/// unlike the minimal phase-only budgets above (FIG-7082).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_and_tool_boundaries_each_commit_in_one_call() {
    use lash_durable::domain::{ModelCallId, ModelCallPinWrite, ModelCallWrite, PromptCallKey};
    use lash_protocol_types::llm::types::LlmCallId;
    let Some(owned) = owned("round-trips-model-boundaries").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "boundaries").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    admit(&mut tx, &owned, &run, input);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit");
    let key = PromptCallKey {
        session: owned.session.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal: 1,
        },
    };
    let record_prompt = |tx: &mut ActorTx, ordinal| {
        use lash_durable::domain::{PromptText, PromptWrite};
        let leaf = PromptText::audit("leaf");
        let prefix = PromptText::fragment(
            "prefix",
            vec![lash_core_store::checkpoint_graph::NodeId::from_hex(&leaf.hash).unwrap()],
        );
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: PromptCallKey {
                session: owned.session.clone(),
                call: ModelCallId::Turn {
                    run: run.clone(),
                    ordinal,
                },
            },
            snapshot: Some("audit".into()),
            request: "request".into(),
            snapshot_texts: Vec::new(),
            request_texts: vec![leaf, prefix.clone()].into(),
            prefix: Some(prefix.hash),
        }));
    };
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.set_requires(vec!["model-component".into()]);
    record_prompt(&mut tx, 1);
    model_start(&mut tx, &owned, &run, 1);
    tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Reserve {
        key: key.clone(),
        call_id: LlmCallId("boundary-call".into()),
        attempt: 1,
        enclosing: None,
    }));
    let (started, start) = round_trips(store.commit(tx, CommitLabel::MODEL_START)).await;
    started.expect("model.start");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    let answer = staged(&mut tx, NodeKind::NAMESPACE_VALUE, "answer");
    tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Answered {
        key: key.clone(),
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Answer {
        session: owned.session.clone(),
        run: run.clone(),
        answer,
    }));
    let (answered, answer_trips) = round_trips(store.commit(tx, CommitLabel::MODEL_ANSWER)).await;
    answered.expect("answer");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.set_requires(vec!["tool-component".into()]);
    let record = staged(&mut tx, NodeKind::NAMESPACE_VALUE, "sealed model call");
    tx.write(DomainWrite::ModelCall(ModelCallWrite {
        key: key.clone(),
        call_id: LlmCallId("boundary-call".into()),
        record,
    }));
    let checkpoint = staged(&mut tx, NodeKind::NAMESPACE_VALUE, "tools");
    tx.write(DomainWrite::Turn(TurnWrite::Advance {
        session: owned.session.clone(),
        run: run.clone(),
        phase: PhaseAdvance::Tools {
            run: RunSeq(0),
            checkpoint,
        },
        iteration: 0,
    }));
    let (done, done_trips) = round_trips(store.commit(tx, CommitLabel::MODEL_DONE)).await;
    done.expect("model.done");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.set_requires(vec!["model-component".into()]);
    record_prompt(&mut tx, 2);
    model_start(&mut tx, &owned, &run, 2);
    let (next, next_trips) =
        round_trips(store.commit(tx, CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START)).await;
    next.expect("next model call");
    eprintln!(
        "boundary calls: start={start} answer={answer_trips} done={done_trips} next={next_trips}"
    );
    assert_eq!([start, answer_trips, done_trips, next_trips], [1; 4]);
    let requirements = store
        .actor_requirements(None, 10)
        .await
        .expect("requirements");
    assert!(
        requirements
            .iter()
            .any(|(actor, requires)| actor == &owned.actor && requires == &["model-component"])
    );
    assert!(
        store
            .model_call_pins(&owned.session, &ModelCallId::turn_owner(&run))
            .await
            .expect("pins")
            .is_empty()
    );

    // A different immutable root must refuse before the later (also invalid)
    // phase write, and the failed call must roll its requirement stamp back.
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open collision");
    tx.set_requires(vec!["must-not-publish".into()]);
    let record = staged(
        &mut tx,
        NodeKind::NAMESPACE_VALUE,
        "conflicting sealed call",
    );
    tx.write(DomainWrite::ModelCall(ModelCallWrite {
        key,
        call_id: LlmCallId("boundary-call".into()),
        record,
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Answer {
        session: owned.session.clone(),
        run: TurnId::from("absent-run"),
        answer: record,
    }));
    let error = store
        .commit(tx, CommitLabel::MODEL_ANSWER)
        .await
        .expect_err("immutable record refuses");
    assert!(
        matches!(error, lash_durable::DurableError::Store(lash_durable::StoreFailure { kind: lash_durable::StoreFailureKind::Corrupt, ref message, .. }) if message.contains("a sealed call cannot be replaced")),
        "the first refusal stays durable: {error}"
    );
    let requirements = store
        .actor_requirements(None, 10)
        .await
        .expect("requirements after refusal");
    assert!(
        requirements
            .iter()
            .any(|(actor, requires)| actor == &owned.actor && requires == &["model-component"])
    );
    // Held-text hints never vouch for retention: a reclaimed forest must
    // be installed again from its supplied bodies through the direct form.
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open release");
    tx.write(DomainWrite::Prompt(
        lash_durable::domain::PromptWrite::Release {
            session: owned.session.clone(),
            run: None,
        },
    ));
    store
        .commit(tx, CommitLabel::MODEL_DONE)
        .await
        .expect("release texts");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open re-record");
    record_prompt(&mut tx, 3);
    let (recorded, trips) = round_trips(store.commit(tx, CommitLabel::MODEL_START)).await;
    recorded.expect("stale hint falls back to supplied bodies");
    assert!(trips > 1, "the stale held-text hint was refused");

    // The root's duplicate identity wins before any missing fragment; on
    // a new call, its missing fragment wins before a later phase refusal.
    let missing = lash_core_store::store::BlobRef::for_content(b"missing prompt fragment").0;
    for ordinal in [3, 4] {
        let call = PromptCallKey {
            session: owned.session.clone(),
            call: ModelCallId::Turn {
                run: run.clone(),
                ordinal,
            },
        };
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open prompt refusal");
        tx.write(DomainWrite::Prompt(
            lash_durable::domain::PromptWrite::Record {
                call: call.clone(),
                snapshot: None,
                request: "missing".into(),
                snapshot_texts: Vec::new(),
                request_texts: Default::default(),
                prefix: Some(missing.clone()),
            },
        ));
        let later_answer = staged(&mut tx, NodeKind::NAMESPACE_VALUE, "later invalid phase");
        tx.write(DomainWrite::Turn(TurnWrite::Answer {
            session: owned.session.clone(),
            run: TurnId::from("absent-run"),
            answer: later_answer,
        }));
        let error = store
            .commit(tx, CommitLabel::MODEL_START)
            .await
            .expect_err("prompt refuses first");
        let expected = if ordinal == 3 {
            lash_durable::domain::DomainRefusal::PromptCallRecorded { call }
        } else {
            lash_durable::domain::DomainRefusal::PromptFragmentMissing {
                hash: missing.clone(),
            }
        };
        assert_eq!(error, lash_durable::DurableError::Domain(expected));
    }
}

/// How many tool rounds the five-tool turn law drives.
const TOOL_ROUNDS: u32 = 5;

/// The most round trips each commit of a tool turn pays (FIG-7576): the
/// open, which reads the owner's fence, the turn's cancel and the clock in
/// one statement, and the one call. Every commit of the turn writes its
/// rows, its graph and its tool calls inside that call, so neither number
/// grows with the turn's tool calls: a five-tool turn's opens and commits
/// are each one round trip, and the turn pays two per commit.
const TOOL_TURN_OPEN_ROUND_TRIPS: u64 = 1;
const TOOL_TURN_COMMIT_ROUND_TRIPS: u64 = 1;

/// A five-tool turn's every open and commit stays within its budget: each
/// round's `model.done`, which seals the model call, advances to the tools
/// phase and admits the call, and its `round.outcome+present+model.start`,
/// which completes the call, appends its outcome and starts the next model
/// call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_five_tool_turn_commits_each_boundary_in_one_call() {
    use lash_durable::domain::{
        CallOwner, ModelCallId, ModelCallPinWrite, ModelCallWrite, PromptCallKey, ToolCallAnswer,
        ToolCallDecision, ToolCallWrite,
    };
    use lash_protocol_types::llm::types::LlmCallId;
    let Some(owned) = owned("round-trips-five-tools").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "five-tools").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    admit(&mut tx, &owned, &run, input);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit");
    let pin = |ordinal: u32| PromptCallKey {
        session: owned.session.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal,
        },
    };
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    model_start(&mut tx, &owned, &run, 1);
    tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Reserve {
        key: pin(1),
        call_id: LlmCallId("five-tools-1".into()),
        attempt: 1,
        enclosing: None,
    }));
    store
        .commit(tx, CommitLabel::MODEL_START)
        .await
        .expect("model.start");
    let execution = OwnerKey::Turn(owned.session.clone(), run.clone());
    let mut trips = Vec::new();
    for round in 1..=TOOL_ROUNDS {
        let call =
            lash_protocol_types::ToolCallId::parse(&format!("tc_{round:064x}")).expect("call id");
        let (tx, open) = round_trips(store.begin(&owned.actor, owned.epoch, u64::MAX)).await;
        let mut tx = tx.expect("open");
        let record = staged(
            &mut tx,
            NodeKind::NAMESPACE_VALUE,
            &format!("sealed {round}"),
        );
        tx.write(DomainWrite::ModelCall(ModelCallWrite {
            key: pin(round),
            call_id: LlmCallId(format!("five-tools-{round}")),
            record,
        }));
        let checkpoint = staged(
            &mut tx,
            NodeKind::NAMESPACE_VALUE,
            &format!("tools {round}"),
        );
        tx.write(DomainWrite::Turn(TurnWrite::Advance {
            session: owned.session.clone(),
            run: run.clone(),
            phase: PhaseAdvance::Tools {
                run: RunSeq(u64::from(round)),
                checkpoint,
            },
            iteration: 0,
        }));
        tx.write(DomainWrite::ToolCall(ToolCallWrite::Admit {
            counted: true,
            logical: CallOwner::Turn(owned.session.clone(), run.clone()),
            execution: execution.clone(),
            run: RunSeq(u64::from(round)),
            member: 0,
            call: call.clone(),
            cancellable: true,
        }));
        let (done, commit) = round_trips(store.commit(tx, CommitLabel::MODEL_DONE)).await;
        done.expect("model.done");
        trips.push(("model.done", open, commit));
        let (tx, open) = round_trips(store.begin(&owned.actor, owned.epoch, u64::MAX)).await;
        let mut tx = tx.expect("open");
        let fact = staged(&mut tx, NodeKind::FROZEN_VALUE, &format!("answer {round}"));
        tx.write(DomainWrite::ToolCall(ToolCallWrite::Complete {
            owner: execution.clone(),
            call: call.clone(),
            fact,
            answer: ToolCallAnswer::Materialized {
                decision: ToolCallDecision::Final,
                body: fact,
            },
        }));
        let outcome = staged(&mut tx, NodeKind::ROUND_FACT, &format!("outcome {round}"));
        tx.write(DomainWrite::RunFact(RunFactWrite::Append {
            owner: execution.clone(),
            run: RunSeq(u64::from(round)),
            ordinal: Ordinal(0),
            kind: RunFactKind::XOutcome,
            call: Some(call),
            record: outcome,
        }));
        model_start(&mut tx, &owned, &run, round + 1);
        tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Reserve {
            key: pin(round + 1),
            call_id: LlmCallId(format!("five-tools-{}", round + 1)),
            attempt: 1,
            enclosing: None,
        }));
        let (next, commit) =
            round_trips(store.commit(tx, CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START)).await;
        next.expect("round.outcome+present+model.start");
        trips.push(("round.outcome+present+model.start", open, commit));
    }
    for (label, open, commit) in &trips {
        within(&format!("{label} open"), *open, TOOL_TURN_OPEN_ROUND_TRIPS);
        within(label, *commit, TOOL_TURN_COMMIT_ROUND_TRIPS);
    }
}

/// The most round trips each open and each commit of a session's opening
/// turn pays (FIG-7576): the open, and the one call, which also prepares
/// the environment's version owner (`environment.prepare`) and publishes it
/// with the session's environment head (`model.start`). The call compares
/// the expected head with the stored one by its content-addressed root, so
/// no commit decodes a head or an owner's body.
const SESSION_OPEN_ROUND_TRIPS: u64 = 1;

/// A session's opening turn prepares, publishes and heads its environment
/// with each open and commit in one round trip, and the head it publishes
/// is the one the store reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_opening_turn_commits_its_environment_in_one_call() {
    use lash_core_store::environment::{EnvironmentHead, VersionOwnerState, assemble};
    use lash_durable::environment::EnvironmentWrite;
    let Some(owned) = owned("round-trips-session-open").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "session-open").await;
    let run = TurnId::from("round-trips-turn");
    let body = assemble(None, []).expect("an empty environment");
    let owner = lash_core_store::environment::VersionOwner::preparing(body.id().clone());
    let mut trips = Vec::new();
    let (tx, open) = round_trips(store.begin(&owned.actor, owned.epoch, u64::MAX)).await;
    let mut tx = tx.expect("open");
    admit(&mut tx, &owned, &run, input);
    tx.write(DomainWrite::Environment(
        EnvironmentWrite::prepare(owner.clone(), &body).expect("the preparation"),
    ));
    let (prepared, commit) = round_trips(store.commit(tx, CommitLabel::ENVIRONMENT_PREPARE)).await;
    prepared.expect("environment.prepare");
    trips.push(("environment.prepare", open, commit));
    let head = EnvironmentHead {
        version: owner.version().clone(),
        owner: owner.id().clone(),
        input_frontier: Default::default(),
    };
    let (tx, open) = round_trips(store.begin(&owned.actor, owned.epoch, u64::MAX)).await;
    let mut tx = tx.expect("open");
    let mut nodes = lash_durable::environment::staging();
    lash_durable::environment::stage_head(&mut nodes, &head).expect("the head stages");
    tx.stage(nodes.into_nodes().into_values());
    tx.write(DomainWrite::Environment(EnvironmentWrite::Publish {
        owner: owner.id().clone(),
    }));
    tx.write(DomainWrite::Environment(EnvironmentWrite::Head {
        expected: None,
        head: Some(head.clone()),
    }));
    model_start(&mut tx, &owned, &run, 1);
    let (started, commit) = round_trips(store.commit(tx, CommitLabel::MODEL_START)).await;
    started.expect("model.start");
    trips.push(("model.start", open, commit));
    let environments = store.environments();
    assert_eq!(
        environments
            .environment_head(&owned.session)
            .await
            .expect("read the head"),
        Some(head)
    );
    assert_eq!(
        environments
            .environment_owner(owner.id())
            .await
            .expect("read the owner")
            .expect("the owner")
            .owner
            .state(),
        VersionOwnerState::Published
    );
    for (label, open, commit) in &trips {
        within(&format!("{label} open"), *open, SESSION_OPEN_ROUND_TRIPS);
        within(label, *commit, SESSION_OPEN_ROUND_TRIPS);
    }
}

/// A commit that ends the turn's registered execution, as a session's
/// publication does, is one call (FIG-7576): the end is a guarded step, and
/// the actor's effective requirements return to its base ones in the call
/// once no live execution owes it more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_execution_end_commits_in_one_call() {
    use lash_durable::resolutions::{ExecutionId, ExecutionRegistration};
    let Some(owned) = owned("round-trips-execution-end").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let run = TurnId::from("round-trips-turn");
    let journal = lash_protocol_types::ExecutionScope::turn(owned.session.clone(), run.clone())
        .journal_identity()
        .expect("turn journal");
    let execution = ExecutionId::cell(&owned.session, &run, "cell");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.register_execution(ExecutionRegistration {
        execution: execution.clone(),
        uses: Default::default(),
        referrer: lash_core_store::artifact_referrer::ArtifactReferrer::Execution(journal),
        requirements: Default::default(),
    });
    store
        .commit(tx, CommitLabel::CELL_SNAPSHOT)
        .await
        .expect("register the execution");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.end_execution(execution.clone());
    let (ended, trips) = round_trips(store.commit(tx, CommitLabel::MODEL_START)).await;
    ended.expect("the execution ends");
    within("an execution's end", trips, 1);
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.end_execution(ExecutionId::cell(&owned.session, &run, "unregistered"));
    assert_eq!(
        store.commit(tx, CommitLabel::MODEL_START).await,
        Err(lash_durable::packages::PackageRefusal::ExecutionUse.into()),
        "an execution the actor never registered does not end"
    );
}

/// A registration, an adoption and a seed each commit in one call
/// (FIG-7576), with live execution debt and new base requirements: the
/// referrer lock and fence, the registration's replay equality, the
/// adoption's kept requests and the seed's winner run as guarded steps, and
/// the call merges the actor's debt into its effective requirements. A
/// refused adoption or seed is named as the explicit form names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_registration_adoption_and_seed_each_commit_in_one_call() {
    use lash_durable::packages::PackageRefusal;
    use lash_durable::resolutions::{
        DeferredUse, ExecutionId, ExecutionRegistration, ResolutionEntry, ResolutionKey,
        ResolutionRecord,
    };
    use lash_kernel_value::names::{ImportSlot, OperationUse, QualifiedName};
    let Some(owned) = owned("round-trips-execution-writes").await else {
        return;
    };
    let run = TurnId::from("round-trips-turn");
    let journal = lash_protocol_types::ExecutionScope::turn(owned.session.clone(), run.clone())
        .journal_identity()
        .expect("turn journal");
    let name = |member: &str| QualifiedName {
        package: lash_kernel_value::names::PackagePath::parse("wip").expect("package"),
        member: lash_kernel_value::names::Segment::new(member).expect("member"),
        type_member: None,
    };
    let use_ = OperationUse {
        link: lash_kernel_value::package::LinkId::from_bytes([7; 32]),
        slot: ImportSlot(1).into(),
    };
    let kernel = |version: u32| lash_durable::ClaimRequirements {
        machine: lash_kernel_value::claims::NodeRequirements {
            kernels: [version].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let registration = ExecutionRegistration {
        execution: ExecutionId::cell(&owned.session, &run, "cell"),
        uses: [(
            use_,
            DeferredUse {
                requested: name("lookup"),
                discovery: kernel(2),
            },
        )]
        .into(),
        referrer: lash_core_store::artifact_referrer::ArtifactReferrer::Execution(journal),
        requirements: kernel(1),
    };
    async fn counted(
        owned: &Owned,
        write: impl FnOnce(&mut ActorTx),
    ) -> (
        Result<lash_durable::ActorCommit, lash_durable::DurableError>,
        u64,
    ) {
        let store = owned.storage.durable_store();
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        write(&mut tx);
        round_trips(store.commit(tx, CommitLabel::CELL_SNAPSHOT)).await
    }
    let (registered, trips) = counted(&owned, |tx| {
        tx.register_execution(registration.clone())
            .set_requires(vec!["kernel:9".to_owned()]);
    })
    .await;
    registered.expect("register the execution");
    within("an execution's registration", trips, 1);
    let mut successor = registration.clone();
    successor.requirements = kernel(3);
    let (adopted, trips) = counted(&owned, |tx| {
        tx.adopt_execution(successor.clone());
    })
    .await;
    adopted.expect("adopt the execution");
    within("an execution's adoption", trips, 1);
    let winner = ResolutionRecord {
        key: ResolutionKey {
            execution: registration.execution.clone(),
            use_,
        },
        requested: name("lookup"),
        entry: ResolutionEntry::Refused {
            note: "first".into(),
        },
    };
    let (seeded, trips) = counted(&owned, |tx| {
        tx.seed_resolution(winner.clone());
    })
    .await;
    seeded.expect("seed the winner");
    within("a resolution's seed", trips, 1);
    let mut stored = owned
        .storage
        .durable_store()
        .actor_requirements(None, 8)
        .await
        .expect("requirements");
    stored.retain(|(actor, _)| *actor == owned.actor);
    assert_eq!(
        stored,
        vec![(
            owned.actor.clone(),
            vec!["kernel:3".to_owned(), "kernel:9".to_owned()]
        )],
        "the base requirement and the adopted stable debt, the use's discovery \
         debt settled by its winner"
    );
    let mut conflicting = winner.clone();
    conflicting.entry = ResolutionEntry::Refused {
        note: "second".into(),
    };
    let (refused, _) = counted(&owned, |tx| {
        tx.seed_resolution(conflicting);
    })
    .await;
    assert!(
        matches!(
            refused,
            Err(lash_durable::DurableError::Package(
                PackageRefusal::Immutable(_)
            ))
        ),
        "another winner is refused as immutable: {refused:?}"
    );
    let mut renamed = successor;
    renamed.uses.insert(
        use_,
        DeferredUse {
            requested: name("other"),
            discovery: Default::default(),
        },
    );
    let (refused, _) = counted(&owned, |tx| {
        tx.adopt_execution(renamed);
    })
    .await;
    assert_eq!(
        refused,
        Err(PackageRefusal::ExecutionUse.into()),
        "an adoption that renames a kept use is refused"
    );
}

/// The most round trips each commit of a session's close pays from its
/// open (FIG-7576): the one call, which begins the close or records a step
/// once the step before it is the one done, guarded in the call. The
/// artifacts step deletes the session's rows in the same call.
const SESSION_CLOSE_ROUND_TRIPS: u64 = 1;

/// A session's close begins and records its steps each in one call, in
/// order, refuses a step out of order, and deletes the session in its
/// artifacts step's one call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_close_commits_each_step_in_one_call() {
    use lash_durable::domain::{DomainRefusal, SessionCloseStep, SessionCloseWrite};
    let Some(owned) = owned("round-trips-session-close").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let writes = [
        (
            CommitLabel::SESSION_CLOSE_BEGIN,
            SessionCloseWrite::Begin {
                session: owned.session.clone(),
            },
        ),
        (
            CommitLabel::SESSION_CLOSE_CANCEL,
            SessionCloseWrite::Step {
                session: owned.session.clone(),
                step: SessionCloseStep::Cancel,
            },
        ),
        (
            CommitLabel::SESSION_CLOSE_REVOKE,
            SessionCloseWrite::Step {
                session: owned.session.clone(),
                step: SessionCloseStep::Revoke,
            },
        ),
        (
            CommitLabel::SESSION_CLOSE_END_SCOPE,
            SessionCloseWrite::Step {
                session: owned.session.clone(),
                step: SessionCloseStep::EndScope,
            },
        ),
    ];
    for (label, write) in writes {
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open");
        tx.write(DomainWrite::SessionClose(write));
        let (committed, trips) = round_trips(store.commit(tx, label)).await;
        committed.expect("the close step");
        within(label.as_str(), trips, SESSION_CLOSE_ROUND_TRIPS);
    }
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: owned.session.clone(),
        step: SessionCloseStep::Tombstone,
    }));
    assert_eq!(
        store
            .commit(tx, CommitLabel::SESSION_CLOSE_TOMBSTONE)
            .await
            .expect_err("the tombstone before the artifacts step"),
        lash_durable::DurableError::Domain(DomainRefusal::SessionCloseOutOfOrder {
            session: owned.session.clone(),
            step: SessionCloseStep::Tombstone,
            done: Some(SessionCloseStep::EndScope),
        })
    );
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: owned.session.clone(),
        step: SessionCloseStep::Artifacts,
    }));
    let label = CommitLabel::SESSION_CLOSE_ARTIFACTS;
    let (committed, trips) = round_trips(store.commit(tx, label)).await;
    committed.expect("the artifacts step");
    within(label.as_str(), trips, SESSION_CLOSE_ROUND_TRIPS);
    assert!(matches!(
        lash_core_store::store::SessionCatalogStore::lookup_session(
            &owned.storage.store(),
            &owned.session
        )
        .await
        .expect("look the session up"),
        lash_core_store::store::SessionLookup::Deleted
    ));
}

/// The round trips a candidate's page installation pays ahead of its
/// owner commit (FIG-6191, FIG-7576). A candidate whose entries, with the
/// commit's own graph, fit one batch of [`CandidateBuffers::DEFAULT`]
/// entries and its edges one edge batch rides in the commit's one call and
/// pays none. A larger one installs ahead in one transaction: its `BEGIN`,
/// its fence and clock read and the owner's share-locked fence row; for
/// each batch of manifest entries the key-share lock of its nodes and the
/// insert of the absent ones (a third read of what another writer inserted
/// meanwhile has no writer to race here); for each batch of edges the lock
/// of their children and their insert; and `COMMIT`. The commit then names
/// only the candidate's root, and is one call whatever else it writes.
///
/// [`CandidateBuffers::DEFAULT`]: lash_durable::domain::graph_commit::CandidateBuffers::DEFAULT
fn page_round_trips(
    summary: &lash_core_store::checkpoint_graph::stage::ManifestSummary,
    own_entries: u64,
    own_edges: u64,
) -> u64 {
    let buffers = lash_durable::domain::graph_commit::CandidateBuffers::DEFAULT;
    let entries = summary.fresh_nodes + summary.external + own_entries;
    let edges = summary.fresh_edges + own_edges;
    if entries <= buffers.entries as u64 && edges <= buffers.edges as u64 {
        return 0;
    }
    let node_batches = entries.div_ceil(buffers.entries as u64);
    let edge_batches = edges.div_ceil(buffers.edges as u64);
    3 + 2 * node_batches + 2 * edge_batches + 1
}

/// A round outcome publishing a candidate of `leaves` pages with the next
/// model call's start: the candidate's summary and the commit's round
/// trips.
async fn candidate_commit(
    name: &'static str,
    leaves: u32,
) -> Option<(
    lash_core_store::checkpoint_graph::stage::ManifestSummary,
    u64,
)> {
    use lash_durable::domain::pages::{CandidateOwner, CandidatePublication, PageStore as _};
    let owned = owned(name).await?;
    let store = owned.storage.durable_store();
    let input = accept(&owned, "candidate").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    admit(&mut tx, &owned, &run, input);
    model_start(&mut tx, &owned, &run, 1);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit");
    let manifest =
        lash_durable::laws::pages::GeneratedManifest::new(name, leaves, 64, CommitId::mint());
    let summary = lash_core_store::checkpoint_graph::stage::CandidateManifest::summary(&manifest);
    let owner = OwnerKey::Turn(owned.session.clone(), run.clone());
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .expect("open");
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut body = TokenWriter::new();
    body.seq(2);
    body.text("outcome over the candidate");
    body.child(summary.root.child_ref());
    let record = staging
        .encode(NodeKind::ROUND_FACT, TypedBody::logical(&body.into_bytes()))
        .expect("the record stages");
    tx.stage_all(staging);
    tx.write(DomainWrite::RunFact(RunFactWrite::Append {
        owner: owner.clone(),
        run: RunSeq(1),
        ordinal: Ordinal(0),
        kind: RunFactKind::XOutcome,
        call: None,
        record,
    }));
    model_start(&mut tx, &owned, &run, 2);
    tx.write(DomainWrite::ModelCallPin(
        lash_durable::domain::ModelCallPinWrite::Reserve {
            key: lash_durable::domain::PromptCallKey {
                session: owned.session.clone(),
                call: lash_durable::domain::ModelCallId::Turn {
                    run: run.clone(),
                    ordinal: 2,
                },
            },
            call_id: lash_protocol_types::llm::types::LlmCallId("candidate-2".into()),
            attempt: 1,
            enclosing: None,
        },
    ));
    let (receipt, trips) = round_trips(store.commit_candidate(
        tx,
        CandidatePublication {
            owner: CandidateOwner::RunFactAppend {
                owner,
                run: RunSeq(1),
                ordinal: Ordinal(0),
            },
            manifest: Box::new(manifest),
        },
        CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START,
    ))
    .await;
    assert_eq!(receipt.expect("round.outcome").root, summary.root);
    Some((summary, trips))
}

/// A round outcome publishing a candidate commits in one call: a candidate
/// of one batch rides in it, and a candidate of many pages installs ahead
/// in round trips that grow with its batches alone. The commit's own graph
/// is two nodes (the record, which names the candidate's root, and the
/// model call's checkpoint), with the record's one edge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_commit_is_one_call_beside_its_page_batches() {
    for (name, leaves) in [
        ("round-trips-candidate-small", 8),
        ("round-trips-candidate", 600),
    ] {
        let Some((summary, trips)) = candidate_commit(name, leaves).await else {
            return;
        };
        within(
            &format!("round.outcome+model.start with a candidate of {leaves} leaves"),
            trips,
            page_round_trips(&summary, 2, 1) + 1,
        );
    }
}

/// PREFIX-DELTA on the server-call boundary: a carried immutable prefix
/// sends only its new frontier, including metadata, as history grows.
#[tokio::test]
async fn a_growing_prompt_forest_binds_only_its_new_frontier() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptText, PromptWrite};
    let Some(owned) = owned("round-trips-prompt-frontier").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let mut texts: Vec<PromptText> = Vec::new();
    for ordinal in 1..=64 {
        let body = format!("frontier {ordinal}");
        let children = texts
            .last()
            .map(|parent| {
                vec![lash_core_store::checkpoint_graph::NodeId::from_hex(&parent.hash).unwrap()]
            })
            .unwrap_or_default();
        let text = PromptText::fragment(body, children);
        texts.push(text.clone());
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .expect("open frontier");
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: PromptCallKey {
                session: owned.session.clone(),
                call: ModelCallId::Turn {
                    run: TurnId::from("frontier-turn"),
                    ordinal,
                },
            },
            snapshot: None,
            request: "frontier".into(),
            snapshot_texts: Vec::new(),
            request_texts: texts.clone().into(),
            prefix: Some(text.hash.clone()),
        }));
        let (committed, receipt) = lash_core_ids::operational_metrics::sql::collect(
            "frontier",
            "postgres",
            store.commit(tx, CommitLabel::MODEL_START),
        )
        .await;
        committed.expect("publish frontier");
        let bound: u64 = receipt.shapes.values().map(|shape| shape.bound_bytes).sum();
        assert!(
            bound <= 8192,
            "call {ordinal} bound {bound} bytes for one new frontier node"
        );
        let stored = store
            .prompt_texts(std::slice::from_ref(&text.hash))
            .await
            .expect("read frontier");
        assert_eq!(stored, [text]);
    }
    // FIG-7223: measure the real producer too. A hand-built one-node chain
    // misses carry-depth growth and the summary seed's shifted populations.
    for (compacted, rebuilt) in [(true, false), (false, false), (false, true)] {
        use lash_core_store::prompt_sections::OwnerPromptState;
        use lash_protocol_types::llm::types::{
            ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
        };
        let mut owner = OwnerPromptState::default();
        let mut items = RequestItems::default();
        let mut early = Vec::new();
        let mut late = Vec::new();
        for turn in 1_usize..=1000 {
            if compacted && turn > 1 && (turn - 1).is_multiple_of(100) {
                let mut tx = store
                    .begin(&owned.actor, owned.epoch, u64::MAX)
                    .await
                    .unwrap();
                tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
                    session: owned.session.clone(),
                }));
                store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
                owner.clear();
                items = RequestItems::default()
                    .append(&[RequestSegment::item_json(&serde_json::json!({
                        "role": "user", "content": "compaction summary"
                    }))
                    .unwrap()])
                    .unwrap();
            }
            items = items
                .append(&[RequestSegment::item_json(&serde_json::json!({
                    "role": "user", "content": "history turn"
                }))
                .unwrap()])
                .unwrap();
            if rebuilt {
                let segments: Vec<_> = items
                    .iter()
                    .map(|item| {
                        RequestSegment::item_json(
                            &serde_json::from_str::<serde_json::Value>(&item.text_lossy()).unwrap(),
                        )
                        .unwrap()
                    })
                    .collect();
                items = RequestItems::default().append(&segments).unwrap();
            }
            let template = RecordedRequestTemplate::from_item_run(
                ProviderRouteIdentity::new("fixture", "fixture", "fixture"),
                true,
                None,
                "{\"messages\":[".into(),
                items.clone(),
                "]}".into(),
            )
            .unwrap();
            let (chunked, texts) = owner.chunk(&template);
            let run = TurnId::parse(format!("owner-{compacted}-{rebuilt}-{turn}")).unwrap();
            let mut tx = store
                .begin(&owned.actor, owned.epoch, u64::MAX)
                .await
                .unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: PromptCallKey {
                    session: owned.session.clone(),
                    call: ModelCallId::Turn {
                        run: run.clone(),
                        ordinal: 1,
                    },
                },
                snapshot: None,
                request: serde_json::to_string(&chunked).unwrap(),
                snapshot_texts: Vec::new(),
                request_texts: texts,
                prefix: chunked.prefix.map(|root| root.blob.0),
            }));
            tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                session: owned.session.clone(),
                run,
                open: None,
            }));
            let (result, receipt) = lash_core_ids::operational_metrics::sql::collect(
                "owner-frontier",
                "postgres",
                store.commit(tx, CommitLabel::MODEL_DONE),
            )
            .await;
            result.expect("publish and settle the owner's new frontier");
            items = items
                .append(&[RequestSegment::item_json(&serde_json::json!({
                    "role": "assistant", "content": "history reply"
                }))
                .unwrap()])
                .unwrap();
            let bound: u64 = receipt.shapes.values().map(|shape| shape.bound_bytes).sum();
            if (10..=30).contains(&turn) {
                early.push(bound);
            }
            if (if compacted { 910..=930 } else { 980..=1000 }).contains(&turn) {
                late.push(bound);
            }
        }
        assert_eq!(early.len(), 21);
        assert_eq!(late.len(), 21);
        early.sort_unstable();
        late.sort_unstable();
        let (early, late) = (early[10], late[10]);
        eprintln!(
            "PREFIX-DELTA compacted={compacted} rebuilt={rebuilt}: bound bytes early={early} late={late}"
        );
        assert!(early > 0, "the PostgreSQL publication executed");
        assert!(
            late * 100 <= early * 105,
            "the owner's bound frontier must stay flat"
        );
    }
}

/// PREFIX-READER: a carried forest's obsolete links live exactly as long as
/// an admitted reader, even across carry, prefix reset and last-reader release.
#[tokio::test]
async fn held_prompt_links_survive_carries_until_the_last_reader_settles() {
    use lash_core_store::prompt_sections::OwnerPromptState;
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptWrite};

    use lash_protocol_types::llm::types::{
        ProviderRouteIdentity, RecordedRequestTemplate, RequestItems, RequestSegment,
    };
    let Some(owned) = owned("held-prompt-links").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };

    let mut owner = OwnerPromptState::default();
    let mut items = RequestItems::default();
    let mut held = None;
    let mut all = std::collections::BTreeSet::new();
    for turn in 1..=64 {
        items = items
            .append(&[RequestSegment::item_json(
                &serde_json::json!({"content":format!("turn {turn}")}),
            )
            .unwrap()])
            .unwrap();
        let template = RecordedRequestTemplate::from_item_run(
            route.clone(),
            true,
            None,
            "{\"messages\":[".into(),
            items.clone(),
            "]}".into(),
        )
        .unwrap();
        let (chunked, texts) = owner.chunk(&template);
        all.extend(texts.iter().map(|text| text.hash.clone()));
        let run = TurnId::parse(format!("history-{turn}")).unwrap();
        if turn == 14 {
            held = Some((chunked.clone(), texts.clone(), template, run.clone()));
        }
        let mut tx = store
            .begin(&owned.actor, owned.epoch, u64::MAX)
            .await
            .unwrap();
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: PromptCallKey {
                session: owned.session.clone(),
                call: ModelCallId::Turn {
                    run: run.clone(),
                    ordinal: 1,
                },
            },
            snapshot: None,
            request: serde_json::to_string(&chunked).unwrap(),
            snapshot_texts: Vec::new(),
            request_texts: texts,
            prefix: chunked.prefix.map(|root| root.blob.0),
        }));
        if turn != 14 {
            tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                session: owned.session.clone(),
                run,
                open: None,
            }));
        }
        store.commit(tx, CommitLabel::MODEL_START).await.unwrap();
    }
    let (chunked, texts, exact, run) = held.unwrap();
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
        session: owned.session.clone(),
    }));
    store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
    let hashes: Vec<_> = all.into_iter().collect();
    let found = store.prompt_texts(&hashes).await.unwrap();
    assert_eq!(
        found,
        texts
            .iter()
            .cloned()
            .map(|mut text| {
                text.children.sort();
                text.children.dedup();
                text
            })
            .collect::<Vec<_>>(),
        "only the old reader's exact closure survives"
    );
    assert_eq!(
        chunked
            .assemble(|hash| found
                .iter()
                .find(|text| text.hash == hash.as_str())
                .map(|text| &*text.text))
            .unwrap(),
        exact
    );
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::Settle {
        session: owned.session.clone(),
        run,
        open: None,
    }));
    store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
    assert!(
        store.prompt_texts(&hashes).await.unwrap().is_empty(),
        "last reader releases every old link"
    );
    let (stale, request_texts) = owner.chunk(
        &RecordedRequestTemplate::from_item_run(
            route,
            true,
            None,
            "{\"messages\":[".into(),
            items,
            "]}".into(),
        )
        .unwrap(),
    );
    let missing = stale.prefix.as_ref().unwrap().blob.0.clone();
    assert!(
        request_texts.get(&missing).is_some(),
        "the old owner still has bytes"
    );
    let call = PromptCallKey {
        session: owned.session.clone(),
        call: ModelCallId::Turn {
            run: TurnId::from("retired-prefix"),
            ordinal: 1,
        },
    };
    let mut tx = store
        .begin(&owned.actor, owned.epoch, u64::MAX)
        .await
        .unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::Record {
        call: call.clone(),
        snapshot: None,
        request: serde_json::to_string(&stale).unwrap(),
        snapshot_texts: Vec::new(),
        request_texts,
        prefix: Some(missing.clone()),
    }));
    assert_eq!(
        store
            .commit(tx, CommitLabel::MODEL_START)
            .await
            .unwrap_err(),
        lash_durable::DurableError::Domain(
            lash_durable::domain::DomainRefusal::PromptFragmentMissing { hash: missing }
        )
    );
    assert!(
        store.prompt_snapshot(&call).await.unwrap().is_none(),
        "refusal rolls admission back"
    );
}

/// WAKE-MAINTENANCE: a reaper commits only a deletion, including an
/// expired node with no actors. PostgreSQL's wake feed has no hint table.
#[tokio::test]
async fn a_reaper_commits_only_when_a_node_expired() {
    let database_url = crate::testing::required_database_url();
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url()).await.unwrap();
    let clock = std::sync::Arc::new(lash_core_ids::test_clock::TestClock::new(1_000_000));
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    let spec = |name, ttl_millis| NodeSpec {
        node: NodeId::new(name),
        decodes: vec![FormatSet::new("maintenance")],
        serves: Vec::new(),
        capabilities: Default::default(),
        ttl_millis,
    };
    let reaper = store.register_node(&spec("reaper", 15_000)).await.unwrap();
    let commits = |receipt: &lash_core_ids::operational_metrics::sql::Receipt| {
        receipt.shapes.get("commit").map_or(0, |row| row.statements)
    };
    let (answer, empty) = lash_core_ids::operational_metrics::sql::collect(
        "law.empty_reap",
        "postgres",
        store.reap(&reaper),
    )
    .await;
    assert!(answer.unwrap().is_empty());
    assert_eq!(commits(&empty), 0, "empty reaper committed: {empty:?}");
    let expired_node = store.register_node(&spec("expired", 1)).await.unwrap();
    clock.advance(2);
    let (answer, expired) = lash_core_ids::operational_metrics::sql::collect(
        "law.expired_reap",
        "postgres",
        store.reap(&reaper),
    )
    .await;
    assert!(
        answer.unwrap().is_empty(),
        "the expired node owned no actor"
    );
    assert_eq!(
        commits(&expired),
        1,
        "the expired node deletion commits once"
    );
    assert_eq!(
        store.heartbeat(&expired_node).await.unwrap(),
        lash_durable::HeartbeatOutcome::Reaped,
        "the expired node was durably deleted"
    );
    let (answer, empty) = lash_core_ids::operational_metrics::sql::collect(
        "law.empty_reap_again",
        "postgres",
        store.reap(&reaper),
    )
    .await;
    assert!(answer.unwrap().is_empty());
    assert_eq!(commits(&empty), 0);
}
