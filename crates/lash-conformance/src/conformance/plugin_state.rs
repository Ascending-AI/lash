//! ADR 0078 laws: the same plugin and runtime checkpoint path on every backend.
use super::*;
use crate::plugin::{
    PluginFactory, PluginRegistrar, PluginSessionContext, SessionAuthorityContext, SessionPlugin,
    SessionReadyContext,
};
use lash_core::plugin::PluginSessionRequest;
use lash_core::{PluginError, PluginStateError, PluginStateView, StateCommands};
use lash_sansio::sync::MutexExt;
use std::sync::Mutex;

#[path = "plugin_state_support.rs"]
mod support;

/// The fixture plugin's id.
const MOCK: &str = "mock-state";

#[path = "plugin_state_boundary.rs"]
mod boundary;
#[path = "plugin_state_fixtures.rs"]
mod fixtures;
#[path = "plugin_state_formats.rs"]
mod formats;
#[path = "plugin_state_lifecycle.rs"]
mod lifecycle;
#[path = "plugin_state_refusal.rs"]
mod refusal;
#[path = "plugin_state_registration.rs"]
mod registration;

pub use boundary::plugin_state_boundary_trace;
use fixtures::{MockPlugin, Registration, owner_key};
use formats::{FormatPlugin, plugin_format_boundary};
use lifecycle::runtime_plugin_state_park_law;
use refusal::plugin_state_corrupt_boundary;
use registration::registration_state_law;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit(store: &Arc<dyn RuntimeStore>, state: &mut RuntimeSessionState) {
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state),
        "plugin-state-law",
    )
    .await
    .expect("boundary commit");
    state.apply_persisted_commit_result(receipt);
}

pub async fn plugin_state_boundary(make: impl Fn(&str) -> Arc<dyn RuntimeStore>, label: &str) {
    for version in [1, 3] {
        let id = format!("{label}-format-{version}");
        plugin_format_boundary(make(&id), &id, version).await;
    }
    let register_remove = format!("{label}-register-remove");
    registration_state_law(
        make(&register_remove),
        &register_remove,
        Registration::Remove,
    )
    .await;
    let register_admission = format!("{label}-register-admission");
    registration_state_law(
        make(&register_admission),
        &register_admission,
        Registration::Admission,
    )
    .await;
    let parent = format!("{label}-parent");
    let child = format!("{label}-child");
    plugin_state_boundary_trace(make(&parent), &parent, make(&child), &child).await;
    let corrupt = format!("{label}-corrupt");
    plugin_state_corrupt_boundary(make(&corrupt), &corrupt).await;
    let lifecycle = format!("{label}-lifecycle");
    Box::pin(runtime_plugin_state_park_law(make(&lifecycle))).await;
}
