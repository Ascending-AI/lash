//! Every rendered statement set this crate owns renders at all.
//!
//! Rendering is a `LazyLock` behind each `*_sql()` accessor, so a neutral
//! statement the renderer refuses — a relation `lash-store-sql` does not own,
//! a malformed placeholder, an unknown vocabulary term — is a panic at that
//! set's first use and nowhere earlier. That is the right production
//! behaviour: the store fails to open rather than reaching a database with
//! text nobody checked. It is the wrong *test* behaviour, because the first
//! use of most of these sets is inside a suite that needs a live PostgreSQL,
//! and PR CI does not run one: a non-rendering statement is green on every
//! required check and reds the trunk-only `pg-store` job.
//!
//! FIG-3387 hit exactly that. `postgres_connection.select_check_constraints`
//! reads `pg_catalog.pg_constraint` in a table position, which the renderer
//! refuses (correctly — a system catalog must never acquire the `lash_`
//! prefix), and the poisoned `LazyLock` failed thirty-one tests in the
//! service-backed suite while `kiln build`, `kiln clippy` and `kiln test` were
//! all green. This test forces every set, needs no database, and runs in the
//! ordinary unit-test target.

/// Touch every rendered set. Each accessor dereferences its `LazyLock`, so a
/// statement that does not render panics here, naming itself.
#[test]
fn every_rendered_statement_set_renders() {
    let _ = crate::connection_sql::connection_sql();
    let _ = crate::artifact_store::artifact_sql();
    let _ = crate::attachments::attachment_sql();
    let _ = crate::blobs::blob_sql();
    let _ = crate::process_sql::process_sql();
    let _ = crate::session_sql::session_sql();
    let _ = crate::trigger_store::trigger_sql();
    let _ = crate::turn_ingress::turn_ingress_sql();
}
