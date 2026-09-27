//! The deployment layouts this crate's statements are rendered for
//! (FIG-3406).
//!
//! A [`Schema`] *selects a deployment layout*; it is not a qualifier stapled
//! onto every table in a statement. [`Schema::layout`] is the layout in which
//! a connection's `<schema>` database holds every table `lash-store-sql`
//! owns — which is the truth for each storage database provisioned with the
//! tables its statements name
//! (`SqliteDatabase` in `schema.rs` is the checked-in list). A layout that
//! reaches *two* databases at once is declared where it is needed: see
//! `attachments.rs`, whose GC probes join `main.attachment_manifest` to
//! `process_registry.processes`.
//!
//! Every statement over a converted table is rendered once per layout at
//! startup, so a caller names the schema and gets finished SQL rather than
//! building a qualified statement.

use lash_store_sql::{Dialect, SchemaTables, TableLayout};

/// A database a SQLite connection in this crate addresses a shared table
/// through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Schema {
    /// The connection's own database.
    Main,
    /// A bound process registry, attached to a storage connection.
    ProcessRegistry,
}

impl Schema {
    /// The schema's SQL qualifier.
    pub(crate) const fn qualifier(self) -> &'static str {
        match self {
            Self::Main => "main",
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
            Self::ProcessRegistry => PROCESS_REGISTRY_LAYOUT,
        }
    }

    /// The render dialect that addresses tables through this schema.
    pub(crate) const fn dialect(self) -> Dialect {
        Dialect::sqlite(self.layout())
    }
}

/// The connection's own database holds every table this crate's statements
/// name, regardless of which storage database the connection opens.
const MAIN_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::Main.qualifier(),
    lash_store_sql::TABLES,
)]);

/// A bound process registry, attached to this connection.
const PROCESS_REGISTRY_LAYOUT: TableLayout = TableLayout::new(&[SchemaTables::new(
    Schema::ProcessRegistry.qualifier(),
    lash_store_sql::TABLES,
)]);
