//! Real VM copy-out, without publication. Pins the frozen FunctionValue's
//! structure; content identity stability belongs to FN-ID's owning law.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::heap::*;
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Step, Target,
};

use super::*;

#[derive(Clone, Default)]
struct Pages(Rc<RefCell<BTreeMap<PageId, PageAnswer>>>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
                    .borrow()
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut bytes = body.kind.0.to_be_bytes().to_vec();
        bytes.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(k::heap_values::key_digest(&bytes)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
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

struct Quiet;
impl Host for Quiet {
    fn clock(&mut self) -> k::Timestamp {
        k::Timestamp {
            nanoseconds: k::Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &k::Handle, _: &k::Datum) -> Result<k::Datum, k::ErrorDatum> {
        Err(k::ErrorDatum {
            kind: "no_projection".into(),
            message: "corpus has no projection".into(),
            data: k::Datum::Null,
        })
    }
    fn print(&mut self, _: &k::Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

fn machine(
    document: Arc<Document>,
    library: PreparedLibrary,
    target: Target,
    args: Vec<k::Datum>,
) -> Result<KernelMachine> {
    let pages = Pages::default();
    Ok(KernelMachine::start(
        Program {
            closure: PageId([7; 32]),
            document,
            library,
        },
        BOUNDS,
        CellStart {
            target,
            args,
            session: None,
            owner: PageId([9; 32]),
            modes: Default::default(),
            host: vec![],
        },
        Box::new(pages),
    )?)
}

pub(super) fn crossing(lowered: &Lowered, registry: Arc<k::FunctionRegistry>) -> Result<String> {
    ensure!(
        lowered.document.entries.is_empty(),
        "CELL-PLAIN: cells have no entries"
    );
    let library = PreparedLibrary::new(registry.clone());
    let mut vm = machine(
        lowered.document.shared().clone(),
        library.clone(),
        Target::Main,
        vec![],
    )?;
    let mut calls = Vec::new();
    loop {
        match vm.run(&mut Quiet, u64::MAX)? {
            Step::Slice => {}
            Step::Ended(End::Finished(_)) => break,
            Step::Ended(other) => anyhow::bail!("cell ended {other:?}"),
            Step::Parked(park) => {
                for request in park.requests {
                    match request {
                        Request::Effect(effect) => {
                            calls.push((effect.effect.clone(), effect.args.clone()));
                            vm.deliver(effect.wait, Outcome::Completed(k::Datum::Null))?;
                        }
                        Request::Sleep(sleep) => {
                            vm.deliver(sleep.wait, Outcome::Elapsed)?;
                        }
                    }
                }
            }
        }
    }
    let mut text = String::from("cell entries: []\n");
    for (effect, args) in calls {
        writeln!(text, "crossing {effect}")?;
        for datum in args {
            render_datum(&datum, &registry, &library, &mut text)?;
        }
    }
    Ok(text)
}

fn render_datum(
    datum: &k::Datum,
    registry: &k::FunctionRegistry,
    library: &PreparedLibrary,
    text: &mut String,
) -> Result<()> {
    match datum {
        k::Datum::FunctionValue(value) => {
            value.check(
                registry,
                k::FunctionValueLimits::standard(),
                k::KernelDataLimits::standard(),
            )?;
            writeln!(
                text,
                "FunctionValue entry={} signature={:?}",
                value.entry,
                value.signature()
            )?;
            // Extraction normalizes local and capture names itself. Replace
            // helper content ids by their names only in this structural pin.
            let mut body = k::print_document(&value.document);
            for (id, name) in &value.document.manifest.functions {
                body = body.replace(&format!("@{id}"), &format!("@{name}"));
            }
            text.push_str(&body);
            let mut vm = machine(
                Arc::new(value.document.clone()),
                library.clone(),
                Target::Entry(value.entry.clone()),
                vec![k::Datum::Int(k::Integer::from(2))],
            )?;
            let result = loop {
                match vm.run(&mut Quiet, u64::MAX)? {
                    Step::Slice => {}
                    Step::Ended(End::Finished(done)) => break done.result,
                    other => anyhow::bail!("frozen function did not finish: {other:?}"),
                }
            };
            writeln!(text, "started with x=2: {result:?}")?;
        }
        k::Datum::Record(fields) => {
            writeln!(text, "Record {{")?;
            for (key, value) in fields {
                writeln!(text, "field {key}")?;
                render_datum(value, registry, library, text)?;
            }
            writeln!(text, "}}")?;
        }
        k::Datum::Map(fields) => {
            writeln!(text, "Map {{")?;
            for (key, value) in fields {
                writeln!(text, "key {key:?}")?;
                render_datum(value, registry, library, text)?;
            }
            writeln!(text, "}}")?;
        }
        k::Datum::List(values) | k::Datum::Tuple(values) | k::Datum::Set(values) => {
            let kind = match datum {
                k::Datum::List(_) => "List",
                k::Datum::Tuple(_) => "Tuple",
                _ => "Set",
            };
            writeln!(text, "{kind} [")?;
            for value in values {
                render_datum(value, registry, library, text)?;
            }
            writeln!(text, "]")?;
        }
        other => {
            writeln!(text, "{other:?}")?;
        }
    }
    Ok(())
}
