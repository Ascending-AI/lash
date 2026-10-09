//! One LISTEN connection per replica.
//!
//! The listener confirms its LISTEN, then reports itself listening; a
//! subscribe waits for that, so every doorbell after a subscriber registers
//! reaches it. When the connection drops it reconnects with backoff and
//! rings every local subscription, which then re-reads from its cursor:
//! notifications sent while it was away are lost, the rows are not.

use std::sync::Arc;

use sqlx::postgres::PgListener;

use super::Shared;
use super::codec::Doorbell;
use super::schema::db_error;

pub(super) async fn run(shared: Arc<Shared>) {
    let mut failures = 0_u32;
    let mut epoch = 0_u64;
    loop {
        match connect(&shared).await {
            Ok(mut listener) => {
                epoch += 1;
                shared.listening.send_replace(Some(epoch));
                failures = 0;
                shared.ring(&Doorbell::all());
                loop {
                    match listener.try_recv().await {
                        Ok(Some(notification)) => {
                            shared.ring(&Doorbell::unpack(notification.payload()));
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(%error, "the process replay listener failed");
                            break;
                        }
                    }
                }
                shared.listening.send_replace(None);
                tracing::warn!("the process replay listener lost its connection; reconnecting");
            }
            Err(error) => {
                tracing::warn!(%error, "the process replay listener could not connect");
            }
        }
        tokio::time::sleep(shared.reconnect.wait(failures)).await;
        failures = failures.saturating_add(1);
    }
}

async fn connect(shared: &Shared) -> Result<PgListener, lash_core::ProcessReplayStoreError> {
    let mut listener = PgListener::connect_with(&shared.listener_pool)
        .await
        .map_err(db_error("listen"))?;
    listener
        .listen(&shared.sql.channel)
        .await
        .map_err(db_error("listen"))?;
    Ok(listener)
}
