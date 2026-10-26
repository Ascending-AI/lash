//! Document cases whose rules include a host operation as well as execution.
//! Kept out of the dialect printer corpus: editing, carrying bindings and
//! migrating are operations on documents, not source-language constructs.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::Integer;

use lash_kernel_doc::parse_document;
use lash_kernel_vm::{
    CellStart, End, Host, KernelMachine, Machine, PreparedLibrary, Program, Step, Target,
};
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use crate::{Case, CaseBounds, Shard};

mod execution;
mod versions;

include!("protocol_files.rs");

pub(super) fn files() -> &'static [(&'static str, &'static str)] {
    CORPUS_FILES
}

pub(super) fn register_probes(registry: &mut FunctionRegistry) {
    execution::register_probes(registry);
}

mod rule_cases {
    include!("protocol_cases.rs");
}

fn run_rule(rule: &str, text: &str) {
    let shard: Shard = serde_json::from_str(text).expect("decode protocol document shard");
    assert_eq!(shard.rule, rule);
    for case in &shard.cases {
        match rule {
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
    crate::machine::prepare_document(
        &PreparedLibrary::new(registry),
        Arc::new(parse_document(&case.document).expect("case document")),
        None,
    )
    .expect("prepare protocol document")
}

fn start(program: Program, bounds: CaseBounds, session: &crate::pages::Session) -> KernelMachine {
    let mut admission = lash_kernel_check::Environment::new(program.library.registry().as_ref());
    admission.bindings = session.names();
    lash_kernel_check::admit(&program.document, &admission).expect("admit protocol document");
    drop(admission);
    KernelMachine::start(
        program,
        bounds.into(),
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: session.root,
            owner: crate::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(session.source.clone()),
    )
    .expect("start protocol document")
}

#[derive(Default)]
struct World {
    prints: Vec<Datum>,
}

impl Host for World {
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
