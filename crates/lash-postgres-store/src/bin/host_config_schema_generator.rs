//! Emit the schema derived from the host config's serde wire types.

use lash_postgres_store::PostgresHostConfig;
use serde_json::json;

fn main() -> Result<(), serde_json::Error> {
    let mut schema = serde_json::to_value(schemars::schema_for!(PostgresHostConfig))?;
    schema["$id"] = json!("https://lash.dev/schemas/postgres-host-config/v1");
    println!(
        "{}",
        json!([{
            "shape": "postgres-host-config",
            "version": 1,
            "schema": schema,
        }])
    );
    Ok(())
}
