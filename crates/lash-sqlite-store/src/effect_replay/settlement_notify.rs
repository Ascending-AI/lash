//! Process-wide settlement notifiers for the SQLite journal.
//!
//! SQLite has no `NOTIFY`, so the cross-host wake is a shared registry: every
//! rank write ([`EffectReplayRowStore::discharge_child`],
//! [`EffectReplayRowStore::decide_cancel`], a grouped
//! [`EffectReplayRowStore::finalize`]) wakes the notifier for
//! (deployment, group) after its commit, and two hosts on the same deployment
//! in one process share that notifier because the registry keys on the
//! deployment's identity rather than the connection.
//!
//! [`EffectReplayRowStore`]: lash_core_execution::facade_support::effect_replay_driver::EffectReplayRowStore

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use tokio::sync::Notify;

/// What a notifier belongs to, beneath the group key: the identity of the
/// deployment whose journal the group lives in (`sqlite:<canonical path>` or
/// `sqlite-memory:<id>`), the same identity the turn-control binding is keyed
/// on. Two hosts opened on one deployment through different spellings share
/// one notifier without this module touching the filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SettlementNotifierKey(Arc<str>);

impl SettlementNotifierKey {
    /// Key for the journal of the deployment named `identity`.
    pub(crate) fn for_deployment(identity: &Arc<str>) -> Self {
        Self(Arc::clone(identity))
    }
}

/// The process-wide notifier table's row type: `(store, group_key) ->
/// Notify`.
type SettlementNotifiers = HashMap<(SettlementNotifierKey, String), Weak<Notify>>;

/// The process-wide notifier table.
///
/// `Weak` so a group nobody waits on holds no entry's weight — a dead entry
/// is replaced on the next lookup rather than swept.
fn registry() -> MutexGuard<'static, SettlementNotifiers> {
    static REGISTRY: OnceLock<Mutex<SettlementNotifiers>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The notifier a waiter parks on for `group_key` on `store`, creating it on
/// first use.
pub(crate) fn settlement_notifier(store: &SettlementNotifierKey, group_key: &str) -> Arc<Notify> {
    let key = (store.clone(), group_key.to_string());
    let mut registry = registry();
    if let Some(notifier) = registry.get(&key).and_then(Weak::upgrade) {
        return notifier;
    }
    let notifier = Arc::new(Notify::new());
    registry.insert(key, Arc::downgrade(&notifier));
    notifier
}

/// Wake every waiter parked on `group_key` on `store`. Called after the rank
/// write's commit, so a woken waiter that re-reads the journal sees the
/// settlement rather than racing it.
pub(crate) fn notify_group_settled(store: &SettlementNotifierKey, group_key: &str) {
    let notifier = {
        let registry = registry();
        registry
            .get(&(store.clone(), group_key.to_string()))
            .and_then(Weak::upgrade)
    };
    if let Some(notifier) = notifier {
        notifier.notify_waiters();
    }
}
