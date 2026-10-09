//! Publication: a replica gathers its processes' batches for one tick and
//! writes them in one transaction.
//!
//! The transaction locks every head it writes in process order, so the row
//! lock serialises one process's writers across replicas: positions are
//! assigned under it, commit order is position order, and no position is
//! ever reserved and then abandoned. It drops redeliveries, appends the
//! events, trims each window, writes the heads back and rings one doorbell
//! for the tick.
//!
//! The aggregate bounds are admission, not a lock every event takes. A
//! tick locks the sentinel only to create a head (the process count) or to
//! grow a head's byte reservation by whole steps; a window is trimmed to
//! its reservation, so the reservations bound the bytes every window
//! holds. When the budget is spent the idlest other windows are evicted,
//! and a publication that still cannot be held is refused and ends its
//! process's continuity.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use lash_core::{
    ProcessId, ProcessObservationEvent, ProcessObservationEventPayload, ProcessReplayStoreError,
};
use sqlx::{Postgres, Row as _, Transaction};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::Shared;
use super::codec::{Doorbell, EncodedDraft, decode_payload};
use super::heads::{Attempt, Head, Heads, Retry, attempt};
use super::schema::{Incarnation, Sentinel, column, ensure_incarnation, purge};

type Reply = oneshot::Sender<Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError>>;

/// One `publish` call waiting for its tick.
pub(super) struct PublishRequest {
    pub(super) process_id: ProcessId,
    pub(super) drafts: Vec<EncodedDraft>,
    pub(super) reply: Reply,
    /// The request's share of the replica's bounded ingress, held until it
    /// is answered.
    pub(super) _ingress: [OwnedSemaphorePermit; 2],
}

/// What a tick decided for one request.
enum Planned {
    /// Its accepted drafts, by index, hold the positions from `start`.
    Published {
        start: u64,
        accepted: Vec<usize>,
    },
    Refused(ProcessReplayStoreError),
}

pub(super) async fn run(
    shared: Arc<Shared>,
    mut requests: mpsc::UnboundedReceiver<PublishRequest>,
) {
    let tick = shared.config.publish_tick;
    // Ticks in flight at once. Their transactions lock heads in process
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
    // `retry.process_replay` attempts, within the replay profile's deadline.
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
                        ensure_incarnation(&shared.pool, &shared.sql).await?;
                        Err(Attempt::Retry(Retry::Rotate))
                    }
                    written => written,
                }
            },
        )
        .await
        .map_err(|failed| match failed {
            Attempt::Retry(_) => ProcessReplayStoreError::Store(
                "postgres process replay publication kept racing; giving up".into(),
            ),
            Attempt::Failed(error) => error,
        });
    let (incarnation, plans, doorbell) = match outcome {
        Ok(written) => written,
        Err(error) => {
            for request in batch {
                let _ = request.reply.send(Err(error.clone()));
            }
            return;
        }
    };
    // This replica's subscribers need not wait for the notification.
    shared.ring(&doorbell);
    tracing::debug!(
        requests = batch.len(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "process replay tick written"
    );
    for (request, plan) in batch.into_iter().zip(plans) {
        let answer = match plan {
            Planned::Refused(error) => Err(error),
            Planned::Published { start, accepted } => answer(
                &incarnation,
                &request.process_id,
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
    process_id: &ProcessId,
    drafts: Vec<EncodedDraft>,
    start: u64,
    accepted: &[usize],
) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError> {
    let accepted = accepted.iter().copied().collect::<HashSet<_>>();
    drafts
        .into_iter()
        .enumerate()
        .filter(|(index, _)| accepted.contains(index))
        .enumerate()
        .map(|(offset, (_, draft))| {
            let cursor = incarnation.cursor(process_id, draft.sequence, start + offset as u64);
            ProcessObservationEvent::new(cursor, draft.payload)
                .map(Arc::new)
                .map_err(ProcessReplayStoreError::from)
        })
        .collect()
}

/// The observations one process's window holds, by identity key: what the
/// table held when the tick locked the head, and what the tick staged.
#[derive(Default)]
struct Window<'a> {
    stored: HashMap<String, ProcessObservationEventPayload>,
    staged: HashMap<&'a str, &'a ProcessObservationEventPayload>,
}

impl<'a> Window<'a> {
    fn held(&self, key: &str) -> Option<&ProcessObservationEventPayload> {
        self.staged
            .get(key)
            .copied()
            .or_else(|| self.stored.get(key))
    }

    fn clear(&mut self) {
        self.stored.clear();
        self.staged.clear();
    }
}

/// The rows a tick appends, column by column.
#[derive(Default)]
struct Rows {
    process: Vec<String>,
    position: Vec<i64>,
    sequence: Vec<i64>,
    payload: Vec<Vec<u8>>,
    bytes: Vec<i64>,
    key: Vec<String>,
}

impl Rows {
    fn push(&mut self, process: &str, position: u64, draft: &EncodedDraft) {
        self.process.push(process.to_string());
        self.position.push(column(position));
        self.sequence.push(column(draft.sequence.as_u64()));
        self.payload.push(draft.bytes.clone());
        self.bytes.push(column(draft.charge));
        self.key.push(draft.key.clone());
    }

    /// Bind the rows as a statement's first six parameters.
    fn bind<'q>(
        &'q self,
        query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
        query
            .bind(&self.process)
            .bind(&self.position)
            .bind(&self.sequence)
            .bind(&self.payload)
            .bind(&self.bytes)
            .bind(&self.key)
    }

    /// Drop every pending row of `process`: its generation just ended.
    fn discard(&mut self, process: &str) {
        let keep = self
            .process
            .iter()
            .map(|pending| pending != process)
            .collect::<Vec<_>>();
        fn retain<T>(column: &mut Vec<T>, keep: &[bool]) {
            let mut index = 0;
            column.retain(|_| {
                let kept = keep[index];
                index += 1;
                kept
            });
        }
        retain(&mut self.process, &keep);
        retain(&mut self.position, &keep);
        retain(&mut self.sequence, &keep);
        retain(&mut self.payload, &keep);
        retain(&mut self.bytes, &keep);
        retain(&mut self.key, &keep);
    }
}

fn capacity_error() -> ProcessReplayStoreError {
    ProcessReplayStoreError::Store(
        "process replay publication exceeds the store retention capacity".into(),
    )
}

/// Lock the sentinel unless this transaction already holds it. Heads are
/// always locked first.
async fn sentinel<'s>(
    tx: &mut Transaction<'_, Postgres>,
    shared: &Shared,
    held: &'s mut Option<Sentinel>,
) -> Result<&'s mut Sentinel, Attempt> {
    if held.is_none() {
        *held = Sentinel::lock(tx, &shared.sql)
            .await
            .map_err(attempt("lock sentinel"))?;
    }
    held.as_mut().ok_or(Attempt::Retry(Retry::Rotate))
}

/// Evict the idlest heads outside `keep`: `count` of them, and as many more
/// as free `bytes` of reservation. Their events go with them, the watermark
/// rises past their tails, and their subscribers are rung.
async fn evict(
    tx: &mut Transaction<'_, Postgres>,
    shared: &Shared,
    sentinel: &mut Sentinel,
    keep: &[String],
    count: u64,
    bytes: u64,
    doorbell: &mut Doorbell,
) -> Result<(), Attempt> {
    let rows = sqlx::query(&shared.sql.evict_heads)
        .bind(keep)
        .bind(column(count))
        .bind(column(bytes))
        .fetch_all(&mut **tx)
        .await
        .map_err(attempt("evict heads"))?;
    sentinel.released(&rows);
    let evicted = rows
        .iter()
        .map(|row| row.get::<String, _>("process_id"))
        .collect::<Vec<_>>();
    purge(tx, &shared.sql, &evicted)
        .await
        .map_err(attempt("evict events"))?;
    doorbell.processes.extend(evicted);
    Ok(())
}

type Written = (Incarnation, Vec<Planned>, Doorbell);

async fn write_once(shared: &Shared, batch: &[PublishRequest]) -> Result<Written, Attempt> {
    let sql = &shared.sql;
    let config = &shared.config;
    let processes = batch
        .iter()
        .map(|request| request.process_id.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut tx = shared
        .prelude
        .begin(&shared.pool)
        .await
        .map_err(attempt("begin"))?;
    let mut heads = Heads::lock(&mut tx, sql, &processes, config.max_age).await?;
    if matches!(heads.incarnation, Some(None)) {
        return Err(Attempt::Retry(Retry::Rotate));
    }
    let mut held = None;
    let mut doorbell = Doorbell::default();

    // Admission by process count: a head is created under the sentinel's
    // lock, evicting the idlest other heads to make room.
    let missing = heads.missing(&processes);
    let mut denied = BTreeSet::new();
    if !missing.is_empty() {
        let sentinel = sentinel(&mut tx, shared, &mut held).await?;
        let limit = config.max_processes as u64;
        let over = (sentinel.resident + missing.len() as u64).saturating_sub(limit);
        if over > 0 {
            evict(
                &mut tx,
                shared,
                sentinel,
                &processes,
                over,
                0,
                &mut doorbell,
            )
            .await?;
        }
        let room = usize::try_from(limit.saturating_sub(sentinel.resident)).unwrap_or(usize::MAX);
        let (admitted, refused) = missing.split_at(room.min(missing.len()));
        if !admitted.is_empty() {
            let watermark = sentinel.incarnation.watermark;
            let created = sqlx::query(&sql.create_heads)
                .bind(admitted)
                .bind(column(watermark))
                .fetch_all(&mut *tx)
                .await
                .map_err(attempt("create heads"))?;
            if created.len() != admitted.len() {
                // Another writer created one first: lock it next time.
                return Err(Attempt::Retry(Retry::Race));
            }
            sentinel.admit(admitted.len() as u64);
            for process in admitted {
                heads
                    .heads
                    .insert(process.clone(), Head::created(watermark));
            }
        }
        if !refused.is_empty() {
            // A refused process has no head to invalidate: raising the
            // watermark gaps every cursor that names a process without one.
            sentinel.raise(sentinel.incarnation.watermark + 1);
            denied.extend(refused.iter().cloned());
        }
    }

    heads.expire(&mut tx, sql, config.max_age).await?;
    let mut windows = delivered(&mut tx, shared, batch).await?;

    let mut plans = batch
        .iter()
        .map(|_| Planned::Published {
            start: 0,
            accepted: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut rows = Rows::default();
    let floors = heads
        .heads
        .iter()
        .map(|(process, head)| (process.clone(), head.floor))
        .collect::<BTreeMap<_, _>>();
    // The largest publication each process staged since its generation
    // began: its reservation must hold at least that.
    let mut largest = BTreeMap::<&str, u64>::new();
    let per_process = config.max_bytes_per_process as u64;
    for process in &processes {
        let requests = batch
            .iter()
            .enumerate()
            .filter(|(_, request)| request.process_id.as_str() == process);
        if denied.contains(process) {
            for (index, _) in requests {
                plans[index] = Planned::Refused(capacity_error());
            }
            continue;
        }
        let Some(head) = heads.heads.get_mut(process) else {
            return Err(Attempt::Retry(Retry::Race));
        };
        let window = windows.entry(process.clone()).or_default();
        for (index, request) in requests {
            let mut claimed = HashMap::<&str, &ProcessObservationEventPayload>::new();
            let mut accepted = Vec::new();
            let mut conflict = None;
            for (offset, draft) in request.drafts.iter().enumerate() {
                let key = draft.key.as_str();
                match claimed.get(key).copied().or_else(|| window.held(key)) {
                    Some(held) if held.same_fact(&draft.payload) => {}
                    Some(_) => {
                        conflict = Some(draft.identity.clone());
                        break;
                    }
                    None => {
                        claimed.insert(key, &draft.payload);
                        accepted.push(offset);
                    }
                }
            }
            let charge = accepted
                .iter()
                .map(|offset| request.drafts[*offset].charge)
                .sum::<u64>();
            let refusal = match conflict {
                Some(identity) => Some(ProcessReplayStoreError::ConflictingRedelivery {
                    process_id: request.process_id.clone(),
                    identity,
                }),
                None if charge > per_process => Some(capacity_error()),
                None => None,
            };
            if let Some(error) = refusal {
                // Neither a fact that contradicts the window nor a batch
                // larger than it can be published: the process's
                // continuity ends here.
                head.invalidate();
                window.clear();
                rows.discard(process);
                largest.remove(process.as_str());
                doorbell.processes.insert(process.clone());
                plans[index] = Planned::Refused(error);
                continue;
            }
            let start = head.tail + 1;
            for (slot, offset) in accepted.iter().enumerate() {
                rows.push(process, start + slot as u64, &request.drafts[*offset]);
            }
            window.staged.extend(claimed);
            let count = accepted.len() as u64;
            if count > 0 {
                head.tail += count;
                head.events += count;
                head.bytes += charge;
                let most = largest.entry(process.as_str()).or_default();
                *most = (*most).max(charge);
                doorbell.processes.insert(process.clone());
            }
            plans[index] = Planned::Published { start, accepted };
        }
    }

    // Admission by bytes: a head whose window outgrew its reservation asks
    // for whole steps more, up to the per-process bound.
    let step = config.reservation_bytes as u64;
    let wants = heads
        .heads
        .iter()
        .filter_map(|(process, head)| {
            let wanted = head.bytes.min(per_process);
            let target = (wanted.div_ceil(step) * step).min(per_process);
            (target > head.reserved).then(|| (process.clone(), target - head.reserved))
        })
        .collect::<Vec<_>>();
    if !wants.is_empty() {
        let sentinel = sentinel(&mut tx, shared, &mut held).await?;
        let total = wants.iter().map(|(_, want)| want).sum::<u64>();
        let short = (sentinel.reserved + total).saturating_sub(config.max_retained_bytes);
        if short > 0 {
            evict(
                &mut tx,
                shared,
                sentinel,
                &processes,
                0,
                short,
                &mut doorbell,
            )
            .await?;
        }
        for (process, want) in wants {
            let available = config.max_retained_bytes.saturating_sub(sentinel.reserved);
            let grant = want.min(available);
            sentinel.reserve(grant);
            if let Some(head) = heads.heads.get_mut(&process) {
                head.reserved += grant;
            }
        }
    }
    // A process whose reservation cannot hold one of its publications
    // loses the tick: nothing of it is published and its continuity ends.
    for (process, most) in largest {
        let Some(head) = heads.heads.get_mut(process) else {
            continue;
        };
        if most <= head.reserved {
            continue;
        }
        head.invalidate();
        rows.discard(process);
        for (index, request) in batch.iter().enumerate() {
            if request.process_id.as_str() == process
                && matches!(plans[index], Planned::Published { .. })
            {
                plans[index] = Planned::Refused(capacity_error());
            }
        }
    }

    // Rows below the floor belong to an ended generation; rows past the
    // count leave the window.
    let mut below = Vec::new();
    for (process, head) in &heads.heads {
        let invalidated = floors.get(process).is_some_and(|floor| head.floor > *floor);
        let count_cut = (head.tail + 1).saturating_sub(config.max_events_per_process as u64);
        if invalidated {
            below.push((process.clone(), head.floor.max(count_cut)));
        } else if count_cut > head.first_retained {
            below.push((process.clone(), count_cut));
        }
    }
    let trimming = !below.is_empty()
        || heads
            .heads
            .values()
            .any(|head| head.bytes > head.reserved || head.trimmed());
    if trimming || rows.process.is_empty() {
        if !rows.process.is_empty() {
            rows.bind(sqlx::query(&sql.insert_events))
                .execute(&mut *tx)
                .await
                .map_err(attempt("append events"))?;
        }
        heads.delete_below(&mut tx, sql, &below).await?;
        heads.trim_bytes(&mut tx, sql, |head| head.reserved).await?;
        heads.trim_dedupe(&mut tx, sql).await?;
        heads.write(&mut tx, sql, &doorbell).await?;
    } else {
        // The common tick: append, write the heads and ring in one
        // statement.
        heads
            .columns()
            .bind(rows.bind(sqlx::query(&sql.append_and_update)))
            .bind(&sql.channel)
            .bind(doorbell.pack()?)
            .execute(&mut *tx)
            .await
            .map_err(attempt("append events"))?;
    }
    let incarnation = match &held {
        Some(sentinel) => {
            sentinel
                .write(&mut tx, sql)
                .await
                .map_err(attempt("write sentinel"))?;
            sentinel.incarnation.clone()
        }
        None => match heads.incarnation.clone() {
            Some(Some(incarnation)) => incarnation,
            _ => return Err(Attempt::Retry(Retry::Rotate)),
        },
    };
    tx.commit().await.map_err(attempt("commit"))?;
    Ok((incarnation, plans, doorbell))
}

/// What each process's window holds under the identities this tick's
/// drafts name.
async fn delivered<'a>(
    tx: &mut Transaction<'_, Postgres>,
    shared: &Shared,
    batch: &'a [PublishRequest],
) -> Result<HashMap<String, Window<'a>>, Attempt> {
    let (processes, keys): (Vec<String>, Vec<String>) = batch
        .iter()
        .flat_map(|request| {
            request
                .drafts
                .iter()
                .map(|draft| (request.process_id.to_string(), draft.key.clone()))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .unzip();
    let mut windows: HashMap<String, Window<'a>> = HashMap::new();
    for row in sqlx::query(&shared.sql.delivered)
        .bind(&processes)
        .bind(&keys)
        .fetch_all(&mut **tx)
        .await
        .map_err(attempt("read delivered identities"))?
    {
        let payload = decode_payload(&row.get::<Vec<u8>, _>("payload"))?;
        windows
            .entry(row.get("process_id"))
            .or_default()
            .stored
            .insert(row.get("identity"), payload);
    }
    Ok(windows)
}
