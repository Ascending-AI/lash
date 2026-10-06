//! The run of one code cell (FIG-3586): the identities it mints and its
//! issue-ordinal mint, and the execution its snapshot is filed under.
//!
//! Every command the cell issues is keyed by its issue ordinal under the
//! cell's own replay key, never the call site that issued it. A cell resumes
//! from its snapshot (ADR 0132 §8), with the ordinals its last quiet point
//! committed, and never runs its earlier code again.

use lash_core::RuntimeExecutionContext;
use lash_lashlang_runtime::{LashlangHostIdentities, LashlangReplayRun, LashlangRunOrdinals};
use lash_sansio::sync::MutexExt;

/// Why a cell has no logical opener to mint identities under.
///
/// All arms are unreachable from the production entry: the turn driver always
/// installs the code-execution effect as the parent invocation
/// (`crates/lash-core/src/runtime/turn_driver/effects.rs`), and every scope a
/// session turn may run under is an opener — `Turn`, or the
/// `Process` scope a `ProcessInput::SessionTurn` row runs its child turn
/// under, whose admitted incarnation the controller's admitted scope already
/// carries. They are refusals rather than fallbacks because the fallback
/// that used to stand here — the bare session id — minted one identity for the
/// first call of every cell in a session.
#[derive(Debug, thiserror::Error)]
pub(super) enum LashlangCellOpener {
    #[error("lashlang cell runs outside a code-execution effect, so it has no logical opener")]
    NoEffect,
    /// The effect the cell runs under names a different scope than the
    /// controller was admitted under — a claim/opener disagreement, refused
    /// rather than resolved in either direction.
    #[error(
        "code-execution effect names scope `{address}` but this execution was admitted under `{admitted}`"
    )]
    AddressScope {
        /// The scope the installed effect address claims.
        address: String,
        /// The scope the controller's checked admitted pair carries.
        admitted: String,
    },
    #[error(transparent)]
    Scope(lash_core::EffectOpenerError),
}

/// One cell's replay run.
pub(super) struct CellRun {
    identities: LashlangHostIdentities,
    run: LashlangReplayRun,
    /// The linked module this cell ran, once compiled: the seal records it
    /// so a divergence can say whether the journal came from the same
    /// program.
    module_ref: std::sync::Mutex<Option<String>>,
}

impl CellRun {
    /// The run of the cell the turn driver installed as `ctx`'s parent
    /// invocation. The opener is the controller's own admitted scope — the
    /// checked pair it already carries — not a pair re-paired here from a
    /// scope claim and a separately read pin. The address must name that same
    /// scope; a disagreement is refused rather than resolved.
    pub(super) fn open(ctx: &RuntimeExecutionContext<'_>) -> Result<Self, LashlangCellOpener> {
        Self::open_at(ctx, LashlangRunOrdinals::start())
    }

    /// Open a cell under its admitted physical invocation.
    pub(super) fn open_at(
        ctx: &RuntimeExecutionContext<'_>,
        ordinals: LashlangRunOrdinals,
    ) -> Result<Self, LashlangCellOpener> {
        let admitted_scope = ctx.admitted_scope();
        let address = ctx
            .parent_invocation()
            .and_then(lash_core::RuntimeInvocation::effect_address)
            .ok_or(LashlangCellOpener::NoEffect)?;
        if address.execution_scope != *admitted_scope.scope() {
            return Err(LashlangCellOpener::AddressScope {
                address: address.execution_scope.id().to_string(),
                admitted: admitted_scope.scope().id().to_string(),
            });
        }
        let opener = lash_core::EffectOpener::for_scope(&admitted_scope)
            .map_err(LashlangCellOpener::Scope)?;
        let identities = LashlangHostIdentities::cell(opener, address.replay_key.clone());
        let run = LashlangReplayRun::new(identities.namespace(), ordinals);
        Ok(Self {
            identities,
            run,
            module_ref: std::sync::Mutex::new(None),
        })
    }

    pub(super) fn identities(&self) -> &LashlangHostIdentities {
        &self.identities
    }

    /// The ordinal state a segment boundary inside the cell hands over.
    pub(super) fn ordinals(&self) -> LashlangRunOrdinals {
        self.run.ordinals()
    }

    /// Records the linked module the cell ran, for the seal's attribution.
    pub(super) fn ran_module(&self, module_ref: String) {
        *self.module_ref.lock_recover() = Some(module_ref);
    }

    /// Who is running this cell: the compiler, VM ABI and module the seal
    /// names. Attribution only; nothing compares it.
    pub(super) fn producer(&self) -> serde_json::Value {
        serde_json::json!({
            "compiler": lashlang::LASHLANG_COMPILER_VERSION,
            "vm_abi": lashlang::LASHLANG_VM_ABI_VERSION,
            "module_ref": self.module_ref.lock_recover().clone(),
        })
    }

    /// The command protocol for this run over `ctx`.
    pub(super) fn commands<'a, 'run>(
        &'a self,
        ctx: &'a RuntimeExecutionContext<'run>,
        cancellation: &'a lash_lashlang_runtime::ExecutionCancellation,
    ) -> lash_lashlang_runtime::ReplayCommands<'a, 'run> {
        lash_lashlang_runtime::ReplayCommands {
            run: &self.run,
            ctx,
            cancellation,
            producer: self.producer().to_string(),
        }
    }
}

/// The nested error a cell's setup effect met — its link-scoped deferred
/// resolution, journaled before any command. A replay mismatch there is the
/// cell's own divergence: the redrive links a different surface than the run
/// that wrote the journal, so it refuses with the run's typed code and
/// attribution and the turn parks, like a mismatch at any command.
pub(super) fn setup_effect_error(
    cell: &Result<CellRun, LashlangCellOpener>,
    error: lash_core::RuntimeEffectControllerError,
) -> lash_core::RuntimeEffectControllerError {
    match cell {
        Ok(cell) => lash_lashlang_runtime::retype_replay_mismatch(
            error,
            "the cell's deferred resolution",
            &cell.run.attribution(cell.producer().to_string()),
        ),
        Err(_) => error,
    }
}

/// The execution a cell's snapshot is filed under (ADR 0132 §8): its
/// opener's run, and the replay key of the effect that runs it. A cell with
/// no opener runs pure, under a key no other cell resumes.
///
/// # Errors
///
/// An opener whose run names no valid turn identity.
pub(super) fn cell_exec(
    ctx: &RuntimeExecutionContext<'_>,
    cell: &Result<CellRun, LashlangCellOpener>,
) -> Result<lash_vm_broker::ExecKey, String> {
    let (opener, execution) = match cell {
        Ok(cell) => (
            cell.identities.opener().clone(),
            cell.identities
                .code()
                .execution()
                .ok_or_else(|| "the cell has no execution identity".to_owned())?
                .to_owned(),
        ),
        Err(_) => (
            lash_core::EffectOpener::for_scope(&ctx.admitted_scope())
                .map_err(|error| error.to_string())?,
            "pure-cell".to_owned(),
        ),
    };
    let (session, run) = match opener {
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
