//! Admit store-tier control laws through the same atomic root as production.

use super::shift_admission::{ShiftParts, admitted, on_tier};
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "law fixture admits exactly its queued work"
)]
pub(super) async fn admit(
    parts: &ShiftParts,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> lash_core::engine::Admitted {
    let request = parts.request(name);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                    .await
                    .expect("the real root admits the law's input"),
            )
        })
    })
    .await
}

/// Inject an inconsistent recorded base while production still chooses plugins,
/// cancellation authority, the composition and the atomic root fence.
#[expect(
    clippy::expect_used,
    reason = "law fixture records the requested fault"
)]
pub(super) async fn admit_on_base(
    parts: &ShiftParts,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
    base: crate::store::SessionHeadRef,
) -> lash_core::engine::Admitted {
    let mut recording = parts.clone();
    recording.store = Arc::new(InconsistentBase {
        inner: Arc::clone(&parts.store),
        base,
    });
    admit(&recording, runner, name).await
}

struct InconsistentBase {
    inner: Arc<dyn crate::RuntimeStore>,
    base: crate::store::SessionHeadRef,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for InconsistentBase {
    type Inner = dyn crate::RuntimeStore;
    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }
    async fn prepare_run_admission(
        &self,
        request: &crate::store::AdmitRunRequest,
    ) -> Result<Option<crate::store::PreparedRunAdmission>, crate::StoreError> {
        let mut request = request.clone();
        request.base = self.base.clone();
        self.inner.prepare_run_admission(&request).await
    }
}
