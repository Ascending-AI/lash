//! ADR 0109 §7: the settlement waiter's command drain is fenced by the drive
//! epoch, so a drive that seals an admission while the waiter holds the
//! session's lane refuses the waiter's commit.
#![expect(
    clippy::expect_used,
    reason = "law preconditions and outcomes are assertions"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use lash_core::engine::DriveOutcome;
use lash_core::store::{
    RuntimeCommit, RuntimeCommitReceipt, RuntimePersistence, RuntimePersistenceDecorator,
    StoreError,
};

use super::drive_admission::{DriveParts, on_tier};

/// What runs once, inside the waiter's command commit, before the store
/// sees it: the racing drive.
type Race = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// The waiter's store: runs the race inside the first command commit, and
/// counts the command commits the store refused as fenced by a stale drive
/// epoch.
struct RacingStore {
    inner: Arc<dyn RuntimePersistence>,
    race: Mutex<Option<Race>>,
    stale: AtomicUsize,
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for RacingStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        if !commit.completed_queue_claims.is_empty() {
            let race = self.race.lock().expect("race slot").take();
            if let Some(race) = race {
                race().await;
            }
        }
        let result = self.inner.commit_runtime_state(commit).await;
        if matches!(result, Err(StoreError::StaleDriveFence { .. })) {
            self.stale.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

fn model(id: &str) -> crate::ModelSpec {
    crate::ModelSpec::builder(id)
        .context_window_tokens(200_000)
        .build()
        .expect("a fixed conformance model spec is valid")
}

/// ADR 0109 §7: a settlement waiter drains the command lane under the
/// session's drive fence as it read it. A drive on the tier that admits and
/// seals the session's next work while the waiter holds the lane — after its
/// fence read, before its commit — refuses the waiter's command commit: the
/// store writes nothing, and the waiter settles the command on a later pass,
/// under the fence the drive sealed. The command is applied once.
pub async fn a_settlement_drain_is_refused_by_a_drive_that_seals_after_its_fence(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "command-drain-fence", &host, &stores, 8).await;
    // A first drive seals the session's first epoch: the waiter's fence.
    parts
        .enqueue("first", Some("command-drain-fence-root"))
        .await;
    let request = parts.request("command-drain-fence-first");
    let first: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the first drive runs")
        })
    })
    .await;
    assert_eq!(
        first.ran.len(),
        1,
        "the first drive ran its root: {first:?}"
    );
    let sealed = parts.epoch().await.epoch;

    // The race: queued work lands behind the waiter's claimed command, and a
    // drive on the tier admits it and seals. Its root then finds the lane
    // the waiter holds, and stops for a retry.
    let racer = parts.clone();
    let racer_runner = Arc::clone(&runner);
    let race: Race = Box::new(move || {
        Box::pin(async move {
            racer
                .store
                .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                    racer.session_id.clone(),
                    crate::DeliveryPolicy::EarliestSafeBoundary,
                    crate::SessionCommand::RefreshToolCatalog {
                        reason: "work behind the waiter's command".to_owned(),
                    },
                ))
                .await
                .expect("queue the racing drive's work");
            let request = racer.request("command-drain-fence-racer");
            let _ = on_tier(&racer_runner, &racer, move |mut runtime, scope| {
                let request = request.clone();
                Box::pin(async move {
                    lash_core::drive::drive_session(&mut runtime, &scope, &request)
                        .await
                        .is_ok()
                })
            })
            .await;
        })
    });
    let store = Arc::new(RacingStore {
        inner: Arc::clone(&parts.store),
        race: Mutex::new(Some(race)),
        stale: AtomicUsize::new(0),
    });
    let mut waiter = parts
        .runtime_over(Arc::clone(&store) as Arc<dyn RuntimePersistence>)
        .await;
    let settlement = Box::pin(waiter.submit_apply_config_patch_with_idempotency_key(
        crate::ApplyConfigPatch {
            model: Some(model("command-drain-fence-model")),
            ..crate::ApplyConfigPatch::default()
        },
        "command-drain-fence",
    ))
    .await
    .expect("the waiter settles");

    assert_eq!(
        parts.epoch().await.epoch,
        sealed + 1,
        "the racing drive sealed its admission"
    );
    assert_eq!(
        store.stale.load(Ordering::SeqCst),
        1,
        "the waiter's commit under the fence it read first was refused"
    );
    assert!(
        matches!(
            settlement,
            crate::runtime::SessionCommandSettlement::Durable(_)
        ),
        "the command settled durably: {settlement:?}"
    );
    assert_eq!(
        waiter.session_policy().model.id,
        "command-drain-fence-model",
        "the command was applied"
    );
}
