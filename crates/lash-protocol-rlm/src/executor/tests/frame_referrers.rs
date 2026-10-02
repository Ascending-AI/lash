//! A frame holds every module its globals reference (ADR 0113 §3.1).

use super::lifecycle_and_diagnostics::block_on;
use super::*;

const SEED: u64 = 0x0004_031f;

/// One write the executor made through the module port.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ArtifactWrite {
    Publish(lash_core::ArtifactReferrerKind, String),
    Acquire(lash_core::ArtifactReferrerKind, String),
}

/// A module port that records every write, fences the referrers a test
/// names, and answers `ArtifactMissing` for bytes it does not hold.
#[derive(Default)]
struct RecordingArtifactStore {
    stored: Mutex<BTreeMap<String, Vec<u8>>>,
    double: tokio::sync::OnceCell<lash_restate_test::RestateTestBackend>,
    /// Every frame environment has ended: the cells run as a replay of the
    /// turn that switched away from their frame.
    frames_ended: std::sync::atomic::AtomicBool,
    writes: Mutex<Vec<ArtifactWrite>>,
}

impl RecordingArtifactStore {
    fn writes(&self) -> Vec<ArtifactWrite> {
        self.writes.lock_recover().clone()
    }

    fn check_fence(
        &self,
        claim: &lash_core::ReferrerClaim,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        if self.frames_ended.load(Ordering::SeqCst)
            && claim.referrer().kind() == lash_core::ArtifactReferrerKind::FrameEnvironment
        {
            return Err(lash_core::ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer().clone(),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash_core::ModuleArtifactStore for RecordingArtifactStore {
    async fn publish_module_artifact(
        &self,
        claim: &lash_core::ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.check_fence(claim)?;
        self.double
            .get()
            .expect("store initialized")
            .lash_backend()
            .module_artifacts()
            .publish_module_artifact(claim, module_ref, bytes)
            .await?;
        self.stored
            .lock_recover()
            .insert(module_ref.to_string(), bytes.to_vec());
        self.writes.lock_recover().push(ArtifactWrite::Publish(
            claim.referrer().kind(),
            module_ref.to_string(),
        ));
        Ok(())
    }

    async fn acquire_module_artifact(
        &self,
        claim: &lash_core::ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.check_fence(claim)?;
        self.writes.lock_recover().push(ArtifactWrite::Acquire(
            claim.referrer().kind(),
            module_ref.to_string(),
        ));
        self.double
            .get()
            .expect("store initialized")
            .lash_backend()
            .module_artifacts()
            .acquire_module_artifact(claim, module_ref)
            .await
    }

    async fn end_module_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        Ok(self.stored.lock_recover().get(module_ref).cloned())
    }
}

const PROCESS_CELL: &str = r#"
    const worker = async (input: unknown) => { return input; };
    finish(1);
"#;

async fn run_cell(
    state: &mut RlmExecutionState,
    store: &Arc<RecordingArtifactStore>,
    code: &str,
) -> ExecResponse {
    let double = store
        .double
        .get_or_init(|| {
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default())
        })
        .await;
    let handler = double
        .open_handler(crate::testing::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let backend = double.lash_backend();
    let mut ports = crate::testing::double_ports(double, &handler)
        .with_module_artifact_store(&backend, store.clone());
    ports.process_engines = lash_core::ProcessEngineRegistry::new().with_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                lashlang::LashlangArtifacts::new(store.clone()),
                LashlangSurface::default(),
                backend.worker_recovery(),
            ),
        ),
    );
    let ctx = lash_core::testing::code_execution_context(ports);
    assert!(
        super::super::frame_environment(&ctx).is_some(),
        "the test context admits cells on a frame"
    );
    let response = execute_code_with_test_render(
        state,
        ctx,
        ExecRequest {
            code: code.to_string(),
        },
        lashlang::LashlangArtifacts::new(
            Arc::clone(store) as Arc<dyn lash_core::ModuleArtifactStore>
        ),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    handler.close().await.expect("close the cell's handler");
    response
}

/// A process definition value naming a module built from `source`.
fn definition_value(source: &str) -> (String, FlowValue) {
    let module_ref = lashlang::ModuleRef::new(&lashlang::ContentHash::new(source));
    let identity = lashlang::ProcessDefinitionIdentity::new(
        module_ref.clone(),
        lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("host")),
        lashlang::ProcessRef::new(lashlang::ContentHash::new("component"), 0),
        "run",
    );
    (
        module_ref.to_string(),
        lashlang::from_json(identity.to_process_value()),
    )
}

#[test]
fn a_cell_module_is_published_under_its_execution_then_held_by_its_frame() {
    block_on(async {
        let store = Arc::new(RecordingArtifactStore::default());
        let mut state = RlmExecutionState::for_engine("typescript");
        let first = run_cell(&mut state, &store, PROCESS_CELL).await;
        assert!(first.error.is_none(), "{:?}", first.error);
        let writes = store.writes();
        let Some(ArtifactWrite::Publish(lash_core::ArtifactReferrerKind::Execution, published)) =
            writes.first()
        else {
            panic!("publication must precede acquisition: {writes:?}");
        };
        assert!(writes.iter().any(|write| matches!(write, ArtifactWrite::Acquire(lash_core::ArtifactReferrerKind::FrameEnvironment, held) if held == published)));

        // The frame already holds it: the same cell again writes nothing.
        let second = run_cell(&mut state, &store, PROCESS_CELL).await;
        assert!(second.error.is_none(), "{:?}", second.error);
        assert_eq!(store.writes(), writes);
    });
}

#[test]
fn a_switched_frame_refusing_its_edge_is_a_replay_and_the_cell_goes_on() {
    block_on(async {
        let store = Arc::new(RecordingArtifactStore::default());
        store.frames_ended.store(true, Ordering::SeqCst);
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = run_cell(&mut state, &store, PROCESS_CELL).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            matches!(
                store.writes().first(),
                Some(ArtifactWrite::Publish(
                    lash_core::ArtifactReferrerKind::Execution,
                    _
                ))
            ),
            "the publication still lands under its execution: {:?}",
            store.writes()
        );
        assert_eq!(state.frame_held_module_refs().count(), 0);
    });
}

#[test]
fn a_bare_module_reference_does_not_acquire_a_definition() {
    block_on(async {
        let store = Arc::new(RecordingArtifactStore::default());
        let (_, value) = definition_value("never-published");
        let mut state = RlmExecutionState::for_engine("typescript");
        state
            .vm
            .state_mut()
            .insert_global("bare", value)
            .await
            .expect("bind bare module refs");
        let response = run_cell(&mut state, &store, "finish(1);").await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            store.writes().is_empty(),
            "bare module refs are not definition holders"
        );
    });
}

#[test]
fn a_definition_held_only_inside_a_map_is_held_by_the_frame() {
    block_on(async {
        let store = Arc::new(RecordingArtifactStore::default());
        let mut state = RlmExecutionState::for_engine("typescript");
        let bound = run_cell(
            &mut state,
            &store,
            "const q = async () => 2; const m = new Map([['q', q]]); finish(1);",
        )
        .await;
        assert!(bound.error.is_none(), "{:?}", bound.error);
        let module_ref = state
            .frame_held_module_refs()
            .next()
            .expect("the cell's module")
            .to_string();

        // A cold process: the frame cache starts empty, and only the map
        // (which the host view omits) still names the module.
        let snapshot = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("capture");
        let mut restored = RlmExecutionState::for_engine("typescript");
        restored
            .restore_execution_state(
                &super::lifecycle_and_diagnostics::hydrate_snapshot(snapshot),
                lash_core::FleetFormat::current(),
            )
            .await
            .expect("restore");
        assert!(
            restored
                .vm
                .state_mut()
                .remove_global("q")
                .await
                .expect("remove binding")
        );
        assert!(
            restored.vm.state().globals().get("m").is_none(),
            "the host view omits the map"
        );
        let before = store.writes().len();
        let response = run_cell(&mut restored, &store, "finish(2);").await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            store.writes()[before..].is_empty(),
            "SQL owns the descriptor and its module closure acquisition"
        );
        let ids = restored.vm.state().referenced_definition_ids();
        assert_eq!(
            ids.len(),
            1,
            "the map remains a guest root after q is removed"
        );
        let backend = store.double.get().expect("double").lash_backend();
        let definition = backend
            .definition_store()
            .get_process_definition(ids.first().expect("map candidate"))
            .await
            .expect("descriptor read");
        assert!(
            definition.is_some(),
            "the parent validated and acquired the stored descriptor"
        );
        assert!(
            backend
                .module_artifacts()
                .get_module_artifact(&module_ref)
                .await
                .expect("module read")
                .is_some()
        );
    });
}
