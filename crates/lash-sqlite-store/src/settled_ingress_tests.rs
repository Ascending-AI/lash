//! SETTLED-INGRESS-COMPACTION (FIG-6607): a settled input's and a settled
//! command's bodies follow the session's revision `Retention`, while their
//! identities, digests and receipts stay.

use super::*;
use lash_core_store::store::QueuedWorkStore as _;
use lash_core_store::store::Retention;
use lash_core_store::store::SessionCatalogStore as _;
use lash_core_store::store::Target;
use lash_core_store::store::TurnInputStore as _;
use lash_kernel_value::identity::SessionId;

/// A body large enough that keeping it is a real cost.
fn large(tag: &str) -> String {
    format!("{tag}:{}", "x".repeat(64 * 1024))
}

fn input_draft(
    session: &SessionId,
    text: &str,
) -> lash_core_store::turn_input_vocabulary::PendingTurnInputDraft {
    lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
        session.clone(),
        lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
        lash_core_store::turn_input_vocabulary::TurnInput::text(text),
    )
    .with_source_key("input:large")
}

fn command_draft(session: &SessionId, reason: &str) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session.clone(),
        lash_core_store::queued_work_vocabulary::DeliveryPolicy::EarliestSafeBoundary,
        lash_core_store::queued_work_vocabulary::SessionCommand::RefreshToolCatalog {
            reason: reason.to_owned(),
        },
    )
    .with_source_key("command:large")
}

/// A commit over the store's current head.
async fn head_commit(store: &SqliteStore, session: &SessionId) -> RuntimeCommit {
    let head_revision = store
        .load_session_head_meta(session)
        .await
        .expect("head")
        .map_or(0, |meta| meta.head_revision);
    let state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session.clone(),
        head_revision,
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state);
    commit.checkpoint.components.insert(
        format!("settled-ingress/{head_revision}"),
        HydratedCheckpointComponent::changed(format!("revision {head_revision}").into_bytes()),
    );
    commit
}

/// Whether the stored rows still hold the input's and the command's bodies.
async fn stored_bodies(store: &SqliteStore, input: &str, batch: &str) -> (bool, bool) {
    let (input, batch) = (input.to_owned(), batch.to_owned());
    store
        .conn
        .call(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT input_json IS NOT NULL FROM pending_turn_inputs WHERE input_id = ?1",
                    [input.as_str()],
                    |row| row.get::<_, bool>(0),
                )?,
                conn.query_row(
                    "SELECT payload_json IS NOT NULL FROM queued_work_batches WHERE batch_id = ?1",
                    [batch.as_str()],
                    |row| row.get::<_, bool>(0),
                )?,
            ))
        })
        .await
        .expect("read stored bodies")
}

/// Give each input one attachment its admission holds, as an input that
/// carries an attachment is admitted: a session edge and an input hold.
async fn hold_attachments(store: &SqliteStore, session: &SessionId, inputs: &[(&str, &str)]) {
    let session = session.to_string();
    let inputs: Vec<(String, String)> = inputs
        .iter()
        .map(|(input, attachment)| ((*input).to_owned(), (*attachment).to_owned()))
        .collect();
    store
        .conn
        .write(move |tx| {
            for (input, attachment) in &inputs {
                tx.execute(
                    "INSERT INTO attachment_referrer_edges (attachment_id, referrer_kind, referrer_id)
                     VALUES (?1, 'session', ?2)",
                    params![crate::codec::SqlHash(attachment), session],
                )?;
                tx.execute(
                    "INSERT INTO attachment_session_holds
                         (session_id, attachment_id, holder_kind, holder_id)
                     VALUES (?1, ?2, 'input', ?3)",
                    params![session, crate::codec::SqlHash(attachment), input],
                )?;
            }
            Ok(())
        })
        .await
        .expect("hold the inputs' attachments");
}

/// Whether the session's edge to `attachment` remains.
async fn session_edge(store: &SqliteStore, session: &SessionId, attachment: &str) -> bool {
    let (session, attachment) = (session.to_string(), attachment.to_owned());
    store
        .conn
        .call(move |conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM attachment_referrer_edges
                     WHERE attachment_id = ?1 AND referrer_kind = 'session' AND referrer_id = ?2)",
                params![crate::codec::SqlHash(&attachment), session.as_str()],
                |row| row.get::<_, bool>(0),
            )
        })
        .await
        .expect("read the session edge")
}

/// SETTLED-INGRESS-COMPACTION under `HeadOnly`: once the revisions that
/// settled a large input and a large command are no longer retained, neither
/// body is reachable, yet an identical retry keeps its receipt, a changed
/// retry still refuses, the command's original answer still reads and an
/// unconsumed input keeps its body. A pin keeps the input's body until it
/// is lifted. The released body's attachment hold ends with it, so the
/// session lets that attachment go, while the unconsumed input's stays held
/// (FIG-6611).
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one law, read top to bottom")]
async fn settled_ingress_bodies_follow_head_only_retention() {
    let root = tempfile::tempdir().expect("store root");
    let path = root.path().join("ingress.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("store");
    let session = SessionId::from("settled-ingress");
    for admitted_session in [session.clone(), SessionId::from("other-owner")] {
        store
            .admit_session(&SessionStoreCreateRequest {
                session_id: admitted_session,
                relation: lash_core_store::session_identity::SessionRelation::Root,
                config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
                    &lash_core_store::session_policy::SessionPolicy::new(
                        lash_protocol_types::session_model::TurnBudget::Unbounded,
                        lash_protocol_types::session_model::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    ),
                ),
                head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
                pending_observer_intents: Vec::new(),
                owning_process_id: None,
                retention: Retention::HeadOnly,
            })
            .await
            .expect("admit");
    }
    store
        .commit_runtime_state(head_commit(&store, &session).await)
        .await
        .expect("first head");

    let input_text = large("input");
    let input = store
        .enqueue_pending_turn_input(input_draft(&session, &input_text))
        .await
        .expect("enqueue the large input");
    let unconsumed = store
        .enqueue_pending_turn_input(
            lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                session.clone(),
                lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                lash_core_store::turn_input_vocabulary::TurnInput::text("still waiting"),
            ),
        )
        .await
        .expect("enqueue an input no run takes");
    hold_attachments(
        &store,
        &session,
        &[
            (
                input.input_id.as_str(),
                "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
            ),
            (
                unconsumed.input_id.as_str(),
                "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
            ),
        ],
    )
    .await;

    // Run `run-1` takes the input, and its commit delivers it.
    let (session_text, input_id) = (session.to_string(), input.input_id.to_string());
    store
        .conn
        .write(move |tx| {
            tx.execute(
                "INSERT INTO session_runs (session_id, run) VALUES (?1, 'run-1')",
                [session_text.as_str()],
            )?;
            tx.execute(
                "UPDATE pending_turn_inputs SET admitted_run = 'run-1', admitted_by = 'admit'
                 WHERE input_id = ?1",
                [input_id.as_str()],
            )?;
            Ok(())
        })
        .await
        .expect("bind the input to its run");
    store
        .pin_in_catalog(&session, &Target::Input(input.input_id.clone()))
        .await
        .expect("pin the input");
    let mut delivering = head_commit(&store, &session).await;
    let mut settlement = lash_core_store::store::IngressSettlement::new(
        lash_vocabulary::identity::TurnId::from("run-1"),
    );
    settlement.completed_inputs.push(
        lash_core_store::turn_input_vocabulary::TurnInputCompletion {
            session_id: session.clone(),
            data: lash_core_store::turn_input_vocabulary::TurnInputCompletionData {
                input_ids: vec![input.input_id.clone()],
                applications: Vec::new(),
            },
        },
    );
    delivering.ingress = Some(settlement);
    let delivered = store
        .commit_runtime_state(delivering)
        .await
        .expect("the run's commit delivers the input");
    let (session_text, revision) = (session.to_string(), delivered.head_revision);
    store
        .conn
        .write(move |tx| {
            tx.execute(
                "UPDATE session_runs SET terminal_kind = 'answered',
                     terminal_cause_json = '{\"cause\":\"commands_applied\"}',
                     terminal_at_ms = 0, terminal_head_revision = ?2
                 WHERE session_id = ?1 AND run = 'run-1'",
                params![
                    session_text.as_str(),
                    i64::try_from(revision).expect("revision")
                ],
            )?;
            Ok(())
        })
        .await
        .expect("the run ends on its commit");

    // The command lane applies a large command.
    let command_reason = large("command");
    let command = store
        .enqueue_queued_work(command_draft(&session, &command_reason))
        .await
        .expect("enqueue the large command");
    store
        .open_session_command_run(&session)
        .await
        .expect("open the command run");
    let mut applying = head_commit(&store, &session).await;
    applying.applied_commands = Some(
        lash_core_store::queued_work_vocabulary::QueuedWorkCompletion {
            session_id: session.clone(),
            batch_ids: vec![command.batch_id.clone()],
        },
    );
    let applied = store
        .commit_runtime_state(applying)
        .await
        .expect("the applying commit settles the command");
    assert_eq!(
        stored_bodies(&store, input.input_id.as_str(), command.batch_id.as_str()).await,
        (true, true),
        "a pin keeps the input's revision, and the head keeps the command's"
    );

    retained_neighbor(&store).await;
    // FIG-6607: cross the 128-row boundary in both material classes. These
    // rows share the original run/receipt but each has its own replay key.
    let (input_id, batch_id) = (input.input_id.to_string(), command.batch_id.to_string());
    store.conn.write(move |tx| {
        for index in 1..=128 {
            tx.execute("INSERT INTO attachment_session_holds VALUES ('settled-ingress', X'a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1', 'input', 'input-extra-' || ?1)", [index])?;
            tx.execute("INSERT INTO pending_turn_inputs
                (origin_json, enqueue_seq, input_id, session_id, source_key, ingress_json, state,
                 input_json, submission_digest, enqueued_at_ms, admitted_run, terminal_at_ms)
                SELECT '\"submitted\"', 1000 + ?1, 'input-extra-' || ?1, session_id, 'input:extra-' || ?1,
                 ingress_json, state, input_json, submission_digest, enqueued_at_ms,
                 admitted_run, terminal_at_ms FROM pending_turn_inputs WHERE input_id = ?2",
                params![index, input_id])?;
            tx.execute("INSERT INTO queued_work_batches
                (enqueue_seq, batch_id, session_id, source_key, delivery_policy, authority_json,
                 enqueued_at_ms, submission_digest, payload_json, settled_operation_key,
                 terminal_cause, terminal_at_ms)
                SELECT 1000 + ?1, 'batch-extra-' || ?1, session_id, 'command:extra-' || ?1,
                 delivery_policy, authority_json, enqueued_at_ms, submission_digest, payload_json,
                 settled_operation_key, terminal_cause, terminal_at_ms
                 FROM queued_work_batches WHERE batch_id = ?2", params![index, batch_id])?;
        }
        Ok(())
    }).await.expect("seed settled backlog and another owner");
    assert_eq!(backlog_bodies(&store, &session).await, (129, 129));
    let evidence = replay_evidence(&store, &session).await;

    store
        .unpin_in_catalog(&session, &Target::Input(input.input_id.clone()))
        .await
        .expect("lift the pin");
    assert_eq!(
        backlog_bodies(&store, &session).await,
        (1, 129),
        "unpin drains exactly 128 inputs; the current command revision is retained"
    );
    for index in 1..=128 {
        let replay = store
            .enqueue_pending_turn_input(
                input_draft(&session, &input_text).with_source_key(format!("input:extra-{index}")),
            )
            .await
            .expect("replay input");
        assert_eq!(replay.input_id.as_str(), format!("input-extra-{index}"));
        assert!(
            replay.input.retained().is_none(),
            "expiry precedes physical drainage"
        );
    }
    store
        .commit_runtime_state(head_commit(&store, &session).await)
        .await
        .expect("a later head");
    assert_eq!(
        backlog_bodies(&store, &session).await,
        (0, 1),
        "one command page releases exactly 128 payloads"
    );
    for index in 1..=128 {
        let QueuedWorkEnqueueOutcome::Existing(replay) = store
            .enqueue_queued_work_with_outcome(
                command_draft(&session, &command_reason)
                    .with_source_key(format!("command:extra-{index}")),
            )
            .await
            .expect("replay command")
        else {
            panic!("the receipt already exists");
        };
        assert_eq!(replay.batch_id.as_str(), format!("batch-extra-{index}"));
        assert!(
            replay.payload.retained().is_none(),
            "even the undrained payload is released"
        );
        let answer = store
            .queued_work_batch_completion(&session, replay.batch_id.as_str())
            .await
            .expect("read receipt")
            .expect("original receipt");
        assert_eq!(answer.head_revision, applied.head_revision);
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            serde_json::to_value(&applied).unwrap()
        );
    }
    assert_eq!(
        replay_evidence(&store, &session).await,
        evidence,
        "release preserves every original digest and receipt binding"
    );
    // A queued command owns the next head. Admit this pending control
    // after the final owner commit; maintenance must preserve its payload.
    let waiting_command = store
        .enqueue_queued_work(
            command_draft(&session, "not applied").with_source_key("command:waiting"),
        )
        .await
        .expect("pending command control");
    let reopened = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("cold reopen");
    assert_eq!(backlog_bodies(&reopened, &session).await, (0, 1));
    reopened
        .gc_unreachable()
        .await
        .expect("drain without another owner commit");
    assert_eq!(backlog_bodies(&reopened, &session).await, (0, 0));
    let QueuedWorkEnqueueOutcome::Existing(waiting) = reopened
        .enqueue_queued_work_with_outcome(
            command_draft(&session, "not applied").with_source_key("command:waiting"),
        )
        .await
        .expect("pending command replay")
    else {
        panic!("the pending command exists");
    };
    assert_eq!(waiting.batch_id, waiting_command.batch_id);
    assert!(waiting.payload.retained().is_some());
    assert!(waiting.terminal.is_none());
    assert_eq!(replay_evidence(&reopened, &session).await, evidence);
    assert!(
        session_edge(
            &reopened,
            &SessionId::from("other-owner"),
            "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"
        )
        .await
    );
    let other = SessionId::from("other-owner");
    let kept_input = reopened
        .enqueue_pending_turn_input(
            input_draft(&other, "retained other input").with_source_key("input:control"),
        )
        .await
        .expect("pinned other-owner input replay");
    assert!(kept_input.input.retained().is_some());
    let QueuedWorkEnqueueOutcome::Existing(kept_command) = reopened
        .enqueue_queued_work_with_outcome(
            command_draft(&other, "retained other command").with_source_key("command:control"),
        )
        .await
        .expect("other-owner command replay")
    else {
        panic!("the command exists");
    };
    assert!(kept_command.payload.retained().is_some());
    assert!(kept_command.terminal.is_some());

    assert_eq!(
        stored_bodies(&store, input.input_id.as_str(), command.batch_id.as_str()).await,
        (false, false),
        "no settled body outlives its released revision"
    );
    assert!(
        !session_edge(
            &store,
            &session,
            "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"
        )
        .await,
        "a released body's attachment is no longer the session's"
    );
    assert!(
        session_edge(
            &store,
            &session,
            "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2"
        )
        .await,
        "an unconsumed input keeps its attachment"
    );

    // An identical retry keeps its receipt; changed content still refuses.
    let replayed = store
        .enqueue_pending_turn_input(input_draft(&session, &input_text))
        .await
        .expect("an identical input retry is answered");
    assert_eq!(replayed.input_id, input.input_id);
    assert!(
        matches!(
            replayed.input,
            lash_core_store::turn_input_vocabulary::SubmittedBody::Released
        ),
        "a released body reads released, never a substitute"
    );
    assert_eq!(
        replayed.state.kind(),
        lash_core_store::turn_input_vocabulary::TurnInputStateKind::Completed
    );
    assert!(matches!(
        store
            .enqueue_pending_turn_input(input_draft(&session, "changed"))
            .await,
        Err(StoreError::PendingTurnInputSourceKeyConflict { .. })
    ));
    let QueuedWorkEnqueueOutcome::Existing(existing) = store
        .enqueue_queued_work_with_outcome(command_draft(&session, &command_reason))
        .await
        .expect("an identical command retry is answered")
    else {
        panic!("an identical command retry answers the stored batch");
    };
    assert_eq!(existing.batch_id, command.batch_id);
    assert!(existing.payload.retained().is_none());
    assert!(existing.terminal.is_some());
    assert!(matches!(
        store
            .enqueue_queued_work(command_draft(&session, "changed"))
            .await,
        Err(StoreError::QueuedWorkSourceKeyConflict { .. })
    ));
    let answer = store
        .queued_work_batch_completion(&session, command.batch_id.as_str())
        .await
        .expect("read the command's answer")
        .expect("the original answer is kept");
    assert_eq!(answer.head_revision, applied.head_revision);

    let pending = store
        .list_pending_turn_inputs(&session)
        .await
        .expect("list pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input.input_id, unconsumed.input_id);
    assert!(
        pending[0].input.input.retained().is_some(),
        "an unconsumed input keeps its body"
    );
}

/// Physical counts deliberately bypass the logical Released projection.
async fn backlog_bodies(store: &SqliteStore, session: &SessionId) -> (u64, u64) {
    let session = session.to_string();
    store
        .conn
        .call(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT COUNT(*) FROM pending_turn_inputs WHERE session_id = ?1
            AND state = 'completed' AND input_json IS NOT NULL",
                    [&session],
                    |row| row.get(0),
                )?,
                conn.query_row(
                    "SELECT COUNT(*) FROM queued_work_batches WHERE session_id = ?1
            AND terminal_cause = 'applied' AND payload_json IS NOT NULL",
                    [&session],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .expect("physical backlog counts")
}

async fn replay_evidence(
    store: &SqliteStore,
    session: &SessionId,
) -> Vec<(String, String, String)> {
    let session = session.to_string();
    store
        .conn
        .call(move |conn| {
            conn.prepare(
                "SELECT input_id, submission_digest, admitted_run
        FROM pending_turn_inputs WHERE session_id = ?1 AND state = 'completed'
        UNION ALL SELECT batch_id, submission_digest, settled_operation_key
        FROM queued_work_batches WHERE session_id = ?1 AND terminal_cause = 'applied'
        ORDER BY 1",
            )?
            .query_map([session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect()
        })
        .await
        .expect("read replay identities")
}

/// A second owner retains a settled input by its own pin and an applied
/// command by its current revision. It shares the expired owner's attachment.
async fn retained_neighbor(store: &SqliteStore) {
    let session = SessionId::from("other-owner");
    store
        .commit_runtime_state(head_commit(store, &session).await)
        .await
        .expect("neighbor head");
    let input = store
        .enqueue_pending_turn_input(
            input_draft(&session, "retained other input").with_source_key("input:control"),
        )
        .await
        .expect("neighbor input");
    hold_attachments(
        store,
        &session,
        &[(
            input.input_id.as_str(),
            "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
        )],
    )
    .await;
    let id = input.input_id.to_string();
    store
        .conn
        .write(move |tx| {
            tx.execute(
                "INSERT INTO session_runs (session_id, run) VALUES ('other-owner', 'other-run')",
                [],
            )?;
            tx.execute(
                "UPDATE pending_turn_inputs SET admitted_run = 'other-run', admitted_by = 'admit'
            WHERE input_id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .expect("bind neighbor input");
    store
        .pin_in_catalog(&session, &Target::Input(input.input_id.clone()))
        .await
        .expect("neighbor pin");
    let mut delivering = head_commit(store, &session).await;
    let mut settlement = lash_core_store::store::IngressSettlement::new(
        lash_vocabulary::identity::TurnId::from("other-run"),
    );
    settlement.completed_inputs.push(
        lash_core_store::turn_input_vocabulary::TurnInputCompletion {
            session_id: session.clone(),
            data: lash_core_store::turn_input_vocabulary::TurnInputCompletionData {
                input_ids: vec![input.input_id],
                applications: Vec::new(),
            },
        },
    );
    delivering.ingress = Some(settlement);
    let delivered = store
        .commit_runtime_state(delivering)
        .await
        .expect("neighbor input commit");
    store
        .conn
        .write(move |tx| {
            tx.execute(
                "UPDATE session_runs SET terminal_kind = 'answered',
            terminal_cause_json = '{\"cause\":\"commands_applied\"}', terminal_at_ms = 0,
            terminal_head_revision = ?1 WHERE session_id = 'other-owner' AND run = 'other-run'",
                [i64::try_from(delivered.head_revision).expect("revision")],
            )?;
            Ok(())
        })
        .await
        .expect("neighbor terminal");
    let command = store
        .enqueue_queued_work(
            command_draft(&session, "retained other command").with_source_key("command:control"),
        )
        .await
        .expect("neighbor command");
    store
        .open_session_command_run(&session)
        .await
        .expect("neighbor command run");
    let mut applying = head_commit(store, &session).await;
    applying.applied_commands = Some(
        lash_core_store::queued_work_vocabulary::QueuedWorkCompletion {
            session_id: session,
            batch_ids: vec![command.batch_id],
        },
    );
    store
        .commit_runtime_state(applying)
        .await
        .expect("neighbor command commit");
}
