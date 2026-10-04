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
