//! A frame holds every module its globals reference (ADR 0113 §3.1).

use super::lifecycle_and_diagnostics::block_on;
use super::*;

const SEED: u64 = 0x4031_f;

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
    stored: Mutex<BTreeSet<String>>,
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
        _bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.check_fence(claim)?;
        self.stored.lock_recover().insert(module_ref.to_string());
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
        if !self.stored.lock_recover().contains(module_ref) {
            return Err(lash_core::ArtifactStoreError::ArtifactMissing {
                artifact_ref: module_ref.to_string(),
            });
        }
        Ok(())
    }

    async fn end_module_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        Ok(None)
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
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(crate::testing::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let ctx =
        lash_core::testing::code_execution_context(crate::testing::double_ports(&double, &handler));
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
        RlmLashlangExecutionTraceConfig::default(),
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
        let [
            ArtifactWrite::Publish(lash_core::ArtifactReferrerKind::Execution, published),
            ArtifactWrite::Acquire(lash_core::ArtifactReferrerKind::FrameEnvironment, held),
            ..,
        ] = writes.as_slice()
        else {
            panic!("expected publish under the execution, then the frame's edge: {writes:?}");
        };
        assert_eq!(published, held);

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
fn every_module_a_global_references_is_held_by_the_frame_once() {
    block_on(async {
        let store = Arc::new(RecordingArtifactStore::default());
        let (held, held_value) = definition_value("held");
        let (missing, missing_value) = definition_value("never-published");
        store.stored.lock_recover().insert(held.clone());
        let mut state = RlmExecutionState::for_engine("typescript");
        let mut record = FlowRecord::new();
        record.insert("inner".to_string(), held_value);
        state
            .rlm
            .insert_global("nested", FlowValue::Record(Arc::new(record)))
            .expect("bind a global holding a definition");
        state
            .rlm
            .insert_global("dangling", missing_value)
            .expect("bind a global naming absent bytes");

        let first = run_cell(&mut state, &store, "finish(1);").await;
        assert!(first.error.is_none(), "{:?}", first.error);
        let frame = lash_core::ArtifactReferrerKind::FrameEnvironment;
        let mut acquired = store.writes();
        acquired.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        let mut expected = vec![
            ArtifactWrite::Acquire(frame, held.clone()),
            ArtifactWrite::Acquire(frame, missing.clone()),
        ];
        expected.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        assert_eq!(acquired, expected);

        // Absent bytes cannot be revived by an edge, so they are not asked
        // for again; the held module is cached.
        let second = run_cell(&mut state, &store, "finish(2);").await;
        assert!(second.error.is_none(), "{:?}", second.error);
        assert_eq!(store.writes().len(), 2, "{:?}", store.writes());
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
            .expect("capture");
        let mut restored = RlmExecutionState::for_engine("typescript");
        restored
            .restore_execution_state(
                &super::lifecycle_and_diagnostics::hydrate_snapshot(snapshot),
                lash_core::FleetFormat::current(),
            )
            .expect("restore");
        assert!(restored.rlm.remove_global("q"));
        assert!(
            restored.rlm.globals().get("m").is_none(),
            "the host view omits the map"
        );
        let before = store.writes().len();
        let response = run_cell(&mut restored, &store, "finish(2);").await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            store.writes()[before..],
            [ArtifactWrite::Acquire(
                lash_core::ArtifactReferrerKind::FrameEnvironment,
                module_ref
            )]
        );
    });
}
