#[test]
fn sqlite_attachment_owner_kind_sql_derives_from_the_enum() {
    let sources = [
        ("attachments.rs", include_str!("../../src/attachments.rs")),
        (
            "persistence/mod.rs",
            include_str!("../../src/persistence/mod.rs"),
        ),
        (
            "persistence/claim_support.rs",
            include_str!("../../src/persistence/claim_support.rs"),
        ),
        (
            "persistence/maintenance.rs",
            include_str!("../../src/persistence/maintenance.rs"),
        ),
        (
            "persistence/queued_work.rs",
            include_str!("../../src/persistence/queued_work.rs"),
        ),
        (
            "persistence/session_commit.rs",
            include_str!("../../src/persistence/session_commit.rs"),
        ),
        (
            "persistence/session_execution_lease.rs",
            include_str!("../../src/persistence/session_execution_lease.rs"),
        ),
        (
            "persistence/turn_input.rs",
            include_str!("../../src/persistence/turn_input.rs"),
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
        "SQLite owner-kind SQL literals must derive from AttachmentOwnerKind::as_str; found {raw_sites:?}"
    );
}
