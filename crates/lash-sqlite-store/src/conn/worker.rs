use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use super::batch::{self, Member};

type Work = Box<dyn FnOnce(&mut rusqlite::Connection) + Send>;

enum Command {
    Call(Work),
    /// A write transaction: it may share a batch with the writes queued
    /// behind it (FIG-5958).
    Write(Member),
    Close(oneshot::Sender<rusqlite::Result<()>>),
}

// One fixed ring per connection. Admission waits in the calling future,
// never on an executor thread. A dequeued command releases its slot before
// running, so writes behind the active writer can still share a commit.
const QUEUE_CAPACITY: usize = 256;

type Queued = (Command, OwnedSemaphorePermit);

struct Sender {
    commands: mpsc::SyncSender<Queued>,
    slots: Arc<Semaphore>,
}

struct Receiver {
    commands: mpsc::Receiver<Queued>,
    slots: Arc<Semaphore>,
}

impl Sender {
    async fn send(&self, command: Command) -> tokio_rusqlite::Result<()> {
        let slot = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?;
        self.enqueue(command, slot)
    }

    fn enqueue(&self, command: Command, slot: OwnedSemaphorePermit) -> tokio_rusqlite::Result<()> {
        // Each ring entry owns a permit. Holding one proves the ring has
        // room, including when several callers acquire concurrently.
        self.commands
            .try_send((command, slot))
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }

    #[cfg(test)]
    fn try_send(&self, command: Command) -> tokio_rusqlite::Result<()> {
        let slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?;
        self.enqueue(command, slot)
    }
}

impl Receiver {
    fn recv(&self) -> Result<Command, mpsc::RecvError> {
        self.commands.recv().map(|(command, _slot)| command)
    }

    fn try_recv(&self) -> Result<Command, mpsc::TryRecvError> {
        self.commands.try_recv().map(|(command, _slot)| command)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // A failed open, callback panic or close must also release callers
        // waiting for admission, even when no ring entry becomes free.
        self.slots.close();
    }
}

fn channel() -> (Sender, Receiver) {
    let (commands, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let slots = Arc::new(Semaphore::new(QUEUE_CAPACITY));
    (
        Sender {
            commands,
            slots: Arc::clone(&slots),
        },
        Receiver {
            commands: receiver,
            slots,
        },
    )
}

/// The writes queued right behind `first` that may join its batch, taken
/// in queue order without waiting: from `held` first, then the channel. The
/// first command that cannot join stays at the head of `held`.
fn gather(first: Member, held: &mut VecDeque<Command>, receiver: &Receiver) -> Vec<Member> {
    let limit = first.limit();
    let mut members = vec![first];
    while members.len() < limit {
        let next = match held.pop_front() {
            Some(next) => next,
            None => match receiver.try_recv() {
                Ok(next) => next,
                Err(_) => break,
            },
        };
        match next {
            Command::Write(member) if member.joins(&members[0]) => members.push(member),
            other => {
                held.push_front(other);
                break;
            }
        }
    }
    members
}

struct Owner {
    #[cfg(feature = "testing")]
    completion: Arc<std::sync::atomic::AtomicU64>,
    sender: Option<Sender>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Owner {
    fn join(&self) {
        let worker = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.sender.take();
        // Disconnect before joining: accepted work drains even when its
        // caller was cancelled. A handle released by its own callback cannot
        // join itself; disconnect still ends its loop after that callback.
        self.join();
    }
}

// Simulations and counted replay wait for replies without advancing their
// executor or allocating a waiter depending on which thread answers first.
#[cfg(feature = "testing")]
struct SynchronousAnswer<T> {
    completion: Arc<std::sync::atomic::AtomicU64>,
    result: Mutex<Option<Result<(T, u64), ()>>>,
    ready: std::sync::Condvar,
}

#[cfg(feature = "testing")]
pub(super) struct SynchronousReply<T>(Arc<SynchronousAnswer<T>>);

#[cfg(feature = "testing")]
pub(super) struct SynchronousResult<T>(Arc<SynchronousAnswer<T>>);

// Advance even if a callback panics, so no synchronous reply can wait for
// a disposal boundary that the worker will never reach.
#[cfg(feature = "testing")]
struct Completed(Arc<std::sync::atomic::AtomicU64>);

#[cfg(feature = "testing")]
impl Completed {
    fn advance(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(feature = "testing")]
impl Drop for Completed {
    fn drop(&mut self) {
        self.advance();
    }
}

#[cfg(feature = "testing")]
impl<T> SynchronousReply<T> {
    pub(super) fn send(self, result: T) {
        *self
            .0
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok((
            result,
            self.0.completion.load(std::sync::atomic::Ordering::Relaxed),
        )));
    }
}

#[cfg(feature = "testing")]
impl<T> Drop for SynchronousReply<T> {
    fn drop(&mut self) {
        let mut result = self
            .0
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A dropped command or a panicked callback must release its caller.
        if result.is_none() {
            *result = Some(Err(()));
        }
        self.0.ready.notify_one();
    }
}

#[cfg(feature = "testing")]
impl<T> SynchronousResult<T> {
    pub(super) fn recv(self) -> Result<T, ()> {
        let mut result = self
            .0
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while result.is_none() {
            result = self
                .0
                .ready
                .wait(result)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let (value, epoch) = result.take().ok_or(())??;
        drop(result);
        // The entire command, including a write batch's temporary vectors,
        // must be disposed before the counted fixture cuts retained bytes.
        while self.0.completion.load(std::sync::atomic::Ordering::Acquire) == epoch {
            std::thread::yield_now();
        }
        Ok(value)
    }
}

#[derive(Clone)]
pub(super) struct Connection(Arc<Owner>);

impl Connection {
    pub(super) async fn start(
        open: impl FnOnce() -> rusqlite::Result<rusqlite::Connection> + Send + 'static,
    ) -> rusqlite::Result<Self> {
        let (sender, receiver) = channel();
        let (opened, ready) = oneshot::channel();
        #[cfg(feature = "testing")]
        let completion = Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(feature = "testing")]
        let completed = Completed(Arc::clone(&completion));
        let worker = thread::Builder::new()
            .name("lash-sqlite".into())
            .spawn(move || {
                let mut connection = match open() {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = opened.send(Err(error));
                        return;
                    }
                };
                if opened.send(Ok(())).is_err() {
                    return;
                }
                let mut held = VecDeque::new();
                while let Some(command) = held.pop_front().or_else(|| receiver.recv().ok()) {
                    match command {
                        Command::Call(work) => work(&mut connection),
                        Command::Write(first) => {
                            let members = gather(first, &mut held, &receiver);
                            for unrun in batch::run(&mut connection, members).into_iter().rev() {
                                held.push_front(Command::Write(unrun));
                            }
                        }
                        Command::Close(answer) => match connection.close() {
                            Ok(()) => {
                                let _ = answer.send(Ok(()));
                                return;
                            }
                            Err((still_open, error)) => {
                                connection = still_open;
                                let _ = answer.send(Err(error));
                            }
                        },
                    }
                    #[cfg(feature = "testing")]
                    completed.advance();
                }
            })
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let connection = Self(Arc::new(Owner {
            #[cfg(feature = "testing")]
            completion,
            sender: Some(sender),
            thread: Mutex::new(Some(worker)),
        }));
        ready
            .await
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))??;
        Ok(connection)
    }

    pub(super) async fn call<T, F>(&self, work: F) -> tokio_rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (answer, result) = oneshot::channel();
        self.send(Command::Call(Box::new(move |connection| {
            let _ = answer.send(work(connection));
        })))
        .await?;
        result
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?
            .map_err(tokio_rusqlite::Error::Error)
    }

    /// [`Self::call`], blocking the caller's thread until the connection
    /// thread answers.
    #[cfg(feature = "testing")]
    pub(super) async fn call_synchronous<T, F>(&self, work: F) -> tokio_rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (answer, result) = self.synchronous_reply();
        self.send(Command::Call(Box::new(move |connection| {
            answer.send(work(connection));
        })))
        .await?;
        result
            .recv()
            .map_err(|()| tokio_rusqlite::Error::ConnectionClosed)?
            .map_err(tokio_rusqlite::Error::Error)
    }

    #[cfg(feature = "testing")]
    pub(super) fn synchronous_reply<T>(&self) -> (SynchronousReply<T>, SynchronousResult<T>) {
        let answer = Arc::new(SynchronousAnswer {
            completion: Arc::clone(&self.0.completion),
            result: Mutex::new(None),
            ready: std::sync::Condvar::new(),
        });
        (
            SynchronousReply(Arc::clone(&answer)),
            SynchronousResult(answer),
        )
    }

    /// Queue `member` as a write transaction; it answers through its own
    /// reply once its batch is settled.
    pub(super) async fn write(&self, member: Member) -> tokio_rusqlite::Result<()> {
        self.send(Command::Write(member)).await
    }

    #[cfg(test)]
    pub(super) fn try_write(&self, member: Member) -> tokio_rusqlite::Result<()> {
        self.0
            .sender
            .as_ref()
            .ok_or(tokio_rusqlite::Error::ConnectionClosed)?
            .try_send(Command::Write(member))
    }

    pub(super) async fn close(self) -> tokio_rusqlite::Result<()> {
        let (answer, result) = oneshot::channel();
        if self.send(Command::Close(answer)).await.is_err() {
            self.0.join();
            return Ok(());
        }
        match result.await {
            Ok(Err(error)) => Err(tokio_rusqlite::Error::Error(error)),
            Ok(Ok(())) | Err(_) => {
                self.0.join();
                Ok(())
            }
        }
    }

    async fn send(&self, command: Command) -> tokio_rusqlite::Result<()> {
        // Capture before enqueue, transfer the clock with the work, and close
        // it on the worker even if the awaiting caller was cancelled.
        #[cfg(feature = "perf-witness")]
        let command = match command {
            Command::Call(work) => {
                if let Some(mut timing) =
                    lash_core_ids::perf_witness::queues::Timer::enqueue("sqlite.worker")
                {
                    Command::Call(Box::new(move |connection| {
                        timing.start_service();
                        {
                            let _scope = timing.worker_scope();
                            work(connection);
                        }
                        timing.complete();
                    }))
                } else {
                    Command::Call(work)
                }
            }
            other => other,
        };
        self.0
            .sender
            .as_ref()
            .ok_or(tokio_rusqlite::Error::ConnectionClosed)?
            .send(command)
            .await
    }
}

#[cfg(all(test, feature = "perf-witness"))]
mod tests {
    use super::*;
    use lash_core_ids::perf_witness::queues::Collector;
    use std::future::Future;

    // FIG-5663: work accepted behind a blocked writer belongs to queue wait,
    // not service. Channel handshakes establish the backlog without a race.
    #[tokio::test]
    async fn writer_backlog_is_queue_wait_not_service() {
        let connection = Connection::start(rusqlite::Connection::open_in_memory)
            .await
            .expect("open connection");
        let worker = connection
            .0
            .thread
            .lock()
            .expect("worker handle")
            .as_ref()
            .expect("worker running")
            .thread()
            .id();
        let collector = Collector::install_for_thread(worker).expect("install queue witness");
        let (entered, blocked) = oneshot::channel();
        let (release, released) = mpsc::channel();
        connection
            .send(Command::Call(Box::new(move |_| {
                entered.send(()).expect("signal blocked writer");
                released.recv().expect("release writer");
            })))
            .await
            .expect("block worker");
        blocked.await.expect("worker entered");
        let reader = connection.clone();
        let mut second =
            Box::pin(reader.call(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))));
        std::future::poll_fn(|cx| {
            assert!(second.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let backlog_started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let forced_wait = backlog_started.elapsed();
        release.send(()).expect("release");
        assert_eq!(second.await.expect("read"), 1);
        connection.close().await.expect("close worker");
        let snapshot = collector.snapshot();
        let record = snapshot
            .records
            .iter()
            .rfind(|r| r.site == "sqlite.worker")
            .expect("queued operation must have a receipt");
        assert!(u128::from(record.queue_wait_ns) >= forced_wait.as_nanos());
        assert_eq!(record.elapsed_ns, record.queue_wait_ns + record.service_ns);
        assert!(record.completed);
    }
}

#[cfg(test)]
mod drop_tests {
    use crate::conn::tests::installed;
    use crate::conn::{SqliteConnectionPolicy, SqliteSynchronous};
    use crate::location::DatabaseTarget;

    async fn rows(connection: &crate::conn::SqliteConnection) -> i64 {
        connection
            .call(|c| c.query_row("SELECT COUNT(*) FROM swept", [], |row| row.get(0)))
            .await
            .expect("count the swept rows")
    }

    /// Ownership at the claim (FIG-3975): a write is the connection thread's
    /// from the moment it is queued, and the write gate is held only there,
    /// so a write future dropped at each await either never queued or commits
    /// whole, and the next write proceeds behind it.
    #[tokio::test]
    async fn a_write_dropped_at_each_await_commits_whole_or_not_at_all() {
        let dir = tempfile::tempdir().expect("sweep tempdir");
        let connection = installed(
            &DatabaseTarget::File(dir.path().join("core.db")),
            SqliteConnectionPolicy::standard(SqliteSynchronous::Normal),
        )
        .await;
        connection
            .write(|tx| tx.execute_batch("CREATE TABLE swept (n INTEGER)"))
            .await
            .expect("create the swept table");
        lash_core_ids::cancel_sweep::sweep_drops(
            async || {
                let before = rows(&connection).await;
                let writer = connection.clone();
                let write = async move {
                    writer
                        .write(|tx| {
                            tx.execute_batch(
                                "INSERT INTO swept VALUES (1); INSERT INTO swept VALUES (2)",
                            )
                        })
                        .await
                        .expect("the swept write");
                };
                (before, write)
            },
            async |before, cut| {
                connection.write(|_| Ok(())).await.unwrap_or_else(|error| {
                    panic!("the next write proceeds once the write is {cut}: {error}")
                });
                let committed = if cut.polls == 0 { 0 } else { 2 };
                assert_eq!(
                    rows(&connection).await,
                    before + committed,
                    "the write committed whole or not at all once it is {cut}"
                );
            },
        )
        .await;
    }
}
