//! Process-wide settlement notifiers for the SQLite journal.
//!
//! SQLite has no `NOTIFY`, so the cross-host wake is a shared registry: every
//! rank write ([`EffectReplayRowStore::discharge_child`],
//! [`EffectReplayRowStore::decide_cancel`], a grouped
//! [`EffectReplayRowStore::finalize`]) wakes the notifier for
//! (database file, group) after its commit, and two hosts over the same file
//! in one process share that notifier because the registry keys on the
//! canonical path rather than the connection.
//!
//! [`EffectReplayRowStore`]: lash_core_execution::facade_support::effect_replay_driver::EffectReplayRowStore

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use tokio::sync::Notify;

/// What a notifier belongs to, beneath the group key.
///
/// `File` is the resolved database path the opener already computed — the
/// same canonical identity the replay binding and turn-control binding are
/// keyed on — so two hosts opened on the same file through different
/// spellings share one notifier without this module touching the filesystem.
/// `Memory` is a per-open token: two in-memory connections never share a
/// file, so sharing a key between them would only name a wake that can never
/// happen.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SettlementNotifierKey {
    /// The resolved path of the journal's database file, as the opener
    /// canonicalized it.
    File(PathBuf),
    /// A fresh identity for an in-memory journal; scoped to the open.
    Memory(u64),
}

impl SettlementNotifierKey {
    /// Key for a file-backed journal. `resolved` is the canonical path the
    /// open already computed for the binding identity — this type performs no
    /// filesystem access of its own (FIG-2971).
    pub(crate) fn for_file(resolved: &Path) -> Self {
        Self::File(resolved.to_path_buf())
    }

    /// Key for the in-memory backing: never shared, by construction.
    pub(crate) fn for_memory() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self::Memory(NEXT.fetch_add(1, Ordering::Relaxed))
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
