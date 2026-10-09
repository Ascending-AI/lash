//! K-DIALECT-001/002 over every admitted conformance document.
use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_conformance::{ExpectedEnd, MachineRunner, check_case, load_corpus};
use lash_kernel_dialect::{Environment, NamedLibrary};
use lash_kernel_doc::{parse_document, print_document};
use lash_kernel_vm::KernelMachine;

// Reuse the owning corpus index so new shards enter this law with its own laws.
include!("../../../lash-kernel-conformance/src/corpus_files.rs");

const SUPPLEMENTS: &[(&str, &str)] = &[
    (
        "K-KEY-002.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-KEY-002.json"),
    ),
    (
        "K-KEY-003.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-KEY-003.json"),
    ),
    (
        "K-KEY-004.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-KEY-004.json"),
    ),
    (
        "K-KEY-005.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-KEY-005.json"),
    ),
    (
        "K-VAL-001.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-VAL-001.json"),
    ),
    (
        "K-VAL-026.json",
        include_str!("../../../lash-kernel-lib/corpus/numbers/K-VAL-026.json"),
    ),
    (
        "K-LJSON-005.json",
        include_str!("../../../lash-kernel-conformance/supplements/json/K-LJSON-005.json"),
    ),
];

#[test]
fn every_admitted_conformance_document_preserves_observations_when_printed_and_lowered() {
    let shards = load_corpus(CORPUS_FILES.iter().chain(SUPPLEMENTS).copied())
        .expect("decode every conformance document shard");
    let registry = Arc::new(super::kernel_registry());
    let library = NamedLibrary::from_registry(&registry).expect("corpus library names");
    let mut runner = MachineRunner::<KernelMachine>::new(Arc::clone(&registry));
    let mut executed = 0;
    let mut refusals = 0;
    let mut failures = Vec::new();
    for shard in &shards {
        for case in &shard.cases {
            // Parsing and admission refusals retain their own corpus oracle.
            if case.expected.end == ExpectedEnd::Refused {
                refusals += 1;
                continue;
            }
            let result = (|| -> Result<(), String> {
                let original = check_case(&mut runner, case).map_err(|e| e.to_string())?;
                let document = parse_document(&case.document).map_err(|e| e.to_string())?;
                let effects = case
                    .environment
                    .effects
                    .as_ref()
                    .unwrap_or(&document.manifest.effects);
                let bindings = BTreeSet::new();
                let environment = Environment {
                    library: &library,
                    effects,
                    bindings: &bindings,
                };
                let source = crate::print(&document).map_err(|e| e.to_string())?;
                let lowered = crate::lower(&source, &environment).map_err(|e| e.to_string())?;
                let mut clone = case.clone();
                clone.document = print_document(&lowered.document);
                let roundtrip = check_case(&mut runner, &clone).map_err(|e| e.to_string())?;
                if original != roundtrip {
                    return Err(format!("original {original:?}\nroundtrip {roundtrip:?}"));
                }
                Ok(())
            })();
            executed += 1;
            if let Err(error) = result {
                failures.push(format!("{} {}: {error}", shard.rule, case.name));
            }
        }
    }
    eprintln!(
        "printer corpus: shards={} admitted_cases={executed} refusal_cases={refusals} failed_cases={}",
        shards.len(),
        failures.len()
    );
    assert!(executed > 0, "the corpus must contain admitted documents");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
