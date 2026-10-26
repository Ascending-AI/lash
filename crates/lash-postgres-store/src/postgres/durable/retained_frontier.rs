//! A warm owner's authority and bounded external mailbox/control frontier.

use super::{PostgresDurableStore, SQL, corrupt, get, sqlx_failure, turns};
use lash_durable::{
    ActorKey, ActorState, ActorTx, DurableError, DurableInstant, Epoch, Fenced, Mail, MailKind,
    MailSeq, OpenedActor, StateRevision,
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
        let rows = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query(SQL.postgres.open_retained.sql())
                        .bind(actor.as_str())
                        .bind(session)
                        .bind(i64::try_from(max_mail_bytes).unwrap_or(i64::MAX))
                        .fetch_all(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
        let Some(first) = rows.first() else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let current = Epoch(get(first, 0)?);
        if current != epoch || get::<String>(first, 1)? != ActorState::Owned.as_str() {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: Some(current),
            }));
        }
        if StateRevision(get(first, 2)?) != expected {
            return Err(DurableError::RevisionConflict {
                actor: actor.clone(),
                epoch,
                expected,
                actual: StateRevision(get(first, 2)?),
            });
        }
        let turn_cancel =
            if let (Some(session), Some(run)) = (session, get::<Option<String>>(first, 11)?) {
                let session = lash_kernel_value::identity::SessionId::try_from(session.to_owned())
                    .map_err(|_| corrupt("session id", session))?;
                let run = lash_vocabulary::identity::TurnId::try_from(run.clone())
                    .map_err(|_| corrupt("turn id", &run))?;
                Some(turns::decode_cancel(
                    &session,
                    &run,
                    (
                        get(first, 12)?,
                        get(first, 13)?,
                        get(first, 14)?,
                        get(first, 15)?,
                        get(first, 16)?,
                    ),
                )?)
            } else {
                None
            };
        let mut opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision: expected,
            acked: MailSeq(get(first, 3)?),
            seen: MailSeq(get(first, 4)?),
            mail: Vec::new(),
            at: self.instant_or(get(first, 17)?)?,
            turn_cancel,
        };
        let mut partial = false;
        for row in &rows {
            if let Some(seq) = get::<Option<i64>>(row, 5)? {
                if let Some(body) = get::<Option<String>>(row, 9)? {
                    opened.mail.push(Mail {
                        seq: MailSeq(seq),
                        kind: MailKind::new(get::<String>(row, 8)?),
                        body,
                        appended_at: DurableInstant(get(row, 10)?),
                    });
                } else {
                    partial = true;
                    if opened.mail.is_empty() && max_mail_bytes > 0 {
                        return Err(DurableError::MailboxItemTooLarge {
                            actor: actor.clone(),
                            bytes: u64::try_from(get::<i64>(row, 6)?).unwrap_or(u64::MAX),
                            limit: max_mail_bytes,
                        });
                    }
                }
            }
        }
        if partial {
            opened.seen = opened.mail.last().map_or(opened.acked, |mail| mail.seq);
        }
        Ok(ActorTx::opened(opened))
    }
}
