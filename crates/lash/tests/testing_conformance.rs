//! Facade proof for base conformance without the RLM feature.

#![cfg(feature = "testing")]

use std::sync::Arc;

use lash::process::ProcessRegistry;
use lash::testing::TestLocalProcessRegistry;
use lash::testing::conformance::process_registry;

#[tokio::test]
async fn base_process_registry_conformance_runs_with_testing_alone() {
    process_registry(|| Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>)
        .await;
}
