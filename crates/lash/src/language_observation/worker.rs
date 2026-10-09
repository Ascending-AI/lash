//! One drainer per class. All execution-side work is synchronous admission;
//! retries and store invalidation stay on these workers. A worker takes
//! everything pending at once; the process worker publishes it through its
//! store's batch API, one process's drafts in order.
//!
//! A loss the process worker can name is that process's alone: a draft the
//! ingress could not admit for its size, or one the store refused,
//! invalidates its own process. Only a loss it cannot attribute (a queue
//! that overflowed, a process it could not invalidate) invalidates them all.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use lash_core::{
    LiveReplayEventDraft, LiveReplayStore, ProcessReplayEventDraft, ProcessReplayStore,
    ProcessSequence, SessionRevision,
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
    /// A commit's own publication holds its process's publication window
    /// open until the store answered for this draft, or the draft was
    /// dropped unpublished.
    pub(super) mark: Option<ProcessPublicationMark>,
}

pub(crate) type ProcessPublicationMarks =
    lash_core::runtime::durable::services::PublicationMarks<ProcessId, ProcessSequence>;
pub(super) type ProcessPublicationMark =
    lash_core::runtime::durable::services::PublicationMark<ProcessId, ProcessSequence>;

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

    #[cfg(test)]
    pub(super) fn pending(&self) -> usize {
        self.queue.lock_recover().pending()
    }

    fn failed(&self) {
        let lost = self.queue.lock_recover().invalidate();
        lost.into_iter().for_each(Draft::lost);
    }

    /// Drop what a stopped worker will never publish.
    pub(super) fn stop(&self) {
        drop(self.queue.lock_recover().invalidate());
    }

    fn finish(&self, events: usize, charge: usize) {
        self.queue.lock_recover().finish(events, charge);
    }
}

pub(super) async fn process(
    channel: Arc<Channel<ProcessPublication>>,
    store: Arc<dyn ProcessReplayStore>,
) {
    let limits = store.publish_limits();
    loop {
        let result = match channel.next().await {
            Work::Invalidate => store.invalidate_all().await.map(|_| ()),
            Work::Publish(pending) => {
                // Order matters only within one process: each process's
                // drafts stay in admission order, and the processes publish
                // side by side, as many at once as the store writes.
                futures_util::stream::iter(by_process(pending))
                    .map(|(id, drafts)| {
                        publish_process(&channel, store.as_ref(), limits, id, drafts)
                    })
                    .buffer_unordered(limits.concurrency.get())
                    .fold(
                        Ok(()),
                        |round, published| async move { round.and(published) },
                    )
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "process replay lost every process's continuity");
            channel.failed();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

type Charged = (ProcessPublication, usize);

/// One round's drafts grouped by process, each group in admission order.
fn by_process(pending: Vec<Charged>) -> Vec<(ProcessId, Vec<Charged>)> {
    let mut groups = Vec::<(ProcessId, Vec<Charged>)>::new();
    let mut index = HashMap::<ProcessId, usize>::new();
    for charged in pending {
        let group = *index.entry(charged.0.id.clone()).or_insert_with(|| {
            groups.push((charged.0.id.clone(), Vec::new()));
            groups.len() - 1
        });
        groups[group].1.push(charged);
    }
    groups
}

/// Publish one process's drafts in order, in batches the store takes whole.
/// A batch the store refuses, or a publication that stands for a fact too
/// large to admit, costs the process's continuity in its place in the FIFO
/// and the worker carries on behind it; the error is a process it could not
/// invalidate, which ends the round for the process.
async fn publish_process(
    channel: &Channel<ProcessPublication>,
    store: &dyn ProcessReplayStore,
    limits: lash_core::ProcessReplayPublishLimits,
    id: ProcessId,
    drafts: Vec<Charged>,
) -> Result<(), lash_core::ProcessReplayStoreError> {
    let mut drafts = drafts.into_iter().peekable();
    let mut round = Ok(());
    while drafts.peek().is_some() {
        let mut batch = Vec::new();
        // What outlives each draft's store call: an acknowledgement, answered
        // once the store answered and a refusal invalidated the process,
        // and a publication mark, held until then so a feed that waited on
        // it reads the outcome.
        let mut waiting = Vec::new();
        let (mut events, mut bytes) = (0_usize, 0_usize);
        // A publication without a draft ends the batch before it: the
        // process is invalidated in that fact's place.
        let mut unadmitted = false;
        while let Some((next, charge)) = drafts.peek() {
            if !batch.is_empty()
                && (batch.len() >= limits.batch_events.get()
                    || bytes.saturating_add(*charge) > limits.batch_bytes.get())
            {
                break;
            }
            if next.draft.is_none() && !batch.is_empty() {
                break;
            }
            let Some((publication, charge)) = drafts.next() else {
                break;
            };
            // Exhaustive: a field added to a publication is placed before
            // or after the store call here.
            let ProcessPublication {
                id: _,
                draft,
                completion,
                mark,
            } = publication;
            events += 1;
            bytes += charge;
            waiting.push((completion, mark));
            match draft {
                Some(draft) => batch.push(draft),
                None => {
                    unadmitted = true;
                    break;
                }
            }
        }
        let published = match &round {
            Err(_) => false,
            Ok(()) if unadmitted => false,
            Ok(()) => match store.publish(&id, batch).await {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(%error, process_id = %id, "process replay refused a publication");
                    false
                }
            },
        };
        if !published && round.is_ok() {
            round = store.invalidate_process(&id).await;
        }
        channel.finish(events, bytes);
        let answer = if published {
            CommittedPublication::Published
        } else {
            CommittedPublication::ContinuityLost
        };
        for (completion, mark) in waiting {
            if let Some(completion) = completion {
                let _ = completion.send(answer);
            }
            drop(mark);
        }
    }
    round
}

pub(super) async fn session(
    channel: Arc<Channel<(SessionId, LiveReplayEventDraft)>>,
    store: Arc<dyn LiveReplayStore>,
) {
    loop {
        let result = match channel.next().await {
            Work::Invalidate => store.invalidate_all().await,
            Work::Publish(pending) => {
                let mut round = Ok(());
                for ((id, draft), charge) in pending {
                    if round.is_ok() {
                        // A provisional observation never proves a durable
                        // advance.
                        round = store
                            .publish(&id, SessionRevision::new(0), vec![draft])
                            .await
                            .map(|_| ());
                    }
                    channel.finish(1, charge);
                }
                round
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
