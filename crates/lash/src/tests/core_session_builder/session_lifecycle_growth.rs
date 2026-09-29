use super::*;
use crate::rlm::RlmSendBuilderExt as _;
use lash_core::store::{RuntimeCommit, RuntimeCommitReceipt};
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct CommitSample {
    budget: lash_core::testing::RuntimeCommitBudgetMeasurement,
    rewritten_leaves: Vec<String>,
}

struct GrowthFactory {
    inner: Arc<dyn lash_core::DeploymentStore>,
    samples: Arc<Mutex<Vec<CommitSample>>>,
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for GrowthFactory {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
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

impl lash_core::DeploymentStoreDecorator for GrowthFactory {}

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
            let mut source = format!("let large_{index:02} = {value:?};\n");
            if index == 0 {
                for small in 0..SMALL_BINDINGS {
                    source.push_str(&format!("let small_{small:02} = 100;\n"));
                }
            }
            source.push_str("finish(\"stored\");");
            programs.push(typescript_block(&source));
        }
        for index in 0..DIRTY_TURNS {
            programs.push(typescript_block(&format!(
                "let small_{index:02} = 101;\nfinish(\"stored\");"
            )));
        }
        let replacement = format!("00{}y", "x".repeat(VALUE_BYTES - 3));
        programs.push(typescript_block(&format!(
            "let large_00 = {replacement:?};\nfinish(\"stored\");"
        )));
        programs.push(typescript_block("let small_00 = 102;\nfinish(\"stored\");"));
        let growth_samples = Arc::clone(&samples);
        let backend =
            DecoratedBackend::over(double_backend().await).session_store_factory(move |inner| {
                Arc::new(GrowthFactory {
                    inner,
                    samples: growth_samples,
                })
            });
        let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.into()))
            .provider(queued_text_provider(programs))
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("flat-checkpoint-growth")
            .created()
            .await
            .open()
            .await?;
        for _ in 0..LARGE_BINDINGS {
            session
                .send(TurnInput::text("store"))
                .require_finish()?
                .output()
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
        let state_bytes =
            state.root.len() + state.components.values().map(|v| v.len()).sum::<usize>();
        assert!(state_bytes > LARGE_BINDINGS * VALUE_BYTES);
        samples.lock_recover().clear();
        let mut peaks = Vec::new();
        let mut commit_count = 0;
        for turn in 0..DIRTY_TURNS {
            session
                .send(TurnInput::text("touch"))
                .require_finish()?
                .output()
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
            .send(TurnInput::text("rebind"))
            .require_finish()?
            .output()
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
            .send(TurnInput::text("touch"))
            .require_finish()?
            .output()
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
        let mut programs = vec![typescript_block("let growing = \"\";\nfinish(\"stored\");")];
        for turn in 1..=GROWING_TURNS {
            programs.push(typescript_block(&format!(
                "let growing = {:?};\nfinish(\"stored\");",
                "x".repeat(turn * 256)
            )));
        }
        let growth_samples = Arc::clone(&samples);
        let backend =
            DecoratedBackend::over(double_backend().await).session_store_factory(move |inner| {
                Arc::new(GrowthFactory {
                    inner,
                    samples: growth_samples,
                })
            });
        let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.into()))
            .provider(queued_text_provider(programs))
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("growing-checkpoint-witness")
            .created()
            .await
            .open()
            .await?;
        session
            .send(TurnInput::text("store"))
            .require_finish()?
            .output()
            .await?;
        samples.lock_recover().clear();
        let mut peaks = Vec::new();
        for _ in 0..GROWING_TURNS {
            session
                .send(TurnInput::text("grow"))
                .require_finish()?
                .output()
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
