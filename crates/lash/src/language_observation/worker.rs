//! One FIFO drainer per class. All execution-side work is synchronous
//! admission; retries and store invalidation stay on these workers.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{
    LiveReplayEventDraft, LiveReplayStore, ProcessReplayEventDraft, ProcessReplayStore,
    SessionRevision,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{ProcessId, SessionId};
use tokio::sync::Notify;

use super::ingress::{Ingress, MAX_BYTES, Work};

pub(super) struct ProcessPublication {
    pub(super) id: ProcessId,
    pub(super) draft: ProcessReplayEventDraft,
    pub(super) completion:
        Option<tokio::sync::oneshot::Sender<Result<(), lash_core::ProcessReplayStoreError>>>,
}

pub(super) struct Channel<T> {
    queue: Mutex<Ingress<T>>,
    wake: Notify,
}

impl<T> Channel<T> {
    pub(super) fn new(queue: Ingress<T>) -> Self {
        Self {
            queue: Mutex::new(queue),
            wake: Notify::new(),
        }
    }

    pub(super) fn enqueue(&self, charge: Option<usize>, draft: impl FnOnce() -> T) {
        self.queue.lock_recover().enqueue(charge, draft);
        self.wake.notify_one();
    }

    async fn next(&self) -> Work<T> {
        loop {
            let notified = self.wake.notified();
            if let Some(work) = self.queue.lock_recover().next() {
                return work;
            }
            notified.await;
        }
    }

    pub(super) fn has_work(&self) -> bool {
        self.queue.lock_recover().has_work()
    }

    fn failed(&self) {
        self.queue.lock_recover().invalidate();
    }
    fn finish(&self, charge: usize) {
        self.queue.lock_recover().finish(charge);
    }
}

pub(super) async fn process(
    channel: Arc<Channel<ProcessPublication>>,
    store: Arc<dyn ProcessReplayStore>,
) {
    loop {
        let result = match channel.next().await {
            Work::Invalidate => store.invalidate_all().await.map(|_| ()),
            Work::Publish {
                draft:
                    ProcessPublication {
                        id,
                        draft,
                        completion,
                    },
                charge,
            } => {
                let result = store.publish(&id, vec![draft]).await.map(|_| ());
                channel.finish(charge);
                if let Some(completion) = completion {
                    let _ = completion.send(result.clone());
                }
                result
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "process language replay publication lost continuity");
            channel.failed();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

pub(super) async fn session(
    channel: Arc<Channel<(SessionId, LiveReplayEventDraft)>>,
    store: Arc<dyn LiveReplayStore>,
) {
    loop {
        let result = match channel.next().await {
            Work::Invalidate => store.invalidate_all().await,
            Work::Publish {
                draft: (id, draft),
                charge,
            } => {
                // A provisional observation never proves a durable advance.
                let result = store
                    .publish(&id, SessionRevision::new(0), vec![draft])
                    .await
                    .map(|_| ());
                channel.finish(charge);
                result
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "session language replay publication lost continuity");
            channel.failed();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

pub(super) fn charge(value: &impl serde::Serialize) -> Option<usize> {
    let mut counter = Counter(256);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_BYTES)
            .ok_or_else(|| io::Error::other("language ingress byte capacity"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
