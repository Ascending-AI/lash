//! The session's mailbox on PostgreSQL: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use lash_durable::domain::{
    DomainRefusal, MailBatch, MailBatchKind, MailInput, OperationCancelAnswer,
    OperationCancelRequest, SessionMailWrite, SessionMailbox, queued_input_run,
};
use lash_durable::{ActorKey, DurableInstant, Woken};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::BatchId;
use lash_vocabulary::identity::InputId;
use lash_vocabulary::identity::TurnId;
use sqlx::PgConnection;

use super::steps::{self, Entry, Shape, Step, Ty, Value};
use super::{Committing, SQL, get, sqlx_failure};

lash_store_sql::statements! {
    /// The session-mail statements only PostgreSQL issues: `UNNEST` binds a
    /// run's admitted mail as one list.
    struct PostgresSessionMailStatements @ "durable_session_mail_postgres" {
        /// One snapshot of standing, bound run and all unbound mail.
        mailbox = "SELECT
            (SELECT count(*) FROM session_meta WHERE session_id = ?1),
            (SELECT count(*) FROM deleted_sessions WHERE session_id = ?1),
            (SELECT count(*) FROM session_meta WHERE session_id = ?1 AND closing_intent IS NOT NULL),
            (SELECT admitted_run FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_run IS NOT NULL
                  AND {{nonterminal_turn_input_state(state)}}
             UNION ALL SELECT admitted_run FROM queued_work_batches
                WHERE session_id = ?1 AND admitted_run IS NOT NULL AND terminal_cause IS NULL LIMIT 1) AS bound_run,
            (SELECT COALESCE(json_agg(json_build_array(input_id, enqueue_seq, source_key, run_spec_hash,
                state, ingress_json, trace_cause_json, origin_json) ORDER BY enqueue_seq), '[]'::json)::text
             FROM pending_turn_inputs WHERE session_id = ?1 AND admitted_run IS NULL
               AND {{undelivered_turn_input_state(state)}}) AS inputs,
            (SELECT COALESCE(json_agg(json_build_array(batch_id, enqueue_seq, delivery_policy, payload_json,
                CASE WHEN cancel_request_json IS NULL THEN 0 ELSE 1 END) ORDER BY enqueue_seq), '[]'::json)::text
             FROM queued_work_batches WHERE session_id = ?1 AND admitted_run IS NULL AND terminal_cause IS NULL)";

        /// Take a cross-target actor before its history without waiting on
        /// an actor another source envelope may hold in the opposite order.
        lock_target = "SELECT actor_key FROM actors WHERE actor_key = ?1 FOR UPDATE NOWAIT";

        /// The currently existing rows, without acquiring new locks after history.
        actor_keys = "SELECT actor_key FROM actors WHERE actor_key = ANY(?1) ORDER BY actor_key";

        /// Bind all named inputs to their run, retaining an existing binding
        /// to that run. Return each input still open and available.
        bind_inputs = "UPDATE pending_turn_inputs AS input
             SET admitted_run = ?3, admitted_by = ?3
             FROM UNNEST(?2::text[]) AS named(input_id)
             WHERE input.session_id = ?1 AND input.input_id = named.input_id
               AND (input.admitted_run IS NULL OR input.admitted_run = ?3)
               AND input.state IN ('deferred_next_turn', 'pending_active')
             RETURNING input.input_id";

        /// Bind all named batches to their run, retaining an existing binding
        /// to that run. Return each batch still open and available.
        bind_batches = "UPDATE queued_work_batches AS batch
             SET admitted_run = ?3, admitted_by = ?3
             FROM UNNEST(?2::text[]) AS named(batch_id)
             WHERE batch.session_id = ?1 AND batch.batch_id = named.batch_id
               AND (batch.admitted_run IS NULL OR batch.admitted_run = ?3)
               AND batch.terminal_cause IS NULL
             RETURNING batch.batch_id";
    }
}

static POSTGRES_SQL: std::sync::LazyLock<PostgresSessionMailStatements> =
    std::sync::LazyLock::new(|| {
        PostgresSessionMailStatements::render(
            lash_store_sql::Dialect::postgres()
                .with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        )
    });

impl super::PostgresDurableStore {
    /// Session `session`'s write authority, held to the commit: its actor
    /// row, then its history lock. Every session writer takes the two in
    /// this order, as an owner commit does, so no writer holds history
    /// while it waits for the actor row, and racing producers queue instead
    /// of deadlocking (FIG-5423, FIG-5429).
    ///
    /// A session with no actor yet has no row to lock: history alone orders
    /// its writers, and the one that creates the actor holds history. An
    /// actor created while this transaction waited for history would leave
    /// it holding history without the row, so the history lock is taken in
    /// a savepoint, and released to take the new row first. Actor rows are
    /// never deleted, so this backs off at most once.
    pub(crate) async fn lock_session_writes(
        tx: &mut PgConnection,
        session: &SessionId,
    ) -> Result<(), crate::StoreError> {
        Self::lock_session_writes_with(tx, session, false).await
    }

    async fn lock_session_writes_with(
        tx: &mut PgConnection,
        session: &SessionId,
        nowait: bool,
    ) -> Result<(), crate::StoreError> {
        use crate::support::store_sqlx_error;
        let connection = crate::connection_sql::connection_sql();
        let actor = session_actor(session)?;
        loop {
            let held = if nowait {
                let held = crate::observed_sql::query(POSTGRES_SQL.lock_target.sql())
                    .bind(actor.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?
                    .is_some();
                if held {
                    crate::observed_sql::query(connection.lock_xact_session_history.sql())
                        .bind(session.as_str())
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(store_sqlx_error)?;
                }
                held
            } else {
                crate::observed_sql::query(SQL.postgres.lock_session_writes.sql())
                    .bind(actor.as_str())
                    .bind(session.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?
                    .is_some()
            };
            if held {
                return Ok(());
            }
            crate::observed_sql::query(connection.savepoint_session_history.sql())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            crate::observed_sql::query(connection.lock_xact_session_history.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            let created = crate::observed_sql::query(SQL.actor.epoch_of.sql())
                .bind(actor.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?
                .is_some();
            if created {
                crate::observed_sql::query(connection.rollback_to_session_history.sql())
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?;
            }
            crate::observed_sql::query(connection.release_session_history.sql())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            if !created {
                return Ok(());
            }
        }
    }

    /// All fork participants' existing actor rows, before any history lock.
    pub(crate) async fn lock_session_actors(
        tx: &mut PgConnection,
        actors: &[String],
    ) -> Result<Vec<String>, crate::StoreError> {
        crate::observed_sql::query_scalar(SQL.postgres.lock_actors.sql())
            .bind(actors)
            .fetch_all(crate::observed_sql::executor(tx))
            .await
            .map_err(crate::support::store_sqlx_error)
    }

    pub(crate) async fn session_actor_keys(
        tx: &mut PgConnection,
        actors: &[String],
    ) -> Result<Vec<String>, crate::StoreError> {
        crate::observed_sql::query_scalar(POSTGRES_SQL.actor_keys.sql())
            .bind(actors)
            .fetch_all(crate::observed_sql::executor(tx))
            .await
            .map_err(crate::support::store_sqlx_error)
    }

    pub(crate) async fn lock_cross_target(
        tx: &mut PgConnection,
        session: &SessionId,
    ) -> Result<(), crate::StoreError> {
        Self::lock_session_writes_with(tx, session, true).await
    }

    /// Lock cross-target heads before an owner installs any graph/root rows.
    pub(crate) async fn lock_cross_target_heads(
        tx: &mut PgConnection,
        write: &lash_durable::ActorTx,
    ) -> Result<(), DurableError> {
        let mut targets = write
            .domain()
            .iter()
            .filter_map(|domain| match domain {
                lash_durable::domain::DomainWrite::SessionCommit(commit)
                    if super::turns::head_writer(write.actor(), &commit.session)
                        == lash_core_store::store::HeadWriter::Store =>
                {
                    Some(&commit.session)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        targets.sort();
        targets.dedup();
        for target in targets {
            Self::lock_cross_target(tx, target)
                .await
                .map_err(super::store_failure)?;
        }
        Ok(())
    }

    /// The session's actor row alone: an owner commit's first lock, as a
    /// law's owner takes it.
    #[cfg(test)]
    pub(crate) async fn lock_session_actor(
        tx: &mut PgConnection,
        session: &SessionId,
    ) -> Result<(), crate::StoreError> {
        let actor = session_actor(session)?;
        crate::observed_sql::query(SQL.postgres.lock_actors.sql())
            .bind([actor.as_str()].as_slice())
            .execute(crate::observed_sql::executor(tx))
            .await
            .map_err(crate::support::store_sqlx_error)?;
        Ok(())
    }
}

fn session_actor(session: &SessionId) -> Result<lash_durable::ActorKey, crate::StoreError> {
    lash_durable::ActorKey::session(session.as_str())
        .map_err(|error| crate::StoreError::Backend(error.to_string()))
}

fn undecodable(what: &str, detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("session mail {what} does not decode: {detail}"),
    })
}

/// Record the inputs this stop affects at the terminal transaction's
/// boundary, after its head commit settled delivered inputs. A drop binds
/// them for `settle_held`; deferral keeps their original ingress position.
pub(super) async fn cancel_inputs(
    tx: &mut PgConnection,
    context: &steps::Context<'_>,
    session: &SessionId,
    run: &TurnId,
    disposition: lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy,
) -> Result<lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome, DurableError> {
    let rows = crate::observed_sql::query(SQL.session_mail.cancel_inputs.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let mut outcome = lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome::default();
    for row in &rows {
        let Some(affected) = lash_core_store::store_backend_support::turn_cancel::affected_input(
            run,
            disposition,
            get(row, 0)?,
            &get::<String>(row, 1)?,
            &get::<String>(row, 2)?,
        )
        .map_err(|error| undecodable("cancelled input", error))?
        else {
            continue;
        };
        if disposition == lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Drop
        {
            let write = SessionMailWrite::Admit {
                session: session.clone(),
                run: run.clone(),
                inputs: vec![affected.input_id.clone()],
                batches: Vec::new(),
            };
            admit(&mut *tx, context, &write).await?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(outcome)
}

/// Session `session`'s standing: `(live, closing)`.
pub(super) async fn standing(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<(bool, bool), DurableError> {
    let row = crate::observed_sql::query(SQL.session_mail.standing.sql())
        .bind(session.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let meta: i64 = get(&row, 0)?;
    let deleted: i64 = get(&row, 1)?;
    let closing: i64 = get(&row, 2)?;
    Ok((meta > 0 && deleted == 0, closing > 0))
}

/// The session's open mail, read in one snapshot.
pub(super) async fn read(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<SessionMailbox, DurableError> {
    let row = crate::observed_sql::query(POSTGRES_SQL.mailbox.sql())
        .bind(session.as_str())
        .fetch_one(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    let mut mailbox = SessionMailbox {
        live: get::<i64>(&row, 0)? > 0 && get::<i64>(&row, 1)? == 0,
        closing: get::<i64>(&row, 2)? > 0,
        ..SessionMailbox::default()
    };
    if !mailbox.live {
        return Ok(mailbox);
    }
    mailbox.bound_run = get::<Option<String>>(&row, 3)?
        .map(TurnId::parse)
        .transpose()
        .map_err(|error| undecodable("bound run", error))?;
    type Input = (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        String,
    );
    let inputs: Vec<Input> = serde_json::from_str(&get::<String>(&row, 4)?)
        .map_err(|error| undecodable("input rows", error))?;
    for (input, seq, source, spec, state, ingress, trace, origin) in inputs {
        mailbox.inputs.push(decode_input(
            input,
            seq,
            source,
            spec,
            (&state, &ingress),
            trace.as_deref(),
            &origin,
        )?);
    }
    let batches: Vec<(String, i64, String, String, i32)> =
        serde_json::from_str(&get::<String>(&row, 5)?)
            .map_err(|error| undecodable("batch rows", error))?;
    for (batch, seq, policy, payload, cancel) in batches {
        mailbox
            .batches
            .push(decode_batch(batch, seq, &policy, &payload, cancel > 0)?);
    }
    Ok(mailbox)
}

fn decode_input(
    input: String,
    seq: i64,
    source_key: Option<String>,
    run_spec_hash: Option<String>,
    delivery: (&str, &str),
    trace_cause: Option<&str>,
    origin_json: &str,
) -> Result<MailInput, DurableError> {
    let (state, ingress_json) = delivery;
    let ingress: lash_core_store::turn_input_vocabulary::TurnInputIngress =
        serde_json::from_str(ingress_json).map_err(|error| undecodable("input delivery", error))?;
    let active_turn = match state {
        "pending_active" => ingress.active_turn_id().cloned(),
        _ => None,
    };
    Ok(MailInput {
        input: InputId::parse(input).map_err(|error| undecodable("input id", error))?,
        enqueue_seq: u64::try_from(seq).map_err(|error| undecodable("input order", error))?,
        source_key,
        origin: serde_json::from_str(origin_json)
            .map_err(|error| undecodable("input origin", error))?,
        run_spec_hash,
        active_turn,
        trace_cause: lash_core_store::store_backend_support::decode_trace_cause(
            "pending turn input",
            trace_cause,
        )
        .map_err(|error| undecodable("input trace cause", error))?,
    })
}

fn decode_batch(
    batch: String,
    seq: i64,
    policy: &str,
    payload_json: &str,
    cancel_requested: bool,
) -> Result<MailBatch, DurableError> {
    Ok(MailBatch {
        batch: BatchId::parse(batch).map_err(|error| undecodable("batch id", error))?,
        enqueue_seq: u64::try_from(seq).map_err(|error| undecodable("batch order", error))?,
        kind: batch_kind(payload_json)?,
        after_current_turn: policy == "after_current_turn_commit",
        cancel_requested,
    })
}

/// What a batch whose payload is `payload_json` asks the session for.
fn batch_kind(payload_json: &str) -> Result<MailBatchKind, DurableError> {
    let payload: lash_core_store::queued_work_vocabulary::QueuedWorkPayload =
        serde_json::from_str(payload_json).map_err(|error| undecodable("batch payload", error))?;
    let lash_core_store::queued_work_vocabulary::QueuedWorkPayload::SessionCommand { command } =
        &payload;
    Ok(
        if matches!(
            **command,
            lash_core_store::queued_work_vocabulary::SessionCommand::RunPluginTask { .. }
        ) {
            MailBatchKind::Operation
        } else {
            MailBatchKind::Control
        },
    )
}

/// A host's cancel of plugin task `request.batch`'s operation run
/// (FIG-5343), in one mail transaction: a command no operation run entered
/// is withdrawn, with a wake; an entered one records the first request,
/// with a control wake, so its running handler is cancelled. Each write's
/// `UPDATE` waits on the row an entry holds and then re-reads it, so a
/// withdrawal never passes an entry.
pub(super) async fn request_operation_cancel(
    tx: &mut PgConnection,
    request: &OperationCancelRequest,
    now: DurableInstant,
) -> Result<(OperationCancelAnswer, Option<Woken>), DurableError> {
    let session = request.session.as_str();
    let batch = request.batch.as_str();
    let actor = ActorKey::session(session).map_err(|_| undecodable("session id", session))?;
    let open: Option<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.session_mail.open_operation.sql())
            .bind(session)
            .bind(batch)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let Some((payload_json, _)) = open else {
        return Ok((OperationCancelAnswer::NotOpen, None));
    };
    if batch_kind(&payload_json)? != MailBatchKind::Operation {
        return Ok((OperationCancelAnswer::NotOpen, None));
    }
    let withdrawn: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.withdraw_unentered.sql())
            .bind(session)
            .bind(batch)
            .bind(now.0)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    if withdrawn.is_some() {
        let (woken, _) = super::wake_within(tx, &actor, false, now).await?;
        return Ok((OperationCancelAnswer::Withdrawn, Some(woken)));
    }
    let requested: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.request_operation_cancel.sql())
            .bind(session)
            .bind(batch)
            .bind(&request.request_json)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    if requested.is_some() {
        let (woken, _) = super::wake_within(tx, &actor, true, now).await?;
        return Ok((OperationCancelAnswer::Requested, Some(woken)));
    }
    // The row moved under the writes: it settled, or a racing request
    // was accepted first.
    let open: Option<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.session_mail.open_operation.sql())
            .bind(session)
            .bind(batch)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    Ok((
        match open.and_then(|(_, accepted)| accepted) {
            Some(request_json) => OperationCancelAnswer::AlreadyRequested { request_json },
            None => OperationCancelAnswer::NotOpen,
        },
        None,
    ))
}

/// Withdraw the input session `session` would admit as run `run`, while it
/// is still open and unbound session mail: into its `cancelled` tombstone at
/// `now_ms`, so no run ever takes it (FIG-5262). `None` when no open input
/// names the run, or an admission bound it first: the withdraw's `UPDATE`
/// waits on the row an admission holds and then finds it bound.
pub(super) async fn withdraw_queued(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> Result<Option<InputId>, DurableError> {
    let candidates: Vec<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.session_mail.open_inputs_named.sql())
            .bind(session.as_str())
            .bind(run.as_str())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    for (input, source_key) in candidates {
        let input = InputId::parse(input).map_err(|error| undecodable("input id", error))?;
        if queued_input_run(&input, source_key.as_deref()).as_ref() != Some(run) {
            continue;
        }
        let withdrawn: Option<String> =
            crate::observed_sql::query_scalar(SQL.session_mail.withdraw_input.sql())
                .bind(session.as_str())
                .bind(input.as_str())
                .bind(
                    lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
                )
                .bind(now_ms)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
        return Ok(withdrawn.map(|_| input));
    }
    Ok(None)
}

/// Apply the owner's session-mail write: bind admitted mail to its run, or
/// mail the session its own next-turn input.
pub(super) async fn apply(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Result<(), DurableError> {
    match write {
        SessionMailWrite::Admit { .. } => admit(tx, &commit.steps(), write).await,
        SessionMailWrite::Enqueue {
            session,
            draft_json,
        } => enqueue(tx, commit, session, draft_json).await,
        SessionMailWrite::EnterOperation { session, batch } => {
            enter_operation(tx, commit, session, batch).await
        }
    }
}

/// Enter plugin task `batch`'s operation run, its command still open.
async fn enter_operation(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    session: &SessionId,
    batch: &BatchId,
) -> Result<(), DurableError> {
    let entered: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.enter_operation.sql())
            .bind(session.as_str())
            .bind(batch.as_str())
            .bind(commit.now.0)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    match entered {
        Some(_) => Ok(()),
        None => Err(DurableError::Domain(
            DomainRefusal::SessionCommandWithdrawn {
                session: session.clone(),
                batch: batch.clone(),
            },
        )),
    }
}

/// Insert the owner's input as any producer's acceptance does, with the
/// session's wake, in the owner commit. A closing or gone session takes no
/// mail: its close settles what it holds, and the write inserts nothing.
async fn enqueue(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    session: &SessionId,
    draft_json: &str,
) -> Result<(), DurableError> {
    let refused = |reason: String| {
        DurableError::Domain(DomainRefusal::SessionMailRefused {
            session: session.clone(),
            reason,
        })
    };
    let (live, closing) = standing(tx, session).await?;
    if !live || closing {
        return Ok(());
    }
    let draft: lash_core_store::turn_input_vocabulary::PendingTurnInputDraft =
        serde_json::from_str(draft_json).map_err(|error| undecodable("owner input", error))?;
    let batch = lash_core_store::turn_input_vocabulary::PendingTurnInputBatch::new(
        session.clone(),
        vec![draft],
    )
    .map_err(|error| refused(error.to_string()))?;
    let now = u64::try_from(commit.now.0).unwrap_or(0);
    // The owner mails its own session: its next pass reads the input, so the
    // wake needs no hint.
    match crate::runtime_persistence::enqueue_pending_turn_inputs_tx(tx, &batch, now).await {
        Ok(_) => Ok(()),
        Err(error @ lash_core_store::store::StoreError::Contended) => {
            Err(super::store_failure(error))
        }
        Err(error) => Err(refused(error.to_string())),
    }
}

/// The session-mail statements an owner commit runs as steps.
pub(super) static BIND_INPUTS: Entry = Entry::new(
    || &POSTGRES_SQL.bind_inputs,
    &[Ty::Text, Ty::Texts, Ty::Text],
    Shape::Returning,
);
pub(super) static BIND_BATCHES: Entry = Entry::new(
    || &POSTGRES_SQL.bind_batches,
    &[Ty::Text, Ty::Texts, Ty::Text],
    Shape::Returning,
);

/// `write`'s step program: an admission binds its inputs and batches,
/// retaining each input's run on its row, one statement per kind whatever
/// their number, each bind answering every item it names. An enqueue or an
/// operation's entry has none: what it admits is read.
pub(super) fn steps(write: &SessionMailWrite) -> Option<Vec<Step>> {
    let SessionMailWrite::Admit {
        session,
        run,
        inputs,
        batches,
    } = write
    else {
        return None;
    };
    let named = |items: Vec<&str>| {
        vec![
            Value::text(session.as_str()),
            Value::Texts(
                items
                    .into_iter()
                    .map(|item| Some(item.to_owned()))
                    .collect(),
            ),
            Value::text(run.as_str()),
        ]
    };
    let mut program = Vec::new();
    if !inputs.is_empty() {
        let items: Vec<&str> = inputs.iter().map(InputId::as_str).collect();
        program.push(Step::new(
            &BIND_INPUTS,
            named(items.clone()),
            Some(items.len() as u64),
        ));
    }
    if !batches.is_empty() {
        let items: Vec<&str> = batches.iter().map(BatchId::as_str).collect();
        program.push(Step::new(
            &BIND_BATCHES,
            named(items.clone()),
            Some(items.len() as u64),
        ));
    }
    Some(program)
}

/// Bind the admitted mail to its run, each row still open and unbound. The
/// first item, in the order named, that did not bind refuses the commit.
async fn admit(
    tx: &mut PgConnection,
    context: &steps::Context<'_>,
    write: &SessionMailWrite,
) -> Result<(), DurableError> {
    let SessionMailWrite::Admit {
        session,
        inputs,
        batches,
        ..
    } = write
    else {
        return Ok(());
    };
    let program = steps(write).unwrap_or_default();
    let Some(refused) = steps::run(tx, &program, context).await? else {
        return Ok(());
    };
    let bound: std::collections::HashSet<String> = refused
        .rows
        .iter()
        .map(|row| get::<String>(row, 0))
        .collect::<Result<_, _>>()?;
    let named: Vec<&str> = if program[refused.index].entry.name() == BIND_INPUTS.name() {
        inputs.iter().map(InputId::as_str).collect()
    } else {
        batches.iter().map(BatchId::as_str).collect()
    };
    let item = named
        .into_iter()
        .find(|item| !bound.contains(*item))
        .unwrap_or_default();
    Err(DurableError::Domain(DomainRefusal::SessionMailMoved {
        session: session.clone(),
        item: item.to_owned(),
    }))
}

/// Arm (`true`) or disarm a cut of every session-mail producer transaction
/// at the session actor's wake: while armed, the wake's `mail_seq` bump
/// raises, so the producer's transaction rolls back before it commits. The
/// mail atomicity law's fault.
#[cfg(any(test, feature = "testing"))]
pub(crate) async fn cut_session_wakes(pool: &sqlx::PgPool, armed: bool) -> sqlx::Result<()> {
    sqlx::raw_sql(if armed {
        "CREATE OR REPLACE FUNCTION lash_test_cut_wake() RETURNS trigger
         LANGUAGE plpgsql AS $$ BEGIN
             RAISE EXCEPTION 'cut before the producer commit';
         END; $$;
         CREATE TRIGGER lash_test_cut_wake BEFORE UPDATE OF mail_seq ON lash_actors
         FOR EACH ROW EXECUTE FUNCTION lash_test_cut_wake();"
    } else {
        "DROP TRIGGER lash_test_cut_wake ON lash_actors;
         DROP FUNCTION lash_test_cut_wake();"
    })
    .execute(crate::observed_sql::executor(pool))
    .await
    .map(|_| ())
}

/// Project the production mailbox's bound-run probe for its history-work laws.
#[cfg(test)]
pub(crate) fn bound_run_query() -> String {
    format!(
        "SELECT bound_run FROM ({}) AS mailbox WHERE bound_run IS NOT NULL",
        POSTGRES_SQL.mailbox.sql()
    )
}
