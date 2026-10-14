//! Private lifecycle plumbing for PostgreSQL's provisional replay feeds.
//!
//! Each feed owns its pools, channel, cursor judgments and payloads. This
//! module owns LISTEN readiness/reconnect, refcounted bells and tail draining.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::Stream;
use lash_postgres_store::host::ReconnectPolicy;
use lash_sansio::sync::MutexExt as _;
use sqlx::postgres::PgListener;
use tokio::sync::watch;

pub(crate) mod schema;

pub(crate) struct Listener {
    pool: sqlx::PgPool,
    channel: String,
    reconnect: ReconnectPolicy,
    wait: Duration,
    listening: watch::Sender<Option<u64>>,
}

impl Listener {
    pub(crate) fn new(
        pool: sqlx::PgPool,
        channel: String,
        reconnect: ReconnectPolicy,
        wait: Duration,
    ) -> Self {
        Self {
            pool,
            channel,
            reconnect,
            wait,
            listening: watch::channel(None).0,
        }
    }

    pub(crate) async fn wait<E>(&self, timeout: E, closed: E) -> Result<(), E> {
        let mut listening = self.listening.subscribe();
        tokio::time::timeout(self.wait, listening.wait_for(Option::is_some))
            .await
            .map_err(|_| timeout)?
            .map_err(|_| closed)?;
        Ok(())
    }

    /// Confirm LISTEN, run the feed's reload, then admit subscribers and
    /// ring all bells. Reconnect repeats this order so missed notifications
    /// cause reads from the subscribers' last delivered positions.
    pub(crate) async fn run<E, F>(
        &self,
        reload: impl Fn() -> F + Send + Sync,
        notify: impl Fn(Option<&str>) + Send,
        db_error: impl Fn(sqlx::Error) -> E + Send + Sync,
    ) where
        E: std::fmt::Display + Send,
        F: Future<Output = Result<(), E>> + Send,
    {
        let mut failures = 0_u32;
        let mut epoch = 0_u64;
        loop {
            let connected = async {
                let mut listener = PgListener::connect_with(&self.pool)
                    .await
                    .map_err(&db_error)?;
                listener.listen(&self.channel).await.map_err(&db_error)?;
                reload().await?;
                Ok::<_, E>(listener)
            }
            .await;
            match connected {
                Ok(mut listener) => {
                    epoch += 1;
                    self.listening.send_replace(Some(epoch));
                    failures = 0;
                    notify(None);
                    loop {
                        match listener.try_recv().await {
                            Ok(Some(notification)) => notify(Some(notification.payload())),
                            Ok(None) => break,
                            Err(error) => {
                                tracing::warn!(%error, channel = %self.channel, "the replay listener failed");
                                break;
                            }
                        }
                    }
                    self.listening.send_replace(None);
                    tracing::warn!(channel = %self.channel, "the replay listener lost its connection; reconnecting");
                }
                Err(error) => {
                    tracing::warn!(%error, channel = %self.channel, "the replay listener could not connect")
                }
            }
            tokio::time::sleep(self.reconnect.wait(failures)).await;
            failures = failures.saturating_add(1);
        }
    }
}

struct Bell {
    sender: watch::Sender<u64>,
    subscribers: usize,
}

pub(crate) struct Bells<K>(Arc<Mutex<HashMap<K, Bell>>>);

pub(crate) struct BellGuard<K: Eq + Hash> {
    bells: Bells<K>,
    key: K,
    receiver: watch::Receiver<u64>,
}

impl<K> Default for Bells<K> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<K: Eq + Hash + Clone + ToString> Bells<K> {
    /// Called only after LISTEN readiness; register before the initial read.
    pub(crate) fn register(&self, key: K) -> BellGuard<K> {
        let mut bells = self.0.lock_recover();
        let bell = bells.entry(key.clone()).or_insert_with(|| Bell {
            sender: watch::channel(0).0,
            subscribers: 0,
        });
        bell.subscribers += 1;
        let mut receiver = bell.sender.subscribe();
        receiver.mark_unchanged();
        BellGuard {
            bells: Self(Arc::clone(&self.0)),
            key,
            receiver,
        }
    }

    pub(crate) fn followed(&self) -> Vec<String> {
        self.0
            .lock_recover()
            .keys()
            .map(ToString::to_string)
            .collect()
    }

    pub(crate) fn ring(&self, keys: impl IntoIterator<Item = K>) {
        let bells = self.0.lock_recover();
        for key in keys {
            if let Some(bell) = bells.get(&key) {
                bell.sender.send_modify(|rings| *rings += 1);
            }
        }
    }

    pub(crate) fn ring_all(&self) {
        for bell in self.0.lock_recover().values() {
            bell.sender.send_modify(|rings| *rings += 1);
        }
    }

    pub(crate) fn clear(&self) {
        self.0.lock_recover().clear();
    }
}

impl<K: Eq + Hash> Drop for BellGuard<K> {
    fn drop(&mut self) {
        let mut bells = self.bells.0.lock_recover();
        if let Some(bell) = bells.get_mut(&self.key) {
            bell.subscribers -= 1;
            if bell.subscribers == 0 {
                bells.remove(&self.key);
            }
        }
    }
}

struct Tail<K: Eq + Hash, E, Error, R> {
    bell: BellGuard<K>,
    last: u64,
    queue: VecDeque<E>,
    done: bool,
    closed: Option<Error>,
    read: R,
}

/// Drain queued events before waiting for a bell. A full read rings again;
/// a closed bell or a feed's terminal read error is emitted exactly once.
/// The feed supplies cursor construction, gap judgments and typed errors.
pub(crate) fn live_tail<K, E, Error, R, F>(
    bell: BellGuard<K>,
    last: u64,
    window: usize,
    closed: Error,
    read: R,
) -> impl Stream<Item = Result<E, Error>>
where
    K: Eq + Hash,
    R: Fn(u64) -> F,
    F: Future<Output = Result<(u64, Vec<E>), Error>>,
{
    let tail = Tail {
        bell,
        last,
        queue: VecDeque::new(),
        done: false,
        closed: Some(closed),
        read,
    };
    futures_util::stream::unfold(tail, move |mut tail| async move {
        let item = loop {
            if let Some(event) = tail.queue.pop_front() {
                break Ok(event);
            }
            if tail.done {
                return None;
            }
            if tail.bell.receiver.changed().await.is_err() {
                tail.done = true;
                break Err(tail.closed.take()?);
            }
            match (tail.read)(tail.last).await {
                Ok((position, events)) => {
                    tail.last = position;
                    if !events.is_empty() && events.len() >= window {
                        tail.bell.receiver.mark_changed();
                    }
                    tail.queue.extend(events);
                }
                Err(error) => {
                    tail.done = true;
                    break Err(error);
                }
            }
        };
        Some((item, tail))
    })
}
