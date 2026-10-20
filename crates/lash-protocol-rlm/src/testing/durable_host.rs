//! The durable host a cell-level law runs its cells under.
//!
//! A deployment runs a cell inside its session actor's activation: the
//! actor is claimed by one node over the lash store, and every commit the
//! cell makes (its snapshots, its operations' admissions, its outcomes) is
//! fenced by that claim (ADR 0132 §8). [`DurableHost`] is that, and nothing
//! else: one session actor on a SQLite memory store set, claimed by a node,
//! whose context serves the cells a law runs. [`DurableHost::kill_and_resume`]
//! is a node kill followed by resume on another node: the dead boot is
//! fenced, the actor is claimed again under a new epoch, and a cell resumed
//! there sees only what committed. [`DurableHost::open_holding`] stores one
//! package set whose artifact every node of the host holds, so an actor
//! stamped with it stays claimable by them and by no node lacking it.

use std::sync::Arc;

use std::collections::BTreeSet;

use lash_core::durable_port::packages::{ArtifactInstall, SetInstall};
use lash_core::durable_port::{
    ActorKey, ArtifactDigest, CommitLabel, FormatSet, HeldArtifact, MailTx, NodeId, NodeLease,
    NodeSpec, PackageSetDigest,
};

/// The format set the host's nodes decode and its actor is created in.
const FORMATS: &str = "rlm-cell-law/1";

/// What publishes a capture's staged pages with the host's working commit,
/// as the session's executor does for a turn's (FIG-6418).
#[async_trait::async_trait]
pub(crate) trait Candidates: Send {
    /// The candidate of the pending capture of `root` under `commit`, the
    /// closures it names staged in `tx`; none when the store holds its
    /// pages.
    async fn candidate(
        &mut self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Option<lash_core::plugin::ExecutionCandidate>;
}

#[async_trait::async_trait]
impl Candidates for crate::executor::CodeModeExecutionState {
    async fn candidate(
        &mut self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Option<lash_core::plugin::ExecutionCandidate> {
        self.execution_candidate(root, commit, tx)
            .await
            .expect("the capture's candidate builds")
    }
}

/// A session actor claimed on a SQLite memory durable store.
pub(crate) struct DurableHost {
    backend: lash_execution_contracts::backend::Backend,
    actor: ActorKey,
    admitted: lash_core::AdmittedScope,
    lease: NodeLease,
    context: lash_core::ActorContext,
    probe: Arc<dyn lash_core::durable_port::DurableProbe>,
    /// The package artifacts every node of the host holds.
    held: BTreeSet<HeldArtifact>,
    nodes: usize,
    /// The working state the host's turn row holds; none before its first
    /// [`Self::commit_working`], which admits the turn.
    working: std::sync::Mutex<Option<Option<lash_core_store::checkpoint_graph::NodeId>>>,
}

impl DurableHost {
    /// The host of `admitted`'s session, claimed by its first node.
    pub(crate) async fn open(admitted: lash_core::AdmittedScope) -> Self {
        Self::open_with_probe(admitted, Arc::new(lash_core::durable_port::NoProbe)).await
    }

    /// [`Self::open`], whose contexts report what hidden replay would make
    /// an owner do to `probe`.
    pub(crate) async fn open_with_probe(
        admitted: lash_core::AdmittedScope,
        probe: Arc<dyn lash_core::durable_port::DurableProbe>,
    ) -> Self {
        Self::open_on(admitted, probe, BTreeSet::new(), &[]).await
    }

    /// [`Self::open`], its store holding the package set `packages`, of one
    /// artifact every node of the host holds.
    pub(crate) async fn open_holding(
        admitted: lash_core::AdmittedScope,
        packages: PackageSetDigest,
    ) -> Self {
        let artifact = HeldArtifact {
            digest: ArtifactDigest::from_bytes(*packages.as_bytes()),
            path: "law".to_owned(),
        };
        let set = SetInstall {
            digest: packages,
            members: BTreeSet::from([artifact.clone()]),
            surface: Vec::new(),
            requirements: Default::default(),
        };
        Self::open_on(
            admitted,
            Arc::new(lash_core::durable_port::NoProbe),
            BTreeSet::from([artifact]),
            &[set],
        )
        .await
    }

    async fn open_on(
        admitted: lash_core::AdmittedScope,
        probe: Arc<dyn lash_core::durable_port::DurableProbe>,
        held: BTreeSet<HeldArtifact>,
        sets: &[SetInstall],
    ) -> Self {
        let backend = lash_execution_contracts::backend::Backend::for_testing(
            super::sqlite_memory_store_set().await,
        );
        let artifacts: Vec<ArtifactInstall> = held
            .iter()
            .map(|artifact| ArtifactInstall {
                artifact: artifact.clone(),
                header: "{}".to_owned(),
                definitions: BTreeSet::new(),
                internal: BTreeSet::new(),
                labels: "{}".to_owned(),
            })
            .collect();
        backend
            .durable()
            .install_packages(&artifacts, sets)
            .await
            .expect("install the law's package set");
        let actor = match admitted.scope() {
            lash_core::ExecutionScope::Process { process_id } => {
                ActorKey::process(process_id.as_str())
            }
            lash_core::ExecutionScope::Turn { session_id, .. }
            | lash_core::ExecutionScope::SessionOperation { session_id, .. }
            | lash_core::ExecutionScope::SessionDelete { session_id } => {
                ActorKey::session(session_id.as_str())
            }
            lash_core::ExecutionScope::RuntimeOperation { operation_id } => {
                ActorKey::session(operation_id)
            }
        }
        .expect("the law's scope names a valid actor");
        let mut created = MailTx::new();
        created.create_actor(actor.clone(), FormatSet::new(FORMATS));
        backend
            .durable()
            .commit_mail(created, CommitLabel::new("law.create"))
            .await
            .expect("create the law's session actor");
        let lease = register(&backend, 0, &held).await;
        let context = claim(&backend, &lease, &actor, &admitted, &probe).await;
        Self {
            backend,
            actor,
            admitted,
            lease,
            context,
            probe,
            held,
            nodes: 1,
            working: std::sync::Mutex::new(None),
        }
    }

    /// The claimed context a cell runs under.
    pub(crate) fn context(&self) -> lash_core::ActorContext {
        self.context.clone()
    }

    /// Every port of the host's backend, its claimed context serving the
    /// effects.
    pub(crate) fn ports(&self) -> lash_core::testing::TestExecutionPorts {
        lash_core::testing::TestExecutionPorts::lent(&self.backend, self.context())
    }

    /// Commits `capture` as the working state of the host's turn, as the
    /// turn phase commit that captured it would: under the turn row's
    /// working root, its staged pages the candidate `candidates` builds,
    /// owned by the session (FIG-6418). Answers how many pages the commit
    /// published.
    pub(crate) async fn commit_working(
        &self,
        capture: &lash_core_store::execution_state::ExecutionGraphCapture,
        candidates: &mut dyn Candidates,
    ) -> u64 {
        use lash_core::durable_port::DomainWrite;
        use lash_core::durable_port::domain::turns::TurnWrite;
        let lash_core_store::execution_state::ExecutionGraphCapture::Replace(graph) = capture
        else {
            return 0;
        };
        let root = &graph.root;
        let lash_core::ExecutionScope::Turn {
            session_id,
            turn_id,
        } = self.admitted.scope().clone()
        else {
            panic!("a law's working state belongs to a turn");
        };
        let mut tx = self.context.begin().await.expect("open the working commit");
        let expected = {
            let mut working = self
                .working
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let expected = working.flatten();
            if working.is_none() {
                tx.write(DomainWrite::Turn(TurnWrite::Admit {
                    session: session_id.clone(),
                    run: turn_id.clone(),
                    admission: lash_core_store::store::RunAdmissionRecord::Turn {
                        took: lash_core_store::store::AdmittedTurnRows::Inputs {
                            ids: lash_core_store::store::AdmittedInputIds::one(
                                lash_core::InputId::from("rlm-cell-law"),
                            ),
                        },
                        trace: None,
                    },
                    turn_deadline: None,
                }));
            }
            *working = Some(Some(*root));
            expected
        };
        tx.write(DomainWrite::Turn(TurnWrite::Working {
            session: session_id.clone(),
            run: turn_id,
            expected,
            working: Some(*root),
        }));
        let commit = lash_core_store::checkpoint_graph::CommitId::mint();
        let label = CommitLabel::new("law.working");
        let candidate = candidates.candidate(*root, commit, &mut tx).await;
        let published = candidate
            .as_ref()
            .map_or(0, |candidate| candidate.manifest.summary().fresh_nodes);
        match candidate {
            Some(candidate) => self
                .context
                .commit_candidate_owing(
                    tx,
                    lash_durable::domain::pages::CandidatePublication {
                        owner: lash_durable::domain::pages::CandidateOwner::Session(session_id),
                        manifest: candidate.manifest,
                    },
                    label,
                    None,
                )
                .await
                .map(|_| ()),
            None => {
                tx.stage([graph.node()]);
                self.context.commit(tx, label).await.map(|_| ())
            }
        }
        .expect("commit the working state");
        published
    }

    /// Where the host's session's committed state stands: its turn's
    /// working root, once one was committed.
    pub(crate) fn session_pages(&self) -> Option<lash_core::plugin::SessionPages> {
        let root = self
            .working
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flatten()?;
        let lash_core::ExecutionScope::Turn {
            session_id,
            turn_id,
        } = self.admitted.scope().clone()
        else {
            return None;
        };
        Some(lash_core::plugin::SessionPages {
            store: self.context.pages().ok()?,
            actor: self.actor.clone(),
            epoch: self.context.epoch(),
            owner: lash_core::durable_port::domain::graph::RootOwner::WorkingExecution {
                session: session_id,
                run: turn_id,
            },
            root,
        })
    }

    /// The node serving the actor dies without releasing it, and another
    /// node claims it: the dead boot's epoch is fenced, and the new context
    /// sees only what committed.
    pub(crate) async fn kill_and_resume(&mut self) {
        self.kill_and_resume_past(None).await;
    }

    /// [`Self::kill_and_resume`], a node that decodes the actor's formats
    /// and holds none of the host's packages asking to claim it first:
    /// whether that node claimed the actor to run it. Nothing resumes when
    /// it did.
    pub(crate) async fn kill_and_resume_past_a_lacking_node(&mut self) -> bool {
        let lacking = register(&self.backend, self.nodes, &BTreeSet::new()).await;
        self.nodes += 1;
        self.kill_and_resume_past(Some(&lacking)).await
    }

    async fn kill_and_resume_past(&mut self, first: Option<&NodeLease>) -> bool {
        let next = register(&self.backend, self.nodes, &self.held).await;
        // The dead node's lease is ended for it, as its expiry and a reap
        // would: its actors are released with their epochs bumped.
        self.backend
            .durable()
            .release_node(&self.lease)
            .await
            .expect("fence the dead node");
        if let Some(first) = first {
            let claimed = self
                .backend
                .durable()
                .claim(first, 16)
                .await
                .expect("the first node's claim")
                .iter()
                .any(|claimed| {
                    claimed.actor == self.actor
                        && claimed.purpose == lash_core::durable_port::ClaimPurpose::Run
                });
            if claimed {
                return true;
            }
        }
        self.context = claim(
            &self.backend,
            &next,
            &self.actor,
            &self.admitted,
            &self.probe,
        )
        .await;
        self.lease = next;
        self.nodes += 1;
        false
    }
}

async fn register(
    backend: &lash_execution_contracts::backend::Backend,
    index: usize,
    held: &BTreeSet<HeldArtifact>,
) -> NodeLease {
    backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new(format!("rlm-cell-law-{index}")),
            decodes: vec![FormatSet::new(FORMATS)],
            serves: Vec::new(),
            held: held.clone(),
            capabilities: Default::default(),
            ttl_millis: 600_000,
        })
        .await
        .expect("register the law's node")
}

async fn claim(
    backend: &lash_execution_contracts::backend::Backend,
    lease: &NodeLease,
    actor: &ActorKey,
    admitted: &lash_core::AdmittedScope,
    probe: &Arc<dyn lash_core::durable_port::DurableProbe>,
) -> lash_core::ActorContext {
    let epoch = backend
        .durable()
        .claim(lease, 16)
        .await
        .expect("claim the law's session actor")
        .into_iter()
        .find(|claimed| &claimed.actor == actor)
        .map(|claimed| claimed.epoch)
        .expect("the law's session actor is claimable");
    lash_core::ActorContext::new(
        backend.clone(),
        actor.clone(),
        epoch,
        admitted.clone(),
        lash_core::CancellationToken::new(),
        Arc::clone(probe),
    )
}
