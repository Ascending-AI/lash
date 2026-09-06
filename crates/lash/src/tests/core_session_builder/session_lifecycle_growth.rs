use super::*;
use lash_core::store::{
    RuntimeCommit, RuntimeCommitReceipt, RuntimePersistence, RuntimePersistenceDecorator,
};
use lash_core::{AttachmentId, AttachmentRootSet, SessionStoreCreateRequest, SessionStoreFactory};
use std::collections::BTreeSet;
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct CommitSample {
    budget: lash_core::testing::RuntimeCommitBudgetMeasurement,
    rewritten_leaves: Vec<String>,
}

struct GrowthStore {
    inner: Arc<dyn RuntimePersistence>,
    samples: Arc<Mutex<Vec<CommitSample>>>,
}

#[async_trait]
impl RuntimePersistenceDecorator for GrowthStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> std::result::Result<RuntimeCommitReceipt, StoreError> {
        let sample = CommitSample {
            budget: lash_core::testing::measure_runtime_commit_budget(&commit)?,
            rewritten_leaves: commit
                .checkpoint
                .components
                .iter()
                .filter(|(key, component)| {
                    key.starts_with("execution_state/") && component.blob_ref().is_none()
                })
                .map(|(key, _)| key.clone())
                .collect(),
        };
        let receipt = self.inner.commit_runtime_state(commit).await?;
        self.samples.lock_recover().push(sample);
        Ok(receipt)
    }
}

struct GrowthFactory {
    inner: lash_core::facade_support::InMemorySessionStoreFactory,
    samples: Arc<Mutex<Vec<CommitSample>>>,
}

#[async_trait]
impl SessionStoreFactory for GrowthFactory {
    async fn session_was_deleted(&self, session_id: &str) -> std::result::Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &str,
    ) -> lash_core::store::MaintenanceResult<lash_core::store::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(Arc::new(GrowthStore {
            inner: self.inner.create_store(request).await?,
            samples: Arc::clone(&self.samples),
        }))
    }
}

#[async_trait]
impl AttachmentRootSet for GrowthFactory {
    async fn live_attachment_refs(
        &self,
        cutoff: u64,
    ) -> std::result::Result<BTreeSet<AttachmentId>, StoreError> {
        self.inner.live_attachment_refs(cutoff).await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        cutoff: u64,
    ) -> std::result::Result<bool, StoreError> {
        self.inner.has_live_attachment_ref(id, cutoff).await
    }
}

fn assert_flat_checkpoint_sizes(peaks: &[usize]) {
    let min = peaks.iter().min().expect("checkpoint samples");
    let max = peaks.iter().max().expect("checkpoint samples");
    assert_eq!(
        min, max,
        "checkpoint size must remain flat across dirty turns: {peaks:?}"
    );
}

#[test]
fn flat_commit_growth_after_large_bindings_stabilize() -> Result<()> {
    run_async_test_on_stack_budget("flat-checkpoint-growth", || async {
        const LARGE_BINDINGS: usize = 16;
        const SMALL_BINDINGS: usize = 80;
        const DIRTY_TURNS: usize = 40;
        const VALUE_BYTES: usize = 8192;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let mut programs = Vec::new();
        for index in 0..LARGE_BINDINGS {
            let value = format!("{index:02}{}", "x".repeat(VALUE_BYTES - 2));
            let mut source = format!("large_{index:02} = {value:?}\n");
            if index == 0 {
                for small in 0..SMALL_BINDINGS {
                    source.push_str(&format!("small_{small:02} = 100\n"));
                }
            }
            source.push_str("finish \"stored\"");
            programs.push(lashlang_block(&source));
        }
        for index in 0..DIRTY_TURNS {
            programs.push(lashlang_block(&format!(
                "small_{index:02} = 101\nfinish \"stored\""
            )));
        }
        let replacement = format!("00{}y", "x".repeat(VALUE_BYTES - 3));
        programs.push(lashlang_block(&format!(
            "large_00 = {replacement:?}\nfinish \"stored\""
        )));
        programs.push(lashlang_block("small_00 = 102\nfinish \"stored\""));
        let core = explicit_ephemeral_facets(rlm_core_builder())
            .provider(queued_text_provider(programs))
            .model(mock_model_spec())
            .store_factory(Arc::new(GrowthFactory {
                inner: lash_core::facade_support::InMemorySessionStoreFactory::new(),
                samples: Arc::clone(&samples),
            }))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("flat-checkpoint-growth").open().await?;
        for _ in 0..LARGE_BINDINGS {
            session
                .turn(TurnInput::text("store"))
                .require_finish()?
                .run()
                .await?;
        }
        let state = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("bound execution state");
        assert_eq!(
            state.components.len(),
            LARGE_BINDINGS,
            "sixteen distinct large leaves"
        );
        let state_bytes = state.root.len() + state.components.values().map(Vec::len).sum::<usize>();
        assert!(state_bytes > LARGE_BINDINGS * VALUE_BYTES);
        samples.lock_recover().clear();
        let mut peaks = Vec::new();
        let mut commit_count = 0;
        for turn in 0..DIRTY_TURNS {
            session
                .turn(TurnInput::text("touch"))
                .require_finish()?
                .run()
                .await?;
            let writes = std::mem::take(&mut *samples.lock_recover());
            assert!(!writes.is_empty(), "turn {turn} committed");
            assert!(
                writes
                    .iter()
                    .all(|sample| sample.rewritten_leaves.is_empty()),
                "stable large values must never be resubmitted: turn {turn}: {writes:?}"
            );
            assert!(
                writes
                    .iter()
                    .all(|sample| sample.budget.total_bytes < state_bytes / 2),
                "sanity floor: stable leaf bodies must be excluded from the commit budget"
            );
            commit_count += writes.len();
            peaks.push(
                writes
                    .iter()
                    // FIG-1196 bounds checkpoint growth, not graph JSON: the
                    // SystemClock RFC3339 timestamp can shed three fractional
                    // digits. Use the typed budget's named-MessagePack manifest
                    // plus submitted component bodies, retaining all checkpoint
                    // state and reference overhead in the exact flatness law.
                    .map(|sample| sample.budget.checkpoint_bytes)
                    .max()
                    .unwrap(),
            );
        }
        let min = *peaks.iter().min().unwrap();
        let max = *peaks.iter().max().unwrap();
        eprintln!(
            "FIG-1196 dirty-turn checkpoint peaks={peaks:?}; min={min}; max={max}; spread={}; state_bytes={state_bytes}; commits={commit_count}",
            max - min
        );
        assert_flat_checkpoint_sizes(&peaks);
        session
            .turn(TurnInput::text("rebind"))
            .require_finish()?
            .run()
            .await?;
        let writes = std::mem::take(&mut *samples.lock_recover());
        let rewritten = writes
            .iter()
            .flat_map(|sample| &sample.rewritten_leaves)
            .collect::<Vec<_>>();
        assert_eq!(
            rewritten.len(),
            1,
            "rebinding one large value must submit exactly one leaf body: {writes:?}"
        );
        let after = session
            .admin()
            .state()
            .snapshot_execution()
            .await?
            .expect("rebound execution state");
        assert_eq!(after.components.len(), LARGE_BINDINGS);
        assert_eq!(
            after
                .components
                .keys()
                .filter(|key| !state.components.contains_key(*key))
                .count(),
            1
        );
        assert_eq!(
            state
                .components
                .keys()
                .filter(|key| !after.components.contains_key(*key))
                .count(),
            1
        );
        eprintln!(
            "FIG-1196 rebind: exactly one rewritten leaf; fifteen retained leaf identities; state_bytes={state_bytes}"
        );
        session
            .turn(TurnInput::text("touch"))
            .require_finish()?
            .run()
            .await?;
        assert!(
            samples
                .lock_recover()
                .iter()
                .all(|sample| sample.rewritten_leaves.is_empty()),
            "replacement leaf must also stabilize"
        );
        Ok(())
    })
}

#[test]
fn checkpoint_flatness_rejects_a_binding_that_grows_each_turn() -> Result<()> {
    run_async_test_on_stack_budget("growing-checkpoint-witness", || async {
        const GROWING_TURNS: usize = 4;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let mut programs = vec![lashlang_block("growing = \"\"\nfinish \"stored\"")];
        for turn in 1..=GROWING_TURNS {
            programs.push(lashlang_block(&format!(
                "growing = {:?}\nfinish \"stored\"",
                "x".repeat(turn * 256)
            )));
        }
        let core = explicit_ephemeral_facets(rlm_core_builder())
            .provider(queued_text_provider(programs))
            .model(mock_model_spec())
            .store_factory(Arc::new(GrowthFactory {
                inner: lash_core::facade_support::InMemorySessionStoreFactory::new(),
                samples: Arc::clone(&samples),
            }))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("growing-checkpoint-witness").open().await?;
        session
            .turn(TurnInput::text("store"))
            .require_finish()?
            .run()
            .await?;
        samples.lock_recover().clear();
        let mut peaks = Vec::new();
        for _ in 0..GROWING_TURNS {
            session
                .turn(TurnInput::text("grow"))
                .require_finish()?
                .run()
                .await?;
            let writes = std::mem::take(&mut *samples.lock_recover());
            peaks.push(
                writes
                    .iter()
                    .map(|sample| sample.budget.checkpoint_bytes)
                    .max()
                    .expect("growing turn committed"),
            );
        }
        assert!(
            peaks.windows(2).all(|pair| pair[1] > pair[0]),
            "real binding growth must increase each checkpoint: {peaks:?}"
        );
        // Catch only the shared law: a setup/runtime panic cannot satisfy the witness.
        assert!(
            std::panic::catch_unwind(|| assert_flat_checkpoint_sizes(&peaks)).is_err(),
            "the flatness law must reject real per-turn growth: {peaks:?}"
        );
        Ok(())
    })
}
