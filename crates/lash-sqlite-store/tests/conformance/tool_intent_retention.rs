//! The host tool-intent submission ledger's retention law (FIG-1509).

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;

use super::{Retained, SUBSTRATE};
use crate::backend_fixture::TestBackend;

async fn handles(backend: &TestBackend) -> lash_conformance::ToolIntentRetentionHandles {
    lash_conformance::ToolIntentRetentionHandles {
        registry: backend.process_registry() as Arc<dyn ProcessRegistry>,
        sessions: backend.store().await as Arc<dyn lash_core_execution::DeploymentStore>,
    }
}

lash_conformance::tool_intent_retention_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    ((), move || {
        let retained = retained.clone();
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            let open = handles(&backend).await;
            let reopen: lash_conformance::ToolIntentRetentionReopen = Arc::new(move || {
                let backend = backend.clone();
                let retained = retained.clone();
                Box::pin(async move {
                    let reopened = backend.reopen().await;
                    retained.keep(&reopened);
                    handles(&reopened).await
                })
            });
            lash_conformance::ToolIntentRetentionFixture { open, reopen }
        }
    })
});
