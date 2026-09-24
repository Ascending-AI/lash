//! T13 (FIG-3586): the effect journal's key columns compare bytewise.
//!
//! A lashlang run reads its recorded frontier as one key range
//! `[{namespace}:, {namespace}:~seal]`, and its seal must sort after every
//! ordinal of its namespace. SQLite compares TEXT bytewise; a PostgreSQL column
//! under a locale collation does not (it ignores punctuation such as `:` and
//! `~` at the first level), so the range would admit other namespaces' keys and
//! drop the seal. The columns carry `COLLATE "C"`, and this law holds them to
//! the byte order SQLite answers, whatever the database's own locale.

use sqlx::{Connection, PgConnection, Row};

use crate::support::{SharedDatabaseLock, database_url};

const FIXTURE_SCHEMA: &str = "lash_fig3586_replay_key_collation";
const SCOPE: &str = "turn:replay-key-collation";
const NAMESPACE: &str = "sess:turn:0:exec:e1:lk2";

fn keys() -> Vec<String> {
    let inside = [
        ":0000000000",
        ":0000000000:attempt:1",
        ":0000000000:attempt:1:sleep",
        ":0000000009:child:0:attempt:2",
        ":0000000010",
        ":0000000010:sleep",
        ":0000000011:timers-admitted",
        ":9999999999:signal",
        ":~seal",
    ]
    .into_iter()
    .map(|suffix| format!("{NAMESPACE}{suffix}"));
    let outside = [
        // The bare namespace, a neighbour namespace sharing its prefix, and
        // keys past its seal.
        NAMESPACE.to_string(),
        "sess:turn:0:exec:e10:lk2:0000000000".to_string(),
        "sess:turn:0:exec:e1:lk20:0000000000".to_string(),
        format!("{NAMESPACE}~"),
        format!("{NAMESPACE}:~seal:after"),
        "sess:turn:0:exec:e2:lk2:0000000000".to_string(),
    ];
    inside.chain(outside).collect()
}

#[tokio::test]
async fn replay_key_ranges_compare_bytewise_under_any_database_locale() {
    let Some(url) = database_url() else {
        eprintln!("skipping the replay-key collation law: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect the collation fixture");
    sqlx::raw_sql(&format!(
        "DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE;
         CREATE SCHEMA {FIXTURE_SCHEMA};
         SET search_path TO {FIXTURE_SCHEMA};"
    ))
    .execute(&mut connection)
    .await
    .expect("create the isolated collation fixture schema");
    sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
        .execute(&mut connection)
        .await
        .expect("apply the schema DDL");

    // The catalog pin: every key column of the journal is C-collated.
    for (table, column) in [
        ("lash_runtime_effect_replay", "replay_key"),
        ("lash_runtime_effect_replay", "group_key"),
        ("lash_runtime_effect_group", "group_key"),
        ("lash_runtime_effect_group_child", "group_key"),
        ("lash_runtime_effect_group_child", "replay_key"),
    ] {
        let collation: String = sqlx::query(
            "SELECT c.collname::text
             FROM pg_attribute a
             JOIN pg_class t ON t.oid = a.attrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             JOIN pg_collation c ON c.oid = a.attcollation
             WHERE n.nspname = $1 AND t.relname = $2 AND a.attname = $3",
        )
        .bind(FIXTURE_SCHEMA)
        .bind(table)
        .bind(column)
        .fetch_one(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("read {table}.{column}'s collation: {error}"))
        .get(0);
        assert_eq!(collation, "C", "{table}.{column} must compare bytewise");
    }

    for key in keys() {
        sqlx::query(
            "INSERT INTO lash_runtime_effect_replay (
                 scope_id, replay_key, envelope_hash, envelope_json, status,
                 created_at_ms, updated_at_ms
             ) VALUES ($1, $2, 'hash', '{}', 'in_progress', 0, 0)",
        )
        .bind(SCOPE)
        .bind(&key)
        .execute(&mut connection)
        .await
        .expect("insert a journal key");
    }

    // The product's own range read, rendered for PostgreSQL.
    let statements = lash_store_sql::effect::replay::ReplayStatements::render(
        lash_store_sql::Dialect::postgres(),
    );
    let lower = format!("{NAMESPACE}:");
    let upper = format!("{NAMESPACE}:~seal");
    let read: Vec<String> = sqlx::query(statements.select_keys_in_range.sql())
        .bind(SCOPE)
        .bind(&lower)
        .bind(&upper)
        .fetch_all(&mut connection)
        .await
        .expect("read the recorded range")
        .into_iter()
        .map(|row| row.get(0))
        .collect();

    // What SQLite answers: byte order.
    let mut expected: Vec<String> = keys()
        .into_iter()
        .filter(|key| lower.as_str() <= key.as_str() && key.as_str() <= upper.as_str())
        .collect();
    expected.sort();
    assert_eq!(read, expected, "the range read is the bytewise range");
    assert_eq!(
        read.last(),
        Some(&upper),
        "the seal sorts after every ordinal of its namespace"
    );
    assert_eq!(read.len(), 9, "exactly the namespace's own keys: {read:?}");

    // Under a locale collation the same keys order otherwise, which is why
    // the columns pin `C`: the law above is not vacuous on this database.
    let locale: String =
        sqlx::query("SELECT datcollate::text FROM pg_database WHERE datname = current_database()")
            .fetch_one(&mut connection)
            .await
            .expect("read the database locale")
            .get(0);
    if !matches!(locale.as_str(), "C" | "POSIX" | "C.UTF-8" | "C.utf8") {
        let locale_order: Vec<String> = sqlx::query(
            "SELECT replay_key FROM lash_runtime_effect_replay
             WHERE scope_id = $1 ORDER BY replay_key COLLATE \"default\"",
        )
        .bind(SCOPE)
        .fetch_all(&mut connection)
        .await
        .expect("order the keys under the database locale")
        .into_iter()
        .map(|row| row.get(0))
        .collect();
        let mut bytewise = keys();
        bytewise.sort();
        assert_ne!(
            locale_order, bytewise,
            "database locale {locale} orders these keys bytewise, so it cannot witness the law"
        );
    }

    sqlx::raw_sql(&format!("DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE;"))
        .execute(&mut connection)
        .await
        .expect("drop the collation fixture schema");
}
