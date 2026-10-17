//! SETTLED-INGRESS-COMPACTION (FIG-6607): a settled input's and a settled
//! command's bodies follow the session's revision `Retention`, while their
//! identities, digests and receipts stay.

use super::*;
use lash_core_execution::store::{QueuedWorkStore as _, TurnInputStore as _};
use lash_core_execution::{Retention, SessionCatalogStore as _, Target};
use lash_protocol_types::SessionId;

/// A body large enough that keeping it is a real cost.
fn large(tag: &str) -> String {
    format!("{tag}:{}", "x".repeat(64 * 1024))
}

fn input_draft(session: &SessionId, text: &str) -> lash_core_execution::PendingTurnInputDraft {
    lash_core_execution::PendingTurnInputDraft::new(
        session.clone(),
        lash_core_execution::TurnInputIngress::NextTurn,
        lash_core_execution::TurnInput::text(text),
    )
    .with_source_key("input:large")
}

fn command_draft(session: &SessionId, reason: &str) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session.clone(),
        lash_core_execution::DeliveryPolicy::EarliestSafeBoundary,
        lash_core_execution::runtime::SessionCommand::RefreshToolCatalog {
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
    let state = lash_core_execution::RuntimeSessionState {
        session_id: session.clone(),
        head_revision,
        ..lash_core_execution::RuntimeSessionState::fixture(
            lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
                lash_core_execution::MaxToolCalls::new(1024),
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
                    params![attachment, session],
                )?;
                tx.execute(
                    "INSERT INTO attachment_session_holds
                         (session_id, attachment_id, holder_kind, holder_id)
                     VALUES (?1, ?2, 'input', ?3)",
                    params![session, attachment, input],
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
                [attachment.as_str(), session.as_str()],
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
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("store");
    let session = SessionId::from("settled-ingress");
    store
        .admit_session(&SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core_execution::SessionRelation::Root,
            config: lash_core_execution::PersistedSessionConfig::from_policy(
                &lash_core_execution::SessionPolicy::new(
                    lash_core_execution::TurnBudget::Unbounded,
                    lash_core_execution::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ),
            ),
            head: lash_core_execution::SessionCreationHead::Config,
            pending_observer_intents: Vec::new(),
            owning_process_id: None,
            retention: Retention::HeadOnly,
        })
        .await
        .expect("admit");
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
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            session.clone(),
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("still waiting"),
        ))
        .await
        .expect("enqueue an input no run takes");
    hold_attachments(
        &store,
        &session,
        &[
            (input.input_id.as_str(), "attachment-settled"),
            (unconsumed.input_id.as_str(), "attachment-waiting"),
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
    let mut settlement = lash_core_execution::store::IngressSettlement::new(
        lash_core_execution::TurnId::from("run-1"),
    );
    settlement
        .completed_inputs
        .push(lash_core_execution::TurnInputCompletion {
            session_id: session.clone(),
            data: lash_core_execution::TurnInputCompletionData {
                input_ids: vec![input.input_id.clone()],
                applications: Vec::new(),
            },
        });
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
    applying.applied_commands = Some(lash_core_execution::runtime::QueuedWorkCompletion {
        session_id: session.clone(),
        batch_ids: vec![command.batch_id.clone()],
    });
    let applied = store
        .commit_runtime_state(applying)
        .await
        .expect("the applying commit settles the command");
    assert_eq!(
        stored_bodies(&store, input.input_id.as_str(), command.batch_id.as_str()).await,
        (true, true),
        "a pin keeps the input's revision, and the head keeps the command's"
    );

    store
        .unpin_in_catalog(&session, &Target::Input(input.input_id.clone()))
        .await
        .expect("lift the pin");
    store
        .commit_runtime_state(head_commit(&store, &session).await)
        .await
        .expect("a later head");
    assert_eq!(
        stored_bodies(&store, input.input_id.as_str(), command.batch_id.as_str()).await,
        (false, false),
        "no settled body outlives its released revision"
    );
    assert!(
        !session_edge(&store, &session, "attachment-settled").await,
        "a released body's attachment is no longer the session's"
    );
    assert!(
        session_edge(&store, &session, "attachment-waiting").await,
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
            lash_core_execution::runtime::SubmittedBody::Released
        ),
        "a released body reads released, never a substitute"
    );
    assert_eq!(
        replayed.state.kind(),
        lash_core_execution::runtime::TurnInputStateKind::Completed
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
