//! JSON-lines machine adapter for the live helper fuzzer. No oracle runs here.
#[path = "../shared/helper_differential/comparison.rs"]
mod comparison;
#[expect(
    dead_code,
    reason = "the adapter only needs the ordinary start fixture"
)]
#[path = "../src/tests/pages.rs"]
mod pages;
#[path = "../shared/helper_differential/runner.rs"]
mod runner;
use runner::{Case, Runner};
use serde_json::json;
use std::io::{self, BufRead, Write};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ts = Runner::new("typescript")?;
    let py = Runner::new("python")?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let input: serde_json::Value = serde_json::from_str(&line?)?;
        let case: Case = serde_json::from_value(input.clone())?;
        let engine = if case.dialect == "typescript" {
            &ts
        } else {
            &py
        };
        let answer = match engine.run(&case) {
            Ok(observed) => {
                json!({"id":case.id, "result":observed.result, "helpers":observed.helpers, "matches": input.get("reference").map(|expected| comparison::matches(&observed.result, expected, comparison::implementation_approximated(&observed.helpers)))})
            }
            Err(error) => json!({"id":case.id, "error":error}),
        };
        writeln!(output, "{answer}")?;
        output.flush()?;
    }
    Ok(())
}
