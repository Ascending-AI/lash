//! Change notification over the effect journal: what a parked driver waits on.
//!
//! A driver parks in three places: a claim queued behind another owner's live
//! lease, a discharge the commit-order barrier holds behind a lower-committed
//! sibling, and a reader waiting for a group's next settlement. Each waits for
//! another writer to change one journal fact, a [`EffectJournalSubject`], and
//! the row store hands it an [`EffectJournalWake`] to park on
//! ([`EffectReplayRowStore::journal_wake`](super::EffectReplayRowStore::journal_wake)).
//! How long to wait, and what to race the wake against, is the driver's
//! business alone.
//!
//! [`EffectJournalNotifiers`] is the in-process half every backend shares:
//! a process-wide table of [`Notify`]s keyed by the journal's deployment
//! identity and the subject, which a backend wakes after each commit that can
//! change a subject. Two hosts over one deployment in one process share it
//! because they share the identity, never the connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use tokio::sync::Notify;

/// One journal fact a parked driver waits for another writer to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectJournalSubject<'a> {
    /// Whether the replay row at `(scope_id, replay_key)` can be claimed: its
    /// lease released, its terminal written, or the row deleted. A claim
    /// queued behind another owner's live lease waits on this.
    Row {
        scope_id: &'a str,
        replay_key: &'a str,
    },
    /// The commit order and settlement ranks of group `group_key`. A discharge
    /// held by the commit-order barrier and a reader waiting for the group's
    /// next settlement wait on this.
    Group { group_key: &'a str },
}

/// Which writers wake a subject's notifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectJournalWriters {
    /// Every writer that can change the subject wakes the notifier once its
    /// commit lands, because the journal is private to this process (a SQLite
    /// memory deployment). A parked driver waits on the notifier and its
    /// clock alone.
    Announced,
    /// A writer in another process can change the subject without waking the
    /// notifier (a SQLite file, PostgreSQL rows), or its notification can be
    /// lost (PostgreSQL group `NOTIFY`s across a `LISTEN` reconnect). A
    /// parked driver also re-reads on a bounded poll, which is the only poll
    /// left on these paths.
    Unannounced,
}

/// What a row store hands a driver about to park on a subject.
#[derive(Clone, Debug)]
pub struct EffectJournalWake {
    /// Woken after every commit, by a writer the notifier reaches, that can
    /// change the subject. The driver enables it *before* the read it guards,
    /// so a change committed between that read and the park is caught rather
    /// than slept through.
    pub notify: Arc<Notify>,
    /// Whether some writer can change the subject without waking `notify`.
    pub writers: EffectJournalWriters,
}

/// The process-wide table of in-process journal notifiers.
///
/// Keyed by the identity of the journal's deployment (`sqlite:<path>`,
/// `sqlite-memory:<id>`, `postgres:<digest>`) and then by subject. Entries are
/// `Weak`: a subject nobody waits on keeps no notifier alive, and the table
/// sweeps dead entries as it grows. Announcing a subject nobody waits on is a
/// lock and a map miss, with no allocation, so a backend announces every
/// relevant commit unconditionally.
pub struct EffectJournalNotifiers;

impl EffectJournalNotifiers {
    /// The notifier a waiter parks on for `subject` in `journal`, created on
    /// first use.
    pub fn notifier(journal: &Arc<str>, subject: EffectJournalSubject<'_>) -> Arc<Notify> {
        let mut table = table();
        let table = &mut *table;
        let watchers = table.journals.entry(Arc::clone(journal)).or_default();
        let slot = match subject {
            EffectJournalSubject::Row {
                scope_id,
                replay_key,
            } => watchers
                .rows
                .entry(scope_id.to_string())
                .or_default()
                .entry(replay_key.to_string())
                .or_default(),
            EffectJournalSubject::Group { group_key } => {
                watchers.groups.entry(group_key.to_string()).or_default()
            }
        };
        if let Some(notify) = slot.upgrade() {
            return notify;
        }
        let notify = Arc::new(Notify::new());
        *slot = Arc::downgrade(&notify);
        table.inserted += 1;
        if table.inserted >= table.sweep_at {
            table.sweep();
        }
        notify
    }

    /// Wake every waiter on `subject` in `journal`. Called after the commit
    /// that changed it, so a woken waiter's re-read sees the change.
    pub fn announce(journal: &str, subject: EffectJournalSubject<'_>) {
        let notify = {
            let table = table();
            let Some(watchers) = table.journals.get(journal) else {
                return;
            };
            match subject {
                EffectJournalSubject::Row {
                    scope_id,
                    replay_key,
                } => watchers
                    .rows
                    .get(scope_id)
                    .and_then(|rows| rows.get(replay_key))
                    .and_then(Weak::upgrade),
                EffectJournalSubject::Group { group_key } => {
                    watchers.groups.get(group_key).and_then(Weak::upgrade)
                }
            }
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    /// Wake every waiter on `journal`: a retirement deletes rows and groups
    /// wholesale, and a spurious wake costs a waiter one re-read.
    pub fn announce_journal(journal: &str) {
        let notifiers: Vec<Arc<Notify>> = {
            let table = table();
            let Some(watchers) = table.journals.get(journal) else {
                return;
            };
            watchers
                .rows
                .values()
                .flat_map(HashMap::values)
                .chain(watchers.groups.values())
                .filter_map(Weak::upgrade)
                .collect()
        };
        for notify in notifiers {
            notify.notify_waiters();
        }
    }
}

/// The watchers of one journal.
#[derive(Default)]
struct JournalWatchers {
    /// scope id → replay key → notifier, so a row announcement looks both
    /// keys up borrowed.
    rows: HashMap<String, HashMap<String, Weak<Notify>>>,
    groups: HashMap<String, Weak<Notify>>,
}

impl JournalWatchers {
    /// Drop dead notifiers, and report how many live ones remain.
    fn sweep(&mut self) -> usize {
        self.rows.retain(|_, rows| {
            rows.retain(|_, notify| notify.strong_count() > 0);
            !rows.is_empty()
        });
        self.groups.retain(|_, notify| notify.strong_count() > 0);
        self.rows.values().map(HashMap::len).sum::<usize>() + self.groups.len()
    }
}

/// Sweep no sooner than this many insertions after the last sweep.
const MIN_SWEEP_INTERVAL: usize = 64;

struct NotifierTable {
    journals: HashMap<Arc<str>, JournalWatchers>,
    /// Notifiers created since the last sweep.
    inserted: usize,
    /// Sweep once `inserted` reaches this: at least the live count the last
    /// sweep left, so sweeping stays amortized O(1) per insertion.
    sweep_at: usize,
}

impl NotifierTable {
    fn sweep(&mut self) {
        let mut live = 0;
        self.journals.retain(|_, watchers| {
            let journal_live = watchers.sweep();
            live += journal_live;
            journal_live > 0
        });
        self.inserted = 0;
        self.sweep_at = live.max(MIN_SWEEP_INTERVAL);
    }
}

fn table() -> MutexGuard<'static, NotifierTable> {
    static TABLE: OnceLock<Mutex<NotifierTable>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            Mutex::new(NotifierTable {
                journals: HashMap::new(),
                inserted: 0,
                sweep_at: MIN_SWEEP_INTERVAL,
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row<'a>(scope_id: &'a str, replay_key: &'a str) -> EffectJournalSubject<'a> {
        EffectJournalSubject::Row {
            scope_id,
            replay_key,
        }
    }

    async fn woken(notified: std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>) -> bool {
        tokio::time::timeout(Duration::from_millis(50), notified)
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn an_announcement_wakes_exactly_its_journal_and_subject() {
        let journal: Arc<str> = Arc::from("test:wake-exactly");
        let stranger: Arc<str> = Arc::from("test:wake-stranger");
        let notify = EffectJournalNotifiers::notifier(&journal, row("scope", "key"));
        assert!(Arc::ptr_eq(
            &notify,
            &EffectJournalNotifiers::notifier(&journal, row("scope", "key"))
        ));

        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        EffectJournalNotifiers::announce(&stranger, row("scope", "key"));
        EffectJournalNotifiers::announce(&journal, row("scope", "other-key"));
        EffectJournalNotifiers::announce(
            &journal,
            EffectJournalSubject::Group { group_key: "key" },
        );
        assert!(
            !woken(notified.as_mut()).await,
            "only its own subject wakes it"
        );
        EffectJournalNotifiers::announce(&journal, row("scope", "key"));
        assert!(woken(notified.as_mut()).await);

        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        EffectJournalNotifiers::announce_journal(&journal);
        assert!(
            woken(notified.as_mut()).await,
            "a journal-wide wake reaches every subject"
        );
    }

    #[test]
    fn dead_notifiers_are_swept_as_the_table_grows() {
        let journal: Arc<str> = Arc::from("test:wake-sweep");
        for index in 0..(4 * MIN_SWEEP_INTERVAL) {
            drop(EffectJournalNotifiers::notifier(
                &journal,
                row("scope", &index.to_string()),
            ));
        }
        let held = EffectJournalNotifiers::notifier(&journal, row("scope", "held"));
        let mut table = table();
        table.sweep();
        let rows = table
            .journals
            .get(&journal)
            .map_or(0, |watchers| watchers.rows.values().map(HashMap::len).sum());
        assert_eq!(rows, 1, "only the held notifier survives a sweep");
        drop(table);
        drop(held);
    }
}
