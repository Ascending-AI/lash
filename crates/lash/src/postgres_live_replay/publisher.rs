//! Publication: a replica gathers its sessions' batches for one tick and
//! writes them in one transaction.
//!
//! The transaction locks every head it writes in session order, so the
//! row lock serialises one session's writers across replicas: positions
//! are assigned under it, commit order is position order, and no position
//! is ever reserved and then abandoned. It appends the events, trims each
//! window, writes the heads back and rings one doorbell for the tick.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use lash_core::{LiveReplayStoreError, SessionCursor, SessionObservationEvent, SessionRevision};
use lash_sansio::SessionId;
use sqlx::Row as _;
use tokio::sync::{Semaphore, mpsc, oneshot};

use super::Shared;
use super::codec::{Doorbell, EncodedDraft, Identity, pack_doorbells};
use super::heads::{Attempt, Heads, Retry, attempt};
use super::schema::{Incarnation, column, ensure_incarnation};

type Reply = oneshot::Sender<Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError>>;

/// One `publish` call waiting for its tick.
pub(super) struct PublishRequest {
    pub(super) session_id: SessionId,
    pub(super) revision: SessionRevision,
    pub(super) drafts: Vec<EncodedDraft>,
    pub(super) reply: Reply,
}

/// What a tick decided for one request.
enum Planned {
    /// Its accepted drafts, by index, hold the positions from `start`.
    Published {
        start: u64,
        accepted: Vec<usize>,
    },
    Refused(LiveReplayStoreError),
}

pub(super) async fn run(
    shared: Arc<Shared>,
    mut requests: mpsc::UnboundedReceiver<PublishRequest>,
) {
    let tick = shared.config.publish_tick;
    // Ticks in flight at once. Their transactions lock heads in session
    // order, so they never deadlock, and a caller awaits each publication,
    // so one writer's batches still commit in its order.
    let in_flight = Arc::new(Semaphore::new(shared.config.publish_concurrency));
    while let Some(first) = requests.recv().await {
        let mut events = first.drafts.len();
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + tick;
        while events < shared.config.max_batch_events {
            let next = if tick.is_zero() {
                requests.try_recv().ok()
            } else {
                tokio::select! {
                    request = requests.recv() => request,
                    () = tokio::time::sleep_until(deadline) => None,
                }
            };
            let Some(request) = next else {
                break;
            };
            events += request.drafts.len();
            batch.push(request);
        }
        let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
            return;
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            write(&shared, batch).await;
            drop(permit);
        });
    }
}

/// Write one tick's requests and answer each.
async fn write(shared: &Shared, batch: Vec<PublishRequest>) {
    let started = std::time::Instant::now();
    // A tick starts over after a race or a rotation, up to the host's
    // `retry.live_replay` attempts, within the replay profile's deadline.
    let deadline = shared
        .prelude
        .deadline()
        .map(|deadline| tokio::time::Instant::now() + deadline);
    let batch_ref = &batch;
    let outcome = shared
        .retry
        .run(
            deadline,
            |failed| matches!(failed, Attempt::Retry(_)),
            || async move {
                match write_once(shared, batch_ref).await {
                    Err(Attempt::Retry(Retry::Rotate)) => {
                        shared.adopt(ensure_incarnation(&shared.pool, &shared.sql).await?);
                        Err(Attempt::Retry(Retry::Rotate))
                    }
                    written => written,
                }
            },
        )
        .await
        .map_err(|failed| match failed {
            Attempt::Retry(_) => LiveReplayStoreError::Store(
                "postgres live replay publication kept racing; giving up".into(),
            ),
            Attempt::Failed(error) => error,
        });
    let (incarnation, plans, doorbells) = match outcome {
        Ok(written) => written,
        Err(error) => {
            for request in batch {
                let _ = request.reply.send(Err(error.clone()));
            }
            return;
        }
    };
    shared.ring_mirror(&doorbells);
    tracing::debug!(
        requests = batch.len(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "live replay tick written"
    );
    for (request, plan) in batch.into_iter().zip(plans) {
        let answer = match plan {
            Planned::Refused(error) => Err(error),
            Planned::Published { start, accepted } => answer(
                &incarnation,
                request.session_id,
                request.revision,
                request.drafts,
                start,
                &accepted,
            ),
        };
        let _ = request.reply.send(answer);
    }
}

/// The published events of one request, built only after the commit.
fn answer(
    incarnation: &Incarnation,
    session_id: SessionId,
    revision: SessionRevision,
    drafts: Vec<EncodedDraft>,
    start: u64,
    accepted: &[usize],
) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
    let accepted = accepted.iter().copied().collect::<HashSet<_>>();
    drafts
        .into_iter()
        .enumerate()
        .filter(|(index, _)| accepted.contains(index))
        .enumerate()
        .map(|(offset, (_, draft))| {
            let cursor: SessionCursor =
                incarnation.cursor(&session_id, revision, start + offset as u64);
            SessionObservationEvent::new(draft.turn_id, cursor, draft.payload)
                .map(Arc::new)
                .map_err(LiveReplayStoreError::from)
        })
        .collect()
}

/// What the session's window already delivered, by activity identity.
#[derive(Default)]
struct Delivered {
    spans: HashMap<String, (u32, u32)>,
    opaque: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Claim {
    Fresh,
    Delivered,
    Overlapping,
}

impl Delivered {
    /// How `identity` meets what the window delivered and what this
    /// request claimed before it: inside a delivered span is a redelivery,
    /// wholly outside is fresh, across its edge cannot be cut (FIG-5098).
    fn claim(&self, identity: &Identity, claimed: &[&Identity]) -> Claim {
        match identity {
            Identity::Opaque(id) => {
                let delivered = self.opaque.contains(id)
                    || claimed
                        .iter()
                        .any(|claimed| matches!(claimed, Identity::Opaque(other) if other == id));
                if delivered {
                    Claim::Delivered
                } else {
                    Claim::Fresh
                }
            }
            Identity::Span { key, first, last } => {
                let span = claimed
                    .iter()
                    .filter_map(|claimed| match claimed {
                        Identity::Span {
                            key: other,
                            first,
                            last,
                        } if other == key => Some((*first, *last)),
                        _ => None,
                    })
                    .chain(self.spans.get(key).copied())
                    .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)));
                match span {
                    None => Claim::Fresh,
                    Some((start, end)) if *first > end || *last < start => Claim::Fresh,
                    Some((start, end)) if *first >= start && *last <= end => Claim::Delivered,
                    Some(_) => Claim::Overlapping,
                }
            }
        }
    }

    fn insert(&mut self, identity: &Identity) {
        match identity {
            Identity::Opaque(id) => {
                self.opaque.insert(id.clone());
            }
            Identity::Span { key, first, last } => {
                let span = self.spans.entry(key.clone()).or_insert((*first, *last));
                *span = (span.0.min(*first), span.1.max(*last));
            }
        }
    }
}

/// The rows a tick appends, column by column.
#[derive(Default)]
struct Rows {
    session: Vec<String>,
    position: Vec<i64>,
    revision: Vec<i64>,
    turn_id: Vec<Option<String>>,
    activity_id: Vec<Option<String>>,
    activity_key: Vec<Option<String>>,
    activity_first: Vec<Option<i64>>,
    activity_last: Vec<Option<i64>>,
    payload: Vec<Vec<u8>>,
    bytes: Vec<i64>,
}

impl Rows {
    fn push(&mut self, session: &str, position: u64, revision: u64, draft: &EncodedDraft) {
        let (id, key, first, last) = match &draft.identity {
            None => (None, None, None, None),
            Some(Identity::Opaque(id)) => (Some(id.clone()), None, None, None),
            Some(Identity::Span { key, first, last }) => (
                None,
                Some(key.clone()),
                Some(i64::from(*first)),
                Some(i64::from(*last)),
            ),
        };
        self.session.push(session.to_string());
        self.position.push(column(position));
        self.revision.push(column(revision));
        self.turn_id
            .push(draft.turn_id.as_ref().map(ToString::to_string));
        self.activity_id.push(id);
        self.activity_key.push(key);
        self.activity_first.push(first);
        self.activity_last.push(last);
        self.payload.push(draft.bytes.clone());
        self.bytes.push(column(draft.charge));
    }

    /// Bind the rows as a statement's first ten parameters.
    fn bind<'q>(
        &'q self,
        query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        query
            .bind(&self.session)
            .bind(&self.position)
            .bind(&self.revision)
            .bind(&self.turn_id)
            .bind(&self.activity_id)
            .bind(&self.activity_key)
            .bind(&self.activity_first)
            .bind(&self.activity_last)
            .bind(&self.payload)
            .bind(&self.bytes)
    }

    /// Drop every pending row of `session`: its generation just ended.
    fn discard(&mut self, session: &str) {
        let keep = self
            .session
            .iter()
            .map(|pending| pending != session)
            .collect::<Vec<_>>();
        fn retain<T>(column: &mut Vec<T>, keep: &[bool]) {
            let mut index = 0;
            column.retain(|_| {
                let kept = keep[index];
                index += 1;
                kept
            });
        }
        retain(&mut self.session, &keep);
        retain(&mut self.position, &keep);
        retain(&mut self.revision, &keep);
        retain(&mut self.turn_id, &keep);
        retain(&mut self.activity_id, &keep);
        retain(&mut self.activity_key, &keep);
        retain(&mut self.activity_first, &keep);
        retain(&mut self.activity_last, &keep);
        retain(&mut self.payload, &keep);
        retain(&mut self.bytes, &keep);
    }
}

type Written = (Incarnation, Vec<Planned>, Vec<Doorbell>);

async fn write_once(shared: &Shared, batch: &[PublishRequest]) -> Result<Written, Attempt> {
    let sql = &shared.sql;
    let config = &shared.config;
    let sessions = batch
        .iter()
        .map(|request| request.session_id.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut tx = shared
        .prelude
        .begin(&shared.pool)
        .await
        .map_err(attempt("begin"))?;
    let mut heads = Heads::lock(&mut tx, sql, &sessions, config.max_age, true).await?;
    let Some(Some(incarnation)) = heads.incarnation.clone() else {
        return Err(Attempt::Retry(Retry::Rotate));
    };
    heads.expire(&mut tx, sql, config.max_age).await?;
    let mut delivered = delivered(&mut tx, shared, batch).await?;

    let mut plans = batch
        .iter()
        .map(|_| Planned::Published {
            start: 0,
            accepted: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut rows = Rows::default();
    let mut doorbells = Vec::new();
    let mut below = Vec::new();
    for session in &sessions {
        let Some(head) = heads.heads.get_mut(session) else {
            return Err(Attempt::Retry(Retry::Race));
        };
        let window = delivered.entry(session.clone()).or_default();
        let floor_at_lock = head.floor;
        for (index, request) in batch
            .iter()
            .enumerate()
            .filter(|(_, request)| request.session_id.as_str() == session)
        {
            let mut claimed = Vec::new();
            let mut accepted = Vec::new();
            let mut overlapping = false;
            for (offset, draft) in request.drafts.iter().enumerate() {
                let Some(identity) = &draft.identity else {
                    accepted.push(offset);
                    continue;
                };
                let claim = window.claim(identity, &claimed);
                claimed.push(identity);
                overlapping |= claim == Claim::Overlapping;
                if claim == Claim::Fresh {
                    accepted.push(offset);
                }
            }
            let charge = accepted
                .iter()
                .map(|offset| request.drafts[*offset].charge)
                .sum::<u64>();
            let oversized = charge > config.max_bytes_per_session as u64;
            if overlapping || oversized {
                // A redelivery straddling what was delivered cannot be cut
                // (FIG-5098), and a batch larger than the window cannot be
                // held: either way the session's continuity ends here.
                head.invalidate();
                *window = Delivered::default();
                rows.discard(session);
                doorbells.push(Doorbell::Invalidated {
                    session: session.clone(),
                    floor: head.floor,
                });
            }
            if oversized {
                plans[index] = Planned::Refused(LiveReplayStoreError::Store(
                    "live replay publication exceeds the store retention capacity".into(),
                ));
                continue;
            }
            let start = head.tail + 1;
            for (slot, offset) in accepted.iter().enumerate() {
                let draft = &request.drafts[*offset];
                rows.push(
                    session,
                    start + slot as u64,
                    request.revision.as_u64(),
                    draft,
                );
                if let Some(identity) = &draft.identity {
                    window.insert(identity);
                }
            }
            let count = accepted.len() as u64;
            if count > 0 {
                head.tail += count;
                head.events += count;
                head.bytes += charge;
                doorbells.push(Doorbell::Published {
                    session: session.clone(),
                    floor: head.floor,
                    first: start,
                    last: head.tail,
                    revision: request.revision.as_u64(),
                });
            }
            plans[index] = Planned::Published { start, accepted };
        }
        // Rows below the floor belong to an ended generation; rows past the
        // count leave the window.
        let invalidated = head.floor > floor_at_lock;
        let count_cut = (head.tail + 1).saturating_sub(config.max_events_per_session as u64);
        if invalidated {
            below.push((session.clone(), head.floor.max(count_cut)));
        } else if count_cut > head.first_retained {
            below.push((session.clone(), count_cut));
        }
    }

    let trimming = !below.is_empty()
        || heads
            .heads
            .values()
            .any(|head| head.bytes > config.max_bytes_per_session as u64);
    if trimming || rows.session.is_empty() {
        if !rows.session.is_empty() {
            rows.bind(sqlx::query(&sql.insert_events))
                .execute(&mut *tx)
                .await
                .map_err(attempt("append events"))?;
        }
        heads.delete_below(&mut tx, sql, &below).await?;
        heads
            .trim_bytes(&mut tx, sql, config.max_bytes_per_session as u64)
            .await?;
        heads.trimmed(&mut doorbells);
        heads.write(&mut tx, sql, &doorbells).await?;
    } else {
        // The common tick: append, write the heads and ring in one
        // statement.
        heads.trimmed(&mut doorbells);
        let columns = heads.columns();
        rows.bind(sqlx::query(&sql.append_and_update))
            .bind(&columns.session)
            .bind(&columns.tail)
            .bind(&columns.floor)
            .bind(&columns.first_retained)
            .bind(&columns.events)
            .bind(&columns.bytes)
            .bind(&sql.channel)
            .bind(pack_doorbells(&doorbells)?)
            .execute(&mut *tx)
            .await
            .map_err(attempt("append events"))?;
    }
    tx.commit().await.map_err(attempt("commit"))?;
    Ok((incarnation, plans, doorbells))
}

/// The activity identities each session's window holds that this tick's
/// drafts name: delivered spans by replay key, and opaque ids.
async fn delivered(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    shared: &Shared,
    batch: &[PublishRequest],
) -> Result<HashMap<String, Delivered>, Attempt> {
    let mut spans = BTreeSet::new();
    let mut opaque = BTreeSet::new();
    for request in batch {
        for draft in &request.drafts {
            match &draft.identity {
                Some(Identity::Span { key, .. }) => {
                    spans.insert((request.session_id.to_string(), key.clone()));
                }
                Some(Identity::Opaque(id)) => {
                    opaque.insert((request.session_id.to_string(), id.clone()));
                }
                None => {}
            }
        }
    }
    let mut delivered: HashMap<String, Delivered> = HashMap::new();
    if spans.is_empty() && opaque.is_empty() {
        return Ok(delivered);
    }
    let (span_sessions, keys): (Vec<String>, Vec<String>) = spans.into_iter().unzip();
    let (opaque_sessions, ids): (Vec<String>, Vec<String>) = opaque.into_iter().unzip();
    for row in sqlx::query(&shared.sql.delivered)
        .bind(&span_sessions)
        .bind(&keys)
        .bind(&opaque_sessions)
        .bind(&ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(attempt("read delivered activities"))?
    {
        let window = delivered.entry(row.get("session_id")).or_default();
        match row.get::<Option<String>, _>("activity_key") {
            Some(key) => {
                let ordinal = |name: &str| {
                    row.get::<Option<i64>, _>(name)
                        .and_then(|value| u32::try_from(value).ok())
                };
                if let (Some(first), Some(last)) = (ordinal("first"), ordinal("last")) {
                    window.spans.insert(key, (first, last));
                }
            }
            None => {
                if let Some(id) = row.get::<Option<String>, _>("activity_id") {
                    window.opaque.insert(id);
                }
            }
        }
    }
    Ok(delivered)
}
