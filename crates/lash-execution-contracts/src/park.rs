use crate::ProcessId;
use lash_durable::DurableError;
use lash_durable::StoreFailure;
use lash_durable::StoreFailureKind;
/// Why a process actor parked: the actor row's typed reason. A parked
/// process runs no engine code until an operator redrives it; a cancel ends
/// it without running its engine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessParkReason {
    /// Its activations crashed without progress `failed_activations` times
    /// in a row: the activation-loop budget.
    ActivationLoop {
        /// The claims in a row that committed nothing.
        failed_activations: u32,
    },
    /// The claims that only end it for its pending cancel (a node that does
    /// not decode or serve it) crashed without progress
    /// `failed_activations` times in a row: the activation-loop budget
    /// (FIG-6099). The cancel stays recorded, and its mail stays pending:
    /// an operator's redrive runs it again.
    CancelLoop {
        /// The claims in a row that committed nothing.
        failed_activations: u32,
    },
    /// No engine of its kind is installed on the node that claimed it.
    UnknownEngine {
        /// The engine kind its row names.
        kind: String,
    },
    /// Its engine state is in a format this node's engine does not read.
    UndecodableState {
        /// The format kind the state was written in.
        kind: String,
        /// Its version.
        version: u32,
    },
    /// Its engine state is in an earlier build's format, and the engine
    /// refuses to carry it to its own (ADR 0106 §1). The state is as that
    /// build left it.
    MigrationRefused {
        /// The format kind the state is in.
        kind: String,
        /// Its version.
        version: u32,
        /// The kernel migration's typed refusal.
        refusal: lash_kernel_migrate::KernelMigrationRefusal,
        /// The reason in words.
        message: String,
    },
    /// Its driver row, what the actor records of its steps or of its child
    /// turn between transitions, does not decode on this node: another
    /// build wrote it. Nothing of the process ran or committed for it
    /// (FIG-5601).
    UndecodableDriver {
        /// The decoder's account.
        message: String,
    },
    /// Its engine refused a transition.
    AdvanceRefused {
        /// The engine's account.
        message: String,
    },
    /// It runs a turn in a child session, and the node that claimed it runs
    /// no session turns.
    UnservedSessionTurn,
    /// The composition its environment names does not build on the node
    /// that claimed it (FIG-6034) for a reason no env refusal types: a
    /// component's contribution refused, or its committed state did not
    /// replay. Nothing of the process ran or settled for it.
    CompositionUnavailable {
        /// Why, in words.
        message: String,
    },
    /// The node that claimed it refuses its sealed env, typed (FIG-6296): a
    /// component the env names is missing, its config is unreadable or
    /// refused, or the run depends on a component the node declares
    /// retired. Nothing of the process ran or settled for it.
    EnvRefused {
        /// The refusal.
        refusal: crate::ProcessEnvRefusal,
    },
    /// A component refused conversion of its recorded state/config format.
    ComponentFormatRefused { refusal: crate::FormatRefusal },
    /// The store refused its commits as corrupt, its terminal too.
    CommitRefused {
        /// The store's account.
        message: String,
    },
}

impl ProcessParkReason {
    /// The actor row's park encoding.
    #[expect(
        clippy::expect_used,
        reason = "a park reason is plain data whose encoding cannot fail"
    )]
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a park reason encodes")
    }
}

/// Why `process`'s actor is parked, read from its actor row; `None` while
/// it is not parked, or once it has no actor.
///
/// # Errors
///
/// The store's refusal, or a stored reason this build does not decode.
pub async fn park_of(
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
) -> Result<Option<ProcessParkReason>, DurableError> {
    let corrupt = |message: String| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message,
        })
    };
    let actor = lash_durable::ActorKey::process(process.as_str())
        .map_err(|error| corrupt(error.to_string()))?;
    let Some(park) = store.actor(&actor).await?.and_then(|actor| actor.park) else {
        return Ok(None);
    };
    serde_json::from_str(&park).map(Some).map_err(|error| {
        corrupt(format!(
            "the park of process {process} does not decode: {error}"
        ))
    })
}

impl ProcessParkReason {
    /// The reason's code: the `lash.parked_work.reason` of its park.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ActivationLoop { .. } => "activation_loop",
            Self::CancelLoop { .. } => "cancel_loop",
            Self::UnknownEngine { .. } => "unknown_engine",
            Self::UndecodableState { .. } => "undecodable_state",
            Self::MigrationRefused { .. } => "migration_refused",
            Self::UndecodableDriver { .. } => "undecodable_driver",
            Self::AdvanceRefused { .. } => "advance_refused",
            Self::UnservedSessionTurn => "unserved_session_turn",
            Self::CommitRefused { .. } => "commit_refused",
            Self::CompositionUnavailable { .. } => "composition_unavailable",
            Self::EnvRefused { .. } => "env_refused",
            Self::ComponentFormatRefused { .. } => "component_format_refused",
        }
    }
}
