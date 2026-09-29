//! The base an administrative compaction (`compact_context`) runs over
//! (FIG-4133, F3).
//!
//! A compaction summarizes the frame current when it starts and opens its
//! frame from that frame, then commits the frame on its own. Before its
//! summarizer runs, it records that base as one step: the durable head and
//! the frame. A redrive replays the step and adopts the recorded base, so
//! even after the compaction's own commit moved the head it summarizes the
//! same history, reads its summary back from the journal, derives the same
//! frame key and meets its commit's receipt. From the moved head alone a
//! redrive could not tell itself from a repeated compaction; the step tells
//! them apart: a repeated compaction is the run's next compaction, which
//! records a base of its own.

use crate::runtime::LashRuntime;
use crate::runtime::effect::CompactionBase;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode, ScopedEffectController,
};

/// The replay key of the base the run's `ordinal`th compaction records.
fn compaction_base_replay_key(ordinal: u32) -> String {
    format!("compaction-base:{ordinal}")
}

impl LashRuntime {
    /// Record the base the compaction running under `controller` summarizes,
    /// as one recorded step, and adopt it as the resident session.
    ///
    /// The first execution records the resident head and frame, and the drive
    /// fence current when it starts, which the compaction's frame commit
    /// presents as a writer beside the drive (FIG-4134). A replay presents
    /// the recorded fence, so an admission sealed since refuses it. A replay
    /// adopts the recorded head through
    /// [`load_session_at`](crate::store::SessionCommitStore::load_session_at)
    /// when the live head has moved since, and refuses a base whose frame the
    /// store does not answer with.
    pub(in crate::runtime) async fn adopt_recorded_compaction_base(
        &mut self,
        controller: &ScopedEffectController<'_>,
    ) -> Result<CompactionBase, RuntimeError> {
        let ordinal = controller.next_compaction_ordinal();
        let session_id = self.state.session_id.clone();
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                controller.execution_scope().clone(),
                compaction_base_replay_key(ordinal),
            )?,
            RuntimeAttribution::for_session(session_id.clone()),
            format!("compaction-base:{ordinal}"),
        );
        // Read on every execution, recorded by the first: a replay adopts the
        // journaled fence and never the one read here.
        let drive_fence = self.beside_drive_fence().await.map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!("the compaction's drive fence could not be read: {error}"),
            )
        })?;
        let runner = RecordCompactionBaseRunner {
            base: CompactionBase {
                head: crate::store::SessionHeadRef {
                    generation: 0,
                    revision: self.state.head_revision,
                    leaf: self.state.session_graph.leaf_node_id.clone(),
                    checkpoint: self.state.checkpoint_ref.clone(),
                },
                frame: self.state.current_frame_node_id.clone(),
                drive_fence,
            },
        };
        let base = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::RecordCompactionBase {
                        session: session_id,
                    },
                ),
                RuntimeEffectLocalExecutor::owned_runner(Box::new(runner), None),
            )
            .await
            .and_then(RuntimeEffectOutcome::into_compaction_base)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        self.adopt_admission_base(&base.head)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!("the compaction's recorded base could not be read: {error}"),
                )
            })?;
        if self.state.current_frame_node_id != base.frame {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!(
                    "the compaction's recorded base stands in frame {:?}, but the store read it \
                     in frame {:?}",
                    base.frame, self.state.current_frame_node_id
                ),
            ));
        }
        Ok(base)
    }
}

/// The first execution of one `RecordCompactionBase` step: it records the
/// base captured from the resident session. None of it enters the envelope,
/// which names only the session.
struct RecordCompactionBaseRunner {
    base: CompactionBase,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RecordCompactionBaseRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::RecordCompactionBase { .. } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "compaction-base executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        Ok(RuntimeEffectOutcome::RecordCompactionBase {
            base: Box::new(self.base),
        })
    }
}
