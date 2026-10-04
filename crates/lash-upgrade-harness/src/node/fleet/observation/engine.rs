//! A harness-only observer around the installed native shift port.
use super::NativeCapture;
use lash_core::engine::*;
use lash_core::testing::{EffectLayer, LayeredEffectHost};
use lash_core::{
    EffectEngine, ScopedEffectController, SessionId, SessionShifts, SessionWorkEngine,
};
use std::sync::Arc;

pub(in crate::node::fleet) struct ObservedEngine {
    inner: Arc<lash_restate::RestateEngine>,
    capture: Arc<NativeCapture>,
}
impl ObservedEngine {
    pub(in crate::node::fleet) fn new(
        inner: Arc<lash_restate::RestateEngine>,
        capture: Arc<NativeCapture>,
    ) -> Arc<Self> {
        Arc::new(Self { inner, capture })
    }
}
impl EffectEngine for ObservedEngine {
    fn stores(&self) -> Arc<dyn lash_core::StoreSet> {
        self.inner.stores()
    }
    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.inner.effect_host()
    }
    fn generation(&self) -> &EngineGeneration {
        self.inner.generation()
    }
    fn process_work(&self) -> lash_core::ProcessWorkWiring {
        self.inner.process_work()
    }
    fn session_work(&self) -> Arc<dyn SessionWorkEngine> {
        Arc::new(ObservedWork {
            inner: self.inner.session_work(),
            capture: self.capture.clone(),
        })
    }
    fn deployment_registry(&self) -> Arc<dyn lash_core::store::fleet_finalize::DeploymentRegistry> {
        self.inner.deployment_registry()
    }
}
struct ObservedWork {
    inner: Arc<dyn SessionWorkEngine>,
    capture: Arc<NativeCapture>,
}
#[async_trait::async_trait]
impl SessionWorkEngine for ObservedWork {
    fn schedule_shift(&self, session: &SessionId, request: ShiftRequestId) {
        self.inner.schedule_shift(session, request);
    }
    async fn request_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<(), EngineRefusal> {
        self.inner.request_shift(session, request).await
    }
    fn control(&self) -> Arc<dyn SessionControlEngine> {
        self.inner.control()
    }
    async fn await_shift(
        &self,
        session: &SessionId,
        request: &ShiftRequestId,
    ) -> Result<ShiftOutcome, ShiftAbort> {
        self.inner.await_shift(session, request).await
    }
    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        self.inner.install_session_shifts(Arc::new(ObservedShifts {
            inner: shifts,
            capture: self.capture.clone(),
        }))
    }
}
struct ObservedShifts {
    inner: Arc<dyn SessionShifts>,
    capture: Arc<NativeCapture>,
}
#[async_trait::async_trait]
impl SessionShifts for ObservedShifts {
    fn owns_reconciliation(&self) -> bool {
        self.inner.owns_reconciliation()
    }
    fn runs_on(&self, shifts: &dyn SessionShifts) -> bool {
        self.inner.runs_on(shifts)
    }
    fn hold_shift(&self, session: &SessionId) -> ShiftHold {
        self.inner.hold_shift(session)
    }
    async fn reconcile(
        &self,
        cursor: &ReconcileCursor,
        page: std::num::NonZeroUsize,
    ) -> Result<ReconcileCursor, lash_core::StoreError> {
        self.inner.reconcile(cursor, page).await
    }
    async fn admit(
        &self,
        controller: ScopedEffectController<'_>,
        request: &ShiftRequest,
        generation: &BuildGeneration,
        ordinal: u32,
        draining: Option<&BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        self.inner
            .admit(controller, request, generation, ordinal, draining)
            .await
    }
    async fn close_run(
        &self,
        controller: ScopedEffectController<'_>,
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> Result<(), ShiftAbort> {
        self.inner.close_run(controller, session, run).await
    }
    async fn execute_run(
        &self,
        controller: ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> RunEnd {
        let original = controller.clone();
        let layer: Arc<dyn EffectLayer> = self
            .capture
            .for_run(admitted.session().clone(), admitted.run().clone());
        match LayeredEffectHost::layer_scoped(controller, layer) {
            Ok(mut observed) => {
                observed = observed.in_drive_of(&original);
                if let Some(scope) = original.trace_scope() {
                    observed = observed.with_trace_scope(scope.clone());
                }
                self.inner.execute_run(observed, admitted).await
            }
            Err(error) => RunEnd::owing_nothing(Err(ShiftAbort::Retry(error))),
        }
    }
}
