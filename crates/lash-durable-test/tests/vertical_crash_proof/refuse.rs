//! D-PARKREFUSE: refuse unreadable same-build state before redelivery.
//!
//! These laws replace the old-image inputs of FIG-5592, FIG-5601,
//! FIG-5613 and FIG-5716 with a park recorded by the running build.
use super::*;
use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, MAX_ROOT_BYTES,
    NodeKind,
    bodies::{ExecRoot, ExecStatus, PhaseRoot},
    check_envelope,
    node::{direct_logical, frame_direct},
};

#[derive(Clone, Copy)]
enum Damage {
    Driver,
    Continuation,
    KernelVersion,
    Checkpoint,
}

/// Edit only the protocol's parked driver payload, retaining its stamp and
/// every other field the same-build checkpoint wrote.
fn damage_driver(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(state) = map
                .get_mut("driver_state")
                .and_then(|state| state.pointer_mut("/payload/state"))
                .and_then(serde_json::Value::as_object_mut)
            {
                state.insert("left_by_another_build".to_owned(), true.into());
                return true;
            }
            map.values_mut().any(damage_driver)
        }
        serde_json::Value::Array(values) => values.iter_mut().any(damage_driver),
        _ => false,
    }
}

/// Rewrite the actual stored graph after closing the first node. The
/// existing fenced test-writer seam publishes new bytes at the turn/cell's
/// own root; it never constructs a runtime or drives the turn. Envelopes,
/// digests and edges stay valid so restore reaches the semantic refusal.
async fn damage(database: &dyn DurableStore, damage: Damage) {
    use lash_core_store::checkpoint_graph::CommitId;
    use lash_durable::domain::{
        DomainWrite, ExecRootWrite, PhaseAdvance, RootOwner, TurnWrite, UnfinishedPhase,
    };

    let snapshot = database.actor(&actor()).await.unwrap().unwrap();
    // A new boot of the stopped node reaps its old lease. This writer is
    // never served: it only acquires the now-unowned row for corruption.
    let writer = database
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("a"),
            decodes: vec![snapshot.formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let snapshot = database.actor(&actor()).await.unwrap().unwrap();
    let epoch = database
        .acquire_idle_for_testing(&writer, &actor(), snapshot.epoch)
        .await
        .unwrap();
    let mut tx = database.begin(&actor(), epoch).await.unwrap();
    let limits = DecodeLimits::within_total(u64::MAX);
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let (graph, old, root, write) = match damage {
        Damage::Driver | Damage::Checkpoint => {
            let row = database
                .turn(&session())
                .await
                .unwrap()
                .expect("the parked turn exists");
            let UnfinishedPhase::Tools {
                run: tool_run,
                checkpoint,
            } = row.phase
            else {
                panic!("the turn did not park on its cell: {:?}", row.phase);
            };
            let graph = database
                .hydrate(
                    RootOwner::TurnPhase {
                        session: session(),
                        run: run(),
                    },
                    Some(checkpoint),
                    limits,
                )
                .await
                .unwrap();
            let mut body: PhaseRoot = GraphDecoder::new(&graph, limits)
                .decode(checkpoint, NodeKind::PHASE_ROOT)
                .unwrap();
            let mut value: serde_json::Value =
                serde_json::from_str(&body.checkpoint).expect("the same-build checkpoint is JSON");
            match damage {
                Damage::Driver => assert!(
                    damage_driver(&mut value),
                    "the turn parked its driver state"
                ),
                Damage::Checkpoint => {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("left_by_another_build".to_owned(), true.into());
                }
                _ => unreachable!(),
            }
            body.checkpoint = value.to_string();
            let root = staging.encode_value(NodeKind::PHASE_ROOT, &body).unwrap();
            let write = DomainWrite::Turn(TurnWrite::Advance {
                session: session(),
                run: run(),
                phase: PhaseAdvance::Tools {
                    run: tool_run,
                    checkpoint: root,
                },
                iteration: row.iteration,
            });
            (graph, checkpoint, root, write)
        }
        Damage::Continuation | Damage::KernelVersion => {
            let mut cells = database.cell_roots(&session(), &run()).await.unwrap();
            assert_eq!(cells.len(), 1, "the turn parked exactly one cell");
            let row = cells.pop().unwrap();
            let graph = database
                .hydrate(RootOwner::Exec(row.exec.clone()), Some(row.root), limits)
                .await
                .unwrap();
            let mut body: ExecRoot = GraphDecoder::new(&graph, limits)
                .decode(row.root, NodeKind::EXEC_ROOT)
                .unwrap();
            assert_eq!(
                body.status,
                ExecStatus::Parked,
                "the cell has a parked kernel run"
            );
            match damage {
                Damage::Continuation => body.format = u32::MAX,
                Damage::KernelVersion => {
                    let page = graph
                        .nodes
                        .get(&body.state)
                        .expect("the parked checkpoint root exists");
                    let envelope = check_envelope(&page.bytes, body.state, &limits).unwrap();
                    let logical = direct_logical(&envelope, body.state).unwrap();
                    let changed = lash_kernel_state::testkit::checkpoint_with_kernel(
                        &logical,
                        body.format + 1,
                    )
                    .expect("the same-build kernel checkpoint decodes");
                    let changed = frame_direct(NodeKind::CHECKPOINT_ROOT, &changed, MAX_ROOT_BYTES)
                        .expect("the changed checkpoint frames");
                    body.state = changed.id;
                    staging.adopt(changed).unwrap();
                }
                _ => unreachable!(),
            }
            let root = staging.encode_value(NodeKind::EXEC_ROOT, &body).unwrap();
            let write = DomainWrite::ExecRoot(ExecRootWrite::Put {
                exec: row.exec,
                expected: Some(row.rev),
                expected_root: Some(row.root),
                root,
                commit_id: CommitId::mint(),
            });
            (graph, row.root, root, write)
        }
    };
    assert_ne!(root, old, "the stored root must actually change");
    for node in graph.nodes.keys() {
        tx.require(*node);
    }
    tx.stage_all(staging);
    tx.write(write);
    tx.give_up(lash_durable::Release::Ready);
    database
        .commit(tx, CommitLabel::new("law.corrupt_parked_state"))
        .await
        .expect("the corrupted stored bytes commit");
    database
        .release_node(&writer)
        .await
        .expect("the corruption writer stops");
}

struct Refused {
    resumed: V0,
    database: Arc<dyn DurableStore>,
    nodes: SimNodes,
    clock: Arc<SimClock>,
    _dir: tempfile::TempDir,
}

impl Refused {
    async fn park_reason(&self) -> Option<lash::SessionParkReason> {
        self.resumed
            .core()
            .session(session())
            .durable()
            .await
            .unwrap()
            .park_reason()
            .await
            .unwrap()
    }

    fn no_redelivery(&self) {
        let counts = self.resumed.tripwire.counts();
        assert!(
            counts.vm_programs.is_empty(),
            "no cell program is entered: {counts:?}"
        );
        assert!(
            counts.bodies.is_empty(),
            "no operation body is entered: {counts:?}"
        );
        assert!(self.resumed.world.writes().is_empty(), "no external writes");
        assert!(
            self.resumed.seen.lock_recover().requests.is_empty(),
            "zero model calls"
        );
    }

    async fn assert_refused(&self) {
        self.no_redelivery();
        let labels: Vec<_> = self
            .nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.kind == WriteKind::Actor && write.committed())
            .map(|write| write.point.label)
            .collect();
        assert_eq!(
            labels,
            [CommitLabel::SESSION_RELEASE],
            "restore commits only its refusal, before any redelivery:\n{}",
            self.nodes.script().rendered_trace()
        );
        assert_eq!(
            self.database.actor(&actor()).await.unwrap().unwrap().state,
            ActorState::Parked
        );
        assert_eq!(
            self.database.turn(&session()).await.unwrap().unwrap().run,
            run()
        );
        assert!(
            self.database
                .turn_end(&session(), &run())
                .await
                .unwrap()
                .is_none()
        );
    }

    async fn stop(self) {
        self.nodes.kill("b");
        self.nodes.quiesce().await;
        self.resumed
            .core()
            .shutdown()
            .await
            .expect("the fresh core stops");
    }
}

async fn refused(damage_kind: Damage) -> Refused {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lash.db");
    let clock = SimClock::new();
    {
        let first = V0::new(Protocol::Code, Dialect::SqliteFile, None);
        let stores = sim::file(&path, Arc::clone(&clock)).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        *first.backend.lock_recover() = Some(shipped_backend(Arc::new(stores)));
        *first.clock.lock_recover() = Some(Arc::clone(&clock));
        let script = Script::new();
        script.cut_on("a", CommitLabel::ROUND_OUTCOME, 1, Fault::CommitThenAbort);
        let before = SimNodes::new(
            Arc::clone(&database),
            Arc::clone(&clock),
            script,
            first.config(),
            first.activation(),
        );
        first
            .send()
            .await
            .expect("the input is sent through the production engine");
        before.start("a");
        before.quiesce().await;
        let horizon = clock.logical_ms() + 600_000;
        while before.script().cuts().is_empty() {
            assert!(clock.logical_ms() < horizon, "the cell never parked");
            let step = before.step().await;
            assert!(
                step.is_some() || !before.script().cuts().is_empty(),
                "the cell stalled before its operation's outcome committed:\n{}",
                before.script().rendered_trace()
            );
        }
        before.kill("a");
        before.quiesce().await;
        assert_eq!(before.life("a"), Life::Dead);
        assert_eq!(
            first.world.writes().len(),
            1,
            "the same-build cell reached its operation"
        );
        assert!(!first.done(&before).await, "the turn is unfinished");
        first
            .core()
            .shutdown()
            .await
            .expect("the recording core stops");
    }
    {
        let stores = sim::file(&path, Arc::clone(&clock)).await;
        damage(&stores.durable_store(), damage_kind).await;
    }
    // Open the actual SQLite file again, with fresh stores, core, probe,
    // model and external-world observations. No live activation is reused.
    let resumed = V0::new(Protocol::Code, Dialect::SqliteFile, None);
    let stores = sim::file(&path, Arc::clone(&clock)).await;
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    *resumed.backend.lock_recover() = Some(shipped_backend(Arc::new(stores)));
    *resumed.clock.lock_recover() = Some(Arc::clone(&clock));
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        resumed.config(),
        resumed.activation(),
    );
    nodes.start("b");
    let horizon = clock.logical_ms() + 600_000;
    loop {
        if matches!(
            database.actor(&actor()).await.unwrap().unwrap().state,
            ActorState::Parked | ActorState::Idle
        ) {
            break;
        }
        assert!(
            clock.logical_ms() < horizon,
            "the restored session neither parked nor ended"
        );
        assert!(
            nodes.step().await.is_some(),
            "the restored session stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    Refused {
        resumed,
        database,
        nodes,
        clock,
        _dir: dir,
    }
}

/// D-PARKREFUSE / FIG-5592: unknown driver state refuses at restore,
/// with no cell, operation, model or turn commit redelivered.
#[tokio::test]
async fn a_parked_driver_state_this_build_cannot_decode_is_refused_at_restore_before_any_redelivery()
 {
    let law = refused(Damage::Driver).await;
    let park = law.park_reason().await;
    assert!(
        matches!(park, Some(lash::SessionParkReason::UndecodableState {
        state: lash::ParkedTurnState::DriverState { ref driver }, ref message,
    }) if driver == lash::rlm::RLM_PROTOCOL_PLUGIN_ID && message.contains("left_by_another_build")),
        "{park:?}"
    );
    law.assert_refused().await;
    law.stop().await;
}

/// D-PARKREFUSE / FIG-5613: the parent's restore refuses the parked cell's
/// unreadable kernel version before redelivering its work to the worker.
#[tokio::test]
async fn a_parked_cell_continuation_this_build_cannot_decode_is_refused_at_restore_before_any_model_call()
 {
    cell_refusal(Damage::Continuation, "outside read range").await;
}

/// FIG-5716: checkpoint bytes cannot state a kernel version other than
/// the execution root's seal, even when both envelopes are well formed.
#[tokio::test]
async fn a_parked_run_whose_bytes_state_another_kernel_version_than_its_seal_is_refused_at_restore()
{
    cell_refusal(
        Damage::KernelVersion,
        "its checkpoint root states kernel version",
    )
    .await;
}

async fn cell_refusal(damage: Damage, account: &str) {
    let law = refused(damage).await;
    let park = law.park_reason().await;
    assert!(
        matches!(park, Some(lash::SessionParkReason::UndecodableState {
        state: lash::ParkedTurnState::CellSnapshot, ref message,
    }) if message.contains(account)),
        "{park:?}"
    );
    law.assert_refused().await;
    law.stop().await;
}

/// D-PARKREFUSE / FIG-5601: an undecodable checkpoint refuses at restore,
/// but cancelling that parked turn still ends it without decoding or work.
#[tokio::test]
async fn cancelling_a_turn_whose_checkpoint_this_build_cannot_decode_ends_the_turn() {
    let law = refused(Damage::Checkpoint).await;
    let park = law.park_reason().await;
    assert!(
        matches!(
            park,
            Some(lash::SessionParkReason::UndecodableState {
                state: lash::ParkedTurnState::Checkpoint,
                ..
            })
        ),
        "{park:?}"
    );
    law.assert_refused().await;
    let answer = lash_core::runtime::durable::session::request_turn_cancel(
        &law.resumed.backend(),
        lash_durable::domain::TurnCancelRequest {
            session: session(),
            run: run(),
            request_id: "stop-the-refused-turn".to_owned(),
            origin: None,
            reason: None,
            undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_core::TurnCancelMode::Immediate,
        },
    )
    .await
    .expect("the cancel request commits");
    assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);
    law.resumed
        .run_until_done(&law.nodes, &law.clock, "the cancelled turn")
        .await;
    let end = law
        .database
        .turn_end(&session(), &run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        end.kind(),
        lash_core_store::store::RunTerminalKind::Cancelled
    );
    assert!(law.database.turn(&session()).await.unwrap().is_none());
    assert_ne!(
        law.database.actor(&actor()).await.unwrap().unwrap().state,
        ActorState::Parked
    );
    law.no_redelivery();
    law.stop().await;
}
