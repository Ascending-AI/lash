use lash_execution_contracts::state_commands::StateCommands;
use lash_execution_contracts::tool_intent::ToolIntents;
/// A completed leaf-provider value. Unlike [`crate::ToolOutcome`], this type has
/// no deferred variant, so a completed result can be paired with intents
/// without making `Pending + intents` representable.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcomeDone(Box<DoneParts>);

/// The terminal output and the state commands returned with it, boxed
/// together so a completed value stays one pointer wide.
#[derive(Clone, Debug, PartialEq)]
struct DoneParts {
    output: crate::ToolCallOutput,
    state: StateCommands,
}

impl ToolOutcomeDone {
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self(Box::new(DoneParts {
            output,
            state: StateCommands::default(),
        }))
    }

    /// Return `commands` against the tool's owning plugin's namespace with
    /// this result (K10). They publish with the attempt's recorded outcome,
    /// and only when the call's final result, after its result checks, is a
    /// success.
    #[must_use]
    pub fn with_state(mut self, commands: StateCommands) -> Self {
        self.0.state = commands;
        self
    }

    /// The terminal output and the state commands returned with it.
    pub(crate) fn into_parts(self) -> (crate::ToolCallOutput, StateCommands) {
        let DoneParts { output, state } = *self.0;
        (output, state)
    }

    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(lash_protocol_types::ToolCallOutput::success(result))
    }

    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(lash_protocol_types::ToolCallOutput::failure(failure))
    }

    /// Recover the terminal output for protocol and process-engine implementors.
    pub fn into_output(self) -> crate::ToolCallOutput {
        self.0.output
    }
}

/// Value returned by a leaf provider body before the attempt effect records it.
/// The enum is the law: only the completed variant has an intents field.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug)]
pub enum ToolAttemptOutcome {
    /// Terminal provider output with ordered durable declarations.
    Done {
        /// Completed provider result.
        result: ToolOutcomeDone,
        /// Follow-on declarations admitted only after the attempt is recorded.
        intents: ToolIntents,
    },
    /// A host fault that aborts the attempt before it has a recorded result.
    /// Its journal disposition determines whether the engine may retry it.
    HostFailed(Box<crate::RuntimeEffectControllerError>),
    /// Deferred provider output with no representable declarations.
    Pending(crate::PendingCompletion),
}

impl ToolAttemptOutcome {
    /// Pair a completed result with its declarations for protocol and process-engine implementors.
    pub fn done(result: ToolOutcomeDone, intents: ToolIntents) -> Self {
        Self::Done { result, intents }
    }

    pub fn done_without_intents(result: ToolOutcomeDone) -> Self {
        Self::done(result, ToolIntents::default())
    }

    pub fn host_failed(error: crate::RuntimeEffectControllerError) -> Self {
        Self::HostFailed(Box::new(error))
    }

    pub fn pending(pending: crate::PendingCompletion) -> Self {
        Self::Pending(pending)
    }
}

/// Lift a plain tool outcome into an attempt outcome at the execution seam.
/// A completed outcome becomes [`ToolAttemptOutcome::Done`] with no declared
/// intents; a pending outcome stays [`ToolAttemptOutcome::Pending`] with its
/// completion payload intact. There is deliberately no conversion in the
/// other direction: projecting away `Done` intents would silently drop
/// durable declarations.
impl From<crate::ToolOutcome> for ToolAttemptOutcome {
    fn from(result: crate::ToolOutcome) -> Self {
        match result {
            crate::ToolOutcome::Done(output) => {
                Self::done_without_intents(ToolOutcomeDone::from_output(*output))
            }
            crate::ToolOutcome::Pending(pending) => Self::Pending(*pending),
        }
    }
}
