//! H4's real-store publication cut. The decorator retains an actual runtime
//! commit outside the invocation, so disconnecting its caller cannot silently
//! drop the stale request. It delegates every other store operation unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use lash_core::store::{RuntimeCommit, RuntimeCommitReceipt, RuntimeStoreDecorator, ShiftFence};
use lash_core::{DeploymentStore, SessionId, StoreError, StoreSet};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

type CommitAnswer = std::result::Result<RuntimeCommitReceipt, StoreError>;

/// Install the cut before constructing the Restate engine, so its executing
/// handlers and the host use the same decorated store port.
pub fn intercept_publication(
    stores: Arc<dyn StoreSet>,
    session: SessionId,
) -> (Arc<dyn StoreSet>, mpsc::Receiver<HeldPublication>) {
    let (cut, held) = PublicationCutStore::new(stores.session_store_factory(), session);
    let layered = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
        .map_session_store_factory(|_| Arc::new(cut))
        .into_store_set();
    (layered, held)
}

/// A settled fleet's independent storage oracle. The SQL projection is read
/// from a single read-only repeatable-read transaction. Plugin bytes are read
/// through the published store port and must belong to this same head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub session: SessionId,
    pub run: lash_core::TurnId,
    pub shift_epoch: u64,
    pub head_revision: u64,
    pub checkpoint: Option<lash_core::store::BlobRef>,
    pub plugin_component: Option<Vec<u8>>,
    pub admissions: usize,
    pub unfinished_runs: usize,
    pub terminal: lash_core::store::RunTerminalCause,
    pub terminal_head_revision: u64,
    pub input_rows: usize,
    pub bound_inputs: usize,
    pub bound_batches: usize,
    pub open_batches: usize,
    pub commits: Vec<String>,
}

impl FleetSnapshot {
    /// Read only the case's exact session and Run. A missing row is a missing
    /// witness, never a zero that could satisfy an absence assertion.
    pub async fn read(
        pool: &sqlx::PgPool,
        store: &dyn DeploymentStore,
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> Result<Self> {
        use sqlx::Row as _;
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let head = sqlx::query(
            "SELECT meta.shift_epoch, head.head_revision, head.checkpoint_ref
             FROM lash_session_meta meta JOIN lash_session_head head USING (session_id)
             WHERE meta.session_id = $1",
        )
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let terminal = sqlx::query(
            "SELECT terminal_cause_json, terminal_head_revision FROM lash_session_runs
             WHERE session_id = $1 AND run = $2 AND terminal_kind IS NOT NULL",
        )
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let counts = sqlx::query(
            "SELECT
             (SELECT COUNT(*) FROM lash_session_runs WHERE session_id = $1
                AND admission_json IS NOT NULL) AS admissions,
             (SELECT COUNT(*) FROM lash_session_runs WHERE session_id = $1
                AND admission_json IS NOT NULL AND terminal_kind IS NULL) AS unfinished,
             (SELECT COUNT(*) FROM lash_pending_turn_inputs WHERE session_id = $1) AS inputs,
             (SELECT COUNT(*) FROM lash_pending_turn_inputs WHERE session_id = $1
                AND admitted_run IS NOT NULL) AS bound_inputs,
             (SELECT COUNT(*) FROM lash_queued_work_batches WHERE session_id = $1
                AND admitted_run IS NOT NULL) AS bound_batches,
             (SELECT COUNT(*) FROM lash_queued_work_batches WHERE session_id = $1
                AND terminal_cause IS NULL) AS open_batches",
        )
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let commits = sqlx::query_scalar::<_, String>(
            "SELECT turn_id FROM lash_runtime_turn_commits WHERE session_id = $1 ORDER BY turn_id",
        )
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let read = store
            .load_session_window(session, lash_core::store::WindowSelector::Current)
            .await?
            .context("fleet session has no readable head")?;
        let head_revision = u64::try_from(head.try_get::<i64, _>("head_revision")?)?;
        ensure!(
            read.head_revision == head_revision,
            "head moved during fleet snapshot"
        );
        let checkpoint_text: Option<String> = head.try_get("checkpoint_ref")?;
        ensure!(
            read.checkpoint_ref.as_ref().map(ToString::to_string) == checkpoint_text,
            "checkpoint moved during fleet snapshot"
        );
        Ok(Self {
            session: session.clone(),
            run: run.clone(),
            shift_epoch: u64::try_from(head.try_get::<i64, _>("shift_epoch")?)?,
            head_revision,
            checkpoint: read.checkpoint_ref,
            plugin_component: read.checkpoint.as_ref().and_then(|checkpoint| {
                checkpoint
                    .component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
                    .map(<[u8]>::to_vec)
            }),
            admissions: usize::try_from(counts.try_get::<i64, _>("admissions")?)?,
            unfinished_runs: usize::try_from(counts.try_get::<i64, _>("unfinished")?)?,
            terminal: serde_json::from_str(&terminal.try_get::<String, _>("terminal_cause_json")?)?,
            terminal_head_revision: u64::try_from(
                terminal.try_get::<i64, _>("terminal_head_revision")?,
            )?,
            input_rows: usize::try_from(counts.try_get::<i64, _>("inputs")?)?,
            bound_inputs: usize::try_from(counts.try_get::<i64, _>("bound_inputs")?)?,
            bound_batches: usize::try_from(counts.try_get::<i64, _>("bound_batches")?)?,
            open_batches: usize::try_from(counts.try_get::<i64, _>("open_batches")?)?,
            commits,
        })
    }

    pub fn assert_one_settlement(&self) -> Result<()> {
        ensure!(
            self.admissions == 1,
            "expected one admitted Run, found {}",
            self.admissions
        );
        ensure!(self.unfinished_runs == 0, "unfinished Run remains");
        ensure!(self.input_rows == 1, "input was duplicated or lost");
        ensure!(
            self.bound_inputs == 0 && self.bound_batches == 0,
            "terminal retained ingress bindings"
        );
        ensure!(self.open_batches == 0, "open queued work remains");
        ensure!(
            self.terminal_head_revision == self.head_revision,
            "terminal and head disagree"
        );
        let lash_core::store::RunTerminalCause::Committed { turn, .. } = &self.terminal else {
            anyhow::bail!("fleet Run did not finish with a committed outcome");
        };
        ensure!(
            self.commits
                .iter()
                .filter(|key| key.as_str() == turn.as_str())
                .count()
                == 1,
            "expected one commit for the Run's terminal physical turn"
        );
        Ok(())
    }
}

/// One real outgoing publication held before its PostgreSQL transaction.
/// The controller must record the publication barrier before releasing it.
pub struct HeldPublication {
    store: Arc<dyn DeploymentStore>,
    commit: RuntimeCommit,
    answer: oneshot::Sender<CommitAnswer>,
}

/// Only this typed store refusal satisfies S16. Transport errors and a
/// head-CAS loss cannot stand in for the superseded authority check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalePublicationRefusal {
    pub session: SessionId,
    pub fence_epoch: u64,
    pub current_epoch: u64,
}

impl HeldPublication {
    pub fn commit(&self) -> &RuntimeCommit {
        &self.commit
    }

    pub fn fence(&self) -> Result<&ShiftFence> {
        self.commit
            .shift_fence
            .as_deref()
            .context("held runtime publication has no shift fence")
    }

    /// Dispatch the original request once, preserving its operation, state,
    /// bindings and old fence. Its caller may already have disappeared.
    pub async fn release(self) -> Result<StalePublicationRefusal> {
        let fence = self.fence()?.clone();
        let result = self.store.commit_runtime_state(self.commit).await;
        let refusal = match &result {
            Err(StoreError::StaleShiftFence {
                session_id,
                fence_epoch,
                current_epoch,
            }) => {
                ensure!(
                    session_id == fence.session(),
                    "refusal names another session"
                );
                ensure!(
                    *fence_epoch == fence.epoch(),
                    "refusal names another old fence"
                );
                ensure!(
                    *current_epoch > *fence_epoch,
                    "successor did not supersede the fence"
                );
                Ok(StalePublicationRefusal {
                    session: session_id.clone(),
                    fence_epoch: *fence_epoch,
                    current_epoch: *current_epoch,
                })
            }
            Err(error) => Err(anyhow::anyhow!(
                "stale publication refused for another cause: {error}"
            )),
            Ok(_) => Err(anyhow::anyhow!("stale publication was accepted")),
        };
        // A lost observer does not withdraw the retained write. Return its
        // exact typed answer when the original invocation is still waiting.
        let _ = self.answer.send(result);
        refusal
    }
}

/// Intercept only the target session's first fenced terminal commit. Setup,
/// checkpoints and successor/replay writes are never held by this decorator.
pub struct PublicationCutStore {
    inner: Arc<dyn DeploymentStore>,
    session: SessionId,
    armed: AtomicBool,
    held: mpsc::Sender<HeldPublication>,
}

#[lash::async_trait]
impl lash_core::DeploymentStoreDecorator for PublicationCutStore {}

impl PublicationCutStore {
    pub fn new(
        inner: Arc<dyn DeploymentStore>,
        session: SessionId,
    ) -> (Self, mpsc::Receiver<HeldPublication>) {
        let (held, receive) = mpsc::channel(1);
        (
            Self {
                inner,
                session,
                armed: AtomicBool::new(true),
                held,
            },
            receive,
        )
    }
}

#[lash::async_trait]
impl RuntimeStoreDecorator for PublicationCutStore {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(&self, commit: RuntimeCommit) -> CommitAnswer {
        if commit.session_id == self.session
            && commit.outcome.is_some()
            && commit.shift_fence.is_some()
            && self.armed.swap(false, Ordering::SeqCst)
        {
            let (answer, receive) = oneshot::channel();
            self.held
                .send(HeldPublication {
                    store: Arc::clone(&self.inner),
                    commit,
                    answer,
                })
                .await
                .map_err(|_| {
                    StoreError::Backend(
                        "publication controller stopped before receiving the request".into(),
                    )
                })?;
            receive.await.map_err(|_| {
                StoreError::Backend("publication controller dropped the held request".into())
            })?
        } else {
            self.inner.commit_runtime_state(commit).await
        }
    }
}
