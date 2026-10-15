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
//! there sees only what committed.

use std::sync::Arc;

use lash_core::durable_port::{
    ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeLease, NodeSpec,
};

/// The format set the host's nodes decode and its actor is created in.
const FORMATS: &str = "rlm-cell-law/1";

/// A session actor claimed on a SQLite memory durable store.
pub(crate) struct DurableHost {
    backend: lash_core::Backend,
    actor: ActorKey,
    admitted: lash_core::AdmittedScope,
    lease: NodeLease,
    context: lash_core::ActorContext,
    probe: Arc<dyn lash_core::durable_port::DurableProbe>,
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
        let backend = lash_core::Backend::for_testing(super::sqlite_memory_store_set().await);
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
        let lease = register(&backend, 0).await;
        let context = claim(&backend, &lease, &actor, &admitted, &probe).await;
        Self {
            backend,
            actor,
            admitted,
            lease,
            context,
            probe,
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
    /// turn phase commit that captured it would: its nodes, under the turn
    /// row's working root. Answers where the session's state stands now,
    /// which a later cell's context names.
    pub(crate) async fn commit_working(
        &self,
        capture: &lash_core_store::execution_state::ExecutionGraphCapture,
    ) -> Option<lash_core::plugin::SessionPages> {
        use lash_core::durable_port::DomainWrite;
        use lash_core::durable_port::domain::turns::TurnWrite;
        let lash_core_store::execution_state::ExecutionGraphCapture::Replace { root, nodes } =
            capture
        else {
            return self.session_pages();
        };
        let lash_core::ExecutionScope::Turn {
            session_id,
            turn_id,
        } = self.admitted.scope().clone()
        else {
            panic!("a law's working state belongs to a turn");
        };
        let mut tx = self.context.begin().await.expect("open the working commit");
        tx.stage(nodes.values().cloned());
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
            session: session_id,
            run: turn_id,
            expected,
            working: Some(*root),
        }));
        self.context
            .commit(tx, CommitLabel::new("law.working"))
            .await
            .expect("commit the working state");
        self.session_pages()
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
        let next = register(&self.backend, self.nodes).await;
        // The dead node's lease is ended for it, as its expiry and a reap
        // would: its actors are released with their epochs bumped.
        self.backend
            .durable()
            .release_node(&self.lease)
            .await
            .expect("fence the dead node");
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
    }
}

async fn register(backend: &lash_core::Backend, index: usize) -> NodeLease {
    backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new(format!("rlm-cell-law-{index}")),
            decodes: vec![FormatSet::new(FORMATS)],
            serves: Vec::new(),
            ttl_millis: 600_000,
        })
        .await
        .expect("register the law's node")
}

async fn claim(
    backend: &lash_core::Backend,
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
