//! One FIFO drainer per class. All execution-side work is synchronous
//! admission; retries and store invalidation stay on these workers.
//!
//! A loss the process worker can name is that process's alone: a draft the
//! ingress could not admit for its size, or one the store refused,
//! invalidates its own process. Only a loss it cannot attribute (a queue
//! that overflowed, a process it could not invalidate) invalidates them all.

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

use super::ingress::{Ingress, Work};

/// What became of a committed fact a feed asked the dispatcher to publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommittedPublication {
    /// The replay store holds the fact.
    Published,
    /// The fact was dropped, or refused, before the store held it: the
    /// process's replay does not bridge to it.
    ContinuityLost,
    /// The dispatcher stopped before it published the fact.
    Stopped,
}

/// What the ingress charges for a publication that carries no draft.
pub(super) const LOST_CHARGE: usize = 256;

pub(super) struct ProcessPublication {
    pub(super) id: ProcessId,
    /// The fact to publish. `None` stands for one too large to admit: it
    /// keeps the fact's place in the FIFO, where the worker invalidates the
    /// process.
    pub(super) draft: Option<ProcessReplayEventDraft>,
    /// Answered when a feed's reconcile waits for the fact.
    pub(super) completion: Option<tokio::sync::oneshot::Sender<CommittedPublication>>,
}

/// What a queue tells a draft it drops unpublished.
pub(super) trait Draft {
    fn lost(self);
}

impl Draft for ProcessPublication {
    fn lost(self) {
        if let Some(completion) = self.completion {
            let _ = completion.send(CommittedPublication::ContinuityLost);
        }
    }
}

impl Draft for (SessionId, LiveReplayEventDraft) {
    fn lost(self) {}
}

pub(super) struct Channel<T> {
    queue: Mutex<Ingress<T>>,
    wake: Notify,
}

impl<T: Draft> Channel<T> {
    pub(super) fn new(queue: Ingress<T>) -> Self {
        Self {
            queue: Mutex::new(queue),
            wake: Notify::new(),
        }
    }

    /// Whether the draft was admitted. A refusal loses the pending drafts
    /// with it, each told so.
    pub(super) fn enqueue(&self, charge: Option<usize>, draft: impl FnOnce() -> T) -> bool {
        let admitted = self.queue.lock_recover().enqueue(charge, draft);
        self.wake.notify_one();
        match admitted {
            Ok(()) => true,
            Err(lost) => {
                lost.into_iter().for_each(Draft::lost);
                false
            }
        }
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

    #[cfg(test)]
    pub(super) fn admitted(&self) -> usize {
        self.queue.lock_recover().admitted()
    }

    fn failed(&self) {
        let lost = self.queue.lock_recover().invalidate();
        lost.into_iter().for_each(Draft::lost);
    }

    /// Drop what a stopped worker will never publish.
    pub(super) fn stop(&self) {
        drop(self.queue.lock_recover().invalidate());
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
            Work::Publish { draft, charge } => {
                let result = publish(store.as_ref(), draft).await;
                channel.finish(charge);
                result
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "process replay lost every process's continuity");
            channel.failed();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Publish one draft. A draft the store refuses, or one that stands for a
/// fact too large to admit, costs its own process's continuity and the
/// worker carries on; the error is a process it could not invalidate.
async fn publish(
    store: &dyn ProcessReplayStore,
    publication: ProcessPublication,
) -> Result<(), lash_core::ProcessReplayStoreError> {
    let ProcessPublication {
        id,
        draft,
        completion,
    } = publication;
    if let Some(draft) = draft {
        match store.publish(&id, vec![draft]).await {
            Ok(_) => {
                if let Some(completion) = completion {
                    let _ = completion.send(CommittedPublication::Published);
                }
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(%error, process_id = %id, "process replay refused a publication");
            }
        }
    }
    let invalidated = store.invalidate_process(&id).await;
    if let Some(completion) = completion {
        let _ = completion.send(CommittedPublication::ContinuityLost);
    }
    invalidated
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

/// The ingress charge of `value`, or `None` when it alone is past
/// `max_bytes`.
pub(super) fn charge(value: &impl serde::Serialize, max_bytes: usize) -> Option<usize> {
    let mut counter = Counter {
        bytes: 256,
        max_bytes,
    };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.bytes)
}

struct Counter {
    bytes: usize,
    max_bytes: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|total| *total <= self.max_bytes)
            .ok_or_else(|| io::Error::other("language ingress byte capacity"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
