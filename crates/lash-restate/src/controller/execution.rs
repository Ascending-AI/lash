//! Classifying a runtime effect for the Restate journal.
//!
//! One responsibility: map an effect's command onto the execution shape the
//! Restate journal supports — a direct process call, a recorded `ctx.run`, a
//! durable timer, a durable-wait call — and name the journaled record a replay
//! must match.

use lash_core::{
    AwaitEventKey, ProcessCommand, RuntimeEffectCommand, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectOutcome,
    RuntimeErrorCode, SleepSpec, facade_support::CanonicalRuntimeEffectEnvelope,
    facade_support::refuse_unhonored_group_membership,
    facade_support::validate_replayed_effect_envelope,
};
use restate_sdk::errors::TerminalError;

use super::effect_journal::RecordedRuntimeEffect;
use super::journaled_effect::EngineFaults;

#[derive(Debug)]
pub(crate) enum RestateEffectExecution {
    DirectProcess {
        invocation: RuntimeEffectInvocation,
        command: Box<ProcessCommand>,
    },
    DurableProcessCommand {
        invocation: RuntimeEffectInvocation,
        command: Box<ProcessCommand>,
    },
    DirectLocal {
        envelope: RuntimeEffectEnvelope,
    },
    Timer {
        invocation: RuntimeEffectInvocation,
        /// The journaled sleep intent. The Restate SDK wait duration is derived
        /// from it against the injected clock at execution time, so an absolute
        /// deadline keeps its envelope identity across redrive (FIG-2968).
        spec: SleepSpec,
    },
    AwaitEvent {
        invocation: RuntimeEffectInvocation,
        key: AwaitEventKey,
    },
    PeekAwaitEvent {
        invocation: RuntimeEffectInvocation,
        key: AwaitEventKey,
    },
    JournaledRun {
        envelope: RuntimeEffectEnvelope,
        engine_faults: EngineFaults,
    },
}

impl RestateEffectExecution {
    pub(super) fn invocation(&self) -> &RuntimeEffectInvocation {
        match self {
            Self::DirectProcess { invocation, .. }
            | Self::DurableProcessCommand { invocation, .. }
            | Self::Timer { invocation, .. }
            | Self::AwaitEvent { invocation, .. }
            | Self::PeekAwaitEvent { invocation, .. } => invocation,
            Self::DirectLocal { envelope } | Self::JournaledRun { envelope, .. } => {
                &envelope.invocation
            }
        }
    }
}

/// Selects the Restate journal-command mapping for a Lash runtime effect.
///
/// # RT0016 journal-mismatch label warning
///
/// Restate SDK 0.10.0 reports command-type mismatches with the two human
/// labels swapped. `service_protocol/encoding.rs:96` constructs
/// `CommandTypeMismatchError` with `actual` equal to the journal entry popped
/// from `commands` and `expected` equal to the command the current execution
/// wants. `vm/errors.rs:188-200` then displays `expected` as what the previous
/// execution recorded and `actual` as what the current execution attempts.
///
/// Therefore, when reading RT0016, the line labelled "previous execution ran
/// and recorded" is actually this attempt's intended command, while "current
/// execution attempts" is actually the journal's recorded command. The
/// trailing `Command: ...[command index N]` metadata is captured from
/// `journal.last_command_metadata()` after `journal.transition(&expected)`, so
/// it also names the command this attempt was trying to write, not the
/// journal's contents. FIG-790 was the incident that exposed this SDK
/// diagnostic inversion.
///
/// Fallible because four of its arms rebuild the envelope into a target with no
/// slot for [`EffectGroupMembership`] — `Timer`, `AwaitEvent`, `PeekAwaitEvent`,
/// and both `Process` arms record no canonical envelope at all, so on this tier
/// those commands have no envelope-hash fence to fold a wake rule into. A grouped
/// child reaching them is refused rather than silently stripped of its
/// membership. Worth naming for the Restate layer: `Sleep` and `AwaitEvent` are
/// exactly the two children of the design's deadline/signal select, so this is
/// the refusal that layer must convert into real child invocations.
pub(crate) fn restate_effect_execution(
    envelope: RuntimeEffectEnvelope,
) -> Result<RestateEffectExecution, RuntimeEffectControllerError> {
    let RuntimeEffectEnvelope {
        invocation,
        command,
        group,
    } = envelope;
    Ok(match command {
        RuntimeEffectCommand::Process { command }
            if matches!(command.as_ref(), ProcessCommand::Signal { .. }) =>
        {
            refuse_unhonored_group_membership(group.as_deref(), "restate durable process command")?;
            RestateEffectExecution::DurableProcessCommand {
                invocation,
                command,
            }
        }
        RuntimeEffectCommand::Process { command } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate direct process")?;
            RestateEffectExecution::DirectProcess {
                invocation,
                command,
            }
        }
        // ADR 0103: the one command that replays by re-execution
        // (`RuntimeEffectCommand::replays_by_reexecution`) is never recorded;
        // the direct local call re-runs it on every replay, and the nested
        // effects it issues journal under their own names.
        command @ RuntimeEffectCommand::ExecCode { .. } => RestateEffectExecution::DirectLocal {
            envelope: RuntimeEffectEnvelope {
                invocation,
                command,
                group,
            },
        },
        // Deliberately not `JournaledRun`, and unreachable on the group path.
        // A tool invocation is ADR 0099 §2's handler-level driver: retry,
        // completion-key derivation and the deferred await are coordination,
        // and §2 forbids coordination inside a recorded body ("A recorded body
        // must not emit commands into an ordinal-addressed journal"). The
        // `EffectGroupDispatch::child` handler resolves the `ToolChildDriver`
        // and drives it at handler level with a ctx-bound admitted controller;
        // the driver's own atomic effects arrive here individually. This arm
        // remains the guard for any path that tries to execute the command
        // itself as one recorded step.
        RuntimeEffectCommand::ToolInvocation { .. } => {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "a tool invocation is a handler-level driver, not an atomic effect; the \
                 effect-group child handler drives it, so reaching this controller arm means \
                 coordination was routed into a recorded body, which ADR 0099 section 2 forbids",
            ));
        }
        RuntimeEffectCommand::Sleep { spec } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate timer")?;
            RestateEffectExecution::Timer { invocation, spec }
        }
        RuntimeEffectCommand::AwaitEvent { key } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate await event")?;
            RestateEffectExecution::AwaitEvent { invocation, key }
        }
        RuntimeEffectCommand::PeekAwaitEvent { key } => {
            refuse_unhonored_group_membership(group.as_deref(), "restate peek await event")?;
            RestateEffectExecution::PeekAwaitEvent { invocation, key }
        }
        command @ (RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::LanguageRuntimeValue { .. }
        | RuntimeEffectCommand::AcceptTurnInput { .. }
        | RuntimeEffectCommand::DrawRootStart { .. }
        // A root's session config: the resident config it runs under,
        // captured at the funnel; nothing it does can fault (FIG-3600 S6).
        | RuntimeEffectCommand::ResolveTurnConfig { .. }
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::IncorporateGroupSettlements { .. }
        | RuntimeEffectCommand::PresentToolResult { .. }) => RestateEffectExecution::JournaledRun {
            envelope: RuntimeEffectEnvelope {
                invocation,
                command,
                group,
            },
            engine_faults: EngineFaults::Recorded,
        },
        // Store reads and store-backed derivations: a store or session that
        // did not answer is this attempt's fault, never the step's recorded
        // outcome (FIG-3683, FIG-3726). The executor marks only live faults
        // retryable, so a deterministic outcome — the synced environment, a
        // deterministic hook failure — is journaled as ever.
        // A model call whose body lost its watch on the turn's cancellation
        // gate ends the attempt the same way (FIG-3672 P9): the watch fault is
        // never the call's recorded outcome, and never a cancellation.
        // A drive's admission and its seal read and write the session's
        // store: a store that did not answer is this attempt's fault, so the
        // step runs again, and only a verdict is ever recorded (FIG-3600).
        // A root's claim is the same: its re-admitted root would replay a
        // recorded store fault on every later drive.
        command @ (RuntimeEffectCommand::LoadExecutionEnv { .. }
        | RuntimeEffectCommand::AdmitDrive { .. }
        | RuntimeEffectCommand::SealDriveAdmission { .. }
        | RuntimeEffectCommand::ClaimAcceptedTurnInput { .. }
        | RuntimeEffectCommand::AssistantResponseHooks { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment
        | RuntimeEffectCommand::LlmCall { .. }) => RestateEffectExecution::JournaledRun {
            envelope: RuntimeEffectEnvelope {
                invocation,
                command,
                group,
            },
            engine_faults: EngineFaults::Retried,
        },
    })
}
pub(crate) fn restate_effect_name(invocation: &RuntimeEffectInvocation) -> String {
    format!("lash:{}", invocation.replay_key())
}

pub(crate) fn validate_recorded_effect_envelope(
    recorded: RecordedRuntimeEffect,
    reconstructed: &CanonicalRuntimeEffectEnvelope,
    trace: Option<&lash_core::facade_support::RuntimeEffectReplayTrace>,
) -> Result<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>, RuntimeEffectControllerError>
{
    validate_replayed_effect_envelope(
        recorded.envelope.as_ref(),
        reconstructed,
        RuntimeErrorCode::EffectReplayDivergence,
        trace,
    )?;
    Ok(recorded.outcome)
}

pub(super) fn tracing_sleep_error(invocation: &RuntimeEffectInvocation, err: &TerminalError) {
    tracing::warn!(
        session_id = invocation.attribution.session_id.as_deref().unwrap_or(""),
        effect_id = invocation.effect_id(),
        effect_kind = %RuntimeEffectKind::Sleep.as_str(),
        error = %err,
        "Restate durable sleep failed"
    );
}
