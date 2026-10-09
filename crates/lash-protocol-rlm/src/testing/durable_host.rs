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
        }
    }

    /// The backend the host's store set serves.
    pub(crate) fn backend(&self) -> &lash_core::Backend {
        &self.backend
    }

    /// The claimed context a cell runs under.
    pub(crate) fn context(&self) -> lash_core::ActorContext {
        self.context.clone()
    }

    /// The module store a cell's artifacts live in: the backend's.
    pub(crate) fn artifacts(&self) -> lash_vm::LashVmArtifacts {
        lash_vm::LashVmArtifacts::of_backend(&self.backend)
    }

    /// Every port of the host's backend, its claimed context serving the
    /// effects, and the lash_vm process engine over the backend's module
    /// store.
    pub(crate) fn ports(&self) -> lash_core::testing::TestExecutionPorts {
        let mut ports = lash_core::testing::TestExecutionPorts::lent(&self.backend, self.context());
        ports.process_engines = lash_core::ProcessEngineRegistry::new().with_registration(
            lash_vm_runtime::lash_vm_process_engine_registration(
                lash_vm_runtime::LashVmProcessEngine::new(
                    lash_vm::LashVmArtifacts::of_backend(&self.backend),
                    lash_vm_runtime::LashVmSurface::default(),
                ),
            ),
        );
        ports
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
