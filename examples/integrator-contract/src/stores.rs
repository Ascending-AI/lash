//! A storage integrator's store set, authored against the public facade.
//!
//! This example delegates to one substrate. A third-party implementation
//! replaces the individual ports while preserving their shared transaction
//! boundary and binding identity. Its certification lives in
//! `tests/conformance.rs`; no conformance dependency enters production code.

use std::sync::Arc;

use lash::durable::{DurableStore, NodeWakes};
use lash::persistence::{
    ArtifactCleanupLedger, AttachmentReferrers, AttachmentStore, DeploymentStore,
    ModuleArtifactStore, ObligationLedger, ProcessDefinitionStore, ProcessExecutionEnvStore,
    ProcessRegistry, RecoveryLeaderStore, ToolMaterialStore, TurnPreludeStore,
};
use lash::runtime::Clock;
use lash::{ObligationKind, StoreBindingId, StoreSet};

/// The integrator's implementation of the complete storage-port contract.
#[derive(Clone)]
pub struct IntegratorStores {
    substrate: Arc<dyn StoreSet>,
}

impl IntegratorStores {
    /// Use one substrate for every port, including its identity and clock.
    pub fn new(substrate: Arc<dyn StoreSet>) -> Self {
        Self { substrate }
    }
}

impl StoreSet for IntegratorStores {
    fn durable_store(&self) -> Arc<dyn DurableStore> {
        self.substrate.durable_store()
    }

    fn node_wakes(&self) -> Option<Arc<dyn NodeWakes>> {
        self.substrate.node_wakes()
    }

    fn binding_identity(&self) -> &StoreBindingId {
        self.substrate.binding_identity()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.substrate.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.substrate.session_store_factory()
    }

    fn attachment_referrers(&self) -> Arc<dyn AttachmentReferrers> {
        self.substrate.attachment_referrers()
    }

    fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        self.substrate.process_registry()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.substrate.process_env_store()
    }

    fn turn_prelude_store(&self) -> Arc<dyn TurnPreludeStore> {
        self.substrate.turn_prelude_store()
    }

    fn tool_material_store(&self) -> Arc<dyn ToolMaterialStore> {
        self.substrate.tool_material_store()
    }

    fn definition_store(&self) -> Arc<dyn ProcessDefinitionStore> {
        self.substrate.definition_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.substrate.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.substrate.module_artifacts()
    }

    fn recovery_leader(&self) -> Arc<dyn RecoveryLeaderStore> {
        self.substrate.recovery_leader()
    }

    fn obligation_ledger(&self, kind: ObligationKind) -> Arc<dyn ObligationLedger> {
        self.substrate.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn ArtifactCleanupLedger> {
        self.substrate.artifact_cleanup()
    }
}
