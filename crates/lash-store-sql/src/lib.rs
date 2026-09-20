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
//!   list anywhere. `scripts/check-store-sql-ownership.py` refuses a projection
//!   of two or more columns that is not one of them, which is what stops the
//!   per-call-site column subsets this crate exists to delete.
//! * the row type, when the row is pure SQL shape (see [`wait::waits::WaitRow`]).
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
//! | SQLite | `?1` | `main.runtime_effect_replay` |
//! | PostgreSQL | `$1` | `lash_runtime_effect_replay` |
//!
//! The SQLite schema qualifier is a property of the **table**, resolved from
//! the [`TableLayout`] the dialect carries: the retention sweep reaches the
//! journal's tables through an `ATTACH`ed database while the catalog's stay in
//! `main`, and the attachment GC joins `main.attachment_manifest` to
//! `process_registry.processes`. A statement set is rendered once per
//! deployment layout rather than rebuilt with `format!` per call, and a table
//! the layout does not place is a startup refusal — which is how a statement
//! that only a connection with a process registry attached may issue fails to
//! render for the layout that has none.
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
//! renders, on both backends, to `… WHERE status IN ('running', 'waiting')`.
//! The expansions come from the **backend** crate, which owns the
//! `lash-core` dependency this crate does not have, so the vocabulary still
//! has exactly one source. An unknown term, a dialect with no vocabulary, or
//! a column that is not a plain or qualified identifier is a startup refusal.
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
//! 4. Add one `[[dialect_only]]` entry per forked statement to
//!    `crates/lash-store-sql/dialect-only.toml`, with a reason, and add the
//!    family to `converted` there. Until the family is listed the gate is
//!    silent about it; once listed it is total for it.
//!
//! `docs/store-sql-authoring.md` is the long form.

mod render;

pub mod artifact;
pub mod attachment;
pub mod effect;
pub mod trigger;
pub mod wait;

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
    artifact::owner_retirements::TABLE,
    artifact::owners::TABLE,
    artifact::refs::TABLE,
    attachment::condemnation::TABLE,
    attachment::manifest::TABLE,
    effect::replay::TABLE,
    effect::group::TABLE,
    effect::scope_retirement::TABLE,
    trigger::deliveries::TABLE,
    trigger::mutation_receipts::TABLE,
    trigger::occurrences::TABLE,
    trigger::subscriptions::TABLE,
    wait::waits::TABLE,
    wait::meta::TABLE,
    wait::revoked_sessions::TABLE,
    // Tables a converted family's statements reach but no converted family
    // owns yet. The renderer has to know a name to address it, and a
    // cross-family statement is a statement like any other — it cannot wait
    // for its neighbour's lane. Each one is a bare name rather than a module
    // path precisely because no module owns it: when its family converts, its
    // lane replaces the string with that module's `TABLE` and adds the
    // `[[cross_family]]` entry the gate then starts demanding.
    //
    // `session_head` and `sessions` are one logical table spelled differently
    // by the two backends (ADR 0098 freezes both names), so both appear; each
    // is named only by the backend that has it.
    "checkpoint_blob_refs",
    "deleted_sessions",
    "graph_nodes",
    "lashlang_artifacts",
    "node_anchors",
    // The process registry's own table, reached from the session catalog
    // through an `ATTACH`ed database on SQLite and as `lash_processes` in the
    // one database on PostgreSQL. The attachment GC's owner-death proof is
    // SQL over it; FIG-3384 converts the family.
    "processes",
    "runtime_turn_commits",
    "session_head",
    "sessions",
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
    statements.extend_from_slice(artifact::owners::OwnerStatements::NEUTRAL);
    statements.extend_from_slice(artifact::owner_retirements::OwnerRetirementStatements::NEUTRAL);
    statements.extend_from_slice(attachment::manifest::ManifestStatements::NEUTRAL);
    statements.extend_from_slice(attachment::manifest::ManifestProcessOwnerStatements::NEUTRAL);
    statements.extend_from_slice(attachment::condemnation::CondemnationStatements::NEUTRAL);
    statements.extend_from_slice(effect::EffectJournalStatements::NEUTRAL);
    statements.extend_from_slice(effect::replay::ReplayStatements::NEUTRAL);
    statements.extend_from_slice(effect::group::GroupStatements::NEUTRAL);
    statements.extend_from_slice(effect::scope_retirement::ScopeRetirementStatements::NEUTRAL);
    statements.extend_from_slice(trigger::deliveries::DeliveryStatements::NEUTRAL);
    statements.extend_from_slice(trigger::mutation_receipts::MutationReceiptStatements::NEUTRAL);
    statements.extend_from_slice(trigger::occurrences::OccurrenceStatements::NEUTRAL);
    statements.extend_from_slice(trigger::subscriptions::SubscriptionStatements::NEUTRAL);
    statements.extend_from_slice(wait::waits::WaitStatements::NEUTRAL);
    statements.extend_from_slice(wait::revoked_sessions::RevokedSessionStatements::NEUTRAL);
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

    /// Render this statement for `dialect`.
    ///
    /// # Errors
    ///
    /// Reports the neutral text's own defects: an unknown table, a malformed
    /// placeholder, or an unterminated literal or comment.
    pub fn render(&self, dialect: Dialect) -> Result<Rendered, RenderError> {
        Ok(Rendered {
            name: self.name,
            sql: render(self.neutral, dialect, TABLES)?,
        })
    }

    /// Render this statement, naming it if its neutral text is malformed.
    ///
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

/// Declare a named statement set.
///
/// Every statement is a single string literal in neutral form, so the set is
/// readable as SQL and parseable by the ownership gate. The generated type has
/// one [`Rendered`] field per statement, a `NEUTRAL` inventory, and a `render`
/// constructor a backend calls exactly once.
///
/// ```ignore
/// lash_store_sql::statements! {
///     /// Statements both backends share verbatim.
///     pub struct ReplayStatements @ "effect_replay" {
///         /// Whether a replay row exists.
///         exists_by_key = "SELECT EXISTS(
///                              SELECT 1 FROM runtime_effect_replay
///                              WHERE scope_id = ?1 AND replay_key = ?2
///                          )";
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

            /// Render every statement in this set once, for `dialect`.
            ///
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
