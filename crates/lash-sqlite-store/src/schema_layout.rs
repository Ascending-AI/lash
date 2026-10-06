//! The deployment layout this crate's statements are rendered for
//! (FIG-3406).
//!
//! A SQLite deployment is one database file (ADR 0132 §12): a connection's
//! `main` database holds every table `lash-store-sql` owns, so every
//! statement over a converted table is rendered once, for `main`, at
//! startup, and a caller gets finished SQL rather than building a qualified
//! statement.

use lash_store_sql::{Dialect, SchemaTables, TableLayout};

/// The connection's own database holds every table this crate's statements
/// name.
const MAIN_LAYOUT: TableLayout =
    TableLayout::new(&[SchemaTables::new("main", lash_store_sql::TABLES)]);

/// The render dialect that addresses every table through `main`.
pub(crate) const MAIN: Dialect = Dialect::sqlite(MAIN_LAYOUT);
