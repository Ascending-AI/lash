//! A test's turn, executed the way a host's send executes one.
//!
//! A host never executes a turn: it sends an input, and the session actor
//! admits it and executes its run (ADR 0132 §12). A kernel test that holds
//! its runtime by `&mut` accepts the input through the kernel's in-process
//! turn entry, a journaled acceptance whose transaction wakes the session
//! actor, and answers from the run that took it.
//!
//! A store-less runtime has no durable ingress to accept into, so the kernel
//! runs its input directly, as it does for any store-less runtime.

use crate::runtime::{LashRuntime, TurnOptions};
use crate::{AgentFrameRun, AssembledTurn, RuntimeError, RuntimeErrorCode, TurnInput};

/// A test's turn on a runtime it holds, executed through the kernel's
/// in-process turn entry. See the module docs.
#[async_trait::async_trait]
pub trait TestTurnExecution {
    /// Accept `input` and answer every physical turn the run that took it
    /// ran (a frame switch's follow-on turns included).
    ///
    /// The turn id is `input`'s trace id, else the scope id of the
    /// controller in `opts`; it is the accepted row's source key, so it
    /// names the run that executes the row.
    async fn execute_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError>;

    /// [`execute_turn_frames`](Self::execute_turn_frames), answering the run's
    /// terminal physical turn.
    async fn execute_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;

    /// Run `input` through the kernel's in-process turn entry, the one a
    /// child session's turn takes inside its parent's execution: a journaled
    /// acceptance step (ADR 0069 §6), answering the terminal physical turn
    /// of the run that took the accepted row. The acceptance laws exercise
    /// it.
    async fn execute_child_session_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;
}

#[async_trait::async_trait]
impl TestTurnExecution for LashRuntime {
    async fn execute_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        self.stream_turn_with_agent_frames(input, opts).await
    }

    async fn execute_child_session_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.execute_turn(input, opts).await
    }

    async fn execute_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        let run = self.execute_turn_frames(input, opts).await?;
        run.into_final_turn().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::EmptyAgentFrameRun,
                "an executed run assembled no physical turn",
            )
        })
    }
}
