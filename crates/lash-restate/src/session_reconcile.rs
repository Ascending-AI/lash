//! The Restate engine's recovery-sweep schedule (FIG-3600 S7, ADR 0104
//! O2/O3/O4): a task of the driver's own deployment, not a
//! Restate-scheduled send.
//!
//! A durable object that re-sends its tick keeps firing after its
//! endpoint is gone, and each retried delivery pins an open invocation
//! the deployment can never drain to zero. The schedule therefore lives
//! on a Tokio interval next to the installed driver — the same shape the
//! in-process engine uses — carrying the [`ReconcileCursor`] forward and
//! ending when the driver is dropped. The tick itself stays the
//! engine-neutral [`SessionDriver::reconcile`] pass.

use std::num::NonZeroUsize;
use std::sync::Weak;
use std::time::Duration;

use lash_core::SessionDriver;
use lash_core::engine::{ReconcileCursor, ReconcileTicks};

/// Tick `driver`'s recovery pass every ten seconds until it is dropped.
///
/// A deployment runs one interval per installed driver: the session work
/// starts it only when its slot takes a new driver. Tick ids carry a nonce
/// drawn for this run ([`ReconcileTicks`]), so drive asks from one tick
/// dedupe while asks from two ticks — of this process or of any before
/// it — never do. A failed pass is logged and retried by the next tick; the
/// cursor only advances on success.
pub(crate) async fn run(driver: Weak<dyn SessionDriver>) {
    let mut cursor = ReconcileCursor::default();
    let mut ticks = ReconcileTicks::start("restate");
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        let Some(driver) = driver.upgrade() else {
            break;
        };
        let tick = ticks.next_tick();
        match driver
            .reconcile(&cursor, NonZeroUsize::MIN.saturating_add(63), &tick)
            .await
        {
            Ok(next) => cursor = next,
            Err(error) => tracing::warn!(%error, "restate recovery pass failed"),
        }
    }
}
