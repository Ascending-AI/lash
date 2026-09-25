//! The replay run of one code cell (FIG-3586): the identities it mints, its
//! issue-ordinal mint and recorded frontier, and the seal it journals as its
//! last nested effect.
//!
//! A cell replays by re-execution (ADR 0103), so every nested effect it
//! issues is served from the journal on a redrive only if the redrive reaches
//! the same key. The key is the command's issue ordinal under the cell's own
//! replay key — never the call site that issued it — and the run refuses,
//! with zero dispatch, any command the journal does not hold where the
//! redrive issued it.

use lash_core::RuntimeExecutionContext;
use lash_lashlang_runtime::{LashlangHostIdentities, LashlangReplayRun, LashlangRunOrdinals};
use lash_sansio::sync::MutexExt;

/// Why a cell has no logical opener to mint identities under.
///
/// All arms are unreachable from the production entry: the turn driver always
/// installs the code-execution effect as the parent invocation
/// (`crates/lash-core/src/runtime/turn_driver/effects.rs`), and every scope a
/// session turn may run under is an opener — `Turn`, `QueueDrain`, or the
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
        let run = LashlangReplayRun::new(identities.namespace(), LashlangRunOrdinals::start());
        Ok(Self {
            identities,
            run,
            module_ref: std::sync::Mutex::new(None),
        })
    }

    pub(super) fn identities(&self) -> &LashlangHostIdentities {
        &self.identities
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

    /// Journals the cell's seal as its last nested effect.
    ///
    /// Written whenever the executor returns a response — a setup failure
    /// included — and never after a controller abort. A redrive of a
    /// completed cell must meet the same seal, which refuses a run that ended
    /// early, one that issued a different number of commands, and one that no
    /// longer writes a command the journal holds as written.
    pub(super) async fn seal(
        &self,
        ctx: &RuntimeExecutionContext<'_>,
        cancellation: &lash_lashlang_runtime::ExecutionCancellation,
    ) {
        self.commands(ctx, cancellation).seal(self.producer()).await;
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
