//! The document differential over every corpus (FIG-5578).
//!
//! For every program: lower it, admit it, and run every entry of the module
//! against a deterministic host. Then publish the same program from its
//! workflow document, with no source involved (the draft document through
//! its wire encoding, opened for editing and admitted; and the admitted
//! document admitted again), through `lash_vm::admit_workflow_graph`, and
//! run that. A host must not be able to tell the two apart: same module
//! identity, same admitted document, same outcomes and failure values, same
//! abilities asked in the same order, same execution observations, same
//! final state. Every site a run reports must be a site of the module's
//! document.
//!
//! The law itself is `lash_vm::testing::differential`, shared with the
//! generated-program laws in `lash-vm`'s property suite.

use std::collections::BTreeMap;

use lash_vm::ExecutionOutcome;
use lash_vm::testing::differential;

use super::corpora::{self, CorpusProgram};

fn runs_like_its_source(program: &CorpusProgram) -> Result<differential::SourceRun, String> {
    let environment = program.environment();
    let lowered = lash_typescript::testing::lower_for_link(&program.source, &environment)
        .map_err(|error| format!("does not lower: {error}"))?;
    let run = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(differential::document_runs_like_its_source(
            &lowered,
            &environment,
        ))?;
    differential::observed_sites_are_in_the_document(&run)?;
    Ok(run)
}

/// What the programs of one corpus did when they ran.
#[derive(Debug, Default)]
struct Tally {
    programs: usize,
    /// Entries that reached a `finish` or ran to their end.
    completed: usize,
    /// Entries that failed their process or stopped on a runtime error.
    stopped: usize,
    effects: usize,
}

#[test]
fn every_corpus_program_published_from_its_document_runs_like_its_source() {
    let mut failures = Vec::new();
    let mut tally = BTreeMap::<String, Tally>::new();
    for program in corpora::with_workflows() {
        let corpus = program.id.split(':').next().unwrap_or_default().to_string();
        match runs_like_its_source(&program) {
            Ok(source) => {
                let row = tally.entry(corpus).or_default();
                row.programs += 1;
                for entry in &source.run.entries {
                    match entry.outcome {
                        Ok(ExecutionOutcome::Finished(_) | ExecutionOutcome::Continued) => {
                            row.completed += 1;
                        }
                        Ok(ExecutionOutcome::Failed(_)) | Err(_) => row.stopped += 1,
                    }
                    row.effects += entry.effects.len();
                }
            }
            Err(error) => failures.push(format!(
                "{}: {error}\n--- source\n{}",
                program.id, program.source
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} programs do not run like their source:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // The law is not vacuous: the workflow corpora run, perform effects and
    // complete, and some programs elsewhere stop on a failure, whose value
    // the two runs then had to agree on.
    for corpus in ["ai-workflow", "golden", "typescript-host-flows"] {
        let row = tally
            .get(corpus)
            .unwrap_or_else(|| panic!("no {corpus} program ran"));
        assert!(row.completed > 0 && row.effects > 0, "{corpus}: {row:?}");
    }
    let workflows = &tally["ai-workflow"];
    assert_eq!(workflows.programs, super::ai_workflows::ALL.len());
    assert_eq!(
        workflows.stopped, 0,
        "every AI-style workflow and each process it defines runs to completion: {workflows:?}"
    );
    assert!(
        tally.values().any(|row| row.stopped > 0),
        "no program stopped on a failure: {tally:#?}"
    );
}
