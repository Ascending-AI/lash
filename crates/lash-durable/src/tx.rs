//! The two write authorities: the owner's [`ActorTx`] and everyone else's
//! [`MailTx`].
//!
//! Both are buffered write sets with no I/O of their own. The store applies
//! one in a single transaction when it is committed, so a transaction is
//! never held open across an `.await` in the caller's code.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{
    EncodeBudget, EncodedNode, ExplicitLimit, GraphCommitBudget, GraphRefusal, GraphStaging, NodeId,
};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

use crate::domain::RootOwner;
use crate::domain::namespace_value::NamespaceValue;
use crate::domain::pages::{LeaseFence, RootLeaseId, RootLeaseRequest};
use crate::domain::{
    DomainWrite, MailDomainWrite, RunValuesWrite, StoredNamespaceWrite, StoredValues,
    TurnCancelRequest, TurnNamespaceWrite, TurnWrite,
};
use crate::formats::FormatSet;
use crate::ids::{ActorKey, DurableInstant, Epoch, MailKind, MailSeq, StateRevision};

/// One mail row as the owner reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    /// Its position in the mailbox.
    pub seq: MailSeq,
    /// What it is.
    pub kind: MailKind,
    /// Its body, as the writer encoded it.
    pub body: String,
    /// When it was appended.
    pub appended_at: DurableInstant,
}

/// How an owner gives an actor up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Claimable at once by any node that decodes it: a draining node's
    /// release at a committed phase, or the runner's hand-back of an
    /// abandoned activation's actor (`drain.release`). A commit that writes
    /// nothing else ([`ActorTx::advances`] is false) advances nothing: when
    /// no commit advanced the actor since its claim, the next claim counts
    /// one more failed activation, as after a crash.
    Ready,
    /// Nothing to do until mail arrives.
    Idle,
    /// Blocked until mail arrives or, when set, until `next_due` passes.
    Waiting {
        /// The earliest durable deadline the actor waits on.
        next_due: Option<DurableInstant>,
    },
    /// Set aside for an operator with the current reason recorded on the
    /// actor row ([`DomainWrite::ActorPark`]):
    /// only a control wake readies it. A stop that reached the actor since
    /// the owner's read readies it at once instead, its park kept, so the
    /// park never strands it (FIG-6099): cancel or session close mail past
    /// the read's mailbox position ([`ActorTx::seen`]), or an `Immediate`
    /// cancel of a session's unfinished turn that the read did not hold
    /// ([`ActorTx::immediate_cancel_since_read`]).
    Parked,
    /// Finished: never claimed again, and its mailbox refuses mail.
    Terminal,
}

#[derive(Clone, Debug)]
struct PreparedPhase {
    surface: (
        NodeId,
        Option<std::sync::Arc<lash_core_store::tool_surface::RecordedSurface>>,
    ),
    environments: std::sync::Arc<[NodeId]>,
}

/// The owner's transaction over one actor.
///
/// [`DurableStore::begin`](crate::DurableStore::begin) opens one after a
/// fenced read; it carries the actor's mailbox position and pending mail as
/// of that read. [`DurableStore::commit`](crate::DurableStore::commit)
/// applies it in one transaction whose first statement re-checks the epoch
/// and the planning revision
/// and bumps the actor's state revision; then its [`DomainWrite`]s in the
/// order recorded, then the acknowledgement, then the release. A refused
/// domain write rolls all of it back.
///
/// It also carries the checkpoint graph nodes its domain writes publish
/// ([`Self::stage`]): the commit checks every staged node and the closure
/// each published root needs, installs the new blobs and their edges after
/// the fence and before the domain writes, and refuses with nothing written
/// when a referenced node is neither staged nor stored.
#[derive(Clone, Debug)]
pub struct ActorTx {
    process_preimage: Option<Box<crate::domain::ProcessView>>,
    process_publications: Vec<lash_core_store::tool_run::StateResolution>,
    // Prepared in process, never encoded or interpreted by storage adapters.
    session_postimage: Option<Box<lash_core_store::session_state::RuntimeSessionState>>,
    phase: Option<Box<PreparedPhase>>,
    actor: ActorKey,
    epoch: Epoch,
    revision: StateRevision,
    acked: MailSeq,
    seen: MailSeq,
    mail: Vec<Mail>,
    opened_at: DurableInstant,
    turn_cancel: Option<TurnCancelRequest>,
    prompt_postimage: Option<lash_core_store::prompt_sections::OwnerPromptState>,
    domain: Vec<DomainWrite>,
    root_leases: Vec<RootLeaseRequest>,
    release_root_leases: BTreeSet<RootLeaseId>,
    ack: Option<MailSeq>,
    release: Option<Release>,
    formats: Option<FormatSet>,
    requires: Option<Vec<String>>,
    graph: BTreeMap<NodeId, EncodedNode>,
    reads: BTreeSet<NodeId>,
    exact_reads:
        BTreeMap<NodeId, std::sync::Arc<lash_core_store::checkpoint_graph::node::VerifiedNode>>,
    budget: Option<GraphCommitBudget>,
    owed: BTreeMap<NodeId, Vec<lash_vocabulary::providers::PluginCallbackIdentity>>,
}

/// What a fenced read saw of one owned actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedActor {
    /// The actor.
    pub actor: ActorKey,
    /// The epoch the read was fenced on.
    pub epoch: Epoch,
    /// The actor's state revision.
    pub revision: StateRevision,
    /// The mailbox position the owner last acknowledged.
    pub acked: MailSeq,
    /// The newest mailbox position: every append and every wake takes one.
    pub seen: MailSeq,
    /// The pending mail, oldest first.
    pub mail: Vec<Mail>,
    /// The store's clock at the read: the instant a row the owner records
    /// before its work starts takes, such as a model call's deadline.
    pub at: DurableInstant,
    /// For a session actor, the cancel request its unfinished turn
    /// accepted, read with the actor; `None` for any other actor.
    pub turn_cancel: Option<TurnCancelRequest>,
}

impl ActorTx {
    /// The transaction a store's `begin` hands back after its fenced read.
    /// What is recorded on it is applied only by that store's `commit`,
    /// which fences again.
    #[must_use]
    pub fn opened(read: OpenedActor) -> Self {
        Self {
            process_preimage: None,
            process_publications: Vec::new(),
            session_postimage: None,
            phase: None,
            actor: read.actor,
            epoch: read.epoch,
            revision: read.revision,
            acked: read.acked,
            seen: read.seen,
            mail: read.mail,
            opened_at: read.at,
            turn_cancel: read.turn_cancel,
            prompt_postimage: None,
            domain: Vec::new(),
            root_leases: Vec::new(),
            release_root_leases: BTreeSet::new(),
            ack: None,
            release: None,
            formats: None,
            requires: None,
            graph: BTreeMap::new(),
            reads: BTreeSet::new(),
            exact_reads: BTreeMap::new(),
            budget: None,
            owed: BTreeMap::new(),
        }
    }

    /// Attach the complete prepared session state to this draft. The owner
    /// coordinator consumes it; it is never a persisted owner snapshot.
    #[doc(hidden)]
    pub fn session_postimage(
        &mut self,
        state: lash_core_store::session_state::RuntimeSessionState,
    ) {
        self.session_postimage = Some(Box::new(state));
    }

    /// Transfer the prepared state into the owner adoption boundary.
    #[doc(hidden)]
    pub fn take_session_postimage(
        &mut self,
    ) -> Option<lash_core_store::session_state::RuntimeSessionState> {
        self.session_postimage.take().map(|state| *state)
    }

    /// Retain the surface and ordered environment roots authored by one phase.
    /// They share its exact root and transfer together with a held draft;
    /// storage adapters never encode these prepared facts.
    #[doc(hidden)]
    pub fn phase(
        &mut self,
        root: NodeId,
        surface: Option<std::sync::Arc<lash_core_store::tool_surface::RecordedSurface>>,
        environments: Vec<NodeId>,
    ) {
        self.phase = Some(Box::new(PreparedPhase {
            surface: (root, surface),
            environments: environments.into(),
        }));
    }

    /// The phase surface this draft will acknowledge, without decoding its graph.
    #[doc(hidden)]
    pub fn prepared_phase_surface(
        &self,
    ) -> Option<&(
        NodeId,
        Option<std::sync::Arc<lash_core_store::tool_surface::RecordedSurface>>,
    )> {
        self.phase.as_ref().map(|phase| &phase.surface)
    }

    /// The environment roots authored by this exact prepared phase, without
    /// decoding a graph delta whose retained children are already stored.
    #[doc(hidden)]
    pub fn prepared_phase_environments(&self, root: NodeId) -> Option<&[NodeId]> {
        self.phase
            .as_ref()
            .filter(|phase| phase.surface.0 == root)
            .map(|phase| phase.environments.as_ref())
    }

    /// Carry the acknowledged preimage under this transaction's unchanged basis.
    pub fn retain_process(&mut self, view: crate::domain::ProcessView) {
        self.process_preimage = Some(Box::new(view));
    }

    #[must_use]
    pub fn process_preimage(&self) -> Option<&crate::domain::ProcessView> {
        self.process_preimage.as_deref()
    }

    /// Carry the outcome producer's original namespace publications into
    /// process checkpoint preparation, before candidate pages are published.
    #[doc(hidden)]
    pub fn publish_process_state(&mut self, state: &[lash_core_store::tool_run::StateResolution]) {
        self.process_publications.extend_from_slice(state);
    }

    /// Transient publications, in the same order as their outcome writes.
    #[doc(hidden)]
    pub fn process_publications(&self) -> &[lash_core_store::tool_run::StateResolution] {
        &self.process_publications
    }

    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &ActorKey {
        &self.actor
    }

    /// The epoch every write is fenced on.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The immutable revision from which this attempt was planned. A newer
    /// open validates this value; it never grants this draft a new basis.
    #[must_use]
    pub fn revision(&self) -> StateRevision {
        self.revision
    }

    /// The newest mailbox position the fenced read saw.
    #[must_use]
    pub fn seen(&self) -> MailSeq {
        self.seen
    }

    /// Whether anything was appended or woke the actor since the owner last
    /// acknowledged, as of the fenced read. A wake takes a mailbox position
    /// without a mail row, so this can be true with no [`Self::mail`].
    #[must_use]
    pub fn woken(&self) -> bool {
        self.seen > self.acked
    }

    /// The pending mail as of the fenced read, oldest first.
    #[must_use]
    pub fn mail(&self) -> &[Mail] {
        &self.mail
    }

    /// The store's clock at the fenced read: what an owner records as the
    /// start of work it pins before it begins, with no read of its own.
    #[must_use]
    pub fn opened_at(&self) -> DurableInstant {
        self.opened_at
    }

    /// For a session actor, the cancel request its unfinished turn accepted
    /// as of the fenced read; `None` for any other actor.
    #[must_use]
    pub fn turn_cancel(&self) -> Option<&TurnCancelRequest> {
        self.turn_cancel.as_ref()
    }

    /// Whether `fenced`, the cancel request a commit's fence read for this
    /// session's unfinished turn, is an `Immediate` one that this
    /// transaction's open read did not hold ([`Self::turn_cancel`]): a stop
    /// that reached the session since its owner read it (FIG-6099).
    #[must_use]
    pub fn immediate_cancel_since_read(&self, fenced: Option<&TurnCancelRequest>) -> bool {
        let immediate = |request: Option<&TurnCancelRequest>| {
            request.is_some_and(|request| {
                request.mode == lash_protocol_types::TurnCancelMode::Immediate
            })
        };
        immediate(fenced) && !immediate(self.turn_cancel())
    }

    /// The private prompt draft, transferred only through a committed owner
    /// acknowledgement. It is never encoded or treated as a durable root.
    pub fn stage_prompt_postimage(
        &mut self,
        prompt: lash_core_store::prompt_sections::OwnerPromptState,
    ) {
        self.prompt_postimage = Some(prompt);
    }
    pub fn take_prompt_postimage(
        &mut self,
    ) -> Option<lash_core_store::prompt_sections::OwnerPromptState> {
        self.prompt_postimage.take()
    }

    /// Lease the exact root after this transaction's domain writes, under
    /// this actor's fence. The receipt transfers the lease to its reader;
    /// an unacknowledged lease expires through the retirement reaper.
    pub fn lease_root(&mut self, owner: RootOwner, root: NodeId, ttl_millis: i64) {
        self.root_leases.push(RootLeaseRequest {
            owner,
            expected: root,
            fence: LeaseFence::Actor {
                actor: self.actor.clone(),
                epoch: self.epoch,
            },
            ttl_millis,
        });
    }

    /// Leases acquired by the commit, in request order.
    #[must_use]
    pub fn root_leases(&self) -> &[RootLeaseRequest] {
        &self.root_leases
    }

    /// Transfer cleanup of an ended reader to this commit. The caller
    /// retains its obligation until acknowledgement; a refusal releases
    /// nothing. The commit also keeps retirement holds for released roots.
    pub fn release_root_lease(&mut self, lease: RootLeaseId) {
        self.release_root_leases.insert(lease);
    }

    #[must_use]
    pub fn released_root_leases(&self) -> &BTreeSet<RootLeaseId> {
        &self.release_root_leases
    }

    /// The domain rows this transaction writes, in order.
    #[must_use]
    pub fn domain(&self) -> &[DomainWrite] {
        &self.domain
    }

    /// Whether this commit can change a send follower's authoritative
    /// snapshot. Phase roots, receipts and tool facts alone leave its input
    /// binding, terminal, session standing and park unchanged. Classify the
    /// writes once here, for both local notification and backend capture.
    #[must_use]
    pub fn changes_follow_facts(&self) -> bool {
        self.ack.is_some()
            || self.release.is_some_and(|release| release != Release::Idle)
            || self.domain.iter().any(|write| match write {
                DomainWrite::Turn(turn) => match turn {
                    TurnWrite::Admit { .. } | TurnWrite::Terminal { .. } => true,
                    TurnWrite::Advance { .. }
                    | TurnWrite::AddToolCalls { .. }
                    | TurnWrite::Answer { .. }
                    | TurnWrite::Namespaces { .. }
                    | TurnWrite::Working { .. } => false,
                },
                DomainWrite::Environment(_)
                | DomainWrite::SessionCommit(_)
                | DomainWrite::SessionClose(_)
                | DomainWrite::ActorPark { .. }
                | DomainWrite::SessionMail(_)
                // Keep wakes for cross-actor delivery and scope changes.
                | DomainWrite::Wait(_)
                | DomainWrite::Process(_) => true,
                DomainWrite::RegisterExecution(_)
                | DomainWrite::EndExecution(_)
                | DomainWrite::AdoptExecution(_)
                | DomainWrite::SeedResolution(_)
                | DomainWrite::RunFact(_)
                | DomainWrite::ExecRoot(_)
                | DomainWrite::EffectAdmission(_)
                | DomainWrite::Prompt(_)
                | DomainWrite::ModelCall(_)
                | DomainWrite::ModelCallPin(_) => false,
            })
    }

    /// Write `write` when this transaction commits, after the fence and the
    /// writes recorded before it.
    /// Record a validated inherited resolution with execution admission.
    pub fn seed_resolution(&mut self, record: crate::resolutions::ResolutionRecord) -> &mut Self {
        self.write(DomainWrite::SeedResolution(record))
    }

    /// Extend use membership during an explicitly validated adoption.
    pub fn adopt_execution(
        &mut self,
        record: crate::resolutions::ExecutionRegistration,
    ) -> &mut Self {
        self.write(DomainWrite::AdoptExecution(record))
    }

    /// Retire source debt; set_requires supplies remaining non-execution debt.
    pub fn end_execution(&mut self, execution: crate::resolutions::ExecutionId) -> &mut Self {
        // Force the source reducer to refresh debt, preserving other tokens.
        self.write(DomainWrite::EndExecution(execution))
    }

    /// Register the committing actor's execution before its first resolution.
    pub fn register_execution(
        &mut self,
        registration: crate::resolutions::ExecutionRegistration,
    ) -> &mut Self {
        self.write(DomainWrite::RegisterExecution(registration))
    }

    /// Apply a domain write after the ownership fence, in recorded order.
    pub fn write(&mut self, write: DomainWrite) -> &mut Self {
        self.domain.push(write);
        self
    }

    /// Take the domain rows out of this transaction, for a decorator that
    /// applies them itself in the same database transaction.
    pub fn take_domain(&mut self) -> Vec<DomainWrite> {
        std::mem::take(&mut self.domain)
    }

    /// Stage `nodes` for this commit to install: every node a domain write
    /// publishes, or that a staged node references, and that no
    /// acknowledged commit installed.
    pub fn stage(&mut self, nodes: impl IntoIterator<Item = EncodedNode>) -> &mut Self {
        for node in nodes {
            self.graph.entry(node.id).or_insert(node);
        }
        self
    }

    /// Carry `held`, a transaction of the same actor that was never
    /// submitted: its domain writes after the ones recorded here, its staged
    /// nodes, the stored nodes it requires and what its roots owe. The fresh
    /// mailbox and cancellation read remain this transaction's own; its actor,
    /// epoch and revision must match the held draft before any writes are added.
    ///
    /// # Errors
    ///
    /// A different actor is an invalid composition. A lost epoch or a changed
    /// revision keeps its typed refusal; discard both drafts and reload.
    pub fn append(&mut self, held: Self) -> Result<&mut Self, crate::DurableError> {
        if self.actor != held.actor {
            return Err(crate::DurableError::Store(crate::StoreFailure {
                code: None,
                kind: crate::StoreFailureKind::Corrupt,
                message: format!("cannot compose {} into {}", held.actor, self.actor),
            }));
        }
        if self.epoch != held.epoch {
            return Err(crate::DurableError::OwnershipLost(crate::Fenced {
                actor: held.actor,
                held: held.epoch,
                current: Some(self.epoch),
            }));
        }
        if self.revision != held.revision {
            return Err(crate::DurableError::RevisionConflict {
                actor: held.actor,
                epoch: held.epoch,
                expected: held.revision,
                actual: self.revision,
            });
        }
        if let Some(preimage) = held.process_preimage {
            if self
                .process_preimage
                .as_ref()
                .is_some_and(|current| current != &preimage)
            {
                return Err(crate::DurableError::Store(crate::StoreFailure {
                    code: None,
                    kind: crate::StoreFailureKind::Corrupt,
                    message: "composed process preimages disagree".into(),
                }));
            }
            self.process_preimage = Some(preimage);
        }
        if held.prompt_postimage.is_some() {
            self.prompt_postimage = held.prompt_postimage;
        }
        self.domain.extend(held.domain);
        self.process_publications.extend(held.process_publications);
        if held.session_postimage.is_some() {
            self.session_postimage = held.session_postimage;
        }
        if held.phase.is_some() {
            self.phase = held.phase;
        }
        for (id, node) in held.graph {
            self.graph.entry(id).or_insert(node);
        }
        self.reads.extend(held.reads);
        self.exact_reads.extend(held.exact_reads);
        self.owed.extend(held.owed);
        Ok(self)
    }

    /// Stage everything `staging` made.
    pub fn stage_all(&mut self, staging: GraphStaging) -> &mut Self {
        self.stage(staging.into_nodes().into_values())
    }

    /// Record that `root`, a root this transaction stages, owes `owed`
    /// (S7): the callbacks its writer staged it owing, which the actor's
    /// claim obligations take as stated. The root is not decoded again: it
    /// names nodes the store already holds.
    pub fn owe(
        &mut self,
        root: NodeId,
        owed: Vec<lash_vocabulary::providers::PluginCallbackIdentity>,
    ) -> &mut Self {
        self.owed.insert(root, owed);
        self
    }

    /// What `root`'s writer stated it owes, when it stated it ([`Self::owe`]).
    #[must_use]
    pub fn owed(
        &self,
        root: &NodeId,
    ) -> Option<&[lash_vocabulary::providers::PluginCallbackIdentity]> {
        self.owed.get(root).map(Vec::as_slice)
    }

    /// Require `node`, which this commit references without staging: the
    /// commit refuses with nothing written when it is not stored.
    pub fn require(&mut self, node: NodeId) -> &mut Self {
        self.reads.insert(node);
        self
    }

    /// The nodes this transaction stages, by identity.
    #[must_use]
    pub fn graph(&self) -> &BTreeMap<NodeId, EncodedNode> {
        &self.graph
    }

    /// Require an acknowledged immutable node without resupplying it for
    /// installation. The store must find it and compare its exact bytes;
    /// absence refuses rather than recreating the node from preparation.
    pub fn require_exact(
        &mut self,
        node: std::sync::Arc<lash_core_store::checkpoint_graph::node::VerifiedNode>,
    ) -> &mut Self {
        self.reads.insert(node.id);
        self.exact_reads.entry(node.id).or_insert(node);
        self
    }

    /// Prepared reference checks, in digest order. They never install nodes.
    pub fn exact_reads(
        &self,
    ) -> &BTreeMap<NodeId, std::sync::Arc<lash_core_store::checkpoint_graph::node::VerifiedNode>>
    {
        &self.exact_reads
    }

    /// The stored nodes this transaction requires without staging them.
    #[must_use]
    pub fn reads(&self) -> &BTreeSet<NodeId> {
        &self.reads
    }

    /// Admit this commit only within `budget`, measured over its whole
    /// encoding (FIG-5887 §9); refused with nothing written past it.
    pub fn within(&mut self, budget: GraphCommitBudget) -> &mut Self {
        self.budget = Some(budget);
        self
    }

    /// The budget this commit is admitted within, if one was chosen.
    #[must_use]
    pub fn budget(&self) -> Option<&GraphCommitBudget> {
        self.budget.as_ref()
    }

    /// Record `changes`, the namespaces `session`'s unfinished `run`
    /// changed, staging each replaced value as its NamespaceValue node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn write_namespaces(
        &mut self,
        session: &SessionId,
        run: &TurnId,
        changes: &[TurnNamespaceWrite],
    ) -> Result<&mut Self, GraphRefusal> {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let mut namespaces = Vec::with_capacity(changes.len());
        for change in changes {
            let values = match &change.values {
                RunValuesWrite::Base => StoredValues::Base,
                RunValuesWrite::Held => StoredValues::Held,
                RunValuesWrite::Body(body) => StoredValues::Root(
                    NamespaceValue {
                        plugin: change.plugin.clone(),
                        body: body.clone(),
                    }
                    .stage(&mut staging)?,
                ),
            };
            namespaces.push(StoredNamespaceWrite {
                plugin: change.plugin.clone(),
                entry: change.entry.clone(),
                values,
            });
        }
        self.stage_all(staging);
        self.write(DomainWrite::Turn(TurnWrite::Namespaces {
            session: session.clone(),
            run: run.clone(),
            namespaces,
        }));
        Ok(self)
    }

    /// The position this transaction acknowledges through, if any.
    #[must_use]
    pub fn ack(&self) -> Option<MailSeq> {
        self.ack
    }

    /// Whether committing this transaction advances the actor: whether it
    /// writes anything beside its release.
    #[must_use]
    pub fn advances(&self) -> bool {
        !self.domain.is_empty()
            || !self.root_leases.is_empty()
            || !self.release_root_leases.is_empty()
            || !self.graph.is_empty()
            || self.ack.is_some()
            || self.formats.is_some()
            || self.requires.is_some()
    }

    /// How this transaction releases the actor, if it does.
    #[must_use]
    pub fn release(&self) -> Option<Release> {
        self.release
    }

    /// The format set this transaction records the actor's state in, if it
    /// moves it.
    #[must_use]
    pub fn formats(&self) -> Option<&FormatSet> {
        self.formats.as_ref()
    }

    /// Record that the actor's state is written in `formats` from this
    /// commit on: from then, only a node that decodes `formats` claims it.
    pub fn stamp_formats(&mut self, formats: FormatSet) -> &mut Self {
        self.formats = Some(formats);
        self
    }

    /// The claim requirements this transaction records for the actor, if it
    /// rewrites them.
    #[must_use]
    pub fn requires(&self) -> Option<&[String]> {
        self.requires.as_deref()
    }

    /// Record that, from this commit on, only a node serving every one of
    /// `requires` claims the actor to run it (FIG-6034 D3).
    pub fn set_requires(&mut self, requires: Vec<String>) -> &mut Self {
        self.requires = Some(requires);
        self
    }

    /// Acknowledge everything the fenced read saw: its mail and its wakes.
    pub fn ack_seen(&mut self) -> &mut Self {
        self.ack_through(self.seen)
    }

    /// Acknowledge through `through`, deleting the mail up to it.
    /// Acknowledging past [`Self::seen`] is refused at commit with
    /// [`DurableError::AckBeyondRead`](crate::DurableError::AckBeyondRead).
    pub fn ack_through(&mut self, through: MailSeq) -> &mut Self {
        self.ack = Some(self.ack.map_or(through, |ack| ack.max(through)));
        self
    }

    /// Give the actor up when this transaction commits. Mail that is still
    /// unacknowledged then makes it ready at once, unless it ends
    /// [`Release::Terminal`]; the last call wins.
    /// A waiting or idle caller must acknowledge the frontier it observed
    /// before reading the state it waits on. This does not acknowledge it:
    /// a hand-back that ran no work must preserve every outstanding wake.
    pub fn give_up(&mut self, release: Release) -> &mut Self {
        self.release = Some(release);
        self
    }
}

/// One mailbox write, as the store applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailWrite {
    /// Create an actor, ready to be claimed.
    CreateActor {
        /// Its key.
        actor: ActorKey,
        /// The format set its state is written in.
        formats: FormatSet,
    },
    /// Append mail and wake the actor.
    Append {
        /// The addressee.
        actor: ActorKey,
        /// What the mail is.
        kind: MailKind,
        /// Its encoded body.
        body: String,
    },
    /// Wake the actor without mail: something it waits on changed in a
    /// table of its own.
    Wake {
        /// The actor.
        actor: ActorKey,
    },
    /// A conditional domain write; answered in
    /// [`MailCommit::answers`](crate::MailCommit::answers).
    Domain(MailDomainWrite),
}

impl MailWrite {
    /// Whether this append or wake is a control wake, which readies a
    /// parked actor as a cancel or a redrive does: a session's close mail
    /// ([`SESSION_CLOSE_MAIL`](crate::domain::SESSION_CLOSE_MAIL)), so a
    /// park never holds a close back (FIG-6099).
    #[must_use]
    pub fn controls(&self) -> bool {
        matches!(
            self,
            Self::Append { kind, .. } if kind.as_str() == crate::domain::SESSION_CLOSE_MAIL
        )
    }
}

/// A non-owner's transaction: it creates actors, appends mail and wakes,
/// and nothing else. It has no owner-state writer.
#[derive(Clone, Debug, Default)]
pub struct MailTx {
    writes: Vec<MailWrite>,
}

impl MailTx {
    /// An empty mailbox transaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The writes recorded so far, in order.
    #[must_use]
    pub fn writes(&self) -> &[MailWrite] {
        &self.writes
    }

    /// Create `actor`, ready to be claimed. Refused at commit if it exists.
    pub fn create_actor(&mut self, actor: ActorKey, formats: FormatSet) -> &mut Self {
        self.writes.push(MailWrite::CreateActor { actor, formats });
        self
    }

    /// Append mail to `actor` and wake it; session close mail wakes it as
    /// a control wake ([`MailWrite::controls`]).
    pub fn append(
        &mut self,
        actor: ActorKey,
        kind: MailKind,
        body: impl Into<String>,
    ) -> &mut Self {
        self.writes.push(MailWrite::Append {
            actor,
            kind,
            body: body.into(),
        });
        self
    }

    /// Wake `actor` without mail.
    pub fn wake(&mut self, actor: ActorKey) -> &mut Self {
        self.writes.push(MailWrite::Wake { actor });
        self
    }

    /// Make the conditional domain write `write` in its place among this
    /// transaction's writes.
    pub fn write(&mut self, write: MailDomainWrite) -> &mut Self {
        self.writes.push(MailWrite::Domain(write));
        self
    }
}
