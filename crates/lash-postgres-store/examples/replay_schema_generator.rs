//! Print a replay schema shape from a private database for release tooling.
use lash::postgres::{PostgresLiveReplayStore, PostgresProcessReplayStore};
use lash_postgres_store::testing::{IsolatedDatabase, required_database_url};
use sqlx::{Connection as _, PgConnection, PgPool};

#[expect(
    clippy::disallowed_methods,
    reason = "the artifact command reads its requested artifact"
)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("expected replay shape artifact")?;
    let ddl = match artifact.as_str() {
        "postgres-live-replay-schema-shape.txt" => PostgresLiveReplayStore::schema_ddl(),
        "postgres-process-replay-schema-shape.txt" => PostgresProcessReplayStore::schema_ddl(),
        _ => return Err("unknown replay shape artifact".into()),
    };
    let database = IsolatedDatabase::create(&required_database_url()).await;
    let schema = format!("replay_recorder_{}", uuid::Uuid::new_v4().simple());
    let mut connection = PgConnection::connect(database.url()).await?;
    sqlx::raw_sql(&format!(
        "CREATE SCHEMA \"{schema}\"; SET search_path TO \"{schema}\";"
    ))
    .execute(&mut connection)
    .await?;
    for _ in 0..2 {
        sqlx::raw_sql(ddl).execute(&mut connection).await?;
    }
    connection.close().await?;
    let pool = PgPool::connect(database.url()).await?;
    let shape = if artifact == "postgres-live-replay-schema-shape.txt" {
        PostgresLiveReplayStore::verify_schema(&pool, &schema)
            .await?
            .found_shape()
            .to_string()
    } else {
        PostgresProcessReplayStore::verify_schema(&pool, &schema)
            .await?
            .found_shape()
            .to_string()
    };
    pool.close().await;
    print!("{shape}");
    Ok(())
}
