use std::collections::BTreeSet;

const SQLITE_SCHEMA_SOURCE: &str = include_str!("../../lash-sqlite-store/src/schema.rs");
const POSTGRES_SCHEMA_SHAPE: &str = include_str!("../../lash-postgres-store/schema-shape.txt");

fn consume_keyword<'a>(source: &'a str, keyword: &str) -> Option<&'a str> {
    let source = source.trim_start();
    let candidate = source.get(..keyword.len())?;
    if candidate.eq_ignore_ascii_case(keyword)
        && source
            .get(keyword.len()..)
            .is_none_or(|rest| rest.starts_with(char::is_whitespace))
    {
        source.get(keyword.len()..)
    } else {
        None
    }
}

fn consume_identifier(source: &str) -> Option<String> {
    let source = source.trim_start();
    let first = source.chars().next()?;
    let (closing, offset) = match first {
        '"' => ('"', 1),
        '`' => ('`', 1),
        '[' => (']', 1),
        _ => {
            let end = source
                .find(|character: char| character.is_whitespace() || character == '(')
                .unwrap_or(source.len());
            return (end > 0).then(|| source[..end].to_string());
        }
    };
    let rest = &source[offset..];
    let end = rest.find(closing)?;
    Some(rest[..end].to_string())
}

fn sqlite_table_names() -> BTreeSet<String> {
    SQLITE_SCHEMA_SOURCE
        .match_indices(|character: char| character.eq_ignore_ascii_case(&'c'))
        .filter_map(|(offset, _)| {
            let source = SQLITE_SCHEMA_SOURCE.get(offset..)?;
            let source = consume_keyword(source, "CREATE")?;
            let source = consume_keyword(source, "TABLE")?;
            let source = consume_keyword(source, "IF")
                .and_then(|source| consume_keyword(source, "NOT"))
                .and_then(|source| consume_keyword(source, "EXISTS"))
                .unwrap_or(source);
            consume_identifier(source)
        })
        .collect()
}

fn postgres_table_names() -> BTreeSet<String> {
    POSTGRES_SCHEMA_SHAPE
        .lines()
        .filter_map(|line| line.strip_prefix("table ").map(str::to_string))
        .collect()
}

fn normalize_postgres_table(name: &str) -> Option<String> {
    let name = name.strip_prefix("lash_").unwrap_or(name);
    match name {
        "schema_versions" => None,
        "fork_lineage" => Some("fork_lineage".to_string()),
        "sessions" => Some("session_head".to_string()),
        "lashlang_artifacts" => Some("artifact_refs".to_string()),
        name => Some(name.to_string()),
    }
}

#[test]
fn sqlite_and_postgres_table_sets_are_congruent() {
    let sqlite = sqlite_table_names();
    let postgres_raw = postgres_table_names();

    assert_eq!(
        sqlite.len(),
        36,
        "SQLite schema table count changed; update the explicit cross-backend mapping"
    );
    assert_eq!(
        postgres_raw.len(),
        37,
        "Postgres schema table count changed; update the explicit cross-backend mapping"
    );
    assert!(
        postgres_raw.contains("lash_schema_versions"),
        "Postgres-only schema version ledger disappeared"
    );

    let postgres = postgres_raw
        .iter()
        .filter_map(|name| normalize_postgres_table(name))
        .collect::<BTreeSet<_>>();
    let sqlite_only = sqlite.difference(&postgres).cloned().collect::<Vec<_>>();
    let postgres_only = postgres.difference(&sqlite).cloned().collect::<Vec<_>>();
    assert!(
        sqlite_only.is_empty() && postgres_only.is_empty(),
        "SQLite/Postgres table-set drift: sqlite_only={sqlite_only:?}, postgres_only={postgres_only:?}"
    );
}
