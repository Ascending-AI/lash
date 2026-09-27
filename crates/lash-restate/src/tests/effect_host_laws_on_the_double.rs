//! The engine-neutral effect-host laws the SQLite suite carried, on the
//! in-process server double's deployment host (FIG-3668): `scoped` and
//! `scoped_static` answer scope metadata on the deployment boundary without
//! entering a handler, so the laws port unchanged. The await-event and
//! journaled-effect legs live in their own mounts.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::effect_host_tests!({
    let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
    let make = harness.effect_host_factory();
    (harness, make)
});
