//! Backend-neutral SQL for the lash SQL stores.
//!
//! This crate owns, per table, one column list, the row types that are pure SQL
//! shape, and every statement whose text is byte-identical across backends once
//! rendered. The backend crates own their driver's row decoder and the
//! statements that genuinely fork.
//!
//! # What a table module holds
//!
//! * `TABLE` — the unprefixed table name. [`TABLES`] lists every one of them;
//!   the renderer refuses a statement naming anything else, so a typo is a
//!   startup failure rather than a query that runs against the wrong relation.
//! * one column-list constant per **named projection**, and no other column
//!   list anywhere, which is what stops the per-call-site column subsets this
//!   crate exists to delete.
//!   When the row is already a port type of the shared driver that consumes it,
//!   the table module names the column list and the port type stays where the
//!   driver defines it: one owner per fact, not two.
//! * named statements, declared with [`statements!`]. The name — `family.op` —
//!   is what tracing and store metrics report.
//!
//! # Neutral form
//!
//! Neutral SQL is written with `?N` placeholders and unprefixed, unqualified
//! table names. [`render`] rewrites both, once, at startup:
//!
//! | | placeholder | table |
//! |---|---|---|
//! | SQLite | `?1` | `main.processes` |
//! | PostgreSQL | `$1` | `lash_processes` |
//!
//! SQLite schema qualifiers are resolved from the [`TableLayout`] the dialect
//! carries. Statement sets render once per deployment layout. A table absent
//! from the layout is a startup refusal.
//!
//! Rendering runs through a tokenizer that understands string literals, quoted
//! identifiers and comments, so a `?` inside `'…'` and a `$1` inside `--` are
//! left alone, and a table name is matched as a whole token — never as a
//! substring, which is what a regex would do to `await_event_waits` inside
//! `await_event_waits_archive`.
//!
//! # The vocabulary axis
//!
//! Some predicates are neither dialect nor prose: they are *domain
//! vocabulary*, generated from an enum so that adding a lifecycle variant is
//! one edit rather than seventy-nine. A neutral statement names one as a
//! token and the renderer expands it, once, at startup, from the
//! [`Vocabulary`] the [`Dialect`] carries:
//!
//! ```text
//! SELECT COUNT(*) FROM processes WHERE {{live_process_status(status)}}
//! ```
//!
//! The expansions come from the **backend** crate, which owns the `lash-core` dependency this
//! crate does not have, so the vocabulary still has exactly one source.
//! An unknown term, a dialect with no vocabulary, or a column that is not a plain or qualified
//! identifier is a startup refusal.
//!
//! A token names domain vocabulary only. It is not a template mechanism for
//! dialect forks: a statement whose text differs between the backends is
//! still two statements, two owners and a manifest entry each.
//!
//! # Adding a table
//!
//! 1. Add `mod <table>;` under its family with `TABLE`, its column lists and a
//!    `statements!` block for the statements both backends can share verbatim.
//! 2. Add `TABLE` to [`TABLES`].
//! 3. In each backend, declare a `statements!` block for the statements that
//!    fork, and render both sets once into a `LazyLock`.
//!
//! `docs/store-sql-authoring.md` is the long form.

mod render;

pub mod artifact;
pub mod attachment;
pub mod draining_generations;
pub mod obligation;
pub mod process;
pub mod recovery_leader;
pub mod session;
pub mod session_ingress;
pub mod session_roots;
pub mod trigger;
pub mod turn_ingress;

pub use render::{
    Dialect, Placeholder, RenderError, SchemaTables, TableLayout, Vocabulary, VocabularyTerm,
    render,
};

/// Every table name this crate owns.
///
/// The renderer refuses a statement that names a table outside this list, so
/// the list is also the boundary of what neutral SQL may talk about.
pub const TABLES: &[&str] = &[
    artifact::blobs::TABLE,
    artifact::cleanup_obligations::TABLE,
    artifact::lashlang_artifacts::TABLE,
    artifact::referrer_edges::TABLE,
    artifact::referrer_fences::TABLE,
    artifact::refs::TABLE,
    attachment::blob::TABLE,
    attachment::condemnation::TABLE,
    attachment::edges::TABLE,
    attachment::pending_writes::TABLE,
    attachment::uploads::TABLE,
    attachment::sweep_clock::TABLE,
    draining_generations::TABLE,
    process::abandoned_consumer_holds::TABLE,
    process::change_clock::TABLE,
    process::definitions::TABLE,
    process::events::TABLE,
    process::observers::TABLE,
    process::parent_end_plans::TABLE,
    process::park_clock::TABLE,
    process::park_events::TABLE,
    process::processes::TABLE,
    process::segment_handovers::TABLE,
    process::tombstones::TABLE,
    process::wake_allocation_floors::TABLE,
    process::wake_deliveries::TABLE,
    process::wake_redelivery_fences::TABLE,
    recovery_leader::TABLE,
    trigger::deliveries::TABLE,
    trigger::mutation_receipts::TABLE,
    trigger::occurrences::TABLE,
    trigger::subscriptions::TABLE,
    turn_ingress::cancel_affected_inputs::TABLE,
    turn_ingress::cancel_requests::TABLE,
    turn_ingress::cancellation_bindings::TABLE,
    turn_ingress::closure_authorizations::TABLE,
    turn_ingress::pending_inputs::TABLE,
    turn_ingress::run_specs::TABLE,
    turn_ingress::queued_batches::TABLE,
    turn_ingress::queued_items::TABLE,
    turn_ingress::retired_scopes::TABLE,
    turn_ingress::tool_intent_submissions::TABLE,
    turn_ingress::turn_park_clock::TABLE,
    turn_ingress::turn_park_events::TABLE,
    turn_ingress::turn_parks::TABLE,
    session::checkpoint_blob_refs::TABLE,
    session::deleted_sessions::TABLE,
    session::fleet_format::TABLE,
    session::fork_lineage::TABLE,
    session::graph_nodes::TABLE,
    session::head::TABLE,
    session::meta::TABLE,
    session::meta_pending_observer_intents::TABLE,
    session::node_anchors::TABLE,
    session::release_stamp::TABLE,
    session::sessions::TABLE,
    session::turn_commits::TABLE,
    session::usage_deltas::TABLE,
    session::usage_delta_holes::TABLE,
    session_roots::control_intents::TABLE,
    session_roots::root_inputs::TABLE,
    session_roots::roots::TABLE,
    session_ingress::sequence::TABLE,
];

/// Every shared statement this crate owns, across every family.
///
/// The render self-test walks it for both backends, so a statement that names
/// an unowned table or misspells a placeholder fails the crate's own suite
/// rather than the store that first calls it.
#[must_use]
pub fn all_statements() -> Vec<Statement> {
    let mut statements = Vec::new();
    statements.extend_from_slice(artifact::blobs::BlobStatements::NEUTRAL);
    statements.extend_from_slice(artifact::referrer_edges::ReferrerEdgeStatements::NEUTRAL);
    statements.extend_from_slice(artifact::referrer_fences::ReferrerFenceStatements::NEUTRAL);
    statements
        .extend_from_slice(artifact::cleanup_obligations::CleanupObligationStatements::NEUTRAL);
    statements.extend_from_slice(
        artifact::cleanup_obligations::CleanupObligationLedgerStatements::NEUTRAL,
    );
    statements.extend_from_slice(attachment::edges::AttachmentEdgeStatements::NEUTRAL);
    statements.extend_from_slice(attachment::pending_writes::PendingWriteStatements::NEUTRAL);
    statements.extend_from_slice(attachment::uploads::UploadStatements::NEUTRAL);
    statements.extend_from_slice(attachment::condemnation::CondemnationStatements::NEUTRAL);
    statements.extend_from_slice(attachment::sweep_clock::SweepClockStatements::NEUTRAL);
    statements.extend_from_slice(trigger::deliveries::DeliveryStatements::NEUTRAL);
    statements.extend_from_slice(trigger::deliveries::DeliveryObligationStatements::NEUTRAL);
    statements.extend_from_slice(trigger::mutation_receipts::MutationReceiptStatements::NEUTRAL);
    statements.extend_from_slice(trigger::occurrences::OccurrenceStatements::NEUTRAL);
    statements.extend_from_slice(trigger::subscriptions::SubscriptionStatements::NEUTRAL);
    statements.extend_from_slice(process::definitions::DefinitionStatements::NEUTRAL);
    statements.extend_from_slice(process::events::EventStatements::NEUTRAL);
    statements.extend_from_slice(process::park_events::ProcessParkEventStatements::NEUTRAL);
    statements.extend_from_slice(process::observers::ObserverStatements::NEUTRAL);
    statements.extend_from_slice(process::parent_end_plans::ParentEndPlanStatements::NEUTRAL);
    statements.extend_from_slice(process::processes::ProcessStatements::NEUTRAL);
    statements.extend_from_slice(process::segment_handovers::SegmentHandoverStatements::NEUTRAL);
    statements.extend_from_slice(process::tombstones::TombstoneStatements::NEUTRAL);
    statements.extend_from_slice(session::fork_lineage::ForkLineageStatements::NEUTRAL);
    statements.extend_from_slice(session::graph_nodes::GraphNodeStatements::NEUTRAL);
    statements.extend_from_slice(session::meta::SessionMetaStatements::NEUTRAL);
    statements.extend_from_slice(
        session::meta_pending_observer_intents::ObserverIntentStatements::NEUTRAL,
    );
    statements.extend_from_slice(session::node_anchors::NodeAnchorStatements::NEUTRAL);
    statements.extend_from_slice(session::turn_commits::TurnCommitStatements::NEUTRAL);
    statements.extend_from_slice(session::usage_deltas::UsageDeltaStatements::NEUTRAL);
    statements.extend_from_slice(session::usage_delta_holes::UsageDeltaHoleStatements::NEUTRAL);
    statements.extend_from_slice(session_ingress::SessionIngressStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::roots::SessionRootStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::root_inputs::RootInputVerbStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::control_intents::ControlVerbStatements::NEUTRAL);
    statements.extend_from_slice(session::meta::MetaRootVerbStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::pending_inputs::PendingRootVerbStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::queued_batches::BatchRootVerbStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::queued_items::ItemRootVerbStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::root_inputs::SessionRootInputStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::control_intents::ControlIntentStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::TurnIngressStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::cancel_requests::CancelRequestStatements::NEUTRAL);
    statements.extend_from_slice(
        turn_ingress::cancellation_bindings::CancellationBindingStatements::NEUTRAL,
    );
    statements.extend_from_slice(
        turn_ingress::closure_authorizations::ClosureAuthorizationStatements::NEUTRAL,
    );
    statements.extend_from_slice(turn_ingress::pending_inputs::PendingInputStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::queued_batches::QueuedBatchStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::queued_items::QueuedItemStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::retired_scopes::RetiredScopeStatements::NEUTRAL);
    statements.extend_from_slice(
        turn_ingress::tool_intent_submissions::ToolIntentSubmissionStatements::NEUTRAL,
    );
    statements.extend_from_slice(turn_ingress::turn_parks::TurnParkStatements::NEUTRAL);
    statements.extend_from_slice(turn_ingress::turn_park_events::TurnParkEventStatements::NEUTRAL);
    statements.extend_from_slice(recovery_leader::RecoveryLeaderStatements::NEUTRAL);
    statements.extend_from_slice(draining_generations::DrainingGenerationStatements::NEUTRAL);
    statements.extend_from_slice(
        turn_ingress::pending_inputs::PendingTurnInputObligationStatements::NEUTRAL,
    );
    statements
        .extend_from_slice(turn_ingress::queued_batches::QueuedBatchObligationStatements::NEUTRAL);
    statements.extend_from_slice(
        session_roots::control_intents::ControlIntentObligationStatements::NEUTRAL,
    );
    statements.extend_from_slice(session_roots::roots::SessionRootObligationStatements::NEUTRAL);
    statements.extend_from_slice(session::meta::SessionMetaObligationStatements::NEUTRAL);
    statements
        .extend_from_slice(process::parent_end_plans::ParentEndPlanObligationStatements::NEUTRAL);
    statements.extend_from_slice(process::processes::ProcessObligationStatements::NEUTRAL);
    statements.extend_from_slice(session::meta::SessionMetaDeleteStatements::NEUTRAL);
    statements.extend_from_slice(session_roots::roots::SessionRootCleanupStatements::NEUTRAL);
    statements
        .extend_from_slice(process::parent_end_plans::ParentEndPlanCleanupStatements::NEUTRAL);
    statements
}

/// One named statement in neutral form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Statement {
    name: &'static str,
    neutral: &'static str,
}

impl Statement {
    /// Name a neutral statement. `name` is `family.operation`, and it is what
    /// tracing and store metrics report.
    #[must_use]
    pub const fn new(name: &'static str, neutral: &'static str) -> Self {
        Self { name, neutral }
    }

    /// The statement's reported name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The statement's neutral text.
    #[must_use]
    pub const fn neutral(&self) -> &'static str {
        self.neutral
    }

    /// # Errors
    pub fn render(&self, dialect: Dialect) -> Result<Rendered, RenderError> {
        Ok(Rendered {
            name: self.name,
            sql: render(self.neutral, dialect, TABLES)?,
        })
    }

    /// This is what [`statements!`] calls. A malformed neutral statement is a
    /// defect in this repository's own source, not a runtime condition a
    /// caller could handle, and rendering runs once at startup — so the store
    /// fails to open, naming the statement, rather than reaching a database
    /// with text nobody checked.
    ///
    /// # Panics
    ///
    /// When the neutral text does not render. See [`RenderError`].
    #[must_use]
    pub fn render_or_panic(&self, dialect: Dialect) -> Rendered {
        match self.render(dialect) {
            Ok(rendered) => rendered,
            Err(error) => panic!("neutral statement `{}` does not render: {error}", self.name),
        }
    }
}

/// One statement rendered for one backend, once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    name: &'static str,
    sql: String,
}

impl Rendered {
    /// The statement's reported name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The rendered SQL, ready to hand to the driver.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// Every statement is a single string literal in neutral form, so the set is
/// readable as SQL. The generated type has
/// one [`Rendered`] field per statement, a `NEUTRAL` inventory, and a `render`
/// constructor a backend calls exactly once.
///
/// ```ignore
/// lash_store_sql::statements! {
///     /// Statements both backends share verbatim.
///     pub struct ProcessStatements @ "processes" {
///         /// Whether a process row exists.
///         exists_by_id = "SELECT EXISTS(
///                             SELECT 1 FROM processes WHERE process_id = ?1
///                         )";
///     }
/// }
/// ```
#[macro_export]
macro_rules! statements {
    (
        $(#[$set_meta:meta])*
        $vis:vis struct $set:ident @ $family:literal {
            $(
                $(#[$statement_meta:meta])*
                $field:ident = $sql:literal;
            )*
        }
    ) => {
        $(#[$set_meta])*
        #[derive(Clone, Debug)]
        $vis struct $set {
            $(
                $(#[$statement_meta])*
                pub $field: $crate::Rendered,
            )*
        }

        impl $set {
            /// Every statement in this set, in neutral form.
            pub const NEUTRAL: &'static [$crate::Statement] = &[
                $(
                    $crate::Statement::new(
                        ::core::concat!($family, ".", ::core::stringify!($field)),
                        $sql,
                    ),
                )*
            ];

            /// # Panics
            ///
            /// Panics when a statement's neutral text is malformed — an
            /// unknown table, a bad placeholder, an unterminated literal.
            /// This runs once at startup, so the defect surfaces before the
            /// store answers anything.
            #[must_use]
            pub fn render(dialect: $crate::Dialect) -> Self {
                Self {
                    $(
                        $field: $crate::Statement::new(
                            ::core::concat!($family, ".", ::core::stringify!($field)),
                            $sql,
                        )
                        .render_or_panic(dialect),
                    )*
                }
            }
        }
    };
}
