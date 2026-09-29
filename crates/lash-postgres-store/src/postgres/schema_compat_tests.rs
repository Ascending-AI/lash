use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

use super::*;
use lash_core_execution::compat::CompatRefusal;

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
            "ALTER TABLE lash_sessions ADD COLUMN next_release_note TEXT;
         CREATE TABLE next_release_table (id BIGINT PRIMARY KEY);
         CREATE VIEW next_release_view AS SELECT session_id FROM lash_sessions;
         CREATE INDEX next_release_lookup ON lash_sessions(next_release_note);
         UPDATE lash_schema_versions SET version = 2, min_reader = 1
           WHERE component = 'lash-postgres-store'",
        )
        .await;
    scratch
        .open()
        .await
        .expect("an expanded catalog remains readable");
    scratch.cleanup().await;
}

#[tokio::test]
async fn postgres_refuses_a_raised_floor_typed() {
    let Some(url) = postgres_test_support::database_url() else {
        return;
    };
    let scratch = Scratch::new(&url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 2, min_reader = 2
           WHERE component = 'lash-postgres-store'",
        )
        .await;
    let error = scratch
        .open()
        .await
        .expect_err("reader floor excludes this build");
    assert!(matches!(
        error,
        StoreError::Incompatible {
            refusal: CompatRefusal::ReaderFloorAbove {
                found: 2,
                min_reader: 2,
                ..
            }
        }
    ));
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
            "ALTER TABLE lash_sessions ADD COLUMN unsafe_required INTEGER NOT NULL",
        ),
        (
            "CHECK",
            "ALTER TABLE lash_sessions ADD CONSTRAINT unsafe_check CHECK (head_revision >= 0) NOT VALID",
        ),
        (
            "UNIQUE",
            "ALTER TABLE lash_sessions ADD CONSTRAINT unsafe_unique UNIQUE (head_json)",
        ),
        (
            "FOREIGN KEY",
            "ALTER TABLE lash_sessions ADD CONSTRAINT unsafe_fk FOREIGN KEY (session_id) REFERENCES lash_sessions(session_id) NOT VALID",
        ),
        (
            "EXCLUDE",
            "ALTER TABLE lash_sessions ADD CONSTRAINT unsafe_exclude EXCLUDE USING gist (int8range(head_revision, head_revision + 1) WITH &&)",
        ),
        (
            "trigger",
            "CREATE FUNCTION unsafe_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; CREATE TRIGGER unsafe_trigger BEFORE INSERT ON lash_sessions FOR EACH ROW EXECUTE FUNCTION unsafe_trigger_fn()",
        ),
    ] {
        let scratch = Scratch::new(&url).await;
        scratch.apply(ddl).await;
        scratch.apply("UPDATE lash_schema_versions SET version = 2 WHERE component = 'lash-postgres-store'").await;
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

#[tokio::test]
async fn postgres_refuses_a_populated_catalog_without_a_stamp() {
    let Some(url) = postgres_test_support::database_url() else {
        return;
    };
    let scratch = Scratch::new(&url).await;
    scratch
        .apply("DELETE FROM lash_schema_versions WHERE component = 'lash-postgres-store'")
        .await;
    let error = scratch
        .open()
        .await
        .expect_err("populated catalog has no stamp");
    assert!(matches!(
        error,
        StoreError::Incompatible {
            refusal: CompatRefusal::Unstamped { .. }
        }
    ));
    scratch.cleanup().await;
}
