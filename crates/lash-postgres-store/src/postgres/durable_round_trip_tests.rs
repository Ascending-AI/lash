//! The round trips the owner's commits pay on PostgreSQL (FIG-5275,
//! FIG-5936), counted on an isolated database: the acceptance of one
//! input, a turn's admission, `model.start`, a round's outcome, a VM park
//! and `turn.commit`.

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
use lash_durable::domain::{
    DomainWrite, ExecKey, ExecRootWrite, ModelPin, Ordinal, OwnerKey, PhaseAdvance, RunFactKind,
    RunFactWrite, RunSeq, SessionMailWrite, SnapshotRev, TurnWrite, UnfinishedPhase,
};
use lash_durable::{
    ActorKey, ActorTx, CommitLabel, DurableReads as _, DurableStore as _, Epoch, FormatSet, MailTx,
    NodeId, NodeSpec, Release,
};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use lash_vocabulary::tokens::TokenWriter;

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
    let checkpoint = staged(tx, NodeKind::TURN_CONTENT, &format!("{{\"call\":{call}}}"));
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
        let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
    admit(&mut tx, &owned, &run, input);
    store
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admit the turn");

    let (mut tx, opened) =
        round_trips(async { store.begin(&owned.actor, owned.epoch).await.expect("open") }).await;
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
        let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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

/// A VM park over an earlier park stays within its round-trip budget.
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
    let first = park(&mut tx, "first park", None);
    store
        .commit(tx, CommitLabel::CELL_SNAPSHOT)
        .await
        .expect("the first park");
    let (_, trips) = round_trips(async {
        let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
        park(&mut tx, "second park", Some((1, first)));
        store
            .commit(tx, CommitLabel::CELL_SNAPSHOT)
            .await
            .expect("the second park")
    })
    .await;
    within("cell.snapshot", trips, VM_PARK_ROUND_TRIPS);
}

/// The most round trips `turn.commit` pays from its open (FIG-5942; 38
/// before it): the open, which reads the head the commit is planned on,
/// and the one call, which guards every fact of that plan.
const TURN_COMMIT_ROUND_TRIPS: u64 = 2;

/// A turn's commit, its session's head commit with the run's terminal and
/// its scope's end, stays within its round-trip budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_commit_stays_within_its_round_trip_budget() {
    turn_commit_round_trips(false).await;
    turn_commit_round_trips(true).await;
}

async fn turn_commit_round_trips(holds_artifacts: bool) {
    use lash_core_store::store::SessionHistoryStore as _;
    use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;
    let Some(owned) = owned("round-trips-turn-commit").await else {
        return;
    };
    let store = owned.storage.durable_store();
    let input = accept(&owned, "turn-commit").await;
    let run = TurnId::from("round-trips-turn");
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
                    Some(journal),
                ),
                1,
            )
            .await
            .expect("the frame waits for the turn to settle");
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
        let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
        if holds_artifacts {
            "turn.commit with execution and gated-frame edges"
        } else {
            "turn.commit without artifact edges"
        },
        trips,
        TURN_COMMIT_ROUND_TRIPS,
    );
    assert_eq!(
        artifacts
            .get_module_artifact("round-trips-module")
            .await
            .expect("read artifact"),
        None,
        "the terminal reclaims the execution and gated frame's last-reader bytes"
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
        let leaf = PromptText {
            hash: lash_core_store::store::BlobRef::for_content(b"leaf").0,
            text: "leaf".into(),
            children: Vec::new(),
        };
        let prefix = PromptText {
            hash: lash_core_store::store::BlobRef::for_content(b"prefix").0,
            text: "prefix".into(),
            children: vec![leaf.hash.clone()],
        };
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
    let answer = staged(&mut tx, NodeKind::TURN_CONTENT, "answer");
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
    tx.set_requires(vec!["tool-component".into()]);
    let record = staged(&mut tx, NodeKind::TURN_CONTENT, "sealed model call");
    tx.write(DomainWrite::ModelCall(ModelCallWrite {
        key: key.clone(),
        call_id: LlmCallId("boundary-call".into()),
        record,
    }));
    let checkpoint = staged(&mut tx, NodeKind::TURN_CONTENT, "tools");
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.expect("open");
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
        .begin(&owned.actor, owned.epoch)
        .await
        .expect("open collision");
    tx.set_requires(vec!["must-not-publish".into()]);
    let record = staged(&mut tx, NodeKind::TURN_CONTENT, "conflicting sealed call");
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
        matches!(error, lash_durable::DurableError::Store(lash_durable::StoreFailure { kind: lash_durable::StoreFailureKind::Corrupt, ref message }) if message.contains("a sealed call cannot be replaced")),
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
        .begin(&owned.actor, owned.epoch)
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
        .begin(&owned.actor, owned.epoch)
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
            .begin(&owned.actor, owned.epoch)
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
        let later_answer = staged(&mut tx, NodeKind::TURN_CONTENT, "later invalid phase");
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
        let text = PromptText {
            hash: lash_core_store::store::BlobRef::for_content(body.as_bytes()).0,
            text: body.into(),
            children: texts
                .last()
                .map(|parent| vec![parent.hash.clone()])
                .unwrap_or_default(),
        };
        texts.push(text.clone());
        let mut tx = store
            .begin(&owned.actor, owned.epoch)
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
        let mut tx = store.begin(&owned.actor, owned.epoch).await.unwrap();
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.unwrap();
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
    let mut tx = store.begin(&owned.actor, owned.epoch).await.unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::Settle {
        session: owned.session,
        run,
        open: None,
    }));
    store.commit(tx, CommitLabel::MODEL_DONE).await.unwrap();
    assert!(
        store.prompt_texts(&hashes).await.unwrap().is_empty(),
        "last reader releases every old link"
    );
}
