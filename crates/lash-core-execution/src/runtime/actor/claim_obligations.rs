//! An actor's claim obligations (S7; FIG-6074, FIG-6089): the `requires`
//! its commits write, derived from the work it recorded and has not
//! settled.
//!
//! [`RecordedWork`] is the one traversal. It reads an actor's outstanding
//! records as uses of claim tokens ([`ObligationUse`]): each call admitted
//! and not settled owes its binding's callbacks, and a dynamic match's the
//! provider and codec version it linked; a parked execution (a cell or a
//! process) owes its root's hard requirements record, the natives its code
//! reached, its kernel version and its links' providers and codecs
//! (FIG-6993), and a cell the callbacks of the deferred matches it
//! committed; the turn's phase owes the callbacks of the model-call receipt
//! beside it, a session's head owes the observers of the config change it
//! has not delivered, and a process owes its environment's component
//! configs.
//! `requires` is the union of their tokens. A token stays required while any one of its uses is
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
//! owe. A commit that leaves the head in place never decodes it: when the
//! turn's records owe nothing, what the head owes is the `requires` the
//! claim found, carried unchanged (FIG-6209). A process's advance folds its
//! driver's steps into the same [`RecordedWork`].

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    BlobEncoding, DecodeLimits, EncodedNode, GraphDecoder, NodeId, NodeKind, NodeSource, StoredNode,
};
use lash_durable::domain::graph::{ExecRootWrite, RunFactKind, RunFactWrite, RunFacts};
use lash_durable::domain::{ExecKey, ModelCall, OwnerKey, RootOwner, TurnWrite, UnfinishedPhase};
use lash_durable::{ActorTx, DomainWrite, DurableError, DurableReads, Epoch};
use lash_vocabulary::claims::ObligationToken;
use lash_vocabulary::claims::RegistryKind;

use crate::ToolCallId;
use crate::tool_run::AdmittedBinding;
use lash_vocabulary::providers::PluginCallbackIdentity;

/// Where an outstanding use of a claim token is recorded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObligationSite {
    /// A call admitted under `owner`, a turn's rounds, a cell or a process,
    /// and not settled: its admission's bound callbacks.
    Call { owner: OwnerKey, call: ToolCallId },
    /// A parked execution's root: its hard requirements record, and the
    /// deferred matches its cell committed, whose later calls run on their
    /// providers and codecs, until it ends.
    Execution { exec: ExecKey },
    /// The unfinished turn's phase: the callbacks of the model-call
    /// receipt beside it, until the next phase consumes the receipt or the
    /// turn ends.
    Phase,
    /// The config change the session's head has not delivered, at its
    /// revision: each observer it is owed to, until the head retires it.
    ConfigChange { revision: u64 },
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
    /// A paid model answer whose response callbacks have not derived it.
    Answered,
    /// A committed config change its observer has not been delivered.
    Undelivered,
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
            ObligationSite::Phase => ObligationStage::Answered,
            ObligationSite::ConfigChange { .. } => ObligationStage::Undelivered,
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
/// it: the root's `owed` callbacks and its hard requirements record. The
/// root is not decoded again; it shares nodes with the roots the store
/// already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionDebts {
    pub exec: ExecKey,
    pub owed: Vec<PluginCallbackIdentity>,
    pub requirements: lash_vocabulary::claims::ClaimRequirements,
}

/// An actor's outstanding recorded work, as the claim tokens each record
/// owes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWork {
    registry: RegistryKind,
    uses: BTreeMap<ObligationSite, BTreeSet<ObligationToken>>,
    /// What the session's head owes when its claim did not read it: the
    /// `requires` the claim found, carried unchanged until a commit
    /// publishes the head. It names no record, so no use lists it.
    carried_head: BTreeSet<String>,
}

impl RecordedWork {
    /// A session's work, owing nothing yet.
    #[must_use]
    pub fn session() -> Self {
        Self {
            registry: RegistryKind::Session,
            uses: BTreeMap::new(),
            carried_head: BTreeSet::new(),
        }
    }

    /// The work of `process` under an environment whose components'
    /// configs are `environment`, its engine state's root recording
    /// `record`, owing nothing else yet.
    #[must_use]
    pub fn process(
        process: &lash_vocabulary::identity::ProcessId,
        environment: impl IntoIterator<Item = ObligationToken>,
        record: &lash_vocabulary::claims::ClaimRequirements,
    ) -> Self {
        let mut work = Self {
            registry: RegistryKind::Process,
            uses: BTreeMap::new(),
            carried_head: BTreeSet::new(),
        };
        work.put(ObligationSite::Environment, environment);
        work.put(
            ObligationSite::Execution {
                exec: ExecKey::Process(process.clone()),
            },
            ObligationToken::for_requirements(record),
        );
        work
    }

    /// The outstanding work `session`'s committed records hold, for
    /// diagnostics: the config change its head owes, read from `heads`, its
    /// unfinished turn's unsettled calls and the answer its phase retains,
    /// and the calls and committed matches of each of its cells that is
    /// parked. Every graph read is bounded by `limits`, which also bounds
    /// their aggregate, and each row read by `max_rows`.
    ///
    /// # Errors
    ///
    /// A read's refusal, a record that does not decode, or a read past its
    /// bounds.
    pub async fn read_session_within(
        reads: &dyn DurableReads,
        heads: &dyn crate::store::SessionCommitStore,
        session: &lash_vocabulary::identity::SessionId,
        limits: DecodeLimits,
        max_rows: usize,
    ) -> Result<Self, DurableError> {
        let mut work = Self::read_head(heads, session).await?;
        work.read_turn_into(reads, session, limits, limits, Some(max_rows))
            .await?;
        Ok(work)
    }

    /// The work `session`'s head alone owes: the config change it has not
    /// delivered. What a session that holds no turn owes.
    ///
    /// # Errors
    ///
    /// The head's read refused, with its cause: a typed refusal such as an
    /// unsupported generation, a head that does not decode, or a transient
    /// fault of the store.
    pub async fn read_head(
        heads: &dyn crate::store::SessionCommitStore,
        session: &lash_vocabulary::identity::SessionId,
    ) -> Result<Self, DurableError> {
        let mut work = Self::session();
        let head = heads
            .load_session_head_meta(session)
            .await
            .map_err(head_unread)?;
        work.apply_head(
            head.and_then(|head| head.config.undelivered_change)
                .as_deref(),
        );
        Ok(work)
    }

    /// The work `session`'s unfinished turn owes, its head unread: its
    /// unsettled calls, the answer its phase retains and its parked cells'
    /// calls and committed matches. The phase's graph read is bounded by
    /// `phase_limits`, every other by `limits`.
    ///
    /// # Errors
    ///
    /// A read's refusal, or a record that does not decode.
    pub(crate) async fn read_turn(
        reads: &dyn DurableReads,
        session: &lash_vocabulary::identity::SessionId,
        limits: DecodeLimits,
        phase_limits: DecodeLimits,
    ) -> Result<Self, DurableError> {
        let mut work = Self::session();
        work.read_turn_into(reads, session, limits, phase_limits, None)
            .await?;
        Ok(work)
    }

    async fn read_turn_into(
        &mut self,
        reads: &dyn DurableReads,
        session: &lash_vocabulary::identity::SessionId,
        mut limits: DecodeLimits,
        phase_limits: DecodeLimits,
        max_rows: Option<usize>,
    ) -> Result<(), DurableError> {
        let Some(turn) = reads.turn(session).await? else {
            return Ok(());
        };
        // A paid answer owes the callbacks its derivation resolves (S7); a
        // call in flight owes nothing until its receipt commits.
        let receipt = match &turn.phase {
            UnfinishedPhase::Model {
                call: ModelCall::Answered { receipt },
                ..
            } => Some(*receipt),
            UnfinishedPhase::Admitted
            | UnfinishedPhase::Model {
                call: ModelCall::Requested,
                ..
            }
            | UnfinishedPhase::Tools { .. } => None,
        };
        if let Some(root) = receipt {
            let graph = reads
                .hydrate(
                    RootOwner::ModelAnswer {
                        session: session.clone(),
                        run: turn.run.clone(),
                    },
                    Some(root),
                    phase_limits,
                )
                .await?;
            self.apply_receipt(&graph, root, phase_limits)?;
            if max_rows.is_some() {
                consume_limits(&mut limits, &graph.nodes)?;
            }
        }
        let owner = OwnerKey::Turn(session.clone(), turn.run.clone());
        let facts = match max_rows {
            None => reads.run_facts(&owner).await?,
            Some(max_rows) => {
                reads
                    .run_facts_within(&owner, None, max_rows, limits)
                    .await?
            }
        };
        self.apply_facts(&facts)?;
        if max_rows.is_some() {
            consume_limits(&mut limits, &facts.nodes)?;
        }
        let cells = match max_rows {
            None => reads.cell_roots(session, &turn.run).await?,
            Some(max_rows) => {
                reads
                    .cell_roots_within(session, &turn.run, max_rows)
                    .await?
            }
        };
        if max_rows.is_some_and(|max_rows| cells.len() > max_rows) {
            return Err(read_limit(
                cells.len() as u64,
                max_rows.unwrap_or_default() as u64,
            ));
        }
        for cell in cells {
            let facts = match max_rows {
                None => reads.run_facts(&cell.exec.owner()).await?,
                Some(max_rows) => {
                    reads
                        .run_facts_within(&cell.exec.owner(), None, max_rows, limits)
                        .await?
                }
            };
            self.apply_facts(&facts)?;
            if max_rows.is_some() {
                consume_limits(&mut limits, &facts.nodes)?;
            }
            let graph = reads
                .hydrate(RootOwner::Exec(cell.exec.clone()), Some(cell.root), limits)
                .await?;
            self.apply_root(&cell.exec, &graph, cell.root, limits)?;
            if max_rows.is_some() {
                consume_limits(&mut limits, &graph.nodes)?;
            }
        }
        Ok(())
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
            .map(ObligationToken::as_str)
            .chain(self.carried_head.iter().map(String::as_str))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Whether no record this work read owes anything.
    fn owes_nothing(&self) -> bool {
        self.uses.is_empty() && self.carried_head.is_empty()
    }

    /// This work, with what its session's head owes taken as `head`, the
    /// `requires` its claim found: the head is not read.
    fn carrying_head(mut self, head: &[String]) -> Self {
        self.carried_head = head.iter().cloned().collect();
        self
    }

    /// This work, with what its session's head owes as `head` read it.
    fn with_head(mut self, head: Self) -> Self {
        self.uses.extend(head.uses);
        self
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
        self.admit_linked(owner, call, binding, None);
    }

    /// Record that `owner` admitted `call`, bound to `binding`'s callbacks,
    /// a dynamic match's call also owing the provider and codec version of
    /// the link `matched` committed (FIG-6993): what a node must offer
    /// until the call settles and its committed outcome is reused.
    pub fn admit_linked(
        &mut self,
        owner: OwnerKey,
        call: ToolCallId,
        binding: Option<&AdmittedBinding>,
        matched: Option<&lash_core_store::tool_surface::OperationRoute>,
    ) {
        let registry = self.registry;
        let mut linked = lash_vocabulary::claims::ClaimRequirements::default();
        if let Some(link) = matched {
            link.require(&mut linked);
        }
        let tokens: Vec<ObligationToken> = binding
            .into_iter()
            .flat_map(AdmittedBinding::callbacks)
            .map(|callback| ObligationToken::for_callback(registry, callback))
            .chain(ObligationToken::for_requirements(&linked))
            .collect();
        self.put(ObligationSite::Call { owner, call }, tokens);
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
        phase_limits: DecodeLimits,
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
                        Some(owing) => self.owe(exec, &owing.owed, &owing.requirements),
                        None => self.apply_root(exec, &staged, *root, limits)?,
                    }
                }
                DomainWrite::ExecRoot(
                    ExecRootWrite::Retire { exec, .. } | ExecRootWrite::Clear { exec },
                ) => self.end(exec),
                // The writer of a receipt states what it owes; a receipt it
                // does not state is read from the nodes `tx` stages. The
                // next phase consumes it: a phase owes nothing of its own.
                DomainWrite::Turn(TurnWrite::Answer { answer, .. }) => match tx.owed(answer) {
                    Some(owed) => self.owe_phase(owed),
                    None => self.apply_receipt(&staged, *answer, phase_limits)?,
                },
                DomainWrite::Turn(TurnWrite::Advance { .. }) => self.put(ObligationSite::Phase, []),
                DomainWrite::Turn(TurnWrite::Terminal { .. }) => self.end_turn(),
                _ => {}
            }
        }
        // The head a commit publishes states the config change it owes,
        // whatever else the commit ended.
        for write in tx.domain() {
            if let DomainWrite::SessionCommit(commit) = write {
                let change =
                    crate::store::session_commit_undelivered_change(&commit.commit_json)
                        .map_err(|error| corrupt("a session head commit", &error.to_string()))?;
                self.apply_head(change.as_deref());
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
                    | DomainWrite::Turn(
                        TurnWrite::Advance { .. }
                            | TurnWrite::Answer { .. }
                            | TurnWrite::Terminal { .. }
                    )
                    | DomainWrite::SessionCommit(_)
            )
        })
    }

    /// Whether `tx` ends the session's turn: what the turn's records owe
    /// goes with it.
    #[must_use]
    pub fn ends_turn(tx: &ActorTx) -> bool {
        tx.domain()
            .iter()
            .any(|write| matches!(write, DomainWrite::Turn(TurnWrite::Terminal { .. })))
    }

    /// Whether `tx` publishes the session's head, which states the config
    /// change it owes.
    #[must_use]
    pub fn writes_head(tx: &ActorTx) -> bool {
        tx.domain()
            .iter()
            .any(|write| matches!(write, DomainWrite::SessionCommit(_)))
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
    /// cell's hard requirements record and committed matches, nothing once
    /// it ended. A process's root is its engine state's, whose record the
    /// process's own traversal takes ([`Self::process`]).
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
        match body.status {
            ExecStatus::Parked => self.owe(exec, &body.owed, &body.requirements),
            ExecStatus::Ended => {
                self.owe(
                    exec,
                    &[],
                    &lash_vocabulary::claims::ClaimRequirements::default(),
                );
            }
        }
        Ok(())
    }

    /// Take what the model-call receipt `root`, read from `source`, owes
    /// (S7): its `owed`, the callbacks its derivation runs. With
    /// [`Self::owe_phase`], the one projection of a phase's debts: a cold
    /// read, a commit and diagnostics all take it here.
    fn apply_receipt(
        &mut self,
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<(), DurableError> {
        let receipt =
            super::model_receipt::ModelReceipt::<serde::de::IgnoredAny>::load(source, root, limits)
                .map_err(|reason| corrupt("a model call's receipt", &reason))?;
        self.owe_phase(&receipt.owed);
        Ok(())
    }

    /// The turn's phase owes `owed`, and nothing it owed before.
    fn owe_phase(&mut self, owed: &[PluginCallbackIdentity]) {
        let registry = self.registry;
        self.put(
            ObligationSite::Phase,
            owed.iter()
                .map(|callback| ObligationToken::for_callback(registry, callback)),
        );
    }

    /// Take what `change`, the config change a session's head owes, owes:
    /// each observer it records, and nothing a change it replaced owed,
    /// read or carried.
    fn apply_head(&mut self, change: Option<&crate::UndeliveredConfigChange>) {
        self.carried_head.clear();
        self.uses
            .retain(|site, _| !matches!(site, ObligationSite::ConfigChange { .. }));
        if let Some(change) = change {
            let registry = self.registry;
            self.put(
                ObligationSite::ConfigChange {
                    revision: change.revision,
                },
                change
                    .observers
                    .iter()
                    .map(|observer| ObligationToken::for_callback(registry, observer)),
            );
        }
    }

    /// The session's turn ended: its calls, its phase and its cells owe
    /// nothing more; what its head owes stays.
    fn end_turn(&mut self) {
        self.uses.retain(|site, _| {
            matches!(
                site,
                ObligationSite::ConfigChange { .. } | ObligationSite::Environment
            )
        });
    }

    /// `exec`'s root owes `owed` and `requirements`, and nothing it owed
    /// before.
    fn owe(
        &mut self,
        exec: &ExecKey,
        owed: &[PluginCallbackIdentity],
        requirements: &lash_vocabulary::claims::ClaimRequirements,
    ) {
        let registry = self.registry;
        let tokens: Vec<ObligationToken> = owed
            .iter()
            .map(|callback| ObligationToken::for_callback(registry, callback))
            .chain(ObligationToken::for_requirements(requirements))
            .collect();
        self.put(ObligationSite::Execution { exec: exec.clone() }, tokens);
    }

    /// `exec` ended: its root and its records owe nothing more.
    fn end(&mut self, exec: &ExecKey) {
        let owner = exec.owner();
        self.uses.retain(|site, _| match site {
            ObligationSite::Execution { exec: held } => held != exec,
            ObligationSite::Call { owner: held, .. } => *held != owner,
            ObligationSite::Phase
            | ObligationSite::ConfigChange { .. }
            | ObligationSite::Environment => true,
        });
    }
}

fn read_limit(actual: u64, limit: u64) -> DurableError {
    DurableError::Graph(
        lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
            dimension: lash_vocabulary::tokens::DecodeDimension::Nodes,
            actual,
            limit,
        },
    )
}
fn consume_limits(
    limits: &mut DecodeLimits,
    nodes: &BTreeMap<NodeId, lash_durable::domain::graph::HydratedNode>,
) -> Result<(), DurableError> {
    let bytes = nodes.values().fold(0u64, |bytes, node| {
        bytes.saturating_add(node.bytes.len() as u64)
    });
    if bytes > limits.total_bytes || nodes.len() as u64 > limits.nodes {
        return Err(read_limit(nodes.len() as u64, limits.nodes));
    }
    limits.total_bytes -= bytes;
    limits.nodes -= nodes.len() as u64;
    Ok(())
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

/// A refused read of a session's head, as the port reports it: a typed
/// refusal stays one, a fault of the substrate stays transient, and any
/// other answer is a head this build does not decode, which no retry
/// clears (FIG-6209).
fn head_unread(error: crate::StoreError) -> DurableError {
    if let Some(refusal) = crate::store::StoreRefusal::of_store_error(&error) {
        return DurableError::StoreRefused(refusal);
    }
    let kind = match &error {
        crate::StoreError::Contended => lash_durable::StoreFailureKind::Contended,
        error if error.is_transient() => lash_durable::StoreFailureKind::Unavailable,
        _ => lash_durable::StoreFailureKind::Corrupt,
    };
    DurableError::Store(lash_durable::StoreFailure {
        kind,
        message: format!("the session's head does not read: {error}"),
    })
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
    session: lash_vocabulary::identity::SessionId,
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
    pub(super) fn claimed(
        session: lash_vocabulary::identity::SessionId,
        requires: &[String],
    ) -> Self {
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
        session: lash_vocabulary::identity::SessionId,
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

    pub(super) async fn hydrated(
        &self,
        projection: &super::owner_state::SessionProjection,
        reads: &dyn DurableReads,
        limits: DecodeLimits,
        phase_limits: DecodeLimits,
    ) -> Result<(), DurableError> {
        let mut work = RecordedWork::session();
        work.apply_head(projection.head.undelivered_config_change.as_deref());
        if projection.turn.is_some() {
            work.read_turn_into(reads, &self.session, limits, phase_limits, None)
                .await?;
        }
        self.state.lock().await.work = Some(work);
        Ok(())
    }

    pub(super) fn invalidate_on_exit(&self) {
        if let Ok(mut state) = self.state.try_lock() {
            state.work = None;
            state.written = None;
        }
    }

    pub(super) async fn invalidate(&self) {
        let mut state = self.state.lock().await;
        state.work = None;
        state.written = None;
    }

    /// Commit `tx`, staged as `staged`, through `commit`, with the
    /// `requires` its records leave the session owing written into it when
    /// they change. A transaction whose writer set its requirements keeps
    /// them. Commits through one ledger are serialized. The session's
    /// records are read through `read` when the ledger does not know them
    /// ([`Self::read`]).
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
        R: Fn(lash_vocabulary::identity::SessionId, ReadScope) -> ReadFut,
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
        if !RecordedWork::touches(&tx) {
            return commit(tx).await;
        }
        let mut next = match state.work.take() {
            Some(work) => work,
            None => self.read(read, &tx, state.written.as_deref()).await?,
        };
        next.apply(
            &tx,
            staged.epoch,
            staged.owing,
            staged.limits,
            staged.phase_limits,
        )?;
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

    /// The session's outstanding work before `tx`, read through `read`; the
    /// store holds `written` requirements when they are known.
    ///
    /// The head is read only when `tx` leaves it in place and the turn's
    /// records owe something: a commit that publishes the head states what
    /// it owes, and when the turn's records owe nothing, all the session
    /// owes is the head's, which is `written`, carried unchanged. A commit
    /// that ends the turn reads its records only for that, and reads the
    /// head when they do not read; an unstarted turn ends on a head that
    /// does not read (FIG-6209).
    ///
    /// # Errors
    ///
    /// A read's refusal, with its cause.
    async fn read<R, ReadFut>(
        &self,
        read: R,
        tx: &ActorTx,
        written: Option<&[String]>,
    ) -> Result<RecordedWork, DurableError>
    where
        R: Fn(lash_vocabulary::identity::SessionId, ReadScope) -> ReadFut,
        ReadFut: std::future::Future<Output = Result<RecordedWork, DurableError>>,
    {
        let session = || self.session.clone();
        let writes_head = RecordedWork::writes_head(tx);
        if RecordedWork::ends_turn(tx) {
            // A commit that ends the turn and publishes the head states all
            // the session owes after it.
            if writes_head {
                return Ok(RecordedWork::session());
            }
            // What the turn owes goes with it; a turn whose records do not
            // read leaves the head to say what the session owes.
            if let Some(written) = written
                && let Ok(turn) = read(session(), ReadScope::Turn).await
                && turn.owes_nothing()
            {
                return Ok(turn.carrying_head(written));
            }
            return read(session(), ReadScope::Head).await;
        }
        let turn = read(session(), ReadScope::Turn).await?;
        if writes_head {
            return Ok(turn);
        }
        match written {
            Some(written) if turn.owes_nothing() => Ok(turn.carrying_head(written)),
            written => {
                let work = turn.with_head(read(session(), ReadScope::Head).await?);
                self.report(&work, written);
                Ok(work)
            }
        }
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

/// What of a session's records a [`SessionObligations`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadScope {
    /// Its unfinished turn's records, the head unread
    /// ([`RecordedWork::read_turn`]).
    Turn,
    /// Its head's alone ([`RecordedWork::read_head`]).
    Head,
}

/// How one commit through a [`SessionObligations`] is staged: the epoch it
/// lands at, what the root it publishes owes when its writer states it, and
/// the bounds of an execution's root and of a phase's root it decodes.
pub(super) struct Staged<'a> {
    pub(super) epoch: Epoch,
    pub(super) owing: Option<&'a ExecutionDebts>,
    pub(super) limits: DecodeLimits,
    pub(super) phase_limits: DecodeLimits,
}

#[cfg(test)]
#[path = "claim_obligations_tests.rs"]
mod tests;
