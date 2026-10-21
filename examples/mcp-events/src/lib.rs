//! Reference host dispatcher. Its own SQLite registry and inbox bridge MCP
//! transport to keyed Lash sends. Lash owns only admitted turn execution.

use std::path::Path;
use std::time::Duration;

use lash::LashCore;
use lash::mcp::events::{McpEventClient, McpEventOccurrence, McpEventPoll, McpEventRequest};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const TICKET_CREATED: &str = "ticket.created";

/// This host maps tickets to an existing session. Changes to routing,
/// arguments or event type require a new generation; old inbox rows keep
/// the exact destination and input admitted in their original transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub generation: u64,
    pub server: String,
    pub event: String,
    pub session: SessionId,
    pub arguments: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Transport(#[from] lash::mcp::events::McpEventsCallError),
    #[error(transparent)]
    Lash(#[from] lash::EmbedError),
    #[error("subscription or occurrence identity was reused with changed content")]
    Conflict,
    #[error("subscription is absent or disabled")]
    Disabled,
    #[error("the source delivered an occurrence for a different event type")]
    WrongEvent,
    #[error("source continuity was lost: {0:?}")]
    ContinuityLost(lash::mcp::events::McpEventContinuityLoss),
}

/// Host data, deliberately separate from Lash's store. One dispatcher owns
/// this connection; a distributed host must add its own lease/fencing policy.
/// Source redelivery and Lash dedup evidence must outlive inbox retention.
pub struct HostStore {
    db: Connection,
}

/// A committed delivery. The key and mapped input are immutable on retry.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub key: String,
    pub session: SessionId,
    pub turn: TurnId,
    pub input: String,
}

impl HostStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS subscriptions (
                 id TEXT NOT NULL, generation INTEGER NOT NULL, spec TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1, cursor TEXT,
                 retention_gap INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (id, generation));
             CREATE TABLE IF NOT EXISTS inbox (
                 delivery_key TEXT PRIMARY KEY, session TEXT NOT NULL,
                 turn_id TEXT NOT NULL, input TEXT NOT NULL,
                 delivered INTEGER NOT NULL DEFAULT 0);",
        )?;
        Ok(Self { db })
    }

    /// A host registration, not a process or a model tool. A repeated equal
    /// registration keeps its committed progress and disabled state.
    pub fn register(&self, subscription: &Subscription) -> Result<(), Error> {
        let spec = serde_json::to_string(subscription)?;
        self.db.execute(
            "INSERT INTO subscriptions (id, generation, spec) VALUES (?1, ?2, ?3)
             ON CONFLICT (id, generation) DO NOTHING",
            params![subscription.id, subscription.generation, spec],
        )?;
        let recorded: String = self.db.query_row(
            "SELECT spec FROM subscriptions WHERE id=?1 AND generation=?2",
            params![subscription.id, subscription.generation],
            |row| row.get(0),
        )?;
        if recorded != spec {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    pub fn disable(&self, id: &str, generation: u64) -> Result<(), Error> {
        self.db.execute(
            "UPDATE subscriptions SET enabled=0 WHERE id=?1 AND generation=?2",
            params![id, generation],
        )?;
        Ok(())
    }

    fn read(&self, id: &str, generation: u64) -> Result<(Subscription, Option<String>), Error> {
        let row: Option<(String, Option<String>)> = self.db.query_row(
            "SELECT spec, cursor FROM subscriptions WHERE id=?1 AND generation=?2 AND enabled=1",
            params![id, generation], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let (spec, cursor) = row.ok_or(Error::Disabled)?;
        Ok((serde_json::from_str(&spec)?, cursor))
    }

    /// Atomically accept every occurrence before acknowledging its cursor.
    /// A retention gap is recorded, returned to the host and never hidden.
    /// A bad batch commits neither inbox rows nor progress.
    pub fn accept(&mut self, id: &str, generation: u64, batch: &McpEventPoll) -> Result<(), Error> {
        let transaction = self.db.transaction()?;
        let spec: Option<String> = transaction
            .query_row(
                "SELECT spec FROM subscriptions WHERE id=?1 AND generation=?2 AND enabled=1",
                params![id, generation],
                |row| row.get(0),
            )
            .optional()?;
        let subscription: Subscription = serde_json::from_str(&spec.ok_or(Error::Disabled)?)?;
        for occurrence in &batch.events {
            if occurrence.name != subscription.event {
                return Err(Error::WrongEvent);
            }
            let delivery = map_ticket(&subscription, occurrence)?;
            transaction.execute(
                "INSERT INTO inbox (delivery_key, session, turn_id, input) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (delivery_key) DO NOTHING",
                params![
                    delivery.key,
                    delivery.session.as_str(),
                    delivery.turn.as_str(),
                    delivery.input
                ],
            )?;
            let recorded: (String, String) = transaction.query_row(
                "SELECT session, input FROM inbox WHERE delivery_key=?1",
                [&delivery.key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if recorded != (delivery.session.to_string(), delivery.input) {
                return Err(Error::Conflict);
            }
        }
        transaction.execute(
            "UPDATE subscriptions SET cursor=?3, retention_gap=MAX(retention_gap, ?4)
             WHERE id=?1 AND generation=?2",
            params![id, generation, batch.cursor, batch.truncated],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pending(&self, limit: u32) -> Result<Vec<Delivery>, Error> {
        let mut statement = self.db.prepare(
            "SELECT delivery_key, session, turn_id, input FROM inbox WHERE delivered=0 ORDER BY rowid LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (key, session, turn, input) = row?;
            // Store ids as their authoritative serde representation so reads
            // use the public id validation rather than a parallel host codec.
            Ok(Delivery {
                key,
                session: serde_json::from_value(Value::String(session))?,
                turn: serde_json::from_value(Value::String(turn))?,
                input,
            })
        })
        .collect()
    }

    /// Only call after admission answers. Losing this write leaves a pending
    /// row; reconciliation sends the identical input with the identical id.
    pub fn acknowledge(&self, delivery: &Delivery) -> Result<(), Error> {
        self.db.execute(
            "UPDATE inbox SET delivered=1 WHERE delivery_key=?1",
            [&delivery.key],
        )?;
        Ok(())
    }

    pub async fn dispatch_pending(&self, core: &LashCore, limit: u32) -> Result<(), Error> {
        for delivery in self.pending(limit)? {
            admit(core, &delivery).await?;
            self.acknowledge(&delivery)?;
        }
        Ok(())
    }

    /// One host-selected poll. The caller schedules the next call using its
    /// own floor, retry and gap-reconciliation policy. `has_more` and
    /// `next_poll_ms` remain protocol advice, never a Lash scheduler.
    pub async fn poll_once(
        &mut self,
        client: &McpEventClient,
        id: &str,
        generation: u64,
        max_events: u32,
        timeout: Duration,
    ) -> Result<McpEventPoll, Error> {
        let (subscription, cursor) = self.read(id, generation)?;
        let batch = client
            .poll(
                &subscription.server,
                &McpEventRequest {
                    name: subscription.event,
                    arguments: subscription.arguments,
                    cursor,
                    max_events,
                    max_age_ms: None,
                },
                timeout,
            )
            .await?;
        self.accept(id, generation, &batch)?;
        Ok(batch)
    }
}

/// Scheduling is host policy. Transport itself has no retry or dispatch loop.
pub struct PollPolicy {
    pub floor: Duration,
    pub fetch_timeout: Duration,
    pub max_events: u32,
    pub delivery_batch: u32,
}

/// Run this host's poll/dispatch loop until shutdown. On a transport fault or
/// gap it returns for host reconciliation; already committed deliveries stay
/// in the inbox. Restarting first reconciles them with their original keys.
pub async fn run_polling(
    host: &mut HostStore,
    client: &McpEventClient,
    core: &LashCore,
    subscription: &Subscription,
    policy: &PollPolicy,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), Error> {
    tokio::pin!(stop);
    loop {
        host.dispatch_pending(core, policy.delivery_batch).await?;
        let batch = tokio::select! {
            () = &mut stop => return Ok(()),
            batch = host.poll_once(client, &subscription.id, subscription.generation,
                policy.max_events, policy.fetch_timeout) => batch?,
        };
        if let Some(loss) = batch.continuity_loss() {
            return Err(Error::ContinuityLost(loss));
        }
        // Drain retained server pages promptly; otherwise honor both this
        // host's floor and the server's next-poll advice.
        let delay = if batch.has_more {
            Duration::ZERO
        } else {
            policy
                .floor
                .max(Duration::from_millis(batch.next_poll_ms.unwrap_or(0)))
        };
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = tokio::time::sleep(delay) => {},
        }
    }
}

/// This host's mapping: event metadata and untrusted ticket data become
/// ordinary input. The protocol cursor is excluded so replay has equal content.
fn map_ticket(
    subscription: &Subscription,
    occurrence: &McpEventOccurrence,
) -> Result<Delivery, Error> {
    let identity = serde_json::to_vec(&(
        &subscription.server,
        &subscription.event,
        &subscription.id,
        subscription.generation,
        &occurrence.event_id,
    ))?;
    let key = blake3::hash(&identity).to_hex().to_string();
    let input = serde_json::to_string(&serde_json::json!({
        "eventId": occurrence.event_id, "name": occurrence.name,
        "timestamp": occurrence.timestamp, "data": occurrence.data,
    }))?;
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&blake3::hash(&identity).as_bytes()[..16]);
    Ok(Delivery {
        key,
        session: subscription.session.clone(),
        turn: TurnId::from_uuid(u128::from_be_bytes(bytes)),
        input,
    })
}

/// Submit and observe admission. The engine drives the turn; the host does
/// not execute it or await its answer before recording the delivery receipt.
pub async fn admit(core: &LashCore, delivery: &Delivery) -> Result<(), Error> {
    core.session(delivery.session.clone())
        .durable()
        .await?
        .send(lash::TurnInput::text(delivery.input.clone()))
        .id(delivery.turn.clone())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
