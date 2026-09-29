use crate::ProcessId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::watch;

type ProcessChangeEntries = Mutex<HashMap<ProcessId, Arc<watch::Sender<u64>>>>;

#[derive(Clone, Default)]
pub struct ProcessChangeHub {
    inner: Arc<ProcessChangeEntries>,
}

/// An owned process watch receiver and its registration lease.
///
/// Dropping the final subscription, including clones, immediately removes its
/// hub entry. Subscriptions do not keep the hub or its senders alive.
#[must_use = "dropping the subscription releases its registration"]
pub struct ProcessChangeSubscription {
    receiver: Option<watch::Receiver<u64>>,
    hub: Weak<ProcessChangeEntries>,
    process_id: ProcessId,
    channel: Weak<watch::Sender<u64>>,
}

impl ProcessChangeSubscription {
    /// Wait for a version bump. Re-read the registry after this returns.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.receiver_mut().changed().await
    }

    /// Whether a version bump remains unseen by this subscription.
    pub fn has_changed(&self) -> Result<bool, watch::error::RecvError> {
        match self.receiver.as_ref() {
            Some(receiver) => receiver.has_changed(),
            None => unreachable!("the receiver is only removed during drop"),
        }
    }

    /// Mark the current version as seen by this subscription.
    pub fn mark_unchanged(&mut self) {
        self.receiver_mut().mark_unchanged();
    }

    fn receiver_mut(&mut self) -> &mut watch::Receiver<u64> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver,
            None => unreachable!("the receiver is only removed during drop"),
        }
    }
}

impl Clone for ProcessChangeSubscription {
    fn clone(&self) -> Self {
        let hub = self.hub.upgrade();
        let _guard = hub.as_ref().map(|hub| hub.lock_recover());
        Self {
            receiver: self.receiver.clone(),
            hub: self.hub.clone(),
            process_id: self.process_id.clone(),
            channel: self.channel.clone(),
        }
    }
}

impl Drop for ProcessChangeSubscription {
    fn drop(&mut self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let mut guard = hub.lock_recover();
        // Receiver release and entry removal share subscribe/notify's lock.
        drop(self.receiver.take());
        if guard.get(&self.process_id).is_some_and(|sender| {
            self.channel.ptr_eq(&Arc::downgrade(sender)) && sender.receiver_count() == 0
        }) {
            guard.remove(&self.process_id);
        }
    }
}

impl ProcessChangeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns an owned subscription rather than a raw watch receiver.
    /// The receiver carries only a version counter; waiters re-read the registry
    /// after a bump and keep the subscription alive for their entire wait.
    pub fn subscribe(&self, process_id: &ProcessId) -> ProcessChangeSubscription {
        let mut guard = self.inner.lock_recover();
        let sender = guard
            .entry(process_id.clone())
            .or_insert_with(|| Arc::new(watch::channel(0).0));
        ProcessChangeSubscription {
            receiver: Some(sender.subscribe()),
            hub: Arc::downgrade(&self.inner),
            process_id: process_id.clone(),
            channel: Arc::downgrade(sender),
        }
    }

    pub fn notify(&self, process_id: &ProcessId) {
        let guard = self.inner.lock_recover();
        if let Some(sender) = guard.get(process_id) {
            sender.send_modify(|version| *version = version.wrapping_add(1));
        }
    }

    /// Number of process registrations retained by the hub, for lifecycle laws.
    #[cfg(any(test, feature = "testing"))]
    pub fn tracked_processes(&self) -> usize {
        self.inner.lock_recover().len()
    }
}
