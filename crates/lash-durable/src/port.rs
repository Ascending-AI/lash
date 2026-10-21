//! The [`DurableStore`] port and the records it answers with.

use crate::domain::{DurableReads, MailAnswer, ProcessView};
use crate::error::DurableError;
use crate::formats::FormatSet;
use crate::ids::{
    ActorKey, BootId, CommitLabel, DurableInstant, Epoch, MailSeq, NodeId, StateRevision,
};
use crate::packages::{
    ArtifactInstall, NodeCapability, PackageCatalog, PackageSetDigest, SetInstall,
};
use crate::tx::{ActorTx, MailTx};

/// An actor's scheduling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActorState {
    /// Unowned with nothing to do.
    Idle,
    /// Unowned and claimable now.
    Ready,
    /// Owned by a node.
    Owned,
    /// Unowned, blocked until mail arrives or its due time passes.
    Waiting,
    /// Unowned and set aside for an operator: only a control wake (a
    /// cancel request or a redrive) readies it.
    Parked,
    /// Finished; never claimed again.
    Terminal,
}

impl ActorState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Ready => "ready",
            Self::Owned => "owned",
            Self::Waiting => "waiting",
            Self::Parked => "parked",
            Self::Terminal => "terminal",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "idle" => Some(Self::Idle),
            "ready" => Some(Self::Ready),
            "owned" => Some(Self::Owned),
            "waiting" => Some(Self::Waiting),
            "parked" => Some(Self::Parked),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// A node an actor is owned by: one boot of one node.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    /// The node.
    pub node: NodeId,
    /// Its boot.
    pub boot: BootId,
}

/// What a node registers as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    /// The node's stable name. Registering it fences every earlier boot of
    /// the same name: a node id names one serving process at a time.
    pub node: NodeId,
    /// The format sets this build decodes; it claims only actors in one.
    pub decodes: Vec<FormatSet>,
    /// The claim requirements this build's registrations serve: it runs an
    /// actor only when the actor's requirements are a subset (FIG-6034 D3).
    pub serves: Vec<String>,
    /// What this node offers running code, advertised while its lease is
    /// live (FIG-6991). It also serves the token of each capability, so an
    /// actor whose hard requirements record names a native, kernel,
    /// provider or codec the node lacks is never claimed to run here
    /// (FIG-6993).
    pub capabilities: crate::NodeCapabilities,
    /// How long one heartbeat keeps the lease, in milliseconds. Hosts take
    /// it from a validated [`LeaseConfig`](crate::LeaseConfig).
    pub ttl_millis: i64,
}

impl NodeSpec {
    /// Every claim token the node serves: its registrations', then one per
    /// capability it advertises, sorted and without duplicates. A claim
    /// and an owned recovery compare an actor's `requires` against these
    /// alone.
    #[must_use]
    pub fn served(&self) -> Vec<String> {
        let mut served: std::collections::BTreeSet<String> = self.serves.iter().cloned().collect();
        served.extend(
            lash_core_store::obligations::ObligationToken::for_capabilities(&self.capabilities)
                .map(String::from),
        );
        served.into_iter().collect()
    }
}

/// One boot's live lease, as [`DurableStore::register_node`] grants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLease {
    /// The node and boot.
    pub owner: Owner,
    /// The format sets it decodes.
    pub decodes: Vec<FormatSet>,
    /// The claim requirements it serves: its registrations' and its
    /// capabilities' ([`NodeSpec::served`]).
    pub serves: Vec<String>,
    /// What it offers running code.
    pub capabilities: crate::NodeCapabilities,
    /// How long one heartbeat keeps the lease, in milliseconds.
    pub ttl_millis: i64,
    /// When the lease expires unless renewed, as of the registration.
    pub expires_at: DurableInstant,
}

/// A heartbeat's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// The lease is renewed to `expires_at`.
    Renewed {
        /// The new expiry.
        expires_at: DurableInstant,
    },
    /// The lease is gone: the node was reaped or replaced. It must drop
    /// every actor and stop serving at once.
    Reaped,
}

/// Why a claim took an actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimCause {
    /// It was ready: new, woken by mail, or released by a reap.
    Ready,
    /// It was waiting and its due time passed.
    Due,
    /// This boot already owns it: [`DurableStore::owned`] answers it, so a
    /// node whose claim reply was lost can adopt what the claim took.
    Adopted,
}

/// One actor a claim took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The actor.
    pub actor: ActorKey,
    /// The epoch the claimer now owns it under.
    pub epoch: Epoch,
    /// Why it was claimable.
    pub cause: ClaimCause,
    /// What the claiming node may do with it.
    pub purpose: ClaimPurpose,
    /// The claim requirements the actor carried when it was claimed: what
    /// its owner holds until it derives them again.
    pub requires: Vec<String>,
}

/// What a node claimed an actor for (ADR 0106 §1, ADR 0132 §11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPurpose {
    /// The node decodes the actor's format set and serves every claim
    /// requirement it carries, its hard requirements among them: it runs
    /// the actor.
    Run,
    /// The node does not decode the actor's format set or does not serve a
    /// requirement it carries, and claimed it only for its pending cancel:
    /// a process's cancel mail, or a session's close or `Immediate` turn
    /// cancel (FIG-6072). The activation ends it from registry state and
    /// never reads its engine state or calls its engine or a provider.
    CancelOnly,
}

impl ClaimPurpose {
    /// The purpose `node` claims an actor stored in `formats` and requiring
    /// `requires` for (FIG-6034 D3: a build that lacks a requirement never
    /// claims the actor to run it; FIG-6993: nor one whose capabilities miss
    /// a hard requirement). The one predicate of a claim and of an owned
    /// recovery.
    #[must_use]
    pub fn of(node: &NodeLease, formats: &str, requires: &[String]) -> Self {
        if node.decodes.iter().any(|set| set.as_str() == formats)
            && requires.iter().all(|required| {
                lash_core_store::obligations::ObligationToken::is_served(required, &node.serves)
            })
        {
            Self::Run
        } else {
            Self::CancelOnly
        }
    }

    /// [`Self::of`] an actor whose requirements are stored as `requires`,
    /// the canonical JSON array or `NULL`.
    ///
    /// # Errors
    ///
    /// The stored requirements do not decode: the row is corrupt.
    pub fn of_stored(
        node: &NodeLease,
        formats: &str,
        requires: Option<&str>,
    ) -> Result<Self, crate::DurableError> {
        let requires = parse_requires(requires).map_err(|error| {
            crate::DurableError::Store(crate::StoreFailure {
                kind: crate::StoreFailureKind::Corrupt,
                message: format!("actor claim requirements: {error}"),
            })
        })?;
        Ok(Self::of(node, formats, &requires))
    }
}

/// The claim requirements a store recorded for an actor, as the canonical
/// JSON array it stores; none for `NULL`.
///
/// # Errors
///
/// The stored text is not a JSON array of strings.
pub fn parse_requires(stored: Option<&str>) -> Result<Vec<String>, serde_json::Error> {
    stored.map_or(Ok(Vec::new()), serde_json::from_str)
}

/// The canonical stored spelling of `requires`: a JSON array, sorted and
/// deduplicated.
#[must_use]
pub fn encode_requires(requires: &[String]) -> String {
    let set: std::collections::BTreeSet<&String> = requires.iter().collect();
    serde_json::Value::from(set.into_iter().map(String::as_str).collect::<Vec<_>>()).to_string()
}

/// One actor a reap released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaped {
    /// The actor, now ready.
    pub actor: ActorKey,
    /// The dead owner.
    pub from: Owner,
    /// Its new epoch: the dead owner's is fenced from the reap on.
    pub epoch: Epoch,
}

/// A session head actually committed by an owner transaction.
#[derive(Clone, Debug)]
pub struct CommittedSessionHead {
    /// The target session, in domain-write order.
    pub session: lash_protocol_types::SessionId,
    /// The complete result constructed inside the transaction.
    pub receipt: lash_core_store::store::RuntimeCommitReceipt,
}

impl PartialEq for CommittedSessionHead {
    fn eq(&self, other: &Self) -> bool {
        // Receipt equality includes every encoded fact and the transient
        // replay decision, which deliberately is not part of persistence.
        self.session == other.session
            && self.receipt.receipt_replayed == other.receipt.receipt_replayed
            && match (
                serde_json::to_value(&self.receipt),
                serde_json::to_value(&other.receipt),
            ) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            }
    }
}
impl Eq for CommittedSessionHead {}

/// An owner commit's receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorCommit {
    /// One exact result per successful session-head write, in write order.
    /// No result is published before the enclosing transaction commits.
    pub session_heads: Vec<CommittedSessionHead>,
    /// Root leases acquired atomically with this commit, in request order.
    /// Their expiry remains owned by retirement if acknowledgement is lost.
    pub root_leases: Vec<crate::domain::pages::RootLease>,
    /// The actor's state revision after the commit.
    pub revision: StateRevision,
    /// The state the actor was left in: [`ActorState::Owned`] unless the
    /// commit released it.
    pub state: ActorState,
    /// For a session actor, the cancel request its unfinished turn had
    /// accepted as the commit read it under its fence, before its writes: a
    /// cancel accepted after the owner's open is seen here, inside the
    /// commit, with no read of its own. One accepted while the fence waited
    /// may be missed; the owner's next open reads it. `None` for any other
    /// actor.
    pub turn_cancel: Option<crate::domain::TurnCancelRequest>,
}

/// What a claim could find, as [`DurableStore::readiness`] read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readiness {
    /// The store's time the answer holds at.
    pub now: DurableInstant,
    /// Whether some actor is ready, or waiting with its due time at or
    /// before `now`: a claim may find work.
    pub due: bool,
    /// The earliest due time after `now` that some waiting actor holds.
    pub next_due: Option<DurableInstant>,
}

/// One actor a mailbox commit woke, for the post-commit hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Woken {
    /// The actor.
    pub actor: ActorKey,
    /// Its state after the commit.
    pub state: ActorState,
    /// Its owner, when owned: the node to hint.
    pub owner: Option<Owner>,
}

/// A mailbox commit's receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailCommit {
    /// Where each appended mail landed, in write order.
    pub appended: Vec<(ActorKey, MailSeq)>,
    /// Every actor the commit woke, once each, in first-write order.
    pub woken: Vec<Woken>,
    /// One answer per [`MailDomainWrite`](crate::domain::MailDomainWrite),
    /// in write order.
    pub answers: Vec<MailAnswer>,
}

/// An actor's row, read without a fence. For operators and test laws; an
/// owner reads only its park and failed activations from it, once, as its
/// claim starts (the activation-loop budget).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorSnapshot {
    /// The actor.
    pub actor: ActorKey,
    /// Its scheduling state.
    pub state: ActorState,
    /// Its current epoch.
    pub epoch: Epoch,
    /// Its owner, when owned.
    pub owner: Option<Owner>,
    /// Whether mail arrived since the owner last acknowledged.
    pub has_mail: bool,
    /// The earliest durable deadline it waits on.
    pub next_due: Option<DurableInstant>,
    /// How many owner commits its state has taken.
    pub revision: StateRevision,
    /// The format set its state is written in.
    pub formats: FormatSet,
    /// How many mail rows are pending.
    pub pending_mail: u64,
    /// Why it is parked, encoded by its owner, while it is parked; kept
    /// through a control wake so its claimer knows it was parked.
    pub park: Option<String>,
    /// How many claims in a row found no commit since the previous claim:
    /// the actor's failed activations (ADR 0132 §3).
    pub failed_activations: u32,
}

/// Scheduling facts read together, without claiming or acquiring a writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimFacts {
    pub observed_at: DurableInstant,
    /// The registered boot of the requested node, even when it replaced the caller's.
    pub node: Option<ClaimNodeFacts>,
    pub actor: Option<ClaimActorFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimNodeFacts {
    pub owner: Owner,
    pub draining: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimActorFacts {
    pub actor: ActorKey,
    pub state: ActorState,
    pub epoch: Epoch,
    pub revision: StateRevision,
    pub ready_at: Option<DurableInstant>,
    pub next_due: Option<DurableInstant>,
    pub formats: FormatSet,
    pub requires: Vec<String>,
    pub process_cancel: bool,
    pub immediate_cancel: bool,
    pub close_mail: bool,
    pub unfinished_close: bool,
}

/// The fenced transaction port: one implementation per dialect, its SQL in
/// one module there. Every method that writes runs one database
/// transaction, under a [`CommitLabel`].
///
/// No method has a default body: an implementation states every rule. It
/// answers the domain rows' [`DurableReads`] too.
#[async_trait::async_trait]
pub trait DurableStore: DurableReads {
    /// The store's clock: the database's on PostgreSQL, the injected one on
    /// SQLite.
    async fn now(&self) -> Result<DurableInstant, DurableError>;

    /// The store's persistent-heap page publication (S6), through which an
    /// activation leases roots, reads pages and publishes candidates. A
    /// wrapper answers its inner store's, or its own when it intercepts
    /// publication; `None` for a store that holds no heap pages, which
    /// refuses every kernel activation that parks.
    fn pages(&self) -> Option<std::sync::Arc<dyn crate::domain::pages::PageStore>>;

    /// Register one boot of `spec.node` with a fresh lease.
    ///
    /// Earlier boots of the same node are reaped in the same transaction,
    /// their actors released with their epochs bumped.
    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError>;

    /// Renew `node`'s lease by its ttl. [`HeartbeatOutcome::Reaped`] when
    /// the lease is gone.
    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError>;

    /// Delete every node whose lease expired and release each one's actors
    /// as ready with their epochs bumped, in one transaction. `reaper` is
    /// the node doing it; it must hold a live lease.
    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError>;

    /// End `node`'s lease on a clean shutdown, releasing its actors as
    /// ready with their epochs bumped.
    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError>;

    /// Claim up to `limit` actors that are ready, or waiting with a passed
    /// due time, in a format set `node` decodes and with every claim
    /// requirement it serves ([`ClaimPurpose::Run`]); otherwise, only so
    /// their cancel ends them ([`ClaimPurpose::CancelOnly`]), process actors
    /// with cancel mail pending, and session actors in a set it decodes
    /// whose open turn holds an `Immediate` cancel request, with close mail
    /// ([`SESSION_CLOSE_MAIL`](crate::domain::SESSION_CLOSE_MAIL)) pending,
    /// or whose close has not reached its tombstone (FIG-6072). A session's
    /// `AfterStep` request leaves it to a node that serves it. Each claim
    /// bumps the actor's epoch. A draining node claims nothing. Refused with
    /// [`DurableError::NodeLeaseLost`] when the node holds no live lease.
    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError>;

    /// What a claim could find now, read without a writer or a lock in one
    /// statement (FIG-7044): the store's time, whether any actor is ready or
    /// waiting with a passed due time, and the earliest due time still
    /// ahead. A superset of every node's claimable actors, so a claim after
    /// a negative answer finds nothing it would have found; a positive
    /// answer only permits a claim, which decides under its own fence.
    async fn readiness(&self) -> Result<Readiness, DurableError>;

    /// Acquire only `actor` for a test writer after execution is quiescent.
    /// The compare-and-set requires its observed `epoch` and an unowned,
    /// nonterminal row. Missing and terminal actors use the existing mailbox
    /// refusals; an owned or changed row uses `OwnershipLost`. The caller
    /// must release `node` after its fenced begin/commit. No other actor is
    /// touched, and this grant never authorizes running an activation.
    #[cfg(feature = "testing")]
    async fn acquire_idle_for_testing(
        &self,
        node: &NodeLease,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<Epoch, DurableError>;

    /// Mark `node` draining (ADR 0106 §1): from this commit on it claims
    /// nothing, and its owners release their actors at their next committed
    /// phase. Refused with [`DurableError::NodeLeaseLost`] when the node
    /// holds no live lease.
    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError>;

    /// Each live node's format sets and capabilities, one entry per node,
    /// by node id: what the fleet-format gate
    /// ([`fleet_writable`](crate::fleet_writable)), the package gate and
    /// the strand report read.
    async fn live_nodes(&self) -> Result<Vec<NodeCapability>, DurableError>;

    /// Up to `limit` actors that have not ended and carry claim
    /// requirements, by key after `after`, each with its stored
    /// requirements: what the strand report pages (FIG-6993).
    async fn actor_requirements(
        &self,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<(ActorKey, Vec<String>)>, DurableError>;

    /// Install `artifacts` and `sets` in one transaction. An artifact or set
    /// already stored is kept as it is, its surface and requirements
    /// included; a new artifact takes the next ordinal on its package line,
    /// and a new set the next install sequence. A set naming an artifact
    /// neither stored nor installed here is refused, and nothing is
    /// written.
    async fn install_packages(
        &self,
        artifacts: &[ArtifactInstall],
        sets: &[SetInstall],
    ) -> Result<(), DurableError>;

    /// Every stored artifact, by line and ordinal, and every stored set, by
    /// install sequence, oldest first.
    async fn package_catalog(&self) -> Result<PackageCatalog, DurableError>;

    /// The surface snapshot bytes stored for the set `digest`, as its first
    /// install wrote them; `None` for a set the store does not hold.
    async fn package_surface(
        &self,
        digest: &PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, DurableError>;

    /// The paths of the stored packages among whose definitions each of
    /// `functions` is, helpers included: the membership index installs
    /// write and nothing else does (FIG-6991). A function no stored package
    /// defines is absent.
    async fn package_paths(
        &self,
        functions: &[lash_kernel_doc::FunctionId],
    ) -> Result<
        std::collections::BTreeMap<lash_kernel_doc::FunctionId, std::collections::BTreeSet<String>>,
        DurableError,
    >;

    /// The labels each install of each of `artifacts` wrote, as JSON, in
    /// install order and each once: an artifact two releases of identical
    /// content installed has both. An artifact no install wrote is absent.
    async fn package_labels(
        &self,
        artifacts: &[crate::ArtifactDigest],
    ) -> Result<std::collections::BTreeMap<crate::ArtifactDigest, Vec<String>>, DurableError>;

    /// The paths of the stored packages that declare each of `operations`
    /// internal: what, with [`Self::package_paths`], says whether a stored
    /// definition may perform it. An operation no stored package declares
    /// internal is absent.
    async fn internal_operations(
        &self,
        operations: &[lash_kernel_doc::EffectName],
    ) -> Result<
        std::collections::BTreeMap<lash_kernel_doc::EffectName, std::collections::BTreeSet<String>>,
        DurableError,
    >;

    /// The actors `node`'s boot owns now, with their current epochs, read
    /// unfenced. Each answers [`ClaimCause::Adopted`].
    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError>;

    /// Open the owner's transaction over `actor` at `epoch`: a fenced read
    /// that returns the pending mail, the store's clock and, for a session
    /// actor, its unfinished turn's accepted cancel request, in one read.
    /// Refused with [`DurableError::OwnershipLost`] when `epoch` is not
    /// current.
    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError>;

    /// Refresh only external control and a byte-bounded mailbox page for a
    /// retained owner. An unchanged basis never reloads a head or phase.
    /// Every adapter must implement this boundary explicitly.
    async fn begin_retained(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        max_mail_bytes: u64,
    ) -> Result<ActorTx, DurableError>;

    /// [`begin`](Self::begin) over process actor `actor`, with the
    /// [`ProcessView`] its owner decides a pass from, read in the same
    /// snapshot as the fenced open: one read for the mail, the store's
    /// clock, the process row, its registry record and its execution root.
    /// Refused as `begin` is; an actor that is not a process's is refused as
    /// corrupt.
    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, ProcessView), DurableError>;

    /// Apply `tx` atomically. The transaction's first statement re-checks
    /// the epoch; a stale one is [`DurableError::OwnershipLost`] and nothing
    /// is written.
    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError>;

    /// Apply `tx` atomically: creations, appends and wakes, nothing else.
    async fn commit_mail(&self, tx: MailTx, label: CommitLabel)
    -> Result<MailCommit, DurableError>;

    /// Read node identity/draining and actor scheduling, requirements and cancel
    /// facts in one read snapshot. Nothing is written or activated.
    async fn claim_facts(
        &self,
        node: Option<&Owner>,
        actor: &ActorKey,
    ) -> Result<ClaimFacts, DurableError>;

    /// Read `actor`'s row, unfenced.
    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError>;

    /// Wait out the owner commit of `actor` in flight, if any: a read
    /// begun after this returns sees every owner commit of `actor` that
    /// had begun before it was called. A reader whose own commit another
    /// actor's commit cannot see reads that actor's facts after it
    /// (FIG-6329). Nothing is written or locked past the return.
    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError>;

    /// Up to `limit` actors whose state is written in `formats` and that
    /// have not ended, by key, after `after`, read unfenced: what an
    /// operator lists of the actors a build carries forward or retires
    /// (ADR 0106 §1, ADR 0115 §3.5). An owner never decides anything from
    /// it.
    async fn actors_in(
        &self,
        formats: &FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError>;
}
