//! K6 ownership shared by every phase of one turn. A borrowed coordinator
//! never crosses the boundary; only its quiescent retained transfer does.

use super::*;
use crate::tool_dispatch::{RunCoordinator, SingletonRunError, SingletonToolHandlers};
use crate::tool_run::{ContinuationRefusal, Cut, SegmentOrdinal};

impl OpenerState {
    /// A physical boundary may publish only a completed, retained K6 capture.
    pub fn boundary_snapshot(
        &self,
        reason: crate::BoundaryReason,
    ) -> Result<crate::store::RunOpenerState, RuntimeEffectControllerError> {
        let registry = self.groups.lock_recover();
        if registry.active_run {
            return Err(ContinuationRefusal::NotQuiescent.into());
        }
        if let Some(transfer) = &registry.run {
            transfer
                .check_capture(&Cut::request(reason).observe(0))
                .map_err(RuntimeEffectControllerError::from)?;
        }
        Ok(self.snapshot_with_registry(&registry))
    }

    /// Check the logical owner before recovering any cursor or tool state.
    pub fn from_snapshot_for(
        snapshot: crate::store::RunOpenerState,
        owner: &crate::EffectOpener,
    ) -> Result<Self, RuntimeEffectControllerError> {
        if snapshot.run.as_ref().is_some_and(|run| &run.owner != owner) {
            return Err(ContinuationRefusal::ForeignOwner.into());
        }
        Self::from_snapshot(snapshot)
    }
    /// Rebuild the coordinator from a published transfer before admitting work.
    /// Failed or interrupted adoption keeps capture fenced until journal recovery.
    pub async fn adopt_run<'a>(
        &self,
        scoped: &'a crate::ScopedEffectController<'a>,
        owner: crate::EffectOpener,
        successor: SegmentOrdinal,
        available: Vec<crate::store::plugin_writers::PluginRevision>,
        handlers: Arc<dyn SingletonToolHandlers>,
        clock: &dyn crate::Clock,
    ) -> Result<RunCoordinator<'a>, SingletonRunError> {
        let transfer = {
            let mut registry = self.groups.lock_recover();
            if registry.active_run {
                return Err(
                    RuntimeEffectControllerError::from(ContinuationRefusal::NotQuiescent).into(),
                );
            }
            registry.active_run = true;
            registry.run.clone()
        };
        match transfer {
            Some(transfer) => {
                RunCoordinator::adopt(
                    scoped, owner, successor, available, *transfer, handlers, clock,
                )
                .await
            }
            None => Ok(RunCoordinator::open(scoped, owner, successor, available)),
        }
    }

    /// Freeze admission and poll all issued X handles through ACK, retain their
    /// material, then publish the capture to every phase of this opener.
    /// Cancellation of this future leaves capture fenced; replay owns recovery.
    pub async fn capture_run(
        &self,
        run: &mut RunCoordinator<'_>,
        reason: crate::BoundaryReason,
        materials: &dyn crate::store::ToolMaterialStore,
    ) -> Result<(), SingletonRunError> {
        self.groups.lock_recover().active_run = true;
        run.request_cut(reason);
        let mut transfer = run.quiesce().await?;
        run.retain_cut(&mut transfer, materials).await?;
        let mut registry = self.groups.lock_recover();
        registry.run = Some(Box::new(transfer));
        registry.active_run = false;
        Ok(())
    }
}
