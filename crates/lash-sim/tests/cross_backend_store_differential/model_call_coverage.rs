//! FIG-6687 / FIG-6570: coverage needs a reached, nonempty call ledger.
#![allow(clippy::expect_used)]
use super::surface_sweep::SURFACE_RUN_ID;
use super::*;
use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
use lash_core_store::model_calls::{
    ModelCallEnd, ModelCallEndReason, ModelCallFact, ModelCallRecord,
};
use lash_durable::DurableReads as _;
use lash_durable::domain::{
    DomainWrite, ModelCallId, ModelCallPinWrite, ModelCallWrite, PromptCallKey,
};
use lash_durable::{ActorKey, CommitLabel, DurableStore, FormatSet, MailTx, NodeSpec, Release};
use lash_protocol_types::llm::types::{
    AttemptOutcome, AttemptRecord, LlmCallId, LlmCallRecord, LlmUsage, ProtocolPosition,
};

#[derive(Default)]
pub(super) struct ModelLedger {
    pub(super) visited: std::collections::BTreeSet<&'static str>,
    expected: Vec<ModelCallFact>,
}

fn fact(ordinal: u32, answered: bool) -> ModelCallFact {
    let call_id = LlmCallId(format!("coverage-call-{ordinal}"));
    ModelCallFact {
        call: ordinal,
        call_id: call_id.clone(),
        end: if answered {
            ModelCallEnd::Answered
        } else {
            ModelCallEnd::Unanswered {
                reason: ModelCallEndReason::LostWithProcess,
            }
        },
        record: LlmCallRecord {
            call_id,
            label: None,
            replay_drops: Vec::new(),
            attempts: (1..=2)
                .map(|ordinal| AttemptRecord {
                    ordinal,
                    outcome: if answered && ordinal == 2 {
                        AttemptOutcome::Completed
                    } else {
                        AttemptOutcome::Interrupted
                    },
                    protocol_position: if answered && ordinal == 2 {
                        ProtocolPosition::ResponseObserved
                    } else {
                        ProtocolPosition::Unobserved
                    },
                    retry_budget_consumed: false,
                    retry_decision: None,
                    error: None,
                    evidence: None,
                    generation_disposition: None,
                    usage: (answered && ordinal == 2).then(|| LlmUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        ..Default::default()
                    }),
                })
                .collect(),
        },
    }
}

impl ModelLedger {
    pub(super) async fn drive(
        &mut self,
        store: &dyn DurableStore,
        session: &SessionId,
        method: SurfaceMethod,
    ) -> String {
        match method {
            SurfaceMethod::PrepareModelCalls => self.prepare(store, session).await,
            SurfaceMethod::ModelCalls => {
                let rows = store
                    .model_calls(session, &ModelCallId::turn_owner(&SURFACE_RUN_ID.into()))
                    .await
                    .expect("read sealed calls");
                assert_eq!(
                    rows.len(),
                    2,
                    "coverage must reach a nonempty recovered ledger"
                );
                for (row, expected) in rows.iter().zip(&self.expected) {
                    assert_eq!(
                        (&row.call, &row.call_id, &row.end, &row.record),
                        (
                            &expected.call,
                            &expected.call_id,
                            &expected.end,
                            &expected.record
                        )
                    );
                }
                self.visited.insert("model_calls");
                serde_json::to_string(&rows).expect("project complete typed calls")
            }
            SurfaceMethod::ModelCallPins => {
                let rows = store
                    .model_call_pins(session, &ModelCallId::turn_owner(&SURFACE_RUN_ID.into()))
                    .await
                    .expect("read reservations after terminal");
                assert!(rows.is_empty(), "terminal recovery seals every reservation");
                self.visited.insert("model_call_pins");
                format!("{rows:?}")
            }
            _ => unreachable!("only model ledger operations use this driver"),
        }
    }

    async fn prepare(&mut self, store: &dyn DurableStore, session: &SessionId) -> String {
        let actor =
            ActorKey::session(&format!("{session}:call-ledger")).expect("a separate owner actor");
        let formats = FormatSet::new("coverage-call-ledger");
        let mut mail = MailTx::new();
        mail.create_actor(actor.clone(), formats.clone());
        store
            .commit_mail(mail, CommitLabel::new("coverage.create"))
            .await
            .expect("create owner");
        let node = store
            .register_node(&NodeSpec {
                node: lash_durable::NodeId::new(format!("{session}:ledger-node")),
                decodes: vec![formats],
                serves: Vec::new(),
                held: Default::default(),
                capabilities: Default::default(),
                ttl_millis: 60_000,
            })
            .await
            .expect("register writer");
        let epoch = store.claim(&node, 1).await.expect("claim owner")[0].epoch;
        let key = |ordinal| PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: SURFACE_RUN_ID.into(),
                ordinal,
            },
        };
        let owner = ModelCallId::turn_owner(&SURFACE_RUN_ID.into());
        let mut timeline = Vec::new();
        let mut tx = store
            .begin(&actor, epoch)
            .await
            .expect("begin reservations");
        for ordinal in 1..=2 {
            tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Reserve {
                key: key(ordinal),
                call_id: LlmCallId(format!("coverage-call-{ordinal}")),
                attempt: 2,
                enclosing: None,
            }));
        }
        store
            .commit(tx, CommitLabel::new("coverage.reserve"))
            .await
            .expect("reserve attempts");
        let pins = store
            .model_call_pins(session, &owner)
            .await
            .expect("read actual reservations");
        assert_eq!(pins.len(), 2);
        for (pin, ordinal) in pins.iter().zip(1..=2) {
            assert_eq!(pin.key, key(ordinal));
            assert_eq!(pin.call_id, LlmCallId(format!("coverage-call-{ordinal}")));
            assert_eq!(pin.attempt, 2);
            assert!(!pin.answered);
            assert!(pin.enclosing.is_none());
        }
        timeline.push(format!("reserved={pins:?}"));
        let mut tx = store
            .begin(&actor, epoch)
            .await
            .expect("begin accepted tail");
        tx.write(DomainWrite::ModelCallPin(ModelCallPinWrite::Answered {
            key: key(1),
        }));
        store
            .commit(tx, CommitLabel::new("coverage.answer"))
            .await
            .expect("accept answer before tail seals");
        let pins = store
            .model_call_pins(session, &owner)
            .await
            .expect("read unsealed accepted tail");
        assert!(pins[0].answered && !pins[1].answered);
        assert!(
            store
                .model_calls(session, &owner)
                .await
                .expect("read unsealed ledger")
                .is_empty()
        );
        timeline.push(format!("unsealed={pins:?}"));
        // Recover the lost unanswered call through the real reservation fold;
        // compare it with a separately authored literal fact, including None usage.
        let recovered = pins[1].unobserved();
        assert_eq!(recovered, fact(2, false));
        self.expected = vec![fact(1, true), fact(2, false)];
        let mut graph = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut tx = store.begin(&actor, epoch).await.expect("begin recovery");
        for (ordinal, fact) in [(1, &self.expected[0]), (2, &recovered)] {
            tx.write(DomainWrite::ModelCall(ModelCallWrite {
                key: key(ordinal),
                call_id: fact.call_id.clone(),
                record: fact.stage(&mut graph).expect("stage call fact"),
            }));
        }
        tx.stage(graph.into_nodes().into_values());
        tx.give_up(Release::Terminal);
        store
            .commit(tx, CommitLabel::new("coverage.recover"))
            .await
            .expect("seal recovered calls and terminal");
        assert_eq!(
            store
                .actor(&actor)
                .await
                .expect("read owner")
                .expect("owner persists")
                .state,
            lash_durable::ActorState::Terminal
        );
        store.release_node(&node).await.expect("release writer");
        timeline.join("\n")
    }
}

/// Reuse the actual sweep's operations, so deleting a scheduled reader loses
/// runtime visitation even though its unused match arm still exists.
pub(super) async fn reached_model_methods() -> std::collections::BTreeSet<&'static str> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open coverage store");
    let session = SessionId::fixture("coverage-ledger");
    let mut ledger = ModelLedger::default();
    for operation in surface_sweep::surface_sweep_case().operations {
        if let StoreOperation::DriveSurface { method } = operation
            && matches!(
                method,
                SurfaceMethod::PrepareModelCalls
                    | SurfaceMethod::ModelCalls
                    | SurfaceMethod::ModelCallPins
                    | SurfaceMethod::DurableRead(_)
            )
        {
            let before = sqlite_residue_digest(Path::new(&stores.database_uri()), &session);
            if let SurfaceMethod::DurableRead(method) = method {
                durable_read_coverage::drive(
                    &stores.durable_store(),
                    &session,
                    method,
                    &mut ledger.visited,
                )
                .await;
            } else {
                ledger
                    .drive(&stores.durable_store(), &session, method)
                    .await;
            }
            if !matches!(method, SurfaceMethod::PrepareModelCalls) {
                let after = sqlite_residue_digest(Path::new(&stores.database_uri()), &session);
                assert!(
                    before.changed_tables(&after).is_empty(),
                    "a coverage read changed durable residue"
                );
            }
        }
    }
    // A new adapter over the same store must read full facts after the writer
    // and its terminal commit, without its in-process ledger state.
    let peer = stores
        .reopen_with_clock(Arc::new(DifferentialClock))
        .await
        .expect("reopen call ledger");
    let rows: Vec<ModelCallRecord> = peer
        .durable_store()
        .model_calls(&session, &ModelCallId::turn_owner(&SURFACE_RUN_ID.into()))
        .await
        .expect("cold peer reads calls");
    assert_eq!(rows.len(), 2);
    for (row, expected) in rows.iter().zip(&ledger.expected) {
        assert_eq!(
            (&row.call, &row.call_id, &row.end, &row.record),
            (
                &expected.call,
                &expected.call_id,
                &expected.end,
                &expected.record
            )
        );
    }
    ledger.visited
}
