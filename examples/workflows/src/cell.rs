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
    content: &lash_kernel_package::LinkedContent,
    link: lash_vocabulary::LinkId,
    library: PreparedLibrary,
    target: Target,
    args: Vec<k::Datum>,
) -> Result<KernelMachine> {
    let pages = Pages::default();
    Ok(KernelMachine::start(
        Program::prepare(
            link,
            content
                .documents
                .iter()
                .map(|(id, document)| (*id, Arc::new(document.clone())))
                .collect(),
            &content.links,
            PageId(k::heap_values::key_digest(
                &content.links[&link].document.to_string().into_bytes(),
            )),
            library,
            k::Selection::Fresh,
        )?,
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

pub(super) fn crossing(
    lowered: &Lowered,
    embedding: &lash_vm_worker::Embedding,
    namespace: &FixtureNamespace,
) -> Result<String> {
    ensure!(
        lowered.document.exports.is_empty(),
        "CELL-PLAIN: cells have no entries"
    );
    let registry = embedding.registry()?;
    let library = embedding.prepared_library()?.clone();
    let (content, link) = namespace.link((*lowered.document).clone())?;
    let mut vm = machine(&content, link, library.clone(), Target::Main, vec![])?;
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
                            calls.push((effect.use_, effect.args.clone()));
                            vm.deliver(effect.wait, Outcome::Completed(k::Datum::Null))?;
                        }
                    }
                }
            }
        }
    }
    let mut text = String::from("cell entries: []\n");
    for (effect, args) in calls {
        writeln!(text, "crossing {effect:?}")?;
        for datum in args {
            render_datum(&datum, registry, &library, &content, &mut text)?;
        }
    }
    Ok(text)
}

fn render_datum(
    datum: &k::Datum,
    registry: &k::FunctionRegistry,
    library: &PreparedLibrary,
    content: &lash_kernel_package::LinkedContent,
    text: &mut String,
) -> Result<()> {
    match datum {
        k::Datum::FunctionValue(value) => {
            value.check(
                registry,
                k::FunctionValueLimits::standard(),
                lash_vocabulary::KernelDataLimits::standard(),
            )?;
            writeln!(
                text,
                "FunctionValue callable={:?} signature={:?}",
                value.callable, value.signature
            )?;
            let mut vm = machine(
                content,
                value.callable.link,
                library.clone(),
                Target::Callable(Box::new((**value).clone())),
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
                render_datum(value, registry, library, content, text)?;
            }
            writeln!(text, "}}")?;
        }
        k::Datum::Map(fields) => {
            writeln!(text, "Map {{")?;
            for (key, value) in fields {
                writeln!(text, "key {key:?}")?;
                render_datum(value, registry, library, content, text)?;
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
                render_datum(value, registry, library, content, text)?;
            }
            writeln!(text, "]")?;
        }
        other => {
            writeln!(text, "{other:?}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FN-FREEZE: authored async adapter bodies contribute their free cell
    /// bindings even when the embedding registry holds only shipped helpers.
    #[test]
    fn a_frozen_async_cell_function_keeps_its_authored_adapter_captures() -> Result<()> {
        let embedding = lash_vm_worker::built_embedding(&Default::default())?;
        let effects = tools()?
            .into_iter()
            .map(|definition| {
                (
                    definition.operation().effect_name(),
                    lash_vocabulary::OperationRequirement::new(
                        lash_vocabulary::json_schema::tool_signature(
                            &definition.contract.input_schema.canonical,
                            &definition.contract.output_schema.canonical,
                        ),
                        definition.contract_id(),
                    ),
                )
            })
            .collect();
        let namespace = FixtureNamespace::new(effects, &Default::default())?;
        let namespaces = namespace.namespaces()?;
        let environment = Environment::new(embedding.library(), &namespaces);
        let lowered = lash_dialect_typescript::lower(
            "const limit=3; const shared=[1]; const add=async(x:number)=>x+limit+shared[0]; await corpus.capture({list:[1,add]}); shared[0]=9;",
            &environment,
            Mode::Cell(CellEnvironment::first()),
        )?;
        let crossed = crossing(&lowered, &embedding, &namespace)?;
        assert!(
            crossed.contains("started with x=2: Float(Float(6.0))"),
            "{crossed}"
        );
        Ok(())
    }
}
