//! The deployment layouts this crate's statements are rendered for
//! (FIG-3406).
//!
//! A [`Schema`] *selects a deployment layout*; it is not a qualifier stapled
//! onto every table in a statement. [`Schema::layout`] is the layout in which
//! a connection's `<schema>` database holds every table `lash-store-sql`
//! owns — which is the truth for a lash SQLite deployment whose catalog and
//! journal are one file, and for the journal file and the registry file, each
//! of which is provisioned with the tables its statements name
//! (`SqliteDatabase` in `schema.rs` is the checked-in list). A layout that
//! reaches *two* databases at once is declared where it is needed: see
//! `attachments.rs`, whose GC probes join `main.attachment_manifest` to
//! `process_registry.processes`.
//!
//! Every statement over a converted table is rendered once per layout at
//! startup, so a caller that reaches the journal through an `ATTACH` names the
//! schema and gets finished SQL rather than building a qualified statement.

use lash_store_sql::{Dialect, SchemaTables, TableLayout};

/// A database a SQLite connection in this crate addresses a shared table
/// through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Schema {
    /// The connection's own database: the journal's own file on a journal
    /// connection, the registry's own file on a registry connection.
    Main,
    /// The effect journal, attached to the session catalog for a retention
    /// sweep.
    EffectJournal,
    /// A bound process registry, attached to the journal connection.
    ProcessRegistry,
}

impl Schema {
    /// Every schema, in [`Schema::index`] order.
    pub(crate) const ALL: [Self; 3] = [Self::Main, Self::EffectJournal, Self::ProcessRegistry];

    /// The schema's SQL qualifier.
    pub(crate) const fn qualifier(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::EffectJournal => "effect_journal",
            Self::ProcessRegistry => "process_registry",
        }
    }

    /// The deployment layout in which this database holds every table
    /// `lash-store-sql` owns.
    ///
    /// Total rather than partial on purpose: the three databases this enum
    /// names are each provisioned with the tables the statements rendered for
    /// them address, and a single-file deployment reaches all of them through
    /// `main`. A family whose statements span two databases declares its own
    /// layout instead of reusing one of these.
    pub(crate) const fn layout(self) -> TableLayout {
        match self {
            Self::Main => MAIN_LAYOUT,
            Self::EffectJournal => EFFECT_JOURNAL_LAYOUT,
            Self::ProcessRegistry => PROCESS_REGISTRY_LAYOUT,
        }
    }

    /// The render dialect that addresses tables through this schema.
    pub(crate) const fn dialect(self) -> Dialect {
        Dialect::sqlite(self.layout())
    }

    /// This schema's slot in a per-schema statement table.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::EffectJournal => 1,
            Self::ProcessRegistry => 2,
        }
    }
}

/// The connection's own database holds every table this crate's statements
/// name: the layout of a journal connection, of a registry connection, and of
/// any deployment that keeps its catalog and journal in one file.
const MAIN_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::Main.qualifier(),
    lash_store_sql::TABLES,
)]);

/// The effect journal, attached to a session catalog for a retention sweep.
const EFFECT_JOURNAL_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::EffectJournal.qualifier(),
    lash_store_sql::TABLES,
)]);

/// A bound process registry, attached to this connection. Its own copy of the
/// fence table is reached through this layout rather than through a second
/// entry in the journal's: one table name, two files, two layouts (ADR 0049).
const PROCESS_REGISTRY_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::ProcessRegistry.qualifier(),
    lash_store_sql::TABLES,
)]);
