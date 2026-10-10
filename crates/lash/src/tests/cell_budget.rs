//! The standard instruction budget admits the cells models write
//! (FIG-5822). A TypeScript helper's body is charged as it runs, so a helper
//! that assembles text pays what it copies: once, through the native join.

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

/// Helpers that re-copied their whole text at every piece charged the
/// square of what they printed, and ended this cell at the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_that_builds_and_prints_rows_fits_the_standard_instruction_budget() -> Result<()> {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::standard())
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
        queued_text_provider(vec![typescript_block(CELL)]),
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
        .send(TurnInput::text("report the rows"))
        .output()
        .await?;
    let value = output
        .finished()
        .map(|(_, value)| value)
        .cloned()
        .expect("the cell finishes inside the standard budget");
    assert_eq!(value, serde_json::json!({ "lines": 71, "rows": 1000 }));
    Ok(())
}
