//! The run of one code cell: the identities it mints and the execution its
//! parked state is filed under.
//!
//! Every call the cell makes is identified by the park that admitted it and
//! its place there, inside the cell's own replay key. A cell resumes from
//! its parked state (ADR 0132 §8), whose ledger holds the admissions, and
//! never runs its saved code again.

use lash_core::RuntimeExecutionContext;
use lash_vm_broker::CodeCallIdentities;

/// Why a cell has no logical opener to mint identities under.
///
/// All arms are unreachable from the production entry: the turn driver
/// always installs the code-execution effect as the parent invocation, and
/// every scope a session turn may run under is an opener. They are refusals
/// rather than fallbacks because a fallback identity (the bare session id)
/// would mint one identity for the first call of every cell in a session.
#[derive(Debug, thiserror::Error)]
pub(super) enum CellOpenerError {
    #[error("the cell runs outside a code-execution effect, so it has no logical opener")]
    NoEffect,
    /// The effect the cell runs under names a different scope than the
    /// controller was admitted under: refused rather than resolved in
    /// either direction.
    #[error(
        "code-execution effect names scope `{address}` but this execution was admitted under `{admitted}`"
    )]
    AddressScope { address: String, admitted: String },
    #[error(transparent)]
    Scope(lash_core::EffectOpenerError),
}

/// One cell's run.
pub(super) struct CellRun {
    identities: CodeCallIdentities,
    /// The replay key of the effect that runs the cell: the namespace of
    /// every journaled step the cell records beside its parked state.
    replay_key: String,
}

impl CellRun {
    /// The run of the cell the turn driver installed as `ctx`'s parent
    /// invocation. The opener is the controller's own admitted scope; the
    /// address must name that same scope.
    pub(super) fn open(ctx: &RuntimeExecutionContext<'_>) -> Result<Self, CellOpenerError> {
        let admitted_scope = ctx.admitted_scope();
        let address = ctx
            .parent_invocation()
            .and_then(lash_core::RuntimeInvocation::effect_address)
            .ok_or(CellOpenerError::NoEffect)?;
        if address.execution_scope != *admitted_scope.scope() {
            return Err(CellOpenerError::AddressScope {
                address: address.execution_scope.id().to_string(),
                admitted: admitted_scope.scope().id().to_string(),
            });
        }
        let opener =
            lash_core::EffectOpener::for_scope(&admitted_scope).map_err(CellOpenerError::Scope)?;
        Ok(Self {
            identities: CodeCallIdentities::cell(opener, address.replay_key.clone()),
            replay_key: address.replay_key.clone(),
        })
    }

    pub(super) fn identities(&self) -> &CodeCallIdentities {
        &self.identities
    }

    /// The key of the journaled step that records the cell's outputs.
    pub(super) fn outputs_key(&self) -> String {
        format!("{}:outputs", self.replay_key)
    }
}

/// The identities a cell with no opener mints: it runs pure, under a key no
/// other cell resumes.
pub(super) fn pure_cell_identities(
    ctx: &RuntimeExecutionContext<'_>,
) -> Result<CodeCallIdentities, String> {
    lash_core::EffectOpener::for_scope(&ctx.admitted_scope())
        .map(|opener| CodeCallIdentities::cell(opener, "pure-cell"))
        .map_err(|error| error.to_string())
}

/// The execution a cell's parked state is filed under (ADR 0132 §8): its
/// opener's run, and the replay key of the effect that runs it.
///
/// # Errors
///
/// An opener whose run names no valid turn identity.
pub(super) fn cell_exec(
    ctx: &RuntimeExecutionContext<'_>,
    identities: &CodeCallIdentities,
) -> Result<lash_vm_broker::ExecKey, String> {
    let execution = identities
        .execution()
        .ok_or_else(|| "the cell has no execution identity".to_owned())?
        .to_owned();
    let (session, run) = match identities.opener().clone() {
        lash_core::EffectOpener::Turn {
            session_id,
            turn_id,
        } => (session_id, turn_id),
        lash_core::EffectOpener::SessionOperation {
            session_id,
            operation_id,
        } => (
            session_id,
            lash_core::TurnId::try_from(operation_id).map_err(|error| error.to_string())?,
        ),
        lash_core::EffectOpener::Process { process_id } => (
            ctx.session_scope()
                .map_err(|error| error.to_string())?
                .session_id,
            lash_core::TurnId::try_from(process_id.to_string())
                .map_err(|error| error.to_string())?,
        ),
    };
    Ok(lash_vm_broker::ExecKey::Cell(
        session,
        run,
        lash_vm_broker::CellId::new(execution),
    ))
}
