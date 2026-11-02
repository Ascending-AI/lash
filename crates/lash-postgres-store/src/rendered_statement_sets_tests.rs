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

#[test]
fn ddl_referrer_kind_checks_match_typed_predicates() {
    use lash_core_store::artifact_referrer::ArtifactReferrerKind;
    use std::collections::BTreeSet;
    let ddl = crate::PostgresStorage::schema_ddl();
    for (constraint, accepts) in [
        (
            "ck_attachment_referrer_edges_kind",
            ArtifactReferrerKind::holds_attachments as fn(ArtifactReferrerKind) -> bool,
        ),
        (
            "ck_attachment_pending_writes_kind",
            ArtifactReferrerKind::holds_attachments,
        ),
        (
            "ck_artifact_referrer_edges_kind",
            ArtifactReferrerKind::holds_artifacts,
        ),
        ("ck_referrer_fences_kind", |_| true),
        ("ck_artifact_cleanup_obligations_kind", |_| true),
    ] {
        let line = ddl
            .lines()
            .find(|line| line.contains(constraint))
            .expect("constraint exists");
        let list = line
            .split(" IN (")
            .nth(1)
            .expect("kind list")
            .split(')')
            .next()
            .expect("end of list");
        let actual: BTreeSet<_> = list
            .split(',')
            .map(|label| label.trim().trim_matches('\''))
            .collect();
        let expected: BTreeSet<_> = ArtifactReferrerKind::ALL
            .into_iter()
            .filter(|kind| accepts(*kind))
            .map(ArtifactReferrerKind::as_str)
            .collect();
        assert_eq!(actual, expected, "{constraint}");
    }
}
