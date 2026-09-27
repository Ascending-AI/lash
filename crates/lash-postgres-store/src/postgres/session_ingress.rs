//! The PostgreSQL half of the session-ingress family: the one per-session
//! sequence both admission tables draw `enqueue_seq` from (ADR 0101 §5,
//! amended).
//!
//! `lash-store-sql`'s `session_ingress` module owns the statements; this
//! module renders them once, at startup.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::session_ingress::SessionIngressStatements;

static SESSION_INGRESS_SQL: LazyLock<SessionIngressStatements> =
    LazyLock::new(|| SessionIngressStatements::render(Dialect::postgres()));

/// The session-ingress statements, rendered once at first use.
pub(crate) fn session_ingress_sql() -> &'static SessionIngressStatements {
    &SESSION_INGRESS_SQL
}
