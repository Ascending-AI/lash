//! The Restate engine's recovery-tick schedule (FIG-3600 S7, ADR 0104
//! O2/O3/O4): a task of the driver's own deployment, not a
//! Restate-scheduled send.
//!
//! A durable object that re-sends its tick keeps firing after its
//! endpoint is gone, and each retried delivery pins an open invocation
//! the deployment can never drain to zero. The schedule therefore lives
//! on an interval next to the installed driver, carrying the
//! [`ReconcileCursor`] forward and ending when the installation is dropped. The tick itself stays the
//! engine-neutral [`SessionDriver::reconcile`] pass, on lash-core's
//! [`RecoveryInterval`] grid.

use std::num::NonZeroUsize;
use std::sync::{Arc, Weak};

use lash_core::SessionDriver;
use lash_core::engine::ReconcileCursor;
use lash_core::runtime::drive::{RECOVERY_TICK, RecoveryInterval};

/// Tick `driver`'s recovery pass every [`RECOVERY_TICK`] until it is
/// dropped.
///
/// A deployment runs one interval per installation: the session work starts
/// it only when its slot takes a new driver. The grid is fixed (ADR 0109
/// §1.8): a pass that returns within the period never moves the next one,
/// and a pass never waits on an obligation delivery longer than its tick's
/// lane wait. A failed pass is logged and retried by the next tick; the
/// cursor only advances on success.
pub(crate) async fn run(installation: Weak<dyn SessionDriver>, driver: Weak<dyn SessionDriver>) {
    let mut cursor = ReconcileCursor::default();
    let mut interval = RecoveryInterval::new(
        Arc::new(lash_core::facade_support::SystemClock),
        RECOVERY_TICK,
    );
    loop {
        interval.tick().await;
        let Some(installation) = installation.upgrade() else {
            break;
        };
        let Some(driver) = driver.upgrade() else {
            break;
        };
        // The pass may outlive the core. Hold its driver for this pass, but
        // release the installation so a replacement core can install now.
        drop(installation);
        match driver
            .reconcile(&cursor, NonZeroUsize::MIN.saturating_add(63))
            .await
        {
            Ok(next) => cursor = next,
            Err(error) => tracing::warn!(%error, "restate recovery pass failed"),
        }
    }
}
