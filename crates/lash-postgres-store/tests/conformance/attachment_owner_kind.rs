#[test]
fn postgres_attachment_owner_kind_sql_derives_from_the_enum() {
    let sources = [
        (
            "attachments.rs",
            include_str!("../../src/postgres/attachments.rs"),
        ),
        (
            "runtime_persistence/mod.rs",
            include_str!("../../src/postgres/runtime_persistence/mod.rs"),
        ),
        (
            "runtime_persistence/claim_support.rs",
            include_str!("../../src/postgres/runtime_persistence/claim_support.rs"),
        ),
        (
            "runtime_persistence/commit_claims.rs",
            include_str!("../../src/postgres/runtime_persistence/commit_claims.rs"),
        ),
        (
            "runtime_persistence/maintenance.rs",
            include_str!("../../src/postgres/runtime_persistence/maintenance.rs"),
        ),
        (
            "runtime_persistence/queued_work.rs",
            include_str!("../../src/postgres/runtime_persistence/queued_work.rs"),
        ),
        (
            "runtime_persistence/session_commit.rs",
            include_str!("../../src/postgres/runtime_persistence/session_commit.rs"),
        ),
        (
            "runtime_persistence/session_execution_lease.rs",
            include_str!("../../src/postgres/runtime_persistence/session_execution_lease.rs"),
        ),
        (
            "runtime_persistence/turn_input.rs",
            include_str!("../../src/postgres/runtime_persistence/turn_input.rs"),
        ),
        (
            "session_factory.rs",
            include_str!("../../src/postgres/session_factory.rs"),
        ),
    ];
    let raw_sites = sources
        .into_iter()
        .flat_map(|(name, source)| {
            ["turn", "process"].into_iter().flat_map(move |value| {
                source
                    .match_indices(&format!("owner_kind = '{value}'"))
                    .map(move |(offset, _)| format!("{name}:{offset}:{value}"))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();

    assert!(
        raw_sites.is_empty(),
        "Postgres owner-kind SQL literals must derive from AttachmentOwnerKind::as_str; found {raw_sites:?}"
    );
}
