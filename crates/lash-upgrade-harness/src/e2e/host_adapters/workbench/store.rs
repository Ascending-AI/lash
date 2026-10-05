//! A PostgreSQL workbench's durable rows, read for the case's browser oracles.
//! The rows keep the shape SQLite answers, so one oracle asserts over both.
use super::*;

/// Store tables an oracle reads, and the columns it may select rows by.
const TABLES: &[&str] = &[
    "graph_nodes",
    "runtime_turn_commits",
    "pending_turn_inputs",
    "session_meta",
    "session_runs",
    "tool_call_receipts",
];
const KEYS: &[&str] = &["session_id", "run"];

impl WorkbenchHost {
    /// Answer `{"table", "equal": {column: text}}` with every matching row of
    /// the database this workbench was configured with.
    pub async fn store_rows(&self, request: &Value) -> Result<Value> {
        use sqlx::Connection as _;
        let url = self
            .environment
            .get("AGENT_WORKBENCH_DATABASE_URL")
            .context("store rows are read here only for a PostgreSQL workbench")?;
        let table = request["table"].as_str().context("store table absent")?;
        ensure!(TABLES.contains(&table), "unsupported store table {table}");
        let equal = request["equal"]
            .as_object()
            .context("store row selection absent")?;
        ensure!(!equal.is_empty(), "store rows need a selection");
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        for (column, value) in equal {
            ensure!(
                KEYS.contains(&column.as_str()),
                "unsupported store key {column}"
            );
            values.push(value.as_str().context("store keys are text")?);
            clauses.push(format!("{column} = ${}", values.len()));
        }
        let sql = format!(
            "SELECT row_to_json(t)::text FROM lash_{table} t WHERE {}",
            clauses.join(" AND ")
        );
        let mut connection = sqlx::PgConnection::connect(url).await?;
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for value in values {
            query = query.bind(value);
        }
        let rows = query.fetch_all(&mut connection).await;
        connection.close().await.ok();
        let rows = rows?
            .iter()
            .map(|row| serde_json::from_str(row))
            .collect::<std::result::Result<Vec<Value>, _>>()?;
        Ok(json!({"rows":rows}))
    }
}
