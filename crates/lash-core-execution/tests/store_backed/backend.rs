use std::sync::Arc;

use crate::{Backend, EffectEngine, EffectHost, ProcessWorkWiring, SessionWorkEngine, StoreSet};

struct WrappedEngine {
    inner: Arc<dyn EffectEngine>,
    stores: Arc<dyn StoreSet>,
    generation: crate::engine::BuildGeneration,
    processes: ProcessWorkWiring,
    sessions: Arc<dyn SessionWorkEngine>,
}

impl EffectEngine for WrappedEngine {
    fn stores(&self) -> Arc<dyn StoreSet> {
        Arc::clone(&self.stores)
    }

    fn effect_host(&self) -> Arc<dyn EffectHost> {
        self.inner.effect_host()
    }

    fn build_generation(&self) -> &crate::engine::BuildGeneration {
        &self.generation
    }

    fn process_work(&self) -> ProcessWorkWiring {
        self.processes.clone()
    }

    fn session_work(&self) -> Arc<dyn SessionWorkEngine> {
        Arc::clone(&self.sessions)
    }
}

#[tokio::test]
async fn backend_ports_remain_coherent_through_wrappers() {
    let base = crate::support::memory_store_backend().await;
    let stores: Arc<dyn StoreSet> = Arc::new(CapturedStores::new(base.stores()));
    let host = base.effect_host();
    let mut engine = Arc::clone(base.engine());
    // Each wrapper owns distinct work ports and a generation. Forwarding a
    // work port from the inner engine, or returning the store's undecorated
    // registry, must fail even though all wrappers share the same storage.
    for name in ["wrapper-1", "wrapper-2", "wrapper-3"] {
        let registry: Arc<dyn crate::ProcessRegistry> = Arc::new(
            crate::testing::ProcessRegistryFaults::new(stores.process_registry()),
        );
        let processes = ProcessWorkWiring::without_process_work(registry);
        let sessions: Arc<dyn SessionWorkEngine> = Arc::new(crate::NoSessionWork::new());
        let generation = crate::engine::BuildGeneration::for_test(name);
        engine = Arc::new(WrappedEngine {
            inner: engine,
            stores: Arc::clone(&stores),
            generation: generation.clone(),
            processes: processes.clone(),
            sessions: Arc::clone(&sessions),
        });
        let backend = Backend::new(Arc::clone(&engine));
        for clone in [backend.clone(), backend] {
            assert!(Arc::ptr_eq(clone.engine(), &engine));
            assert!(Arc::ptr_eq(&clone.stores(), &stores));
            assert_eq!(clone.binding_identity(), *stores.binding_identity());
            assert_eq!(clone.build_generation(), &generation);
            assert!(Arc::ptr_eq(&clone.effect_host(), &host));
            assert!(Arc::ptr_eq(&clone.session_work(), &sessions));
            assert!(Arc::ptr_eq(&clone.process_registry(), processes.registry()));
            assert!(Arc::ptr_eq(clone.process_work().port(), processes.port()));
            assert!(!Arc::ptr_eq(
                &clone.process_registry(),
                &stores.process_registry()
            ));
            assert!(Arc::ptr_eq(&clone.clock(), &stores.clock()));
            assert!(Arc::ptr_eq(
                &clone.session_store_factory(),
                &stores.session_store_factory()
            ));
            assert!(Arc::ptr_eq(&clone.trigger_store(), &stores.trigger_store()));
            assert!(Arc::ptr_eq(
                &clone.process_definition_registry(),
                &stores.process_definition_registry()
            ));
            assert!(Arc::ptr_eq(
                &clone.process_env_store(),
                &stores.process_env_store()
            ));
            assert!(Arc::ptr_eq(
                &clone.attachment_store(),
                &stores.attachment_store()
            ));
            assert!(Arc::ptr_eq(
                &clone.module_artifacts(),
                &stores.module_artifacts()
            ));
            assert!(Arc::ptr_eq(
                &clone.recovery_leader(),
                &stores.recovery_leader()
            ));
            assert!(Arc::ptr_eq(
                &clone.generation_drain(),
                &stores.generation_drain()
            ));
            assert!(Arc::ptr_eq(
                &clone.artifact_cleanup(),
                &stores.artifact_cleanup()
            ));
            assert!(Arc::ptr_eq(
                &clone.session_delete_ledger(),
                &stores.session_delete_ledger()
            ));
            for kind in crate::store::ObligationKind::ALL {
                assert!(Arc::ptr_eq(
                    &clone.obligation_ledger(kind),
                    &stores.obligation_ledger(kind)
                ));
            }
        }
    }
}

macro_rules! captured_ports {
    ($($port:ident: $trait:path),+ $(,)?) => {
        struct CapturedStores {
            inner: Arc<dyn StoreSet>,
            $($port: Arc<dyn $trait>,)+
            obligations: std::collections::BTreeMap<crate::store::ObligationKind, Arc<dyn crate::store::ObligationLedger>>,
        }

        impl CapturedStores {
            fn new(inner: Arc<dyn StoreSet>) -> Self {
                Self {
                    $($port: inner.$port(),)+
                    obligations: crate::store::ObligationKind::ALL.into_iter().map(|kind| (kind, inner.obligation_ledger(kind))).collect(),
                    inner,
                }
            }
        }

        impl StoreSet for CapturedStores {
            fn binding_identity(&self) -> &crate::StoreBindingId { self.inner.binding_identity() }
            $(fn $port(&self) -> Arc<dyn $trait> { Arc::clone(&self.$port) })+
            fn obligation_ledger(&self, kind: crate::store::ObligationKind) -> Arc<dyn crate::store::ObligationLedger> {
                Arc::clone(self.obligations.get(&kind).expect("every obligation kind is captured"))
            }
        }
    }
}

// SQLite creates several lightweight handles per call over one connection.
// Capture them once so pointer assertions test Backend's forwarding, without
// confusing a new handle with a different storage binding.
captured_ports! {
    clock: crate::Clock,
    session_store_factory: crate::DeploymentStore,
    process_registry: crate::ProcessRegistry,
    process_continuations: crate::ProcessContinuationStore,
    trigger_store: crate::TriggerStore,
    process_definition_registry: crate::ProcessDefinitionRegistry,
    process_env_store: crate::ProcessExecutionEnvStore,
    attachment_store: crate::AttachmentStore,
    module_artifacts: crate::ModuleArtifactStore,
    recovery_leader: crate::store::RecoveryLeaderStore,
    generation_drain: crate::store::generation_drain::GenerationDrainStore,
    artifact_cleanup: crate::store::ArtifactCleanupLedger,
    session_delete_ledger: crate::store::session_delete::SessionDeleteLedger,
}
