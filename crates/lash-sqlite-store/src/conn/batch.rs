//! Group commit on the connection thread (FIG-5958).
//!
//! Every write already queued when the connection thread becomes free runs
//! in one `BEGIN IMMEDIATE` transaction, up to the batch limits (a member
//! count, and the rows the batch may change before no further member starts
//! in it: SQLite reports no byte count before `COMMIT`, while
//! `sqlite3_total_changes` counts each member's rows exactly): one gate
//! acquisition, one writer fence and one durable `COMMIT` (one WAL sync
//! under `synchronous=FULL`) for all of them. Nothing waits for a batch to
//! fill: a batch is the writes queued when the previous one ended.
//!
//! A batch of one is exactly a plain write transaction. With more members,
//! each runs inside its own savepoint, so a member's refusal or rollback
//! undoes only that member's writes and answers only that member; its
//! siblings still commit. No member answers before the outer `COMMIT`
//! returns: an answer never reports writes, or a refusal decided against
//! writes, that a power loss could still take back. A failed `COMMIT`
//! fails every member, which their owners treat as a crash. The synchronous
//! level is the connection's, fixed at open and never changed per batch:
//! SQLite refuses to change it inside a transaction.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use lash_core_execution::facade_support::sql::Window;
use rusqlite::{Connection, TransactionBehavior};

use super::{CheckpointState, FencedTx, TxOutcome, WriterFence, timed_gate};

/// Everything a queued write needs on the connection thread, shared by the
/// connection's clones: writes on one writer may share a transaction.
pub(super) struct Writer {
    pub(super) operational: crate::SqliteOperationalSettings,
    pub(super) write_gate: Arc<Mutex<()>>,
    pub(super) read_gate: Arc<RwLock<()>>,
    pub(super) checkpoint: Option<Arc<CheckpointState>>,
    pub(super) fence: Arc<WriterFence>,
    #[cfg(feature = "testing")]
    pub(super) pauses: Option<crate::testing::SqlitePauses>,
}

/// One queued write transaction: a batch member.
pub(crate) struct Member {
    writer: Arc<Writer>,
    window: Option<Window>,
    body: Box<dyn Body>,
    #[cfg(feature = "perf-witness")]
    timing: Option<lash_core_execution::perf_witness::queues::Timer>,
}

impl Member {
    /// `f` as a batch member on `writer`, answering through `reply`.
    pub(super) fn new<T, F, R>(writer: &Arc<Writer>, f: F, reply: R) -> Self
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<TxOutcome<T>> + Send + 'static,
        R: FnOnce(rusqlite::Result<T>) + Send + 'static,
    {
        Self {
            writer: Arc::clone(writer),
            window: Window::current(),
            body: Box::new(Typed {
                body: Some(f),
                result: None,
                reply,
            }),
            #[cfg(feature = "perf-witness")]
            timing: lash_core_execution::perf_witness::queues::Timer::enqueue("sqlite.worker"),
        }
    }

    /// Whether `self` may share `first`'s transaction: both write through
    /// the same connection's gates and fence.
    pub(super) fn joins(&self, first: &Self) -> bool {
        Arc::ptr_eq(&self.writer, &first.writer)
    }

    /// How many members a batch led by `self` may hold.
    pub(super) fn limit(&self) -> usize {
        self.writer.operational.write_batch_members.get()
    }

    fn run(&mut self, tx: &FencedTx<'_>) -> Kept {
        let window = self.window.clone();
        let body = &mut self.body;
        #[cfg(feature = "perf-witness")]
        let _scope = self.timing.as_mut().map(|timing| timing.worker_scope());
        Window::on_worker(window, || body.run(tx))
    }

    fn answer(self, failure: Option<rusqlite::Error>) {
        let Self {
            body,
            #[cfg(feature = "perf-witness")]
            mut timing,
            ..
        } = self;
        body.answer(failure);
        #[cfg(feature = "perf-witness")]
        if let Some(timing) = timing.as_mut() {
            timing.complete();
        }
    }
}

/// What a member's body left in the transaction.
enum Kept {
    /// Its writes stay: its savepoint is released into the batch.
    Writes,
    /// It chose to roll back: its savepoint is undone.
    Nothing,
    /// It failed with this error (a replica; the member keeps the
    /// original to answer with): its savepoint is undone.
    Failed(rusqlite::Error),
}

trait Body: Send {
    fn run(&mut self, tx: &FencedTx<'_>) -> Kept;
    /// Answer the caller once the batch is settled: with the body's own
    /// result, or with `failure` when the batch lost the body's writes or
    /// never ran it.
    fn answer(self: Box<Self>, failure: Option<rusqlite::Error>);
}

struct Typed<T, F, R> {
    body: Option<F>,
    result: Option<rusqlite::Result<T>>,
    reply: R,
}

impl<T, F, R> Body for Typed<T, F, R>
where
    T: Send + 'static,
    F: FnOnce(&FencedTx<'_>) -> rusqlite::Result<TxOutcome<T>> + Send + 'static,
    R: FnOnce(rusqlite::Result<T>) + Send + 'static,
{
    fn run(&mut self, tx: &FencedTx<'_>) -> Kept {
        let Some(body) = self.body.take() else {
            return Kept::Nothing;
        };
        let (result, kept) = match body(tx) {
            Ok(TxOutcome::Commit(value)) => (Ok(value), Kept::Writes),
            Ok(TxOutcome::Rollback(value)) => (Ok(value), Kept::Nothing),
            Err(error) => {
                let replica = replicate(&error);
                (Err(error), Kept::Failed(replica))
            }
        };
        self.result = Some(result);
        kept
    }

    fn answer(self: Box<Self>, failure: Option<rusqlite::Error>) {
        let result = match (failure, self.result) {
            (Some(failure), _) => Err(failure),
            (None, Some(result)) => result,
            (None, None) => Err(storage_failure(
                "a write batch settled a member it never ran",
            )),
        };
        (self.reply)(result);
    }
}

/// A batch's end: each member it answers, with the failure that replaces
/// its own result, and the members it never ran.
struct Settled {
    answered: Vec<(Member, Option<rusqlite::Error>)>,
    unrun: Vec<Member>,
    /// Whether the transaction reached its end without failing.
    ended: bool,
}

impl Settled {
    /// `ran` all fail with `failure`; `rest` were never run.
    fn failed(ran: Vec<Member>, failure: &rusqlite::Error, rest: Vec<Member>) -> Self {
        Self {
            answered: ran
                .into_iter()
                .map(|member| (member, Some(replicate(failure))))
                .collect(),
            unrun: rest,
            ended: false,
        }
    }
}

/// Run `members` (in queue order, at least one) as one batch on `c` and
/// answer each member it settled. Returns, in order, the members it did not
/// run, which lead the next batch.
pub(super) fn run(c: &mut Connection, mut members: Vec<Member>) -> Vec<Member> {
    let writer = Arc::clone(&members[0].writer);
    #[cfg(feature = "perf-witness")]
    let busy = lash_core_execution::perf_witness::latency::Busy::enter(false);
    #[cfg(feature = "perf-witness")]
    for member in &mut members {
        if let Some(timing) = member.timing.as_mut() {
            timing.start_service();
        }
    }
    // The batch's own statements count in its first member's window, so a
    // batch of one reports exactly what a plain write transaction does.
    let window = members[0].window.clone();
    let observed =
        members.iter().any(|member| member.window.is_some()) && !cfg!(feature = "perf-witness");
    if observed {
        crate::observed_sql::enable(c);
    }
    let settled = Window::on_worker(window, || {
        let _read_gate = gate(&writer.read_gate, "sqlite.read_gate", |gate| {
            gate.read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let _write_gate = gate(&writer.write_gate, "sqlite.write_gate", |gate| {
            gate.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        transact(c, &writer, std::mem::take(&mut members))
    });
    if observed {
        c.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
    }
    if settled.ended
        && let Some(checkpoint) = &writer.checkpoint
    {
        checkpoint.commits.fetch_add(1, Ordering::Release);
    }
    #[cfg(feature = "perf-witness")]
    drop(busy);
    for (member, failure) in settled.answered {
        member.answer(failure);
    }
    settled.unrun
}

/// Take one of the writer's gates, timing the wait apart from service.
fn gate<'g, G, T>(gate: &'g G, _site: &'static str, take: impl FnOnce(&'g G) -> T) -> T {
    #[cfg(feature = "perf-witness")]
    let mut timing = lash_core_execution::perf_witness::queues::Timer::enqueue(_site);
    let taken = timed_gate(|| take(gate));
    #[cfg(feature = "perf-witness")]
    if let Some(timing) = &mut timing {
        timing.start_service();
        timing.complete();
    }
    taken
}

/// The batch's transaction: `BEGIN IMMEDIATE`, the writer fence once, each
/// member (in a savepoint when it has siblings), then one `COMMIT`.
fn transact(c: &mut Connection, writer: &Writer, members: Vec<Member>) -> Settled {
    let mut members = members.into_iter();
    #[cfg(feature = "perf-witness")]
    let witness = lash_core_execution::perf_witness::latency::Transaction::begin();
    let solo = members.len() == 1;
    let opened = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .and_then(|tx| {
            #[cfg(feature = "perf-witness")]
            lash_core_execution::perf_witness::latency::began_write();
            let fleet = writer.fence.check(&tx)?;
            Ok((tx, fleet))
        });
    let (tx, fleet) = match opened {
        Ok(opened) => opened,
        // A refused begin or fence answers the first member with its typed
        // error; the rest lead the next batch, which fences again.
        Err(error) => {
            let first = members.next();
            return Settled {
                answered: Vec::from_iter(first.map(|member| (member, Some(error)))),
                unrun: members.collect(),
                ended: false,
            };
        }
    };
    #[cfg(feature = "testing")]
    if let Some(pauses) = writer.pauses.as_ref() {
        pauses.reach_after_fence();
    }
    let tx = FencedTx {
        operational: writer.operational,
        tx,
        fleet,
        writable: writer.fence.writable(),
    };
    let mut ran: Vec<Member> = Vec::with_capacity(members.len());
    let mut kept = 0_usize;
    let mut unrun = Vec::new();
    let rows_before = tx.total_changes();
    let rows_cap = writer.operational.write_batch_rows.get() as u64;
    while let Some(mut member) = members.next() {
        // BATCH-BOUNDS: once the batch has changed its share of rows, no
        // further member starts in it; the rest lead the next batch.
        if !ran.is_empty() && tx.total_changes().saturating_sub(rows_before) >= rows_cap {
            unrun.push(member);
            unrun.extend(members.by_ref());
            break;
        }
        if !solo && let Err(error) = tx.execute_batch("SAVEPOINT lash_batch_member") {
            ran.push(member);
            return Settled::failed(ran, &error, members.collect());
        }
        #[cfg(feature = "perf-witness")]
        let staged = witness.as_ref().map(|witness| witness.staged());
        let outcome = member.run(&tx);
        if tx.is_autocommit() {
            // SQLite ended the transaction itself (SQLITE_FULL, an I/O
            // error): the writes of every member so far went with it. The
            // member that ended it keeps its own error.
            let (failure, own) = match outcome {
                Kept::Failed(error) => (error, true),
                Kept::Writes | Kept::Nothing => (
                    storage_failure("the write transaction ended inside a batch member"),
                    false,
                ),
            };
            let mut settled = Settled::failed(ran, &failure, members.collect());
            settled.answered.push((member, (!own).then_some(failure)));
            return settled;
        }
        let undo = match outcome {
            Kept::Writes => {
                kept += 1;
                false
            }
            Kept::Nothing | Kept::Failed(_) => true,
        };
        #[cfg(feature = "perf-witness")]
        if undo && let (Some(witness), Some(staged)) = (witness.as_ref(), staged) {
            witness.undo(staged);
        }
        if !solo {
            let end = if undo {
                "ROLLBACK TO lash_batch_member; RELEASE lash_batch_member"
            } else {
                "RELEASE lash_batch_member"
            };
            if let Err(error) = tx.execute_batch(end) {
                ran.push(member);
                return Settled::failed(ran, &error, members.collect());
            }
        }
        ran.push(member);
    }
    let FencedTx { tx, .. } = tx;
    let ended = if kept == 0 {
        // Nothing to keep. SQLITE_FULL may already have rolled the
        // transaction back; the members keep their own answers then.
        if tx.is_autocommit() {
            Ok(())
        } else {
            tx.rollback()
        }
    } else {
        #[cfg(feature = "perf-witness")]
        let before = witness
            .as_ref()
            .map(|_| lash_core_execution::perf_witness::latency::Stamp::now());
        let committed = tx.commit();
        #[cfg(feature = "perf-witness")]
        if committed.is_ok()
            && let Some((witness, before)) = witness.zip(before)
        {
            witness.committed(
                before,
                lash_core_execution::perf_witness::latency::Stamp::now(),
            );
        }
        committed
    };
    match ended {
        Ok(()) => Settled {
            answered: ran.into_iter().map(|member| (member, None)).collect(),
            unrun,
            ended: true,
        },
        Err(error) => Settled::failed(ran, &error, unrun),
    }
}

/// A copy of `error` for a member that did not cause it. SQLite's own
/// failures copy exactly; anything else keeps its message.
fn replicate(error: &rusqlite::Error) -> rusqlite::Error {
    match error {
        rusqlite::Error::SqliteFailure(code, message) => {
            rusqlite::Error::SqliteFailure(*code, message.clone())
        }
        other => storage_failure(&other.to_string()),
    }
}

fn storage_failure(message: &str) -> rusqlite::Error {
    crate::sqlite_conversion_error(lash_core_execution::StoreError::StorageFailure {
        backend: crate::SQLITE_BACKEND,
        message: message.to_owned(),
    })
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
