use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use tokio::sync::oneshot;

use super::batch::{self, Member};

type Work = Box<dyn FnOnce(&mut rusqlite::Connection) + Send>;

enum Command {
    Call(Work),
    #[cfg(feature = "testing")]
    Inline(Work, Arc<dyn std::any::Any + Send + Sync>),
    /// A write transaction: it may share a batch with the writes queued
    /// behind it (FIG-5958).
    Write(Member),
    Close(oneshot::Sender<rusqlite::Result<()>>),
}

enum Sender {
    Queued(mpsc::Sender<Command>),
    #[cfg(feature = "testing")]
    Inline(mpsc::SyncSender<Command>),
}

impl Sender {
    fn send(&self, command: Command) -> tokio_rusqlite::Result<()> {
        match self {
            Self::Queued(sender) => sender.send(command),
            #[cfg(feature = "testing")]
            Self::Inline(sender) => sender.send(command),
        }
        .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }
}

fn channel(#[cfg(feature = "testing")] inline_calls: bool) -> (Sender, mpsc::Receiver<Command>) {
    #[cfg(feature = "testing")]
    if inline_calls {
        // Inline issuing waits for each answer before sending the next call.
        // One reusable slot avoids unbounded-channel block allocation cycles
        // moving with the number of calls in a counted fixture's cold turn.
        let (sender, receiver) = mpsc::sync_channel(1);
        return (Sender::Inline(sender), receiver);
    }
    let (sender, receiver) = mpsc::channel();
    (Sender::Queued(sender), receiver)
}

/// The writes queued right behind `first` that may join its batch, taken
/// in queue order without waiting: from `held` first, then the channel. The
/// first command that cannot join stays at the head of `held`.
fn gather(
    first: Member,
    held: &mut VecDeque<Command>,
    receiver: &mpsc::Receiver<Command>,
) -> Vec<Member> {
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

// The counted fixture blocks its issuing thread, but must not allocate a
// channel waiter only when the worker loses the race to answer.
#[cfg(feature = "testing")]
struct InlineAnswer<T> {
    result: Mutex<Option<Result<T, ()>>>,
    ready: std::sync::Condvar,
}

#[cfg(feature = "testing")]
pub(super) struct InlineReply<T>(Arc<InlineAnswer<T>>);

#[cfg(feature = "testing")]
pub(super) struct InlineResult<T>(Arc<InlineAnswer<T>>);

#[cfg(feature = "testing")]
pub(super) fn inline_reply<T>() -> (InlineReply<T>, InlineResult<T>) {
    let answer = Arc::new(InlineAnswer {
        result: Mutex::new(None),
        ready: std::sync::Condvar::new(),
    });
    (InlineReply(Arc::clone(&answer)), InlineResult(answer))
}

#[cfg(feature = "testing")]
impl<T> InlineReply<T> {
    pub(super) fn send(self, result: T) {
        *self
            .0
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(result));
    }
}

#[cfg(feature = "testing")]
impl<T> Drop for InlineReply<T> {
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
impl<T> InlineResult<T> {
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
        let value = result.take().ok_or(())?;
        drop(result);
        // The worker's last owner is released after the boxed command and
        // its captures. A reply alone precedes that disposal.
        while Arc::strong_count(&self.0) != 1 {
            std::thread::yield_now();
        }
        value
    }
}

#[cfg(feature = "testing")]
impl<T: Send + 'static> InlineResult<T> {
    pub(super) fn disposal(&self) -> Arc<dyn std::any::Any + Send + Sync> {
        self.0.clone()
    }
}

#[derive(Clone)]
pub(super) struct Connection(Arc<Owner>);

impl Connection {
    pub(super) async fn start(
        open: impl FnOnce() -> rusqlite::Result<rusqlite::Connection> + Send + 'static,
    ) -> rusqlite::Result<Self> {
        Self::start_configured(
            open,
            #[cfg(feature = "testing")]
            false,
        )
        .await
    }

    pub(super) async fn start_configured(
        open: impl FnOnce() -> rusqlite::Result<rusqlite::Connection> + Send + 'static,
        #[cfg(feature = "testing")] inline_calls: bool,
    ) -> rusqlite::Result<Self> {
        let (sender, receiver) = channel(
            #[cfg(feature = "testing")]
            inline_calls,
        );
        let (opened, ready) = oneshot::channel();
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
                        #[cfg(feature = "testing")]
                        Command::Inline(work, disposal) => {
                            work(&mut connection);
                            drop(disposal);
                        }
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
                }
            })
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let connection = Self(Arc::new(Owner {
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
        })))?;
        result
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?
            .map_err(tokio_rusqlite::Error::Error)
    }

    /// [`Self::call`], blocking the caller's thread until the connection
    /// thread answers.
    #[cfg(feature = "testing")]
    pub(super) fn call_inline<T, F>(&self, work: F) -> tokio_rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (answer, result) = inline_reply();
        self.send(Command::Inline(
            Box::new(move |connection| {
                answer.send(work(connection));
            }),
            result.disposal(),
        ))?;
        result
            .recv()
            .map_err(|()| tokio_rusqlite::Error::ConnectionClosed)?
            .map_err(tokio_rusqlite::Error::Error)
    }

    /// Queue `member` as a write transaction; it answers through its own
    /// reply once its batch is settled.
    pub(super) fn write(&self, member: Member) -> tokio_rusqlite::Result<()> {
        self.send(Command::Write(member))
    }

    #[cfg(feature = "testing")]
    pub(super) fn write_inline(
        &self,
        member: Member,
        disposal: Arc<dyn std::any::Any + Send + Sync>,
    ) -> tokio_rusqlite::Result<()> {
        self.send(Command::Inline(
            Box::new(move |connection| {
                let mut members = vec![member];
                while !members.is_empty() {
                    members = batch::run(connection, members);
                }
            }),
            disposal,
        ))
    }

    pub(super) async fn close(self) -> tokio_rusqlite::Result<()> {
        let (answer, result) = oneshot::channel();
        if self.send(Command::Close(answer)).is_err() {
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

    fn send(&self, command: Command) -> tokio_rusqlite::Result<()> {
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

    /// INLINE-REPLY: a panicked worker callback releases its blocked caller,
    /// preserving the channel-disconnection behavior of the counted fixture.
    #[tokio::test]
    async fn an_inline_callback_panic_releases_its_caller() {
        let connection =
            super::Connection::start_configured(rusqlite::Connection::open_in_memory, true)
                .await
                .expect("open inline connection");
        let result = connection.call_inline::<(), _>(|_| panic!("failed callback"));
        assert!(matches!(
            result,
            Err(tokio_rusqlite::Error::ConnectionClosed)
        ));
        connection.close().await.expect("join the failed worker");
    }

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
