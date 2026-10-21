//! The performance harness's view of a code mode session's execution
//! state: its capture, its publication with the session actor's head
//! commit as the commit's candidate (FIG-6418), and its restore.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::durable_port::domain::SessionCommitWrite;
use lash_core::durable_port::domain::pages::{CandidateOwner, CandidatePublication};
use lash_core::durable_port::{
    ActorKey, CommitLabel, DomainWrite, FormatSet, MailTx, NodeId, NodeSpec,
};
use lash_core::{ExecRequest, RuntimeExecutionContext};
use lash_core_store::checkpoint_graph::stage::ManifestEntry;
use lash_core_store::checkpoint_graph::{CommitId, EncodedNode};

use super::{CellServices, CodeModeExecutionState, KernelCarry, execute_cell};
use crate::projection::CodeModeProjectedBindings;

/// The format set the publisher's node decodes and its actor is created in.
const FORMATS: &str = "rlm-checkpoint-perf/1";

/// How many manifest entries a publication reads at once.
const MANIFEST_BATCH: usize = 256;

/// Feature-gated fixture that lets the repository's performance harness
/// shift the production code mode execution-state capture without exposing
/// executor internals as public protocol API.
pub struct CodeModeCheckpointPerfFixture {
    services: CellServices,
    state: CodeModeExecutionState,
    binding_count: usize,
    payload_bytes: usize,
}

impl CodeModeCheckpointPerfFixture {
    /// A fixture whose session holds `binding_count` bindings of
    /// `payload_bytes` each.
    pub async fn new(
        dialect: &crate::CellDialect,
        workers: lash_vm_client::service::Service,
        binding_count: usize,
        payload_bytes: usize,
    ) -> Result<Self, lash_core::SessionError> {
        let mut state = CodeModeExecutionState::new(dialect.name(), dialect.numbers())
            .staging(workers.spools());
        let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
        for index in 0..binding_count {
            patch.set_default.insert(
                format!("mid_{index}"),
                serde_json::json!([format!("binding-{index}-{}", payload(index, payload_bytes))]),
            );
        }
        state.patch_globals(&patch, &BTreeSet::new()).await?;
        Ok(Self {
            services: CellServices {
                events: Arc::default(),
                workers,
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
                code_renderer: crate::render::CodeRendererSlot::default(),
                prompts: dialect.prompts(),
                packages: None,
                kernel: KernelCarry::default(),
            },
            state,
            binding_count,
            payload_bytes,
        })
    }

    pub async fn capture(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionGraphCapture, lash_core::SessionError> {
        self.state
            .capture_execution_graph(lash_core::plugin::ExecutionGraphPurpose::Final)
            .await
    }

    /// Commits `commit`, a head commit of `publisher`'s session, as the
    /// turn's head commit does: the descriptor of the execution it names
    /// is staged beside it, the pages the pending capture staged under that
    /// root travel as the commit's candidate, owned by the session, and the
    /// capture is acknowledged. Answers the pages the commit published.
    ///
    /// # Errors
    ///
    /// The commit's encoding, the candidate's or the store's refusal.
    pub async fn commit(
        &mut self,
        publisher: &mut CodeModeCheckpointPerfPublisher,
        commit: &lash_core::RuntimeCommit,
    ) -> Result<Vec<EncodedNode>, lash_core::SessionError> {
        let failed = |error: lash_core::durable_port::DurableError| {
            lash_core::SessionError::Protocol(format!("checkpoint perf commit: {error}"))
        };
        let encoded = lash_core::store::encode_session_commit(commit).map_err(|error| {
            lash_core::SessionError::Protocol(format!("checkpoint perf commit: {error}"))
        })?;
        let mut tx = publisher.context.begin().await.map_err(failed)?;
        match encoded.descriptor {
            Some(descriptor) => {
                tx.stage([descriptor]);
            }
            None => {
                if let Some(root) = encoded.execution {
                    tx.require(root);
                }
            }
        }
        tx.write(DomainWrite::SessionCommit(SessionCommitWrite {
            session: commit.session_id.clone(),
            expected_head: commit.expected_head_revision,
            commit_json: encoded.json,
            execution: encoded.execution,
        }));
        let label = CommitLabel::new("perf.head");
        let candidate = match encoded.execution {
            Some(root) => {
                self.state
                    .execution_candidate(root, CommitId::mint(), &mut tx)
                    .await?
            }
            None => None,
        };
        let mut published = Vec::new();
        match candidate {
            Some(mut candidate) => {
                let mut after = None;
                loop {
                    let entries =
                        candidate
                            .manifest
                            .entries(after, MANIFEST_BATCH)
                            .map_err(|fault| {
                                lash_core::SessionError::Protocol(format!(
                                    "checkpoint perf manifest: {fault:?}"
                                ))
                            })?;
                    let Some(last) = entries.last() else {
                        break;
                    };
                    after = Some(match last {
                        ManifestEntry::External(id) => *id,
                        ManifestEntry::Fresh(node) => node.id,
                    });
                    published.extend(entries.into_iter().filter_map(|entry| match entry {
                        ManifestEntry::Fresh(node) => Some(node.into_node()),
                        ManifestEntry::External(_) => None,
                    }));
                }
                publisher
                    .context
                    .commit_candidate_owing(
                        tx,
                        CandidatePublication {
                            owner: CandidateOwner::Session(commit.session_id.clone()),
                            manifest: candidate.manifest,
                        },
                        label,
                        None,
                    )
                    .await
                    .map_err(failed)?;
            }
            None => {
                publisher.context.commit(tx, label).await.map_err(failed)?;
            }
        }
        self.state.acknowledge_execution_state_capture().await;
        Ok(published)
    }

    /// Reads the session's committed state, from the next cell on, under
    /// the head revision `revision` of `session` that `publisher` committed,
    /// whose root is `checkpoint`, as a reopened session's reads are
    /// protected.
    ///
    /// # Errors
    ///
    /// The backend serves no page store.
    pub fn read_head(
        &mut self,
        publisher: &CodeModeCheckpointPerfPublisher,
        session: &lash_vocabulary::identity::SessionId,
        revision: u64,
        checkpoint: lash_core_store::checkpoint_graph::NodeId,
    ) -> Result<(), lash_core::SessionError> {
        let store = publisher.context.pages().map_err(|error| {
            lash_core::SessionError::Protocol(format!("checkpoint perf pages: {error}"))
        })?;
        self.state
            .note_pages(Some(&lash_core::plugin::SessionPages {
                store,
                actor: publisher.actor.clone(),
                epoch: publisher.context.epoch(),
                owner: lash_core::durable_port::domain::graph::RootOwner::SessionRevision {
                    session: session.clone(),
                    revision,
                },
                root: checkpoint,
            }));
        Ok(())
    }

    /// Run the edit under the backend's scoped controller and this cell's
    /// invocation: one binding is rebound per turn, keeping its large text
    /// and adding a text an eighth its size.
    pub async fn assign_one(
        &mut self,
        index: usize,
        turn: usize,
        ctx: RuntimeExecutionContext<'_>,
    ) -> Result<(), lash_core::SessionError> {
        let binding = index % self.binding_count.max(1);
        let code = format!(
            "mid_{binding} = [mid_{binding}[0], \"turn-{turn}-{}\"];",
            "y".repeat(self.payload_bytes / 8)
        );
        let response = execute_cell(
            &mut self.state,
            ctx,
            ExecRequest { code },
            &self.services,
            CodeModeProjectedBindings::default(),
        )
        .await;
        self.state.accept_code_execution();
        if let Some(error) = response.error() {
            return Err(lash_core::SessionError::Protocol(format!(
                "code mode checkpoint perf assignment failed: {}",
                error.message,
            )));
        }
        Ok(())
    }

    pub async fn restore(
        dialect: &crate::CellDialect,
        state: &lash_core::plugin::HydratedExecutionGraph,
    ) -> Result<(), lash_core::SessionError> {
        let mut restored = CodeModeExecutionState::new(dialect.name(), dialect.numbers())
            .staging(lash_vm_client::service::Service::default().spools());
        restored
            .restore_execution_graph(state, lash_core::plugin::ExecutionGraphPurpose::Final)
            .map_err(lash_core::SessionError::from)
    }
}

/// `bytes` of letters no two bindings share and no page of one repeats:
/// each binding's value is published whole, never deduplicated against
/// another's pages.
fn payload(binding: usize, bytes: usize) -> String {
    let mut state = (binding as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..bytes)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

/// A session actor, claimed by one node over a backend's store, whose
/// commits publish a [`CodeModeCheckpointPerfFixture`]'s captures with the
/// session's head commits.
pub struct CodeModeCheckpointPerfPublisher {
    context: lash_core::ActorContext,
    actor: ActorKey,
}

impl CodeModeCheckpointPerfPublisher {
    /// Creates `session`'s actor on `backend` and claims it.
    ///
    /// # Errors
    ///
    /// The store's refusal to create, register or claim.
    pub async fn open(
        backend: &lash_execution_contracts::backend::Backend,
        session: &lash_vocabulary::identity::SessionId,
    ) -> Result<Self, lash_core::SessionError> {
        let failed = |error: lash_core::durable_port::DurableError| {
            lash_core::SessionError::Protocol(format!("checkpoint perf publisher: {error}"))
        };
        let actor = ActorKey::session(session.as_str()).map_err(|error| {
            lash_core::SessionError::Protocol(format!("checkpoint perf publisher: {error}"))
        })?;
        let mut created = MailTx::new();
        created.create_actor(actor.clone(), FormatSet::new(FORMATS));
        backend
            .durable()
            .commit_mail(created, CommitLabel::new("perf.create"))
            .await
            .map_err(failed)?;
        let lease = backend
            .durable()
            .register_node(&NodeSpec {
                node: NodeId::new(format!("rlm-checkpoint-perf-{session}")),
                decodes: vec![FormatSet::new(FORMATS)],
                serves: Vec::new(),
                capabilities: Default::default(),
                ttl_millis: 600_000,
            })
            .await
            .map_err(failed)?;
        let epoch = backend
            .durable()
            .claim(&lease, 16)
            .await
            .map_err(failed)?
            .into_iter()
            .find(|claimed| claimed.actor == actor)
            .map(|claimed| claimed.epoch)
            .ok_or_else(|| {
                lash_core::SessionError::Protocol(
                    "checkpoint perf publisher: the session actor is not claimable".to_owned(),
                )
            })?;
        let context = lash_core::ActorContext::new(
            backend.clone(),
            actor.clone(),
            epoch,
            lash_core::AdmittedScope::turn(
                session.clone(),
                lash_vocabulary::identity::TurnId::from("rlm-checkpoint-perf"),
            ),
            lash_core::CancellationToken::new(),
            Arc::new(lash_core::durable_port::NoProbe),
        );
        Ok(Self { context, actor })
    }

    /// How many nodes the head revision `revision` of `session` reaches,
    /// from its root `checkpoint` down through the session's heap: what a
    /// whole read of the session would read, read under one lease.
    ///
    /// # Errors
    ///
    /// The store's refusal, or a node that does not decode.
    pub async fn reachable(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        revision: u64,
        checkpoint: lash_core_store::checkpoint_graph::NodeId,
    ) -> Result<u64, lash_core::SessionError> {
        let failed = |error: lash_core::durable_port::DurableError| {
            lash_core::SessionError::Protocol(format!("checkpoint perf reachable: {error}"))
        };
        let store = self.context.pages().map_err(failed)?;
        let lease = store
            .acquire_root_lease(lash_core::durable_port::domain::pages::RootLeaseRequest {
                owner: lash_core::durable_port::domain::graph::RootOwner::SessionRevision {
                    session: session.clone(),
                    revision,
                },
                expected: checkpoint,
                fence: lash_core::durable_port::domain::pages::LeaseFence::Actor {
                    actor: self.actor.clone(),
                    epoch: self.context.epoch(),
                },
                ttl_millis: 600_000,
            })
            .await
            .map_err(failed)?;
        let mut seen = BTreeSet::from([checkpoint]);
        let mut frontier = vec![checkpoint];
        while !frontier.is_empty() {
            let batch: Vec<_> = frontier
                .drain(..frontier.len().min(MANIFEST_BATCH))
                .collect();
            for page in store.read_pages(&lease, &batch).await.map_err(failed)? {
                let node = EncodedNode {
                    id: page.id,
                    bytes: page.bytes,
                };
                let children = node.children().map_err(|refusal| {
                    lash_core::SessionError::Protocol(format!(
                        "checkpoint perf reachable: {refusal}"
                    ))
                })?;
                frontier.extend(children.into_iter().filter(|child| seen.insert(*child)));
            }
        }
        let _ = store.release_root_lease(lease.id).await;
        Ok(seen.len() as u64)
    }
}
