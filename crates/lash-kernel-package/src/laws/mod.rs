//! The laws of packages (FIG-6917), over the `dataframe` fixture built
//! against a kernel package of lash's kernel library, and run on the real
//! machine over test-owned pages.

mod build;
mod shape;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionDefinition, FunctionId, FunctionRegistry, Handle, Timestamp,
    parse_definition, parse_document,
};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Program, RunError, Step, Target,
};

use crate::{Package, PackagePath, Root};

/// Registers lash's kernel library, natives included.
fn register_kernel(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_kernel_vm::register_machine_functions(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_numbers(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_text_json(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_collections(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_brands(registry).map_err(|error| error.to_string())?;
    Ok(())
}

/// A definition outside the kernel library, under its `text` root, that
/// makes another kernel artifact.
const SHOUT: &str =
    "function text.shout(value: Text) -> Text\nkernel 1\ncharge 1\nbody {\n  return value\n}\n";

/// The kernel package over lash's kernel library, and with `more`
/// definitions beside it.
fn kernel_with(more: &[&str]) -> Package {
    let mut probe = FunctionRegistry::new();
    register_kernel(&mut probe).unwrap();
    let roots: BTreeSet<Root> = probe
        .iter()
        .map(|(_, function)| {
            let name = function.definition().name.as_str();
            Root::new(name.split('.').next().unwrap_or(name))
        })
        .collect();
    Package::lash(
        PackagePath::parse("kernel").unwrap(),
        "1.0",
        roots,
        &[],
        |registry| {
            register_kernel(registry)?;
            for text in more {
                let definition = parse_definition(text).map_err(|error| error.to_string())?;
                registry
                    .register(definition, None)
                    .map_err(|error| error.to_string())?;
            }
            Ok(registry
                .iter()
                .map(|(_, function)| FunctionDefinition::clone(function.definition()))
                .collect())
        },
    )
    .unwrap()
}

fn kernel() -> Package {
    kernel_with(&[])
}

/// A registry that runs `packages` over lash's kernel library, every body
/// as written.
fn runtime(packages: &[&Package]) -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();
    register_kernel(&mut registry).unwrap();
    for package in packages {
        package
            .code()
            .register_into(&mut registry, &|_| None)
            .unwrap();
    }
    Arc::new(registry)
}

/// Runs `main`, a document body calling `functions` by their names.
fn run(registry: &Arc<FunctionRegistry>, functions: &[FunctionId], main: &str) -> End {
    let mut text = String::from("kernel 1\nnumbers by_spelling\n");
    for function in functions {
        let name = &registry.get(function).unwrap().definition().name;
        text.push_str(&format!("use {name} = @{function}\n"));
    }
    text.push_str(&format!("main {{\n{main}\n}}\n"));
    let program = Program {
        closure: PageId([8; 32]),
        document: Arc::new(parse_document(&text).unwrap()),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::clone(registry)),
    };
    let start = CellStart {
        owner: PageId([9; 32]),
        target: Target::Main,
        args: Vec::new(),
        session: None,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine =
        <KernelMachine as Machine>::start(program, BOUNDS, start, Box::<Pages>::default()).unwrap();
    loop {
        match machine.run(&mut World, u64::MAX).unwrap() {
            Step::Ended(end) => return end,
            Step::Slice => {}
            Step::Parked(park) => panic!("the laws' documents never wait: {park:?}"),
        }
    }
}

/// The value a run returned.
fn finished(end: End) -> Datum {
    match end {
        End::Finished(done) => done.result,
        end => panic!("expected a value: {end:?}"),
    }
}

/// The error a run raised and did not catch.
fn raised(end: End) -> ErrorDatum {
    match end {
        End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        }) => *error,
        end => panic!("expected an uncaught error: {end:?}"),
    }
}

/// The identities of `names`, each as `package` defines it, then as the
/// kernel library does.
fn functions(
    registry: &FunctionRegistry,
    packages: &[&Package],
    names: &[&str],
) -> Vec<FunctionId> {
    names
        .iter()
        .map(|name| {
            packages
                .iter()
                .find_map(|package| package.function(name))
                .or_else(|| registry.named(name).next())
                .unwrap_or_else(|| panic!("`{name}` is defined"))
        })
        .collect()
}

/// `datum` as a kernel text literal.
fn literal(datum: &Datum) -> String {
    let join = |items: &[Datum]| items.iter().map(literal).collect::<Vec<_>>().join(", ");
    match datum {
        Datum::Null => "null".into(),
        Datum::Absent => "absent".into(),
        Datum::Bool(value) => value.to_string(),
        Datum::Int(value) => value.to_string(),
        Datum::Text(text) => format!("{text:?}"),
        Datum::Bytes(bytes) => format!("b\"{}\"", bytes.to_hex()),
        Datum::Tuple(items) if items.len() == 1 => format!("({},)", literal(&items[0])),
        Datum::Tuple(items) => format!("({})", join(items)),
        Datum::List(items) => format!("[{}]", join(items)),
        Datum::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", literal(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => panic!("no literal for {other:?}"),
    }
}

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct World;

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: 0.into(),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        unreachable!("the laws' documents read nothing")
    }
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Pages held in memory, each by its content's digest.
#[derive(Clone, Default)]
struct Pages {
    pages: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
}

impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.pages
                    .borrow()
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.pages.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
