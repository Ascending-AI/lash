//! The print → reparse → admit round-trip law over every corpus.
//!
//! For every program: lower and admit it (link), project the admitted
//! artifact through the lens, print the graph, then reparse and admit the
//! printed source. The printed program must admit to the same `module_ref`
//! and `source_identity` as the original: the lens's text is the program, not
//! a paraphrase of it. Where the printer cannot spell a program it refuses
//! with a typed [`TypeScriptSourceError`], and every such refusal is a row of
//! a `refusals/<shard>.tsv` file with its reason (FIG-3727): a row lives in
//! the file its id's shard names, so `differential:<shard>:<n>` rows and
//! `<shard>:<n>` rows never share a file with another lane's. The allowlist
//! is a ratchet: a listed program that now round-trips, or a listed refusal
//! that changed, fails until the row is deleted or corrected.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;

use lash_typescript::workflow_graph::{
    GraphRenderError, typescript_program_source, workflow_graph_from_artifact,
    workflow_graph_to_source_in_session,
};

use super::corpora::{self, CorpusProgram};

/// Every `refusals/<shard>.tsv`, sorted by shard. Reading the directory is
/// what lets a new `differential:<shard>:<n>` id join the allowlist without
/// an edit to any shared file.
fn refusal_shards() -> Vec<(String, String)> {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus_laws/refusals");
    let mut names = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .expect("a refusals entry")
                .file_name()
                .into_string()
                .expect("UTF-8 refusals name")
        })
        .filter(|name| name.ends_with(".tsv"))
        .collect::<Vec<_>>();
    names.sort();
    assert!(!names.is_empty(), "{} is empty", directory.display());
    names
        .into_iter()
        .map(|name| {
            let path = directory.join(&name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (name.trim_end_matches(".tsv").to_owned(), text)
        })
        .collect()
}

/// Programs that break the law through an open lens defect, each with the
/// ticket that owns it. A ratchet like the refusal rows: a listed program
/// that stops violating the law fails until its row is removed.
const OPEN_VIOLATIONS: [(&str, &str); 0] = [];

/// What the round trip did with one program.
#[derive(Debug, PartialEq)]
enum Trip {
    /// The printed program admits to the original's module and identity.
    Agrees,
    /// The printer refused, typed.
    Refused(String),
    /// Anything else: a law violation.
    Violates(String),
}

fn round_trip(program: &CorpusProgram) -> Result<Trip, String> {
    let environment = program.environment();
    let linked = lash_typescript::link(&program.source, &environment)
        .map_err(|error| format!("does not admit: {error}"))?;
    // The lens's canonical text is the printer's spelling of the admitted
    // program; a program the printer cannot spell is refused here, typed,
    // before the graph carries the refusal as an opaque node's placeholder.
    // That text is itself a print of the admitted program (the one node
    // spans address), so it re-admits to the same module as well.
    let canonical = match typescript_program_source(linked.artifact.ir()) {
        Ok(canonical) => canonical,
        Err(error) => return Ok(Trip::Refused(error.to_string())),
    };
    if let Some(violation) = readmits(&linked, &canonical, &environment, "canonical text") {
        return Ok(Trip::Violates(violation));
    }
    let graph = workflow_graph_from_artifact(&linked.artifact);
    let printed = match workflow_graph_to_source_in_session(&graph, &program.globals) {
        Ok(printed) => printed,
        Err(GraphRenderError::CanonicalSource(error)) => {
            return Ok(Trip::Refused(error.to_string()));
        }
        Err(error) => {
            return Ok(Trip::Violates(format!(
                "the admitted view does not render: {error}"
            )));
        }
    };
    Ok(readmits(&linked, &printed, &environment, "printed view")
        .map_or(Trip::Agrees, Trip::Violates))
}

/// Whether `printed` re-admits to `linked`'s module and identity, or how it
/// does not.
fn readmits(
    linked: &lashlang::LinkedModule,
    printed: &str,
    environment: &lashlang::LashlangHostEnvironment,
    what: &str,
) -> Option<String> {
    let relinked = match lash_typescript::link(printed, environment) {
        Ok(relinked) => relinked,
        Err(error) => {
            return Some(format!(
                "the {what} does not admit: {error}\n--- printed\n{printed}"
            ));
        }
    };
    let mut differences = Vec::new();
    if relinked.artifact.module_ref() != linked.artifact.module_ref() {
        differences.push("module_ref");
    }
    if relinked.artifact.source_identity() != linked.artifact.source_identity() {
        differences.push("source_identity");
    }
    (!differences.is_empty()).then(|| {
        format!(
            "the {what} admits to a different {} (first IR difference: {})\n--- printed\n{printed}",
            differences.join(" and "),
            super::first_difference(
                &serde_json::to_value(linked.artifact.ir()).expect("the IR serializes"),
                &serde_json::to_value(relinked.artifact.ir()).expect("the IR serializes"),
                "ir",
            )
            .unwrap_or_else(|| "none; the refs differ outside the IR".to_string()),
        )
    })
}

/// `program id` → (refusal, reason), the union of every shard. An id may
/// appear in only one file: a `differential:<shard>:<n>` or `<shard>:<n>`
/// row's file is `<shard>.tsv`.
fn allowlist() -> BTreeMap<String, (String, String)> {
    let mut rows = BTreeMap::new();
    for (shard, text) in refusal_shards() {
        for line in text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let [id, refusal, reason] = line.split('\t').collect::<Vec<_>>()[..] else {
                panic!("malformed refusal row: {line}")
            };
            assert!(
                id.starts_with(&format!("{shard}:"))
                    || id.starts_with(&format!("differential:{shard}:")),
                "{id}: refusals/{shard}.tsv holds a row outside its shard"
            );
            assert!(reason.len() > 10, "{id}: a refusal row gives its reason");
            assert!(
                rows.insert(id.to_owned(), (refusal.to_owned(), reason.to_owned()))
                    .is_none(),
                "{id} is listed twice"
            );
        }
    }
    rows
}

#[test]
fn every_corpus_program_round_trips_or_is_an_allowlisted_refusal() {
    let mut allowlist = allowlist();
    let mut open_violations = OPEN_VIOLATIONS.into_iter().collect::<BTreeMap<_, _>>();
    let mut failures = Vec::new();
    let mut agreed = 0usize;
    for program in corpora::all() {
        let listed = allowlist.remove(program.id.as_str());
        if let Some(ticket) = open_violations.remove(program.id.as_str()) {
            match round_trip(&program) {
                Ok(Trip::Violates(_)) => {}
                other => failures.push(format!(
                    "{}: no longer violates the law ({other:?}); remove its OPEN_VIOLATIONS row ({ticket})",
                    program.id
                )),
            }
            continue;
        }
        match (round_trip(&program), listed) {
            (Ok(Trip::Agrees), None) => agreed += 1,
            (Ok(Trip::Agrees), Some(_)) => failures.push(format!(
                "{}: round-trips now; delete its refusal row (ratchet)",
                program.id
            )),
            (Ok(Trip::Refused(refusal)), Some((listed, _))) if refusal == listed => {}
            (Ok(Trip::Refused(refusal)), Some((listed, _))) => failures.push(format!(
                "{}: refuses `{refusal}`, but its row names `{listed}`",
                program.id
            )),
            (Ok(Trip::Refused(refusal)), None) => failures.push(format!(
                "{}: the printer refuses `{refusal}` and no row allows it\n{}",
                program.id, program.source
            )),
            (Ok(Trip::Violates(violation)), _) => failures.push(format!(
                "{}: {violation}\n--- source\n{}",
                program.id, program.source
            )),
            (Err(error), _) => failures.push(format!("{}: {error}", program.id)),
        }
    }
    for id in allowlist
        .keys()
        .map(String::as_str)
        .chain(open_violations.keys().copied())
    {
        failures.push(format!("{id}: the row names no corpus program"));
    }
    assert!(
        failures.is_empty(),
        "{} programs round-trip; {} do not:\n{}",
        agreed,
        failures.len(),
        failures.join("\n")
    );
}
