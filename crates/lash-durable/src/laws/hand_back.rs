//! A forced old activation unwind while its successor holds an open tx.

use super::{LABEL, LawBroken, LawOutcome, append, create, ensure, node, session};
use crate::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
use crate::{DurableSettings, DurableStore, LeaseSettings, NodeId};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

mod store;
use store::ObservedStore;

struct HeldActivation {
    entered: mpsc::UnboundedSender<crate::Epoch>,
    unwind: Arc<Notify>,
}

#[async_trait::async_trait]
impl Activation for HeldActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let _ = self.entered.send(owned.epoch());
        self.unwind.notified().await;
        Exit::Abandoned
    }
}

/// A Runner's hand-back uses the activation's original grant. The successor
/// keeps its snapshot, pending mail and already-open transaction unchanged.
pub async fn an_abandoned_activation_hands_back_only_its_original_grant(
    inner: Arc<dyn DurableStore>,
) -> LawOutcome {
    let actor = session("held-successor")?;
    create(inner.as_ref(), std::slice::from_ref(&actor)).await?;
    append(inner.as_ref(), &actor, "successor mail").await?;
    let (entered, mut arrivals) = mpsc::unbounded_channel();
    let (begins, mut receipts) = mpsc::unbounded_channel();
    let unwind = Arc::new(Notify::new());
    let store = Arc::new(ObservedStore {
        inner: Arc::clone(&inner),
        begins,
    });
    // Replacing the boot must not stop the Runner through a claim or reap
    // before the forced unwind. Renewal is held by ObservedStore; put both
    // claim intervals and the reap interval beyond a test action's lifetime.
    // The first claim still runs at once, and hand-back follows the exit.
    let quiet = Duration::from_secs(100 * 365 * 24 * 60 * 60);
    let config = DurableSettings {
        lease: LeaseSettings {
            claim_backoff: quiet,
            claim_poll: quiet,
            reap_every: quiet,
            ..LeaseSettings::default()
        },
        ..DurableSettings { owner_retention: Some(crate::OwnerRetentionSettings { idle_evict: std::time::Duration::from_secs(60),
 max_owner_bytes: const { std::num::NonZeroU64::new(64 * 1024 * 1024).unwrap() },
 max_node_bytes: const { std::num::NonZeroU64::new(512 * 1024 * 1024).unwrap() },
 max_owner_dirty_bytes: const { std::num::NonZeroU64::new(32 * 1024 * 1024).unwrap() },
 max_node_dirty_bytes: const { std::num::NonZeroU64::new(256 * 1024 * 1024).unwrap() }, }), ..DurableSettings::default() }
    }
    .validate()
    .map_err(|error| LawBroken(error.to_string()))?;
    let runner = Runner::new(
        store,
        Arc::new(lash_core_ids::clock::SystemClock),
        RunnerConfig::new(
            NodeId::new("original-grant"),
            vec![super::formats()],
            &config,
        ),
        Arc::new(HeldActivation {
            entered,
            unwind: Arc::clone(&unwind),
        }),
    );
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(runner.run(async {
        let _ = stopped.await;
    }));
    // Abort on an early refusal or a law failure too: no detached Runner.
    let abort = task.abort_handle();
    let guard = AbortOnDrop(abort);
    let held = arrivals
        .recv()
        .await
        .ok_or_else(|| LawBroken("the old activation never entered".into()))?;
    let successor = node(inner.as_ref(), "original-grant").await?;
    let claims = inner.claim(&successor, 1).await?;
    ensure!(
        claims.len() == 1 && claims[0].actor == actor && claims[0].epoch.0 == held.0 + 2,
        "the successor did not replace the held grant: {claims:?}"
    );
    let current = claims[0].epoch;
    let mut open = inner.begin(&actor, current).await?;
    ensure!(
        open.mail().len() == 1 && open.mail()[0].body == "successor mail",
        "the successor lacks its pending mail"
    );
    let before = inner.actor(&actor).await?;
    // Notify retains the permit even if activate has not yet awaited it.
    unwind.notify_one();
    let (attempted, lost) = receipts
        .recv()
        .await
        .ok_or_else(|| LawBroken("the old hand-back never attempted begin".into()))?;
    let _ = stop.send(());
    match task
        .await
        .map_err(|error| LawBroken(format!("Runner join: {error}")))?
    {
        Ok(_) | Err(crate::DurableError::NodeLeaseLost { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    drop(guard);
    ensure!(
        attempted == held && lost == Some((held, Some(current))),
        "the hand-back refreshed its grant or missed the OwnershipLost receipt: {attempted:?}, {lost:?}"
    );
    ensure!(
        inner.actor(&actor).await? == before,
        "the old hand-back changed its successor's snapshot"
    );
    let check = inner.begin(&actor, current).await?;
    ensure!(
        check.mail() == open.mail(),
        "the old hand-back changed its successor's mail"
    );
    open.ack_seen();
    inner.commit(open, LABEL).await?;
    ensure!(
        inner.begin(&actor, current).await?.mail().is_empty(),
        "the already-open successor transaction did not commit"
    );
    Ok(())
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
