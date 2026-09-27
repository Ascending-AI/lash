//! The core's seat in the recovery leader election (ADR 0109 §1.6–§1.7).
//!
//! One slot per core, shared by the core and its session driver: the driver's
//! reconcile tick joins the election on its first pass and asks which duties
//! it runs, and the core resigns at shutdown. The lease keeps its own cadence
//! on a background task that resigns when the slot is dropped.

use std::sync::Arc;

use lash_core::runtime::recovery_lease::{RecoveryDuties, RecoveryLease};
use lash_core::store::{LeaseName, RecoveryLeaderStore};

use crate::support::RuntimeEnvironment;

pub(crate) struct RecoverySlot {
    lease: tokio::sync::OnceCell<Arc<RecoveryLease>>,
    shutdown: tokio_util::sync::CancellationToken,
    store: Arc<dyn RecoveryLeaderStore>,
    name: LeaseName,
    config: lash_core::engine::RecoveryLeaseConfig,
    clock: Arc<dyn lash_core::Clock>,
}

impl RecoverySlot {
    /// The slot for a core over `env`: the lease is named after the engine
    /// authority that owns the effect state, in the storage the backend's
    /// store set holds.
    pub(crate) fn new(env: &RuntimeEnvironment) -> Self {
        let authority = env.core.control.effect_host.turn_control_binding_id();
        Self {
            lease: tokio::sync::OnceCell::new(),
            shutdown: tokio_util::sync::CancellationToken::new(),
            store: env.core.backend().recovery_leader(),
            name: LeaseName::new(format!("recovery:{authority}")),
            config: env.core.control.recovery_lease,
            clock: Arc::clone(&env.core.clock),
        }
    }

    /// This core's lease: on first use it attempts once inline, so the first
    /// tick of an uncontested deployment already leads, then keeps the lease's
    /// cadence in the background.
    pub(crate) async fn lease(&self) -> &Arc<RecoveryLease> {
        self.lease
            .get_or_init(|| async {
                let lease = Arc::new(RecoveryLease::new(
                    Arc::clone(&self.store),
                    self.name.clone(),
                    self.config.generation_rank,
                    self.config.timings,
                    Arc::clone(&self.clock),
                ));
                lease.step().await;
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(keep_cadence(Arc::clone(&lease), self.shutdown.clone()));
                }
                lease
            })
            .await
    }

    /// The duties this deployment runs now.
    pub(crate) async fn duties(&self) -> RecoveryDuties {
        let now_ms = self.clock.timestamp_ms();
        self.lease().await.duties(now_ms)
    }

    /// Give the lease up now, if this core ever joined the election.
    pub(crate) async fn resign(&self) {
        if let Some(lease) = self.lease.get() {
            lease.resign().await;
        }
    }
}

/// Step `lease` on its own cadence until `shutdown` fires, then resign:
/// wait the delay the current standing calls for, then attempt again.
async fn keep_cadence(lease: Arc<RecoveryLease>, shutdown: tokio_util::sync::CancellationToken) {
    let mut standing = lease.standing();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(lease.next_delay(standing)) => {}
        }
        standing = lease.step().await;
    }
    lease.resign().await;
}

impl Drop for RecoverySlot {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
