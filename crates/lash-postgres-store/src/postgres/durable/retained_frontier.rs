//! A warm owner's authority and bounded external mailbox/control frontier.
use super::{PostgresDurableStore, SQL, corrupt, get, sqlx_failure, turns};
use lash_durable::{
    ActorKey, ActorTx, DurableError, Epoch, Fenced, MailSeq, OpenedActor, StateRevision,
};

impl PostgresDurableStore {
    pub(super) async fn retained_frontier(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        max_mail_bytes: u64,
    ) -> Result<ActorTx, DurableError> {
        let session = (actor.kind() == lash_durable::ActorKind::Session).then(|| actor.id());
        let row = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query(SQL.postgres.open_retained.sql())
                        .bind(actor.as_str())
                        .bind(session)
                        .bind(i64::try_from(max_mail_bytes).unwrap_or(i64::MAX))
                        .bind(OpenedActor::MAIL_PAGE_ROWS)
                        .bind(lash_durable::domain::SESSION_CLOSE_MAIL)
                        .fetch_optional(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(row) = row else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let state: String = get(&row, 1)?;
        let revision = lash_durable::fenced_read::validate_authority(
            actor,
            epoch,
            Some(expected),
            Some((Epoch(get(&row, 0)?), &state, StateRevision(get(&row, 2)?))),
        )?;
        let turn_cancel = match (session, get::<Option<String>>(&row, 5)?) {
            (Some(session), Some(run)) => {
                let session = lash_kernel_value::identity::SessionId::try_from(session.to_owned())
                    .map_err(|_| corrupt("session id", session))?;
                let run = lash_vocabulary::identity::TurnId::try_from(run.clone())
                    .map_err(|_| corrupt("turn id", &run))?;
                Some(turns::decode_cancel(
                    &session,
                    &run,
                    (
                        get(&row, 6)?,
                        get(&row, 7)?,
                        get(&row, 8)?,
                        get(&row, 9)?,
                        get(&row, 10)?,
                    ),
                )?)
            }
            _ => None,
        };
        let requests = super::run_records::call_cancels(actor, &get::<String>(&row, 11)?)?;
        let opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision,
            acked: MailSeq(get(&row, 3)?),
            seen: MailSeq(get(&row, 4)?),
            mail: Vec::new(),
            close_requested: get(&row, 13)?,
            at: self.instant_or(get(&row, 14)?)?,
            turn_cancel,
        }
        .with_mail_page(&get::<String>(&row, 12)?, max_mail_bytes)?;
        Ok(ActorTx::opened(opened).with_call_cancels(requests))
    }
}
