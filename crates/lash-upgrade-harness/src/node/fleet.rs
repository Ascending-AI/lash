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

pub mod host;
mod observation;
mod receipts;
pub mod scenario;

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
    pub frontier: Option<lash_core::PluginNamespaceState>,
    pub admissions: usize,
    pub unfinished_runs: usize,
    pub terminal: lash_core::store::RunTerminalCause,
    pub terminal_head_revision: u64,
    pub input_rows: usize,
    pub bound_inputs: usize,
    pub bound_batches: usize,
    pub open_batches: usize,
    pub head: serde_json::Value,
    pub admission: serde_json::Value,
    pub input_bindings: Vec<(String, String)>,
    pub inputs: Vec<FleetInputReceipt>,
    pub commits: Vec<FleetCommitReceipt>,
}

/// Persisted business receipts, excluding the independently changing relay
/// lease columns. Equality pins both identities and committed material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetInputReceipt {
    pub id: String,
    pub source_key: Option<String>,
    pub ingress: serde_json::Value,
    pub input: serde_json::Value,
    pub digest: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCommitReceipt {
    pub turn: String,
    pub hash: String,
    pub result: serde_json::Value,
    pub outcome: Option<String>,
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
            "SELECT meta.shift_epoch, head.head_revision, head.checkpoint_ref, head.head_json
             FROM lash_session_meta meta JOIN lash_session_head head USING (session_id)
             WHERE meta.session_id = $1",
        )
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let terminal = sqlx::query(
            "SELECT terminal_cause_json, terminal_head_revision, admission_json FROM lash_session_runs
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
        let input_bindings = sqlx::query_as::<_, (String, String)>(
            "SELECT input_id, run FROM lash_session_run_inputs WHERE session_id = $1
             ORDER BY input_id",
        )
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let input_rows = sqlx::query(
            "SELECT input_id, source_key, ingress_json, input_json, submission_digest, state
             FROM lash_pending_turn_inputs WHERE session_id = $1 ORDER BY input_id",
        )
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let inputs = input_rows
            .iter()
            .map(|row| {
                Ok(FleetInputReceipt {
                    id: row.try_get("input_id")?,
                    source_key: row.try_get("source_key")?,
                    ingress: serde_json::from_str(&row.try_get::<String, _>("ingress_json")?)?,
                    input: serde_json::from_str(&row.try_get::<String, _>("input_json")?)?,
                    digest: row.try_get("submission_digest")?,
                    state: row.try_get("state")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let commit_rows = sqlx::query(
            "SELECT turn_id, turn_commit_hash, result_json, outcome_code
             FROM lash_runtime_turn_commits WHERE session_id = $1 ORDER BY turn_id",
        )
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await?;
        let commits = commit_rows
            .iter()
            .map(|row| {
                Ok(FleetCommitReceipt {
                    turn: row.try_get("turn_id")?,
                    hash: row.try_get("turn_commit_hash")?,
                    result: serde_json::from_str(&row.try_get::<String, _>("result_json")?)?,
                    outcome: row.try_get("outcome_code")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
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
        let checkpoint = read.checkpoint_ref.clone();
        let plugin_component = read.checkpoint.as_ref().and_then(|checkpoint| {
            checkpoint
                .component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
                .map(<[u8]>::to_vec)
        });
        let state = lash_core::store::window_state(read, store.fleet_format())?.state;
        let frontier = state
            .plugin_state()
            .and_then(|plugins| plugins.plugins.get(FRONTIER_PLUGIN))
            .cloned();
        Ok(Self {
            session: session.clone(),
            run: run.clone(),
            shift_epoch: u64::try_from(head.try_get::<i64, _>("shift_epoch")?)?,
            head_revision,
            checkpoint,
            plugin_component,
            frontier,
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
            head: serde_json::from_str(&head.try_get::<String, _>("head_json")?)?,
            admission: serde_json::to_value(
                serde_json::from_str::<lash_core::store::RunAdmission>(
                    &terminal.try_get::<String, _>("admission_json")?,
                )?,
            )?,
            input_bindings,
            inputs,
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
        ensure!(
            self.input_rows == 1 && self.inputs.len() == 1,
            "input was duplicated or lost"
        );
        ensure!(
            self.input_bindings == vec![(self.inputs[0].id.clone(), self.run.to_string())],
            "accepted input does not retain its original Run binding"
        );
        ensure!(
            matches!(self.inputs[0].state.as_str(), "completed" | "cancelled"),
            "accepted input has no terminal tombstone"
        );
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
            receipts::terminal_commit_count(&self.commits, &self.session, turn)? == 1,
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

impl HeldPublication {
    pub fn commit(&self) -> &RuntimeCommit {
        &self.commit
    }

    /// Preserve the original request and its nonserialized store instructions
    /// before disconnect. This artifact is observation only; release uses the
    /// retained typed request, never a reconstructed JSON commit.
    pub fn capture(&self, path: &std::path::Path) -> Result<()> {
        let terminal = self
            .commit
            .run_terminal
            .as_deref()
            .context("held publication has no logical Run terminal")?;
        let receipt = serde_json::json!({
            "request": &self.commit,
            "fence": self.fence()?,
            "terminal": terminal,
            "ingress_settlement": &self.commit.ingress,
            "park_run": &self.commit.park_run,
        });
        super::write_atomically(path, &serde_json::to_vec_pretty(&receipt)?)
    }

    pub fn fence(&self) -> Result<&ShiftFence> {
        self.commit
            .shift_fence
            .as_deref()
            .context("held runtime publication has no shift fence")
    }

    /// Release the original in-flight terminal at its existing authority.
    /// A quiet-point drain cannot supersede this terminal publication.
    pub async fn release_terminal(self) -> Result<RuntimeCommitReceipt> {
        let result = self.store.commit_runtime_state(self.commit).await;
        let receipt = result
            .as_ref()
            .map(Clone::clone)
            .map_err(|error| anyhow::anyhow!("terminal publication: {error}"));
        let _ = self.answer.send(result);
        receipt
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
            && commit.run_terminal.is_some()
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

/// Each real fleet host installs the same callback binding. Ordered digit
/// reductions make a duplicate publication visible as 7272 rather than 72.
pub const FRONTIER_PLUGIN: &str = "fleet-frontier";

#[derive(Clone)]
pub struct FleetFrontier;

#[lash::async_trait]
impl lash_core::plugin::PluginFactory for FleetFrontier {
    fn id(&self) -> &'static str {
        FRONTIER_PLUGIN
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(FRONTIER_PLUGIN)
    }

    fn build(
        &self,
        _: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::SessionPlugin for FleetFrontier {
    fn id(&self) -> &'static str {
        FRONTIER_PLUGIN
    }

    fn register(
        &self,
        registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        registrar.state_reducer(
            "append-digit",
            Arc::new(|reduction| {
                let current = match reduction.current {
                    Some(value) => value
                        .as_u64()
                        .ok_or_else(|| frontier_refusal("fleet frontier is not an integer"))?,
                    None => 0,
                };
                let digit = reduction
                    .input
                    .as_u64()
                    .filter(|digit| *digit < 10)
                    .ok_or_else(|| frontier_refusal("invalid fleet frontier digit"))?;
                let next = current
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(digit))
                    .ok_or_else(|| frontier_refusal("fleet frontier overflow"))?;
                Ok(Some(serde_json::json!(next)))
            }),
        )?;
        // The frontier advances per publication batch, not per reducer.
        // Keep the two independently receipted proposals the oracle requires.
        for (key, digit) in [
            (lash_core::hook_key!("fleet-frontier-first"), 7),
            (lash_core::hook_key!("fleet-frontier-second"), 2),
        ] {
            registrar.turn().after(
                key,
                Arc::new(move |_| {
                    Box::pin(async move {
                        Ok(lash_core::plugin::AfterTurnContributions {
                            state: lash_core::plugin::StateCommands::new().apply(
                                "digits",
                                "append-digit",
                                serde_json::json!(digit),
                            ),
                            ..Default::default()
                        })
                    })
                }),
            )?;
        }
        Ok(())
    }
}

impl FleetSnapshot {
    pub fn assert_frontier(&self) -> Result<()> {
        let frontier = self
            .frontier
            .as_ref()
            .context("fleet plugin namespace was not published")?;
        ensure!(
            frontier.values.get("digits") == Some(&serde_json::json!(72)),
            "fleet reducer was omitted, reordered or published twice"
        );
        // Each ordered callback contributes one proposal batch.
        ensure!(
            frontier.publication.receipts.len() == 2
                && frontier.publication.applied.map(|ordinal| ordinal.0) == Some(2),
            "fleet namespace has another publication frontier"
        );
        Ok(())
    }
}

fn frontier_refusal(message: &str) -> lash_core_store::tool_run::HookCause {
    lash_core_store::tool_run::HookCause {
        error_type: "fleet-frontier".into(),
        error_version: std::num::NonZeroU32::MIN,
        payload: serde_json::json!({ "reason": message }),
    }
}
