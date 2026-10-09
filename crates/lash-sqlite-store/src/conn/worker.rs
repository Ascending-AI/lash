use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use tokio::sync::oneshot;

type Work = Box<dyn FnOnce(&mut rusqlite::Connection) + Send>;

enum Command {
    Call(Work),
    Close(oneshot::Sender<rusqlite::Result<()>>),
}

struct Owner {
    sender: Option<mpsc::Sender<Command>>,
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

#[derive(Clone)]
pub(super) struct Connection(Arc<Owner>);

impl Connection {
    pub(super) async fn start(
        open: impl FnOnce() -> rusqlite::Result<rusqlite::Connection> + Send + 'static,
    ) -> rusqlite::Result<Self> {
        let (sender, receiver) = mpsc::channel();
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
                while let Ok(command) = receiver.recv() {
                    match command {
                        Command::Call(work) => work(&mut connection),
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
        let (answer, result) = mpsc::sync_channel(1);
        self.send(Command::Call(Box::new(move |connection| {
            let _ = answer.send(work(connection));
        })))?;
        result
            .recv()
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?
            .map_err(tokio_rusqlite::Error::Error)
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
                    lash_core_execution::perf_witness::queues::Timer::enqueue("sqlite.worker")
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
            close => close,
        };
        self.0
            .sender
            .as_ref()
            .ok_or(tokio_rusqlite::Error::ConnectionClosed)?
            .send(command)
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }
}

#[cfg(all(test, feature = "perf-witness"))]
mod tests {
    use super::*;
    use lash_core_execution::perf_witness::queues::Collector;
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
