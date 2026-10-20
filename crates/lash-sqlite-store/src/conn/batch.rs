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
//! fails every member, which their owners treat as a crash.
//!
//! SQLite may end the whole transaction on an error inside a statement
//! (`SQLITE_FULL`, an I/O error, `SQLITE_NOMEM`), not just the member's
//! savepoint, so the batch checks for autocommit after every member and
//! never assumes the savepoint survived (FIG-5968). The writes of every
//! member so far are then gone. A member keeps its own answer when it kept
//! no writes and no member before it kept any: its refusal or error was
//! decided against committed state alone, and stays true. The member whose
//! own error ended the transaction keeps that error too. Every other member
//! that ran fails with a storage failure naming the ended transaction, so
//! none is answered committed; the members not yet run lead the next batch,
//! untouched. Bodies run once, so a lost member is failed, not re-run.
//!
//! A member gets exactly the answer its solo commit would have got, and
//! never changes a sibling's (FIG-5970). SQLite checks a deferred foreign
//! key only at the outermost `COMMIT`, not at a member's `RELEASE`, so a
//! kept member that leaves one violated would fail its siblings' `COMMIT`,
//! or be healed by a later sibling and answered committed. After each kept
//! member with siblings the batch reads `SQLITE_DBSTATUS_DEFERRED_FKS`: none
//! is pending when a member starts, so one pending then is that member's.
//! It is undone and answered the `SQLITE_CONSTRAINT_FOREIGNKEY` failure its
//! solo `COMMIT` would answer. A member whose body panics is that member's
//! failure: its savepoint (or a lone write's transaction) is undone and it
//! answers a storage failure naming the panic, while its siblings run on and
//! the connection thread keeps serving.
//!
//! The synchronous level is the connection's, fixed at open and never
//! changed per batch: SQLite refuses to change it inside a transaction.

use std::sync::{Arc, Mutex, RwLock};

use lash_core_ids::operational_metrics::sql::Window;
use rusqlite::{Connection, TransactionBehavior};

use super::{FencedTx, TxOutcome, WriterFence, timed_gate};
#[cfg(feature = "perf-witness")]
use crate::observed_sql::WriteWitness as Writes;

/// Without the perf witness, a batch counts no writes.
#[cfg(not(feature = "perf-witness"))]
struct Writes;

#[cfg(not(feature = "perf-witness"))]
impl Writes {
    fn start(_: &Connection) -> Self {
        Self
    }

    fn member(&mut self, _: &Connection) {}

    fn finish(self, _: &Connection) {}
}

/// Everything a queued write needs on the connection thread, shared by the
/// connection's clones: writes on one writer may share a transaction.
pub(super) struct Writer {
    pub(super) configs: Arc<lash_core_store::store_backend_support::ConfigCache>,
    pub(super) operational: crate::SqliteOperationalSettings,
    pub(super) write_gate: Arc<Mutex<()>>,
    pub(super) read_gate: Arc<RwLock<()>>,
    pub(super) serializes_readers: bool,
    pub(super) fence: Arc<WriterFence>,
    pub(super) node_wakes: Mutex<Option<crate::durable::NodeWakeWriter>>,
    #[cfg(feature = "testing")]
    pub(super) pauses: Option<crate::testing::SqlitePauses>,
}

/// A WAL writer shares snapshots; a memory writer must wait for every
/// reader before BEGIN because its rollback journal cannot commit over one.
pub(super) enum ReaderAccess<'a> {
    Wal {
        _guard: std::sync::RwLockReadGuard<'a, ()>,
    },
    Memory {
        _guard: std::sync::RwLockWriteGuard<'a, ()>,
    },
}

impl Writer {
    pub(super) fn access(&self) -> ReaderAccess<'_> {
        if self.serializes_readers {
            ReaderAccess::Memory {
                _guard: self
                    .read_gate
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            }
        } else {
            ReaderAccess::Wal {
                _guard: self
                    .read_gate
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            }
        }
    }
}

/// One queued write transaction: a batch member.
pub(crate) struct Member {
    writer: Arc<Writer>,
    window: Option<Window>,
    body: Box<dyn Body>,
    #[cfg(feature = "perf-witness")]
    origin: &'static str,
    #[cfg(feature = "perf-witness")]
    timing: Option<lash_core_ids::perf_witness::queues::Timer>,
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
            #[cfg(feature = "perf-witness")]
            origin: std::any::type_name::<F>(),
            window: Window::current(),
            body: Box::new(Typed {
                body: Some(f),
                result: None,
                reply,
            }),
            #[cfg(feature = "perf-witness")]
            timing: lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.worker"),
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

    /// Replace the member's own result with `error`, the refusal its solo
    /// commit would have answered.
    fn refuse(&mut self, error: rusqlite::Error) {
        self.body.refuse(error);
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
    /// It failed with an error, which it keeps to answer with: its
    /// savepoint is undone.
    Failed,
}

trait Body: Send {
    fn run(&mut self, tx: &FencedTx<'_>) -> Kept;
    fn refuse(&mut self, error: rusqlite::Error);
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
        // A panic is this member's failure alone: unwinding further would
        // end the connection thread and drop every member's answer.
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::observed_sql::with_origin(std::any::type_name::<F>(), || body(tx))
        }));
        let (result, kept) = match ran {
            Ok(Ok(TxOutcome::Commit(value))) => (Ok(value), Kept::Writes),
            Ok(Ok(TxOutcome::Rollback(value))) => (Ok(value), Kept::Nothing),
            Ok(Err(error)) => (Err(error), Kept::Failed),
            Err(panic) => (
                Err(storage_failure(&format!(
                    "a write transaction panicked: {}",
                    panic_message(panic.as_ref())
                ))),
                Kept::Failed,
            ),
        };
        self.result = Some(result);
        kept
    }

    fn refuse(&mut self, error: rusqlite::Error) {
        self.result = Some(Err(error));
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

/// A member that ran, and whether its own answer survives the end of the
/// transaction: it kept no writes and none before it did, so its answer was
/// decided against committed state alone.
struct Ran {
    member: Member,
    standalone: bool,
}

/// A batch's end: each member it answers, with the failure that replaces
/// its own result, and the members it never ran.
struct Settled {
    answered: Vec<(Member, Option<rusqlite::Error>)>,
    unrun: Vec<Member>,
}

impl Settled {
    /// `ran` all fail with `failure`; `rest` were never run.
    fn failed(ran: Vec<Ran>, failure: &rusqlite::Error, rest: Vec<Member>) -> Self {
        Self {
            answered: ran
                .into_iter()
                .map(|ran| (ran.member, Some(replicate(failure))))
                .collect(),
            unrun: rest,
        }
    }

    /// SQLite ended the transaction itself: the standalone members of `ran`
    /// keep their own answers, the rest fail; `rest` were never run.
    fn ended(ran: Vec<Ran>, rest: Vec<Member>) -> Self {
        Self {
            answered: ran
                .into_iter()
                .map(|ran| {
                    let failure = (!ran.standalone).then(|| {
                        storage_failure("the write transaction ended inside a batch member")
                    });
                    (ran.member, failure)
                })
                .collect(),
            unrun: rest,
        }
    }
}

/// Run `members` (in queue order, at least one) as one batch on `c` and
/// answer each member it settled. Returns, in order, the members it did not
/// run, which lead the next batch.
pub(super) fn run(c: &mut Connection, mut members: Vec<Member>) -> Vec<Member> {
    #[cfg(feature = "perf-witness")]
    let origin = if members.len() == 1 {
        members[0].origin
    } else {
        "sqlite.shared_batch_commit"
    };
    #[cfg(not(feature = "perf-witness"))]
    let origin = "";
    let writer = Arc::clone(&members[0].writer);
    #[cfg(feature = "perf-witness")]
    let busy = lash_core_ids::perf_witness::latency::Busy::enter(false);
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
        crate::observed_sql::physical_scope(origin, || {
            let _read_gate = gate(writer.as_ref(), "sqlite.read_gate", Writer::access);
            let _write_gate = gate(&writer.write_gate, "sqlite.write_gate", |gate| {
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            // The witness counts each member's rows as it runs and the pages
            // the batch wrote, so a batch counts what its members' plain
            // transactions did (FIG-5968).
            let mut writes = Writes::start(c);
            let settled = transact(c, &writer, std::mem::take(&mut members), &mut writes);
            writes.finish(c);
            settled
        })
    });
    if observed {
        c.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
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
    let mut timing = lash_core_ids::perf_witness::queues::Timer::enqueue(_site);
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
fn transact(
    c: &mut Connection,
    writer: &Writer,
    members: Vec<Member>,
    writes: &mut Writes,
) -> Settled {
    let mut members = members.into_iter();
    #[cfg(feature = "perf-witness")]
    let witness = lash_core_ids::perf_witness::latency::Transaction::begin();
    let solo = members.len() == 1;
    let opened = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .and_then(|tx| {
            #[cfg(feature = "perf-witness")]
            lash_core_ids::perf_witness::latency::began_write();
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
            };
        }
    };
    #[cfg(feature = "testing")]
    if let Some(pauses) = writer.pauses.as_ref() {
        pauses.reach_after_fence();
    }
    let tx = FencedTx {
        configs: Arc::clone(&writer.configs),
        operational: writer.operational,
        tx,
        fleet,
        writable: writer.fence.writable(),
    };
    let mut ran: Vec<Ran> = Vec::with_capacity(members.len());
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
            ran.push(Ran {
                member,
                standalone: false,
            });
            return Settled::failed(ran, &error, members.collect());
        }
        #[cfg(feature = "perf-witness")]
        let staged = witness.as_ref().map(|witness| witness.staged());
        let mut outcome = member.run(&tx);
        if matches!(outcome, Kept::Writes) && !tx.is_autocommit() {
            let wakes = writer
                .node_wakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(wakes) = wakes.as_ref()
                && let Err(error) = wakes.flush(&tx)
            {
                member.refuse(error);
                outcome = Kept::Failed;
            }
        }
        writes.member(&tx);
        if tx.is_autocommit() {
            // SQLite ended the transaction itself (SQLITE_FULL, an I/O
            // error): the writes of every member so far went with it. A
            // member whose error ended it keeps that error; one that kept
            // nothing keeps its answer while no member before it kept writes.
            let standalone = match outcome {
                Kept::Failed => true,
                Kept::Nothing => kept == 0,
                Kept::Writes => false,
            };
            ran.push(Ran { member, standalone });
            return Settled::ended(ran, members.collect());
        }
        // A kept member with siblings may not leave a deferred foreign key
        // violated for the shared `COMMIT` to find: it is refused as its
        // solo `COMMIT` would be.
        let outcome = match outcome {
            Kept::Writes if !solo => match deferred_violation(&tx) {
                Ok(false) => Kept::Writes,
                Ok(true) => {
                    member.refuse(foreign_key_failure());
                    Kept::Failed
                }
                Err(error) => {
                    ran.push(Ran {
                        member,
                        standalone: false,
                    });
                    return Settled::failed(ran, &error, members.collect());
                }
            },
            outcome => outcome,
        };
        let standalone = kept == 0 && !matches!(outcome, Kept::Writes);
        let undo = match outcome {
            Kept::Writes => {
                kept += 1;
                false
            }
            Kept::Nothing | Kept::Failed => true,
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
                ran.push(Ran { member, standalone });
                return Settled::failed(ran, &error, members.collect());
            }
        }
        ran.push(Ran { member, standalone });
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
            .map(|_| lash_core_ids::perf_witness::latency::Stamp::now());
        let committed = tx.commit();
        #[cfg(feature = "perf-witness")]
        if committed.is_ok()
            && let Some((witness, before)) = witness.zip(before)
        {
            witness.committed(before, lash_core_ids::perf_witness::latency::Stamp::now());
        }
        committed
    };
    match ended {
        Ok(()) => Settled {
            answered: ran.into_iter().map(|ran| (ran.member, None)).collect(),
            unrun,
        },
        Err(error) => Settled::failed(ran, &error, unrun),
    }
}

/// Whether the transaction holds a foreign key violation SQLite defers to
/// `COMMIT` (`SQLITE_DBSTATUS_DEFERRED_FKS`). `ROLLBACK TO` restores the
/// count a savepoint started with.
fn deferred_violation(c: &Connection) -> rusqlite::Result<bool> {
    let mut current = 0;
    let mut highwater = 0;
    // SAFETY: the handle is this live connection's, used on its own thread
    // for one status read that neither retains nor closes it.
    #[expect(unsafe_code, reason = "rusqlite exposes no safe sqlite3_db_status")]
    let status = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            c.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_DEFERRED_FKS,
            &mut current,
            &mut highwater,
            0,
        )
    };
    if status == rusqlite::ffi::SQLITE_OK {
        Ok(current != 0)
    } else {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(status),
            None,
        ))
    }
}

/// The refusal a lone write's `COMMIT` answers when it would leave a
/// deferred foreign key violated.
fn foreign_key_failure() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
        Some("FOREIGN KEY constraint failed".to_owned()),
    )
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "a non-string payload"
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
    crate::sqlite_conversion_error(lash_core_store::store::StoreError::StorageFailure {
        backend: crate::SQLITE_BACKEND,
        message: message.to_owned(),
    })
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
