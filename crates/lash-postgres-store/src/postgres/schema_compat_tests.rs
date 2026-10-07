use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

use super::*;
use lash_core_execution::compat::CompatRefusal;

/// The PostgreSQL component's descriptor in the active tier.
fn postgres_descriptor() -> &'static lash_core_execution::compat::CompatDescriptor {
    lash_core_execution::compat::descriptor(lash_core_execution::compat::ComponentId::POSTGRES)
        .expect("the build declares the PostgreSQL store")
}

struct Scratch {
    pool: PgPool,
    name: String,
    url: String,
}

impl Scratch {
    async fn new(url: &str) -> Self {
        let name = format!("lash_compat_{}", uuid::Uuid::new_v4().simple());
        let mut admin = sqlx::PgConnection::connect(url).await.expect("connect PG");
        admin
            .execute(format!("CREATE SCHEMA {name}").as_str())
            .await
            .expect("create scratch schema");
        admin
            .execute(format!("SET search_path TO {name}").as_str())
            .await
            .expect("set scratch search path");
        sqlx::raw_sql(SCHEMA_DDL)
            .execute(&mut admin)
            .await
            .expect("provision schema");
        admin.close().await.expect("close admin");
        let search_path = name.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _| {
                let name = search_path.clone();
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path TO {name}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("open scratch pool");
        Self {
            pool,
            name,
            url: url.to_owned(),
        }
    }

    async fn apply(&self, sql: &str) {
        sqlx::raw_sql(sql)
            .execute(&self.pool)
            .await
            .expect("apply scratch DDL");
    }

    async fn open(&self) -> Result<(String, lash_core_execution::FleetFormat), StoreError> {
        ensure_schema(
            &self.pool,
            SchemaCheck::Enforce,
            lash_core_execution::FleetFormat::writable(),
        )
        .await
    }

    async fn cleanup(self) {
        self.pool.close().await;
        let mut admin = sqlx::PgConnection::connect(&self.url)
            .await
            .expect("connect admin");
        admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.name).as_str())
            .await
            .expect("drop scratch schema");
    }
}

#[tokio::test]
async fn postgres_opens_an_expanded_catalog_under_its_floor() {
    let Some(url) = postgres_test_support::database_url() else {
        return;
    };
    let scratch = Scratch::new(&url).await;
    scratch
        .apply(
            "ALTER TABLE lash_session_head ADD COLUMN next_release_note TEXT;
         CREATE TABLE next_release_table (id BIGINT PRIMARY KEY);
         CREATE VIEW next_release_view AS SELECT session_id FROM lash_session_head;
         CREATE INDEX next_release_lookup ON lash_session_head(next_release_note);",
        )
        .await;
    // A release past this build's own expanded the catalog and kept this
    // build's oldest read as its floor.
    let descriptor = postgres_descriptor();
    scratch
        .apply(&format!(
            "UPDATE lash_schema_versions SET version = {}, min_reader = {}
               WHERE component = 'lash-postgres-store'",
            descriptor.writes.max() + 1,
            descriptor.reads.min()
        ))
        .await;
    scratch
        .open()
        .await
        .expect("an expanded catalog remains readable");
    scratch.cleanup().await;
}

#[tokio::test]
async fn postgres_refuses_each_unsafe_addition() {
    let Some(url) = postgres_test_support::database_url() else {
        return;
    };
    for (kind, ddl) in [
        (
            "required column",
            "ALTER TABLE lash_session_head ADD COLUMN unsafe_required INTEGER NOT NULL",
        ),
        (
            "CHECK",
            "ALTER TABLE lash_session_head ADD CONSTRAINT unsafe_check CHECK (head_revision >= 0) NOT VALID",
        ),
        (
            "UNIQUE",
            "ALTER TABLE lash_session_revisions ADD CONSTRAINT unsafe_unique UNIQUE (head_json)",
        ),
        (
            "FOREIGN KEY",
            "ALTER TABLE lash_session_head ADD CONSTRAINT unsafe_fk FOREIGN KEY (session_id) REFERENCES lash_session_head(session_id) NOT VALID",
        ),
        (
            "EXCLUDE",
            "ALTER TABLE lash_session_head ADD CONSTRAINT unsafe_exclude EXCLUDE USING gist (int8range(head_revision, head_revision + 1) WITH &&)",
        ),
        (
            "trigger",
            "CREATE FUNCTION unsafe_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; CREATE TRIGGER unsafe_trigger BEFORE INSERT ON lash_session_head FOR EACH ROW EXECUTE FUNCTION unsafe_trigger_fn()",
        ),
    ] {
        let scratch = Scratch::new(&url).await;
        scratch.apply(ddl).await;
        scratch.apply("UPDATE lash_schema_versions SET version = version + 1 WHERE component = 'lash-postgres-store'").await;
        let error = scratch.open().await.expect_err(kind);
        assert!(
            matches!(
                error,
                StoreError::Incompatible {
                    refusal: CompatRefusal::ShapeRefused { .. }
                }
            ),
            "{kind} must refuse as ShapeRefused: {error}"
        );
        scratch.cleanup().await;
    }
}

/// FIG-5270: version-1 counters cannot certify pre-release stored shapes.
#[tokio::test]
async fn release_build_refuses_pre_release_store_before_counters_without_mutation() {
    let url = crate::testing::required_database_url();
    let scratch = Scratch::new(&url).await;
    for writing in ["0.0.0-dev", "0.9.0", "1.0.0-rc.1"] {
        for counter in [1, 2, 99] {
            assert_release_admission(&scratch, writing, "1.0.0", counter, true).await;
        }
    }
    scratch.cleanup().await;
}

/// FIG-5270: the baseline release opens its own version-1 store.
#[tokio::test]
async fn release_build_admits_release_store() {
    let url = crate::testing::required_database_url();
    let scratch = Scratch::new(&url).await;
    assert_release_admission(&scratch, "1.0.0", "1.0.0", 1, false).await;
    scratch.cleanup().await;
}

/// FIG-5270: the release cut rule leaves development admission unchanged.
#[tokio::test]
async fn pre_release_build_admits_pre_release_store() {
    let url = crate::testing::required_database_url();
    let scratch = Scratch::new(&url).await;
    assert_release_admission(&scratch, "0.0.0-dev", "0.0.0-dev", 1, false).await;
    scratch.cleanup().await;
}

async fn assert_release_admission(
    scratch: &Scratch,
    writing: &str,
    build: &str,
    counter: i32,
    refuses: bool,
) {
    sqlx::query("INSERT INTO lash_release_stamp (singleton, release_version, schema_versions, written_at_epoch_ms) VALUES (TRUE, $1, 'Postgres schema=1', 42) ON CONFLICT (singleton) DO UPDATE SET release_version = excluded.release_version")
        .bind(writing).execute(&scratch.pool).await.expect("stamp writing release");
    sqlx::query("UPDATE lash_schema_versions SET version = $1, min_reader = $1 WHERE component = 'lash-postgres-store'")
        .bind(counter).execute(&scratch.pool).await.expect("stamp component counters");
    sqlx::query("UPDATE lash_fleet_format SET format_version = $1")
        .bind(counter)
        .execute(&scratch.pool)
        .await
        .expect("stamp fleet counter");
    let before = snapshot_rows(&scratch.pool).await;
    let descriptor = postgres_descriptor();
    let observation = observe_schema(&scratch.pool, descriptor)
        .await
        .expect("read-only observation");
    let status =
        crate::preflight::project_schema_status(observation, descriptor, "postgres".into(), build);
    let open = ensure_schema_with_release(
        &scratch.pool,
        SchemaCheck::Enforce,
        lash_core_execution::FleetFormat::writable(),
        build,
    )
    .await;
    if refuses {
        let expected = CompatRefusal::PreRelease {
            component: descriptor.component.as_str().into(),
            writing_release: Some(writing.into()),
        };
        assert_eq!(
            status.databases[0].verdict,
            lash_core_execution::StoreSchemaVerdict::Refused {
                refusal: expected.clone()
            }
        );
        assert!(
            matches!(open.expect_err("refuse open"), StoreError::Incompatible { refusal } if refusal == expected)
        );
    } else {
        assert_eq!(
            status.databases[0].verdict,
            lash_core_execution::StoreSchemaVerdict::Matches
        );
        open.expect("admit open");
    }
    assert_eq!(
        snapshot_rows(&scratch.pool).await,
        before,
        "every stored row remains unchanged"
    );
}

/// Compare every stored row, including the release timestamp and all counters.
async fn snapshot_rows(pool: &PgPool) -> Vec<(String, Vec<String>)> {
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema() ORDER BY tablename")
        .fetch_all(pool).await.expect("enumerate stored tables");
    let mut snapshot = Vec::new();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let rows = sqlx::query_scalar::<_, String>(&format!(
            "SELECT to_jsonb(row)::text FROM \"{quoted}\" AS row ORDER BY to_jsonb(row)::text"
        ))
        .fetch_all(pool)
        .await
        .expect("snapshot all stored rows");
        snapshot.push((table, rows));
    }
    snapshot
}
