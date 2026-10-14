//! The closed inventory of root sources (FIG-5887 §7).
//!
//! The root catalog is a view of the actual owner fields below, never a
//! separately maintained count. A publication API admits only these
//! sources, and a peel keeps every node one of them names.
//! A new graph-reference column is registered here, with its lifetime,
//! before anything writes it.

/// One source of graph roots: an owning column and its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum RootSource {
    /// `exec_snapshots.root_ref`: active cells and processes and unconsumed
    /// ends; retired only with the consuming owner or terminal policy.
    Exec = 0,
    /// `run_records.record_ref`: admit, start, wait, retry, decide, present,
    /// outcome and cell-report facts until safely pruned.
    RunFact = 1,
    /// `effect_admissions.admission_ref`: waits and released unsettled
    /// bodies, until no live kernel or released member needs the lookup.
    EffectAdmission = 2,
    /// `turn_phases.checkpoint_ref`: an unfinished phase and the report
    /// views and turn content beneath it.
    TurnPhase = 3,
    /// `turn_phases.working_execution_ref`: post-cell bindings and
    /// functions until the next successful cell or the final session
    /// transfer.
    WorkingExecution = 4,
    /// `turn_namespaces.graph_root_ref`: run namespace values.
    TurnNamespace = 5,
    /// `session_revisions.checkpoint_ref`: every retained revision, not
    /// only the head.
    SessionRevision = 6,
    /// `session_meta.admission_base_checkpoint_ref`: the base pinned for
    /// admission, fork and creation ownership included.
    SessionAdmissionBase = 7,
    /// SQLite `artifact_refs.blob_ref`; PostgreSQL
    /// `lash_lash_vm_artifacts.graph_ref` where `storage_kind = 1`.
    ArtifactBridge = 9,
    /// `graph_release_pins.root_ref`: installed runnable manifests.
    ReleasePin = 10,
    /// `session_runs.terminal_window_checkpoint_ref`: the checkpoint of the
    /// window a run's terminal names, while the run's row lives (FIG-5931).
    RunTerminalWindow = 11,
    /// Configuration named by a retained revision.
    RevisionConfig = 12,
    /// Configuration named by a retained terminal window.
    TerminalConfig = 13,
    /// `root_leases.root_ref`: the exact committed root an activation reads
    /// lazily, while its lease row stands (released, or reaped by a
    /// retirement step once expired).
    ReadLease = 14,
    /// `retirement_holds.root_ref`: a retired root, or the frontier of one,
    /// that a bounded retirement step has not peeled yet (FIG-6191).
    RetirementHold = 15,
}

/// Where one source's roots live in a backend's schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootField {
    pub table: &'static str,
    pub column: &'static str,
    /// The row filter the catalog view applies, when it applies one.
    pub filter: Option<&'static str>,
}

impl RootSource {
    /// Every source, in catalog order.
    pub const ALL: [Self; 15] = [
        Self::Exec,
        Self::RunFact,
        Self::EffectAdmission,
        Self::TurnPhase,
        Self::WorkingExecution,
        Self::TurnNamespace,
        Self::SessionRevision,
        Self::SessionAdmissionBase,
        Self::ArtifactBridge,
        Self::ReleasePin,
        Self::RunTerminalWindow,
        Self::RevisionConfig,
        Self::TerminalConfig,
        Self::ReadLease,
        Self::RetirementHold,
    ];

    /// The `source` column of the root catalog view.
    #[must_use]
    pub const fn catalog_name(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::RunFact => "run_fact",
            Self::EffectAdmission => "admission",
            Self::TurnPhase => "phase",
            Self::WorkingExecution => "working",
            Self::TurnNamespace => "namespace",
            Self::SessionRevision => "revision",
            Self::SessionAdmissionBase => "admission_base",
            Self::ArtifactBridge => "artifact",
            Self::ReleasePin => "release",
            Self::RunTerminalWindow => "terminal_window",
            Self::RevisionConfig => "revision_config",
            Self::TerminalConfig => "terminal_config",
            Self::ReadLease => "lease",
            Self::RetirementHold => "retirement",
        }
    }

    /// The source a catalog row names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|source| source.catalog_name() == name)
    }

    /// The owning field in the SQLite baseline schema.
    #[must_use]
    pub const fn sqlite_field(self) -> RootField {
        let (table, column, filter) = self.unprefixed();
        RootField::of(table, column, filter)
    }

    /// The owning field in the PostgreSQL baseline schema.
    #[must_use]
    pub const fn postgres_field(self) -> RootField {
        match self {
            Self::RevisionConfig => RootField::of("lash_session_revisions", "config_ref", None),
            Self::TerminalConfig => {
                RootField::of("lash_session_runs", "terminal_window_config_ref", None)
            }
            Self::Exec => RootField::of("lash_exec_snapshots", "root_ref", None),
            Self::RunFact => RootField::of("lash_run_records", "record_ref", None),
            Self::EffectAdmission => RootField::of("lash_effect_admissions", "admission_ref", None),
            Self::TurnPhase => RootField::of("lash_turn_phases", "checkpoint_ref", None),
            Self::WorkingExecution => {
                RootField::of("lash_turn_phases", "working_execution_ref", None)
            }
            Self::TurnNamespace => RootField::of("lash_turn_namespaces", "graph_root_ref", None),
            Self::SessionRevision => {
                RootField::of("lash_session_revisions", "checkpoint_ref", None)
            }
            Self::SessionAdmissionBase => {
                RootField::of("lash_session_meta", "admission_base_checkpoint_ref", None)
            }
            Self::ArtifactBridge => RootField::of(
                "lash_lash_vm_artifacts",
                "graph_ref",
                Some("storage_kind = 1"),
            ),
            Self::ReleasePin => RootField::of("lash_graph_release_pins", "root_ref", None),
            Self::RunTerminalWindow => {
                RootField::of("lash_session_runs", "terminal_window_checkpoint_ref", None)
            }
            Self::ReadLease => RootField::of("lash_root_leases", "root_ref", None),
            Self::RetirementHold => RootField::of("lash_retirement_holds", "root_ref", None),
        }
    }

    const fn unprefixed(self) -> (&'static str, &'static str, Option<&'static str>) {
        match self {
            Self::RevisionConfig => ("session_revisions", "config_ref", None),
            Self::TerminalConfig => ("session_runs", "terminal_window_config_ref", None),
            Self::Exec => ("exec_snapshots", "root_ref", None),
            Self::RunFact => ("run_records", "record_ref", None),
            Self::EffectAdmission => ("effect_admissions", "admission_ref", None),
            Self::TurnPhase => ("turn_phases", "checkpoint_ref", None),
            Self::WorkingExecution => ("turn_phases", "working_execution_ref", None),
            Self::TurnNamespace => ("turn_namespaces", "graph_root_ref", None),
            Self::SessionRevision => ("session_revisions", "checkpoint_ref", None),
            Self::SessionAdmissionBase => ("session_meta", "admission_base_checkpoint_ref", None),
            Self::ArtifactBridge => ("artifact_refs", "blob_ref", None),
            Self::ReleasePin => ("graph_release_pins", "root_ref", None),
            Self::RunTerminalWindow => ("session_runs", "terminal_window_checkpoint_ref", None),
            Self::ReadLease => ("root_leases", "root_ref", None),
            Self::RetirementHold => ("retirement_holds", "root_ref", None),
        }
    }
}

impl RootField {
    const fn of(table: &'static str, column: &'static str, filter: Option<&'static str>) -> Self {
        Self {
            table,
            column,
            filter,
        }
    }
}
