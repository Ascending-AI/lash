#[test]
fn sqlite_attachment_owner_kind_sql_derives_from_the_enum() {
    let sources = [
        ("attachments.rs", include_str!("../../src/attachments.rs")),
        ("persistence.rs", include_str!("../../src/persistence.rs")),
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
