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
        self.0
            .sender
            .as_ref()
            .ok_or(tokio_rusqlite::Error::ConnectionClosed)?
            .send(command)
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }
}
