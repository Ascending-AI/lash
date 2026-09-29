//! `lashctl finalize` on PostgreSQL: the operator hold and the move of the
//! fleet epoch `F` (ADR 0106 §2, ADR 0115 §2.1–2.2).
//!
//! Both change the fleet-format row, and both begin at
//! [`begin_fleet_row`](crate::guarded_tx::begin_fleet_row): the row locked
//! `FOR UPDATE`, its recorded epoch admitted against this build's writable
//! range. The flip is finalize's side of the writer fence. A writer that
//! already holds the row `FOR SHARE` commits first under the old `F`; every
//! writer that fences afterwards reads the new `F`, and one whose writable
//! range excludes it is refused `WriterFenced` having written nothing.
//!
//! The hold lives on the same row, so the automatic finalize reads it inside
//! the transaction that would move `F`: a hold set by one operator while
//! another's rollout finalizes is either seen, and the finalize refuses, or
//! set after the move commits.

use lash_core_execution::StoreError;
use lash_core_execution::store::fleet_finalize::{
    FinalizeError, FinalizeHold, FinalizeMode, FinalizeRefusal, FleetEpochFlip,
};
use sqlx::PgPool;

use crate::guarded_tx::{FleetRowTx, WriterFence, begin_fleet_row};
use crate::session_sql::session_sql;
use crate::store_sqlx_error;

/// The operator's hold on the automatic finalize, if one stands. A plain
/// read: the hold is evidence here, and finalize reads it again under the
/// row lock.
pub(crate) async fn read_hold(pool: &PgPool) -> Result<Option<FinalizeHold>, StoreError> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx::query_as(session_sql().fleet_format.select_hold.sql())
            .fetch_optional(pool)
            .await
            .map_err(store_sqlx_error)?;
    Ok(match row {
        Some((Some(reason), Some(held_at_ms))) => Some(FinalizeHold {
            reason,
            held_at_ms: u64::try_from(held_at_ms).unwrap_or_default(),
        }),
        _ => None,
    })
}

/// Hold the automatic finalize with `reason`. A standing hold is replaced,
/// with a fresh instant.
pub(crate) async fn set_hold(
    pool: &PgPool,
    fence: &WriterFence,
    reason: &str,
) -> Result<FinalizeHold, StoreError> {
    let mut row = begin_fleet_row(pool, fence).await?;
    let held_at_ms: i64 = sqlx::query_scalar(session_sql().fleet_format.update_set_hold.sql())
        .bind(reason)
        .fetch_one(row.connection())
        .await
        .map_err(store_sqlx_error)?;
    row.commit().await?;
    Ok(FinalizeHold {
        reason: reason.to_owned(),
        held_at_ms: u64::try_from(held_at_ms).unwrap_or_default(),
    })
}

/// Clear the hold, answering the one that stood.
pub(crate) async fn clear_hold(
    pool: &PgPool,
    fence: &WriterFence,
) -> Result<Option<FinalizeHold>, StoreError> {
    let mut row = begin_fleet_row(pool, fence).await?;
    let cleared = row.hold.take();
    if cleared.is_some() {
        sqlx::query(session_sql().fleet_format.update_clear_hold.sql())
            .execute(row.connection())
            .await
            .map_err(store_sqlx_error)?;
    }
    row.commit().await?;
    Ok(cleared)
}

/// A move of `F` that has locked the row and written it, and has not yet
/// committed. Dropping it rolls the move back.
pub(crate) struct PendingFlip {
    row: FleetRowTx,
    flip: FleetEpochFlip,
}

impl PendingFlip {
    /// Commit the move, and record the new epoch as the fence's last
    /// observed one.
    pub(crate) async fn commit(self, fence: &WriterFence) -> Result<FleetEpochFlip, StoreError> {
        self.row.commit().await?;
        fence.observe(self.flip.fleet());
        Ok(self.flip)
    }
}

/// Lock the row and move `F` to `target`, uncommitted.
///
/// `target` is the finalizing build's `F_self`, the top of its writable
/// range; the recorded epoch was admitted inside that range, so it is at or
/// below `target`. The automatic finalize refuses while an operator hold
/// stands, even when there is nothing left to move.
pub(crate) async fn begin_flip(
    pool: &PgPool,
    fence: &WriterFence,
    target: u32,
    mode: FinalizeMode,
) -> Result<PendingFlip, FinalizeError> {
    let mut row = begin_fleet_row(pool, fence).await?;
    if mode == FinalizeMode::Automatic
        && let Some(hold) = row.hold.clone()
    {
        return Err(FinalizeRefusal::Held { hold }.into());
    }
    let recorded = row.recorded;
    if recorded >= target {
        return Ok(PendingFlip {
            row,
            flip: FleetEpochFlip::AlreadyFinalized { fleet: recorded },
        });
    }
    let version = i32::try_from(target).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "lash_fleet_format.format_version",
        message: format!("not a fleet-format version: {target}"),
    })?;
    sqlx::query(session_sql().fleet_format.update_format_version.sql())
        .bind(version)
        .execute(row.connection())
        .await
        .map_err(store_sqlx_error)?;
    Ok(PendingFlip {
        row,
        flip: FleetEpochFlip::Finalized {
            from: recorded,
            to: target,
        },
    })
}

/// Move `F` to this build's `F_self` in one transaction.
pub(crate) async fn flip(
    pool: &PgPool,
    fence: &WriterFence,
    mode: FinalizeMode,
) -> Result<FleetEpochFlip, FinalizeError> {
    let pending = begin_flip(pool, fence, fence.writable().max(), mode).await?;
    Ok(pending.commit(fence).await?)
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
