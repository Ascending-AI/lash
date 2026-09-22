//! Facade proof for base conformance without the RLM feature.

#![cfg(feature = "testing")]

use std::sync::Arc;

use lash::process::ConformanceProcessRegistry;
use lash::testing::TestLocalProcessRegistry;
lash_conformance::process_registry_tests!({
    ((), |_: &str| {
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ConformanceProcessRegistry>
    })
});
