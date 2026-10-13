//! An actor's claim obligations (S7; FIG-6074, FIG-6089): the `requires`
//! its commits write, derived from the work it recorded and has not
//! settled.
//!
//! [`RecordedWork`] is the one traversal. It reads an actor's outstanding
//! records as uses of claim tokens ([`ObligationUse`]): each call admitted
//! and not settled owes its binding's callbacks, a parked cell owes the
//! providers and codecs of the deferred matches it committed, and a process
//! owes its environment's component configs. `requires` is the union of
//! their tokens. A token stays required while any one of its uses is
//! outstanding: settling one of two calls to a provider leaves it required.
//!
//! The same traversal is the reducer at every write point. A claimed
//! session's commits pass through [`SessionObligations`]: the first commit
//! of a claim that records or settles work reads the session's outstanding
//! records from the store, and each commit then applies what it appends to
//! them and writes the result, all under one lock held across the commit,
//! so the last commit's `requires` is computed from every record before it.
//! A stored `requires` that disagrees with the records it is read against
//! is reported, never taken as truth: the commit writes what the records
//! owe. A process's advance folds its driver's steps into the same
//! [`RecordedWork`].

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeLimits, EncodedNode, GraphDecoder, NodeId, NodeKind, NodeSource, StoredNode,
};
use lash_core_store::obligations::{ObligationToken, RegistryKind};
use lash_durable::domain::graph::{ExecRootWrite, RunFactKind, RunFactWrite, RunFacts};
use lash_durable::domain::{ExecKey, OwnerKey, RootOwner};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, DurableReads, Epoch};

use crate::ToolCallId;
use crate::plugin::PluginCallbackIdentity;
use crate::tool_run::AdmittedBinding;

/// Where an outstanding use of a claim token is recorded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObligationSite {
    /// A call admitted under `owner`, a turn's rounds, a cell or a process,
    /// and not settled: its admission's bound callbacks.
    Call { owner: OwnerKey, call: ToolCallId },
    /// A parked execution's root: the deferred matches its cell committed,
    /// whose later calls run on their providers and codecs, until it ends.
    Execution { exec: ExecKey },
    /// A process's environment: the config of every component it names.
    Environment,
}

/// What stage of its work a use is owed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObligationStage {
    /// An admitted call that has not settled.
    Admitted,
    /// A committed deferred match the execution may still call.
    Matched,
    /// A component config the process is written under.
    Configured,
}

/// One outstanding use of a claim token: the token, and the record that
/// owes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObligationUse {
    pub token: ObligationToken,
    pub site: ObligationSite,
}

impl ObligationUse {
    /// The stage the use is owed at.
    #[must_use]
    pub fn stage(&self) -> ObligationStage {
        match self.site {
            ObligationSite::Call { .. } => ObligationStage::Admitted,
            ObligationSite::Execution { .. } => ObligationStage::Matched,
            ObligationSite::Environment => ObligationStage::Configured,
        }
    }
}

/// A stored `requires` that is not what the records it was read against
/// owe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiresMismatch {
    /// Tokens the records owe that the stored value lacks.
    pub unstored: Vec<String>,
    /// Tokens the stored value holds that no record owes.
    pub unowed: Vec<String>,
}

/// What the root a commit publishes for `exec` owes, as its writer staged
/// it: the root's `owed` callbacks. The root is not decoded again; it
/// shares nodes with the roots the store already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionDebts {
    pub exec: ExecKey,
    pub owed: Vec<PluginCallbackIdentity>,
}

/// An actor's outstanding recorded work, as the claim tokens each record
/// owes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWork {
    registry: RegistryKind,
    uses: BTreeMap<ObligationSite, BTreeSet<ObligationToken>>,
}

impl RecordedWork {
    /// A session's work, owing nothing yet.
    #[must_use]
    pub fn session() -> Self {
        Self {
            registry: RegistryKind::Session,
            uses: BTreeMap::new(),
        }
    }

    /// A process's work under an environment whose components' configs are
    /// `environment`, owing nothing else yet.
    #[must_use]
    pub fn process(environment: impl IntoIterator<Item = ObligationToken>) -> Self {
        let mut work = Self {
            registry: RegistryKind::Process,
            uses: BTreeMap::new(),
        };
        work.put(ObligationSite::Environment, environment);
        work
    }

    /// The outstanding work `session`'s committed records hold: its
    /// unfinished turn's unsettled calls, and the calls and committed
    /// matches of each of its cells that is parked. Graph reads are bounded
    /// by `limits`.
    ///
    /// # Errors
    ///
    /// A read's refusal, or a record that does not decode.
    pub async fn read_session(
        reads: &dyn DurableReads,
        session: &crate::SessionId,
        limits: DecodeLimits,
    ) -> Result<Self, DurableError> {
        let mut work = Self::session();
        let Some(turn) = reads.turn(session).await? else {
            return Ok(work);
        };
        let owner = OwnerKey::Turn(session.clone(), turn.run.clone());
        work.apply_facts(&reads.run_facts(&owner).await?)?;
        for cell in reads.cell_roots(session, &turn.run).await? {
            work.apply_facts(&reads.run_facts(&cell.exec.owner()).await?)?;
            let graph = reads
                .hydrate(RootOwner::Exec(cell.exec.clone()), Some(cell.root), limits)
                .await?;
            work.apply_root(&cell.exec, &graph, cell.root, limits)?;
        }
        Ok(work)
    }

    /// Every outstanding use, by record.
    #[must_use]
    pub fn outstanding_obligations(&self) -> Vec<ObligationUse> {
        self.uses
            .iter()
            .flat_map(|(site, tokens)| {
                tokens.iter().map(|token| ObligationUse {
                    token: token.clone(),
                    site: site.clone(),
                })
            })
            .collect()
    }

    /// The tokens the work requires: every outstanding use's, sorted,
    /// without duplicates.
    #[must_use]
    pub fn requires(&self) -> Vec<String> {
        self.uses
            .values()
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|token| token.as_str().to_owned())
            .collect()
    }

    /// How `stored` differs from what the work requires; `None` when it is
    /// the same set.
    #[must_use]
    pub fn mismatch(&self, stored: &[String]) -> Option<RequiresMismatch> {
        let owed: BTreeSet<String> = self.requires().into_iter().collect();
        let stored: BTreeSet<String> = stored.iter().cloned().collect();
        (owed != stored).then(|| RequiresMismatch {
            unstored: owed.difference(&stored).cloned().collect(),
            unowed: stored.difference(&owed).cloned().collect(),
        })
    }

    /// Record that `owner` admitted `call`, bound to `binding`'s callbacks;
    /// a call bound to none owes nothing.
    pub fn admit(&mut self, owner: OwnerKey, call: ToolCallId, binding: Option<&AdmittedBinding>) {
        let registry = self.registry;
        self.put(
            ObligationSite::Call { owner, call },
            binding
                .into_iter()
                .flat_map(AdmittedBinding::callbacks)
                .map(|callback| ObligationToken::for_callback(registry, callback)),
        );
    }

    /// Record that `owner`'s `call` settled.
    pub fn settle(&mut self, owner: &OwnerKey, call: &ToolCallId) {
        self.uses.remove(&ObligationSite::Call {
            owner: owner.clone(),
            call: call.clone(),
        });
    }

    /// Apply the records `tx` appends, as they stand once it commits at
    /// `epoch`. Whether they change what the work owes or not, nothing else
    /// moves it.
    ///
    /// # Errors
    ///
    /// A record `tx` appends that does not decode; the store refuses its
    /// commit.
    pub fn apply(
        &mut self,
        tx: &ActorTx,
        epoch: Epoch,
        owing: Option<&ExecutionDebts>,
        limits: DecodeLimits,
    ) -> Result<(), DurableError> {
        let facts = RunFacts::appended(tx.domain(), tx.graph(), epoch);
        self.apply_facts(&facts)?;
        let staged = StagedNodes(tx.graph());
        for write in tx.domain() {
            match write {
                // The writer of a cell's root states what it owes; a root
                // it does not state is read from the nodes `tx` stages.
                DomainWrite::ExecRoot(ExecRootWrite::Put { exec, root, .. }) => {
                    match owing.filter(|owing| owing.exec == *exec) {
                        Some(owing) => self.owe(exec, &owing.owed),
                        None => self.apply_root(exec, &staged, *root, limits)?,
                    }
                }
                DomainWrite::ExecRoot(
                    ExecRootWrite::Retire { exec, .. } | ExecRootWrite::Clear { exec },
                ) => self.end(exec),
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether `tx` appends or ends a record this traversal reads.
    #[must_use]
    pub fn touches(tx: &ActorTx) -> bool {
        tx.domain().iter().any(|write| {
            matches!(
                write,
                DomainWrite::RunFact(RunFactWrite::Append {
                    kind: RunFactKind::Admit | RunFactKind::XOutcome,
                    ..
                }) | DomainWrite::ExecRoot(_)
            )
        })
    }

    fn put(&mut self, site: ObligationSite, tokens: impl IntoIterator<Item = ObligationToken>) {
        let tokens: BTreeSet<_> = tokens.into_iter().collect();
        if tokens.is_empty() {
            self.uses.remove(&site);
        } else {
            self.uses.insert(site, tokens);
        }
    }

    /// Admit and settle what `facts` records, in order.
    fn apply_facts(&mut self, facts: &RunFacts) -> Result<(), DurableError> {
        for row in &facts.rows {
            match row.kind {
                RunFactKind::Admit => {
                    for (call, binding) in super::round::admitted_bindings_of(facts, row)
                        .map_err(|reason| corrupt("an admission", &reason))?
                    {
                        self.admit(row.owner.clone(), call, binding.as_ref());
                    }
                }
                RunFactKind::XOutcome => {
                    if let Some(call) = &row.call {
                        self.settle(&row.owner, call);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Take what `exec`'s root `root`, read from `source`, owes: a parked
    /// cell's committed matches, nothing once it ended.
    fn apply_root(
        &mut self,
        exec: &ExecKey,
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<(), DurableError> {
        if !matches!(exec, ExecKey::Cell(..)) {
            return Ok(());
        }
        let body: ExecRoot = GraphDecoder::new(source, limits)
            .decode(root, NodeKind::EXEC_ROOT)
            .map_err(|refusal| corrupt("a cell's root", &refusal.to_string()))?;
        let owed: &[PluginCallbackIdentity] = match body.status {
            ExecStatus::Parked => &body.owed,
            ExecStatus::Ended => &[],
        };
        self.owe(exec, owed);
        Ok(())
    }

    /// `exec`'s root owes `owed`, and nothing it owed before.
    fn owe(&mut self, exec: &ExecKey, owed: &[PluginCallbackIdentity]) {
        let registry = self.registry;
        self.put(
            ObligationSite::Execution { exec: exec.clone() },
            owed.iter()
                .map(|callback| ObligationToken::for_callback(registry, callback)),
        );
    }

    /// `exec` ended: its root and its records owe nothing more.
    fn end(&mut self, exec: &ExecKey) {
        let owner = exec.owner();
        self.uses.retain(|site, _| match site {
            ObligationSite::Execution { exec: held } => held != exec,
            ObligationSite::Call { owner: held, .. } => *held != owner,
            ObligationSite::Environment => true,
        });
    }
}

/// The nodes a transaction stages, as a decoder reads them.
struct StagedNodes<'a>(&'a BTreeMap<NodeId, EncodedNode>);

impl NodeSource for StagedNodes<'_> {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.0.get(id).map(|node| StoredNode {
            encoding: BlobEncoding::Graph,
            bytes: &node.bytes,
        })
    }
}

fn corrupt(record: &str, reason: &str) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: format!("{record} does not decode for its claim obligations: {reason}"),
    })
}

/// The ledger one claim of a session commits its obligations through.
#[derive(Debug)]
pub(super) struct SessionObligations {
    session: crate::SessionId,
    /// Held across each commit, so commits stage and land in one order.
    state: tokio::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// The session's outstanding work as of its last acknowledged commit;
    /// `None` until this claim first reads it, and again once a commit's
    /// fate is unknown.
    work: Option<RecordedWork>,
    /// The `requires` the store holds: what the claim read, then what the
    /// last acknowledged commit wrote; `None` once unknown.
    written: Option<Vec<String>>,
}

impl SessionObligations {
    /// The ledger of a claim of session `session`, which carried `requires`.
    pub(super) fn claimed(session: crate::SessionId, requires: &[String]) -> Self {
        let mut written = requires.to_vec();
        written.sort();
        written.dedup();
        Self {
            session,
            state: tokio::sync::Mutex::new(State {
                work: None,
                written: Some(written),
            }),
        }
    }

    /// The ledger of a claim of session `session` that already knows its
    /// outstanding `work` and the `written` requirements the store holds.
    #[cfg(test)]
    pub(super) fn seeded(
        session: crate::SessionId,
        work: RecordedWork,
        written: Vec<String>,
    ) -> Self {
        Self {
            session,
            state: tokio::sync::Mutex::new(State {
                work: Some(work),
                written: Some(written),
            }),
        }
    }

    /// Commit `tx`, staged as `staged`, through `commit`, with the
    /// `requires` its records leave the session owing written into it when
    /// they change. A transaction whose writer set its requirements keeps
    /// them. Commits through one ledger are serialized. The session's
    /// records are read through `read` when the ledger does not know them.
    ///
    /// # Errors
    ///
    /// The read of the session's records, a record `tx` appends that does
    /// not decode, or `commit`'s refusal.
    pub(super) async fn commit<R, ReadFut, F, Fut, T>(
        &self,
        read: R,
        staged: Staged<'_>,
        mut tx: ActorTx,
        commit: F,
    ) -> Result<T, DurableError>
    where
        R: FnOnce(crate::SessionId) -> ReadFut,
        ReadFut: std::future::Future<Output = Result<RecordedWork, DurableError>>,
        F: FnOnce(ActorTx) -> Fut,
        Fut: std::future::Future<Output = Result<T, DurableError>>,
    {
        let mut state = self.state.lock().await;
        if let Some(explicit) = tx.requires() {
            // An ended turn or a closed session owes nothing; another
            // explicit value is the writer's, and the work is read again.
            let next = explicit.is_empty().then(RecordedWork::session);
            let explicit = explicit.to_vec();
            state.work = None;
            state.written = None;
            let committed = commit(tx).await?;
            state.work = next;
            state.written = Some(explicit);
            return Ok(committed);
        }
        let ends_turn = staged.label == CommitLabel::TURN_COMMIT;
        if !ends_turn && !RecordedWork::touches(&tx) {
            return commit(tx).await;
        }
        let next = if ends_turn {
            RecordedWork::session()
        } else {
            let mut work = match state.work.take() {
                Some(work) => work,
                None => {
                    let work = read(self.session.clone()).await?;
                    self.report(&work, state.written.as_deref());
                    work
                }
            };
            work.apply(&tx, staged.epoch, staged.owing, staged.limits)?;
            work
        };
        let requires = next.requires();
        if state.written.as_ref() != Some(&requires) {
            tx.set_requires(requires.clone());
        }
        // Until the store acknowledges the commit, what it holds is
        // unknown: a commit dropped in flight leaves the next one to read
        // the records again.
        state.work = None;
        state.written = None;
        let committed = commit(tx).await?;
        state.work = Some(next);
        state.written = Some(requires);
        Ok(committed)
    }

    /// Report `written`, the requirements the store holds, when it is not
    /// what `work`, the records it was read against, owe.
    fn report(&self, work: &RecordedWork, written: Option<&[String]>) {
        if let Some(mismatch) = written.and_then(|written| work.mismatch(written)) {
            tracing::warn!(
                session = %self.session,
                unstored = ?mismatch.unstored,
                unowed = ?mismatch.unowed,
                "the session's stored claim requirements disagree with its outstanding records"
            );
        }
    }
}

/// How one commit through a [`SessionObligations`] is staged: its label,
/// the epoch it lands at, what the root it publishes owes when its writer
/// states it, and the bounds of a root it decodes.
pub(super) struct Staged<'a> {
    pub(super) label: CommitLabel,
    pub(super) epoch: Epoch,
    pub(super) owing: Option<&'a ExecutionDebts>,
    pub(super) limits: DecodeLimits,
}

#[cfg(test)]
#[path = "claim_obligations_tests.rs"]
mod tests;
