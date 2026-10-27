//! A copy-loop rewrite certified against its source and its charged read.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_dialect::Library as _;
use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::{Expr, Formula, FunctionDefinition, Rhs, Stmt};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_value::package::Link;
use lash_kernel_value::{FunctionId, Name};
use lash_kernel_vm::{
    CellStart, Host, KernelMachine, Machine, PreparedLibrary, Program, Step, Target,
};

use crate::library::Library;

fn pattern(library: &Library, source: &str) -> Option<Vec<FunctionDefinition>> {
    lash_kernel_dialect::define_functions(source, &mut library.names.clone(), &[]).ok()
}

fn closure<'a>(
    definition: &FunctionDefinition,
    definitions: impl IntoIterator<Item = &'a FunctionDefinition>,
) -> Option<&'a FunctionDefinition> {
    let Stmt::Let {
        value: Rhs::Expr(Expr::Closure(closure)),
        ..
    } = definition.body()?.block.get(1)?
    else {
        return None;
    };
    definitions
        .into_iter()
        .find(|candidate| candidate.identity() == closure.function)
}

pub(super) fn emit(
    library: &Library,
    definition: &FunctionDefinition,
    ident: &str,
) -> Option<(String, String)> {
    let (source, name, dense) = match definition.name.as_str() {
        "ts.array.concat" => (include_str!("concat.kernel"), "law.concat_pattern", false),
        "ts.array.dense" => (include_str!("dense.kernel"), "law.dense_pattern", true),
        _ => return None,
    };
    let expected = pattern(library, source)?;
    let expected = expected.iter().find(|f| f.name.as_str() == name)?;
    if definition.body()? != expected.body()? {
        return None;
    }
    let get = library.names.resolve("ts.get")?;
    let get_definition = library.registry.get(&get)?.definition();
    let expected = pattern(library, include_str!("get.kernel"))?;
    let expected_get = expected
        .iter()
        .find(|f| f.name.as_str() == "law.get_pattern")?;
    let actual = closure(
        get_definition,
        library.registry.iter().map(|(_, f)| &**f.definition()),
    )?;
    let expected_closure = closure(expected_get, expected.iter())?;
    // The list arm returns before every other receiver arm. The surrounding
    // call and return, and the early plain-record arm, are unchanged too.
    let actual_block = &get_definition.body()?.block;
    let expected_block = &expected_get.body()?.block;
    if actual_block.len() != expected_block.len()
        || actual_block[0] != expected_block[0]
        || actual_block[2..] != expected_block[2..]
        || actual.body()?.block.get(..2) != expected_closure.body()?.block.get(..2)
    {
        return None;
    }
    // Comparing a present item with the empty tuple adds exactly the
    // item's immutable nested size to the zero-nested read price.
    let same = library
        .registry
        .get(&library.names.resolve("same")?)?
        .definition();
    let operands: Vec<_> = same
        .signature
        .params
        .iter()
        .map(|p| lash_kernel_doc::Operand::Param(p.name.clone()))
        .collect();
    let comparison = Formula::Sum(vec![
        Formula::Constant(1),
        Formula::Sum(vec![
            Formula::Min(operands.iter().cloned().map(Formula::Size).collect()),
            Formula::NestedSize(operands.first()?.clone()),
            Formula::NestedSize(operands.get(1)?.clone()),
        ]),
        Formula::DeepSize(lash_kernel_doc::Operand::Result),
    ]);
    if same.charge()? != &comparison {
        return None;
    }
    let Formula::Constant(base) = get_definition.charge()? else {
        return None;
    };
    let helpers = Arc::new(
        lash_kernel_doc::BuildHelpers::new(
            library.registry.clone(),
            library.registry.iter().map(|(id, _)| *id),
        )
        .ok()?,
    );
    let prepared = PreparedLibrary::with_helpers(Arc::new(library.registry.clone()), helpers);
    let units = probe(&prepared, get, 2)?
        .checked_sub(probe(&prepared, get, 1)?)?
        .checked_sub(4 + *base)?;
    let descriptor = "Some(p::HelperTemplate { entry: None, metered: None, depth: 16, callees: &[], heap: Some(p::HeapTemplate { entry: ".to_owned()
        + ident + "_bulk, depth: 16, writes: false, element: None, record_write: None, callees: &[] }), forwards: p::Forwards { floats: None, ints: None, texts: None } })";
    let code = if dense {
        let loop_units = definition
            .body()?
            .block
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::While { metered, .. } => Some(*metered),
                _ => None,
            })?;
        format!(
            "fn {ident}_bulk(heap: &mut dyn p::TemplateHeap, args: &[Value], _: &[lash_kernel_doc::PreparedNative]) -> Option<Value> {{\n    let [source] = args else {{ return None }};\n    heap.dense(*source, {units}, {loop_units})\n}}\n"
        )
    } else {
        format!(
            "fn {ident}_bulk(heap: &mut dyn p::TemplateHeap, args: &[Value], _: &[lash_kernel_doc::PreparedNative]) -> Option<Value> {{\n    let [receiver, arguments] = args else {{ return None }};\n    heap.concat(*receiver, *arguments, {units})\n}}\n"
        )
    };
    Some((descriptor, code))
}

/// Difference one additional read against the same stored list. Its only
/// enclosing costs are the let, call and two argument expressions (4).
fn probe(library: &PreparedLibrary, get: FunctionId, calls: usize) -> Option<u64> {
    use lash_kernel_doc::{Document, Float, Literal, NumberPolicy};
    let mut body = vec![Stmt::Let {
        name: Name::new("xs"),
        value: Rhs::Expr(Expr::List(vec![Expr::Literal(Literal::Float(Float::new(
            0.0,
        )))])),
    }];
    for at in 0..calls {
        body.push(Stmt::Let {
            name: Name::new(format!("read{at}")),
            value: Rhs::Expr(Expr::Call {
                function: get,
                args: vec![
                    Expr::Variable(Name::new("xs")),
                    Expr::Literal(Literal::Float(Float::new(0.0))),
                ],
            }),
        });
    }
    body.push(Stmt::Return {
        value: Expr::Variable(Name::new("read0")),
    });
    let mut document = Document::new(NumberPolicy::BySpelling, body);
    document.library.insert(get);
    let link = Link {
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    let program = Program::prepare(
        link.identity(),
        BTreeMap::from([(document.identity(), Arc::new(document))]),
        &BTreeMap::from([(link.identity(), link)]),
        PageId([7; 32]),
        library.clone(),
        lash_kernel_doc::Selection::Fresh,
    )
    .ok()?;
    let bounds = lash_kernel_vm::Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 8,
        requests_per_park: 8,
        join_members: 8,
    };
    let mut machine = KernelMachine::start(
        program,
        bounds,
        CellStart {
            target: Target::Main,
            args: vec![],
            session: None,
            owner: PageId([9; 32]),
            modes: Default::default(),
            host: vec![],
        },
        Box::new(Pages::default()),
    )
    .ok()?;
    match machine.run(&mut NoReads, u64::MAX).ok()? {
        Step::Ended(lash_kernel_vm::End::Finished(_)) => machine.meters().charged,
        _ => None,
    }
}

struct NoReads;
impl Host for NoReads {
    fn print(&mut self, _: &lash_kernel_doc::Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
#[derive(Default)]
struct Pages(BTreeMap<PageId, PageAnswer>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
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
            grant: lash_kernel_doc::heap::PageGrant::NONE,
        };
        self.0.insert(
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
