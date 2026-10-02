fn main() -> Result<(), serde_json::Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(&lash_restate_postgres_workers_e2e::load::ledger::contract())?
    );
    Ok(())
}
