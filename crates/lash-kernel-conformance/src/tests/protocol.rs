//! Document cases whose rules include a host operation as well as execution.
//! Kept out of the dialect printer corpus: editing, carrying bindings and
//! migrating are operations on documents, not source-language constructs.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Site, Timestamp, Unit, parse_document,
};
use lash_kernel_vm::{
    Bindings, End, Host, KernelMachine, Machine, PreparedLibrary, Program, Start, Step, Target,
};

use crate::{Case, CaseBounds, Shard};

mod edits;
mod execution;
mod versions;

include!("protocol_files.rs");

pub(super) fn files() -> &'static [(&'static str, &'static str)] {
    CORPUS_FILES
}

mod rule_cases {
    include!("protocol_cases.rs");
}

fn run_rule(rule: &str, text: &str) {
    let shard: Shard = serde_json::from_str(text).expect("decode protocol document shard");
    assert_eq!(shard.rule, rule);
    for case in &shard.cases {
        match rule {
            rule if rule.starts_with("K-EDIT-") => edits::check(rule, case),
            rule if rule.starts_with("K-VER-") => versions::check(rule, case),
            _ => execution::check(rule, case),
        }
    }
}

fn registry() -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).expect("machine functions");
    Arc::new(registry)
}

fn main(path: &[u32]) -> Site {
    Site {
        unit: Unit::Main,
        path: path.to_vec(),
    }
}

fn program(case: &Case, registry: Arc<FunctionRegistry>) -> Program {
    Program {
        document: Arc::new(parse_document(&case.document).expect("case document")),
        library: PreparedLibrary::new(registry),
    }
}

fn start(program: Program, bounds: CaseBounds, bindings: Bindings) -> KernelMachine {
    let mut admission = lash_kernel_check::Environment::new(program.library.registry().as_ref());
    admission.effects = program.document.manifest.effects.clone();
    admission.bindings = bindings.variables.keys().cloned().collect();
    lash_kernel_check::admit(&program.document, &admission).expect("admit protocol document");
    KernelMachine::start(
        program,
        bounds.into(),
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings,
        },
    )
    .expect("start protocol document")
}

#[derive(Default)]
struct World {
    prints: Vec<Datum>,
}

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        panic!("unexpected clock")
    }
    fn random(&mut self) -> u64 {
        panic!("unexpected random")
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("unexpected read")
    }
    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn end(machine: &mut KernelMachine, world: &mut World) -> End {
    loop {
        match machine.run(world, u64::MAX).expect("run protocol document") {
            Step::Slice => {}
            Step::Ended(end) => return end,
            Step::Parked(park) => panic!("unexpected park: {park:?}"),
        }
    }
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

/// Gate 0: these owners must supply cases, rather than defer their rules.
#[test]
fn owned_rules_have_cases_instead_of_pending_owners() {
    let pending: Vec<crate::PendingRule> =
        serde_json::from_str(include_str!("../../pending.json")).unwrap();
    let shards = super::corpus_shards();
    let rules = (1..=10).map(|n| format!("K-EDIT-{n:03}")).chain(
        [
            "K-BND-003",
            "K-CHG-003",
            "K-CHG-004",
            "K-CHG-005",
            "K-CHG-006",
            "K-LIB-010",
            "K-SES-001",
            "K-SES-002",
            "K-SES-003",
            "K-SITE-001",
            "K-SITE-002",
            "K-SITE-003",
            "K-VER-003",
            "K-VER-004",
            "K-VER-005",
            "K-EFF-011",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    for rule in rules {
        assert!(
            shards
                .iter()
                .any(|shard| shard.rule == rule && !shard.cases.is_empty()),
            "{rule} has no case"
        );
        assert!(
            !pending.iter().any(|row| row.rule == rule),
            "{rule} still has an owner instead of completed coverage"
        );
    }
}
