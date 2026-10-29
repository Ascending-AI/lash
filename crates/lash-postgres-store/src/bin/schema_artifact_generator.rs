//! Generate PostgreSQL artifacts for release reset without running a test.

#[expect(
    clippy::disallowed_methods,
    reason = "artifact generator parses its command-line argument in main"
)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = match std::env::args().nth(1).as_deref() {
        Some("teardown.sql") => lash_postgres_store::testing::teardown_artifact(),
        Some("schema-shape.txt") => {
            let url = lash_postgres_store::testing::required_database_url();
            lash_postgres_store::testing::schema_shape_artifact(&url).await
        }
        _ => return Err("expected teardown.sql or schema-shape.txt".into()),
    };
    print!("{artifact}");
    Ok(())
}
