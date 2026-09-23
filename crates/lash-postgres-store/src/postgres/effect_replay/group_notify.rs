//! Settlement notifiers for the PostgreSQL journal: `NOTIFY` on a per-group
//! channel inside the rank write's transaction, and one dedicated `LISTEN`
//! connection per driver that fans the deliveries into the in-process
//! [`Notify`]s its `settlement_notifier` hands out.
//!
//! The listener is a spawned task holding a [`PgListener`]. A `try_recv` that
//! reports the connection was lost, or fails outright, wakes *every* parked
//! waiter — notifications received while disconnected are gone, so the
//! waiters re-read the journal rather than strand on a settlement that already
//! committed — and a failure rebuilds the listener, waking them again once it
//! listens. `LISTEN` is the fast path, not the guarantee: the driver still
//! polls a group across processes on a bound, because a notification can
//! be lost in ways no wake-all covers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgListener;
use sqlx::{PgPool, Postgres};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use lash_core_execution::RuntimeEffectControllerError;

use super::effect_store_message;

/// Bound on the pause between a failed `try_recv` and the next one; the
/// listener's own reconnect is unbounded beneath it because a permanently
/// dead subscription is a stranded waiter either way, and the wake-all on
/// reconnect is what bounds the *missed notification* window instead.
const RECONNECT_PAUSE: Duration = Duration::from_millis(200);

/// The channel one group's settlements notify on: a digest of the group key,
/// so an arbitrary key stays a short, identifier-safe channel name.
pub(crate) fn settlement_channel(group_key: &str) -> String {
    let digest = Sha256::digest(group_key.as_bytes());
    let mut channel = String::from("lash_gs_");
    for byte in &digest[..16] {
        channel.push_str(&format!("{byte:02x}"));
    }
    channel
}

/// `pg_notify` inside the writing transaction, so the wake rides the same
/// commit as the rank write: a waiter that re-reads can never see the
/// notification ahead of the settlement it announces.
pub(crate) async fn notify_group_settled(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    group_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(crate::connection_sql::connection_sql().notify_channel.sql())
        .bind(settlement_channel(group_key))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

enum HubCommand {
    /// Subscribe the listener to `channel`; `ack` completes once the `LISTEN`
    /// has executed, which is the ordering the caller's enable → read → park
    /// sequence needs across processes.
    Listen {
        channel: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
}

/// The per-driver fan-in between PostgreSQL `LISTEN`/`NOTIFY` and the
/// in-process notifiers the await loop parks on.
pub(crate) struct GroupNotifyHub {
    /// channel → live notifier, shared with the listener task. `Weak`, so a
    /// group nobody waits on keeps no weight; its `LISTEN` staying registered
    /// costs nothing.
    channels: Arc<Mutex<HashMap<String, Weak<Notify>>>>,
    commands: mpsc::UnboundedSender<HubCommand>,
    task: JoinHandle<()>,
}

impl GroupNotifyHub {
    /// Open the dedicated `LISTEN` connection and start fanning its
    /// notifications into this hub's notifiers.
    pub(crate) fn spawn(pool: &PgPool) -> Arc<Self> {
        let (commands, rx) = mpsc::unbounded_channel();
        let channels = Arc::new(Mutex::new(HashMap::new()));
        let task = tokio::spawn(run_hub(pool.clone(), Arc::clone(&channels), rx));
        Arc::new(Self {
            channels,
            commands,
            task,
        })
    }

    /// The notifier for `group_key`, with its channel subscribed before the
    /// call returns: a settlement committed after this point either precedes
    /// the caller's journal read (caught by the read) or follows the `LISTEN`
    /// (caught by the wake).
    pub(crate) async fn settlement_notifier(
        &self,
        group_key: &str,
    ) -> Result<Arc<Notify>, RuntimeEffectControllerError> {
        let channel = settlement_channel(group_key);
        let (notifier, acked) = {
            let mut channels = lock_recover(&self.channels);
            if let Some(notifier) = channels.get(&channel).and_then(Weak::upgrade) {
                return Ok(notifier);
            }
            let notifier = Arc::new(Notify::new());
            channels.insert(channel.clone(), Arc::downgrade(&notifier));
            let (ack, acked) = oneshot::channel();
            if self
                .commands
                .send(HubCommand::Listen {
                    channel: channel.clone(),
                    ack,
                })
                .is_err()
            {
                channels.remove(&channel);
                return Err(effect_store_message(
                    "settlement notifier listener is not running".to_string(),
                ));
            }
            (notifier, acked)
        };
        match acked.await {
            Ok(Ok(())) => Ok(notifier),
            Ok(Err(error)) => {
                lock_recover(&self.channels).remove(&channel);
                Err(effect_store_message(error))
            }
            Err(_) => {
                lock_recover(&self.channels).remove(&channel);
                Err(effect_store_message(
                    "settlement notifier listener closed without answering".to_string(),
                ))
            }
        }
    }
}

impl GroupNotifyHub {
    /// Stop the listener task, as a crashed or wedged `LISTEN` connection
    /// would: notifiers already handed out are never woken again.
    #[cfg(test)]
    pub(crate) fn stop(&self) {
        self.task.abort();
    }

    /// Wake every waiter parked on this hub, for a change that carried no
    /// `NOTIFY` (a retirement deleting groups wholesale).
    pub(crate) fn wake_all(&self) {
        wake_all(&self.channels);
    }
}

impl Drop for GroupNotifyHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Wake every parked waiter: the recovery path for notifications lost while
/// the `LISTEN` connection was down — each waiter re-reads the journal and
/// finds whatever it missed.
fn wake_all(channels: &Arc<Mutex<HashMap<String, Weak<Notify>>>>) {
    let channels = lock_recover(channels);
    for notifier in channels.values().filter_map(Weak::upgrade) {
        notifier.notify_waiters();
    }
}

async fn run_hub(
    pool: PgPool,
    channels: Arc<Mutex<HashMap<String, Weak<Notify>>>>,
    mut commands: mpsc::UnboundedReceiver<HubCommand>,
) {
    let mut listener = connect(&pool, &channels).await;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => return,
                Some(HubCommand::Listen { channel, ack }) => {
                    let result = listener
                        .listen(&channel)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = ack.send(result);
                }
            },
            received = listener.try_recv() => match received {
                Ok(Some(notification)) => {
                    let notifier = lock_recover(&channels)
                        .get(notification.channel())
                        .and_then(Weak::upgrade);
                    if let Some(notifier) = notifier {
                        notifier.notify_waiters();
                    }
                }
                // `None` is a lost connection — sqlx has already reconnected
                // and re-issued every LISTEN by the time it returns here. What
                // it cannot return is the notifications that window dropped,
                // so every parked waiter wakes once and re-reads.
                Ok(None) => wake_all(&channels),
                // Any other failure may have dropped notifications too, and
                // sqlx replaces the connection only for some I/O error kinds
                // (not `ConnectionReset`), so the listener is rebuilt rather
                // than retried: wake every waiter now, and again once the
                // new listener is subscribed, so a settlement committed in
                // the gap is re-read rather than slept through.
                Err(error) => {
                    tracing::warn!(%error, "effect-group settlement listener receive failed; reconnecting");
                    wake_all(&channels);
                    tokio::time::sleep(RECONNECT_PAUSE).await;
                    listener = connect(&pool, &channels).await;
                    wake_all(&channels);
                }
            },
        }
    }
}

/// A fresh listener subscribed to every channel the hub has handed out,
/// retried until one connects. Commands wait meanwhile; a caller whose
/// subscription is not acknowledged in time parks on the driver's
/// cross-process poll instead.
async fn connect(
    pool: &PgPool,
    channels: &Arc<Mutex<HashMap<String, Weak<Notify>>>>,
) -> PgListener {
    loop {
        let known: Vec<String> = lock_recover(channels).keys().cloned().collect();
        let attempt = async {
            let mut listener = PgListener::connect_with(pool).await?;
            listener
                .listen_all(known.iter().map(String::as_str))
                .await?;
            Ok::<_, sqlx::Error>(listener)
        }
        .await;
        match attempt {
            Ok(listener) => return listener,
            Err(error) => {
                tracing::warn!(%error, "effect-group settlement listener failed to connect; retrying");
                tokio::time::sleep(RECONNECT_PAUSE).await;
            }
        }
    }
}
