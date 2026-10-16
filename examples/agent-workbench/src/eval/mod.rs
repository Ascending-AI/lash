//! Unpublished operator evals: dataset → isolated host runs → evaluator reports.
//! Runtime correctness remains the deterministic conformance layer's concern.
mod host;
mod manifest;
mod report;
mod runner;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use manifest::{Case, Manifest, Model};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Job {
    case: Case,
    model: Model,
    directory: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Evidence {
    messages: Vec<lash::messages::Message>,
    turn_index: usize,
    settled_view: Value,
    trace: Vec<lash::tracing::TraceRecord>,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

pub(crate) async fn cli(args: Vec<String>) -> Result<()> {
    match args.as_slice() {
        [command, manifest, output] if command == "run" => {
            let source = std::fs::read_to_string(manifest)?;
            let dataset = Manifest::parse(&source)?;
            let executable = runner::Executable::current()?;
            let judge = report::Judge::Live;
            runner::run(&dataset, &source, Path::new(output), &executable, &judge).await?;
            println!("eval report: {output}/report.html");
            Ok(())
        }
        [command, manifest, case_id, bundle] if command == "judge" => {
            let dataset = Manifest::parse(&std::fs::read_to_string(manifest)?)?;
            let case = dataset
                .cases
                .iter()
                .find(|case| &case.id == case_id)
                .context("unknown case")?;
            // Re-evaluate an exported bundle using exactly the same evaluators.
            // Keep captured evidence immutable; write another evaluator report.
            let path = Path::new(bundle);
            let evidence = serde_json::from_slice(&std::fs::read(path.join("evidence.json"))?)?;
            let cut = serde_json::from_slice(&std::fs::read(path.join("recovery.json"))?)?;
            let gates = report::gates(case, &evidence, cut);
            let score = report::Judge::Live
                .score(case, &dataset.judge, &evidence, &gates)
                .await?;
            write_json(&path.join("reevaluation.json"), &score)
        }
        _ => bail!(
            "usage: agent-workbench eval run MANIFEST.toml NEW_OUTPUT_DIR | eval judge MANIFEST.toml CASE_ID BUNDLE_DIR (Python 3.11+ required)"
        ),
    }
}

pub(crate) async fn worker_cli(args: Vec<String>) -> Result<()> {
    let [job, phase] = args.as_slice() else {
        bail!("eval-worker needs JOB and initial|resume");
    };
    host::worker(Path::new(job), phase, false).await
}
