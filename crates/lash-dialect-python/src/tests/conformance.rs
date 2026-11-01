//! PY-CORPUS-001: every admitted source keeps its CPython observation and
//! disposition. Refusals, exclusions and ticket-owned failures are not passes.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use lash_kernel_conformance::stress::{Agreement, Mode};

use super::machine::{self, Recorded};

#[derive(Deserialize)]
struct Case {
    path: String,
    source: String,
    cell: Option<String>,
    licence: String,
    status: String,
    reason: String,
}

#[derive(Deserialize)]
struct Manifest {
    licences: BTreeMap<String, String>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Outcome {
    status: String,
    qualifier: String,
    observed: Option<Recorded>,
}

#[derive(Deserialize)]
struct ReferenceCase {
    path: String,
    #[serde(flatten)]
    recorded: Recorded,
}

#[derive(Deserialize)]
struct Reference {
    python: String,
    cases: Vec<ReferenceCase>,
}

#[test]
fn admitted_basics_keep_cpython_observations_and_dispositions() {
    let manifest: Manifest = serde_json::from_str(include_str!("../../conformance/manifest.json"))
        .expect("the reviewed admission manifest");
    let outcomes: BTreeMap<String, Outcome> =
        serde_json::from_str(include_str!("../../conformance/outcomes.json"))
            .expect("the per-case outcome ratchet");
    let reference: Reference =
        serde_json::from_str(include_str!("../../conformance/recorded.json"))
            .expect("witness/record.py --conformance records CPython");
    assert!(reference.python.starts_with("3.12."));
    let references: BTreeMap<_, _> = reference
        .cases
        .iter()
        .map(|case| (case.path.as_str(), &case.recorded))
        .collect();
    assert_eq!(
        references.len(),
        reference.cases.len(),
        "unique oracle paths"
    );
    let paths: BTreeSet<_> = manifest.cases.iter().map(|case| &case.path).collect();
    assert_eq!(paths.len(), manifest.cases.len(), "unique admission paths");
    assert_eq!(
        paths,
        outcomes.keys().collect(),
        "no case leaves the ratchet"
    );
    let admitted: BTreeSet<_> = manifest
        .cases
        .iter()
        .filter(|case| case.status == "admitted")
        .map(|case| case.path.as_str())
        .collect();
    assert_eq!(admitted, references.keys().copied().collect());

    let (mut passing, mut failing, mut refused, mut excluded) = (0, 0, 0, 0);
    let mut problems = Vec::new();
    for case in &manifest.cases {
        let path = &case.path;
        assert!(manifest.licences[&case.licence].contains("Permission is hereby granted"));
        let outcome = &outcomes[path];
        let source = case.cell.as_deref().unwrap_or(&case.source);
        match case.status.as_str() {
            "excluded" => {
                assert_eq!(outcome.status, "excluded", "{path}: cannot exclude a pass");
                assert_eq!(outcome.qualifier, case.reason, "{path}: reviewed reason");
                assert!(!case.reason.is_empty());
                excluded += 1;
                println!("excluded {path}: {}", case.reason);
            }
            "refused" => {
                assert_eq!(outcome.status, "refused", "{path}: cannot refuse a pass");
                assert_eq!(outcome.qualifier, case.reason);
                match machine::lower(source) {
                    Err(diagnostic) if diagnostic.code() == outcome.qualifier => {
                        refused += 1;
                        println!("refused {path}: {}", diagnostic.code());
                    }
                    other => problems.push(format!(
                        "{path}: expected refusal {}, got {other:?}",
                        outcome.qualifier
                    )),
                }
            }
            "admitted" => {
                if let Err(diagnostic) = machine::lower(source) {
                    problems.push(format!(
                        "{path}: admitted case became refused: {diagnostic}"
                    ));
                    continue;
                }
                let actual = machine::run(source, &[]);
                let expected = references[path.as_str()];
                match outcome.status.as_str() {
                    "pass" | "fail" if &actual == expected => {
                        passing += 1;
                        println!("pass {path} (baseline {})", outcome.status);
                    }
                    "fail" if &actual != expected && outcome.observed.as_ref() == Some(&actual) => {
                        assert!(
                            outcome.qualifier == "FIG-5906",
                            "{path}: failure needs its owning ticket"
                        );
                        failing += 1;
                        println!("fail {path}: {}", outcome.qualifier);
                    }
                    _ => {
                        let epochs: Vec<_> = actual.epochs.iter().map(|epoch| serde_json::json!({
                            "delivered": epoch.delivered, "asked": epoch.asked, "logged": epoch.logged,
                        })).collect();
                        problems.push(format!(
                            "{path}: expected {} {}, CPython={expected:?}, actual={}",
                            outcome.status,
                            outcome.qualifier,
                            serde_json::json!({"epochs": epochs, "end": actual.end})
                        ));
                    }
                }
            }
            other => panic!("{path}: unknown admission {other}"),
        }
    }
    println!(
        "admitted={} passing={passing} failing={failing} refused={refused} excluded={excluded}",
        admitted.len()
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// K-CLO-001: folding a conditional keeps each source function's calling convention.
#[test]
fn conditional_functions_keep_their_written_signatures() {
    for source in [
        "f = (lambda left: left) if True else (lambda right: right)",
        "(lambda left: left) if True else (lambda right: right)",
        "holder = {}\nholder['f'] = (lambda left: left) if True else (lambda right: right)",
    ] {
        let lowered = super::machine::lower(source).unwrap();
        assert_eq!(lowered.written.len(), 2);
        for site in lowered.written.keys() {
            let mut node = lash_kernel_doc::Node::Block(
                lowered.document.main.as_ref().expect("authored main"),
            );
            for child in &site.path {
                node = node.children()[*child as usize];
            }
            assert!(
                matches!(
                    node,
                    lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Closure(_))
                ),
                "{site}"
            );
        }
        assert!(
            lowered
                .written
                .values()
                .any(|written| written.signature.contains("left"))
        );
        assert!(
            lowered
                .written
                .values()
                .any(|written| written.signature.contains("right"))
        );
    }
}

/// FIG-7589: every case the corpus records as passing runs alike plain and
/// in `mode` (end, prints, requests, exact charge, carried session), except
/// the divergences the mode's ratchet shard names, each of which still
/// diverges in the field it records.
fn stress_mode(mode: Mode, shard: &str) {
    use lash_kernel_conformance::stress::{self, Drive};
    let manifest: Manifest = serde_json::from_str(include_str!("../../conformance/manifest.json"))
        .expect("the reviewed admission manifest");
    let outcomes: BTreeMap<String, Outcome> =
        serde_json::from_str(include_str!("../../conformance/outcomes.json"))
            .expect("the per-case outcome ratchet");
    let plain_library = machine::installed().prepared.clone();
    let mode_library = match mode {
        Mode::ParkEverywhere => plain_library.clone(),
        Mode::BodiesOnly => lash_kernel_vm::PreparedLibrary::with_helpers(
            std::sync::Arc::new(
                lash_kernel_conformance::installed_bodies().expect("installed kernel bodies"),
            ),
            std::sync::Arc::clone(
                &lash_kernel_releases::helper_release()
                    .expect("build helpers")
                    .helpers,
            ),
        ),
    };
    let started = std::time::Instant::now();
    let observations: Vec<(String, Agreement)> = manifest
        .cases
        .iter()
        .map(|case| {
            let source = case.cell.as_deref().unwrap_or(&case.source);
            let agreement = if case.status == "admitted" && outcomes[&case.path].status == "pass" {
                let plain = machine::stress_observation(source, &plain_library, &Drive::Straight)
                    .unwrap_or_else(|error| panic!("{}: the plain run: {error}", case.path));
                let drive = match mode {
                    Mode::ParkEverywhere => Drive::ParkEverywhere {
                        stops: stress::stops(&case.path, plain.charged),
                    },
                    Mode::BodiesOnly => Drive::Straight,
                };
                Agreement::of(
                    &plain,
                    machine::stress_observation(source, &mode_library, &drive),
                )
            } else {
                Agreement::Skipped
            };
            println!("mode\t{}\t{}\t{agreement:?}", mode.name(), case.path);
            (case.path.clone(), agreement)
        })
        .collect();
    let count = |wanted: fn(&Agreement) -> bool| {
        observations
            .iter()
            .filter(|(_, agreement)| wanted(agreement))
            .count()
    };
    println!(
        "mode-total\t{}\tagreed={} diverged={} skipped={}\t{:.1}s",
        mode.name(),
        count(|agreement| *agreement == Agreement::Agreed),
        count(|agreement| matches!(agreement, Agreement::Diverged { .. })),
        count(|agreement| *agreement == Agreement::Skipped),
        started.elapsed().as_secs_f64()
    );
    let problems = stress::ratchet(&observations, &stress::known_divergences(shard));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
#[ignore = "a nightly stress mode: run with --ignored"]
fn the_corpus_runs_alike_parked_everywhere() {
    stress_mode(
        Mode::ParkEverywhere,
        include_str!("../../conformance/divergences/park-everywhere.tsv"),
    );
}

#[test]
#[ignore = "a nightly stress mode: run with --ignored"]
fn the_corpus_runs_alike_on_kernel_bodies() {
    stress_mode(
        Mode::BodiesOnly,
        include_str!("../../conformance/divergences/bodies-only.tsv"),
    );
}
