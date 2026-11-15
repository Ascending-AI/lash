//! Literal TS obligations that survive removal of the development differential.
use std::sync::Arc;

use lash_kernel_conformance::{Case, Shard};

#[test]
fn ts_entry_conventions_and_live_iteration_follow_their_literals() {
    let conventions: Shard = serde_json::from_str(include_str!(
        "../../../lash-kernel-conformance/corpus/dialect/K-DIALECT-002.json"
    ))
    .expect("dialect literals");
    let iteration: Shard = serde_json::from_str(include_str!(
        "../../../lash-kernel-conformance/probes/dialect/r56-t1.json"
    ))
    .expect("live iteration literal");
    let cases: Vec<Case> = conventions
        .cases
        .into_iter()
        .filter(|case| case.name.starts_with("ts_"))
        .chain(iteration.cases)
        .collect();
    assert_eq!(
        cases.len(),
        5,
        "four entry rules and the live iterator rule"
    );
    for case in cases {
        let document =
            Arc::new(lash_kernel_doc::parse_document(&case.document).expect("literal document"));
        for slice in [1, 7, 50] {
            let program =
                super::linked::program(Arc::clone(&document), super::machine::prepared().clone());
            let mut case = case.clone();
            case.environment.slice = slice;
            lash_kernel_conformance::check_program::<lash_kernel_vm::CoreMachine>(
                program,
                &case,
                lash_kernel_vm::Charge::Exact,
            )
            .unwrap_or_else(|error| panic!("slice {slice}: {error}"));
        }
    }
}
