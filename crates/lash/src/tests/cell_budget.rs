//! The standard instruction budget admits the cells models write
//! (FIG-5822). A TypeScript helper's body is charged as it runs, so a helper
//! that assembles text pays what it copies: once, through the native join.
//! The budget is twice the costliest cell it was measured over, which keeps
//! inside half of it (FIG-5825).

use super::*;

/// A model-written cell: a report built line by line, a thousand rows
/// pushed, both printed, a summary finished.
const CELL: &str = r#"
let report = "";
for (let i = 0; i < 70; i++) {
  report = `${report}line ${i}: abcdefghijklmnopqrstuvwxyz0123456789 abcdefghijklmnopqrstuvwxyz0123456789\n`;
}
const rows = [];
for (let i = 0; i < 1000; i++) {
  rows.push({ id: `row_${i}`, status: "ok", exit_code: 0 });
}
console.log(rows);
console.log(report);
await control.finish({ lines: report.split("\n").length, rows: rows.length });"#;

/// The costliest of the cells `InstructionBound::standard()` was measured
/// over, at 9,097,832 units: it builds 3,000 rows, prints them and finishes
/// with them (`output_retention`'s code mode cell).
const COSTLIEST: &str = r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
console.log(rows);
await control.finish({ rows });"#;

/// The value `cell` finishes with, run under `instructions`.
async fn finished_under(
    cell: &str,
    instructions: lash_protocol_rlm::InstructionBound,
) -> Result<serde_json::Value> {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(instructions)
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    )
    .with_worker_service(untimed_fixture_workers());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        sqlite_memory_store_backend().await,
        factory,
    ))
    .serve_test_llm_profile(
        queued_text_provider(vec![typescript_block(cell)]),
        mock_llm_profile_spec(),
    )
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("standard-budget").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("run the cell"))
        .output()
        .await?;
    Ok(output
        .finished()
        .map(|(_, value)| value)
        .cloned()
        .expect("the cell finishes inside its budget"))
}

/// Helpers that re-copied their whole text at every piece charged the
/// square of what they printed, and ended this cell at the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_that_builds_and_prints_rows_fits_the_standard_instruction_budget() -> Result<()> {
    let value = finished_under(CELL, lash_protocol_rlm::InstructionBound::standard()).await?;
    assert_eq!(value, serde_json::json!({ "lines": 71, "rows": 1000 }));
    Ok(())
}

/// The standard budget is twice its costliest measured cell: while that cell
/// fits half of it, the derivation its comment states still holds. A helper
/// change that takes the cell past half needs a new measurement, never a
/// larger budget by guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_costliest_measured_cell_fits_half_the_standard_instruction_budget() -> Result<()> {
    let standard = lash_protocol_rlm::InstructionBound::standard()
        .limit()
        .expect("the standard budget is finite");
    let half = lash_protocol_rlm::InstructionBound::instructions(standard.get() / 2);
    let value = finished_under(COSTLIEST, half).await?;
    assert_eq!(value["rows"].as_array().map(Vec::len), Some(3000));
    Ok(())
}
