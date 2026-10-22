//! Capture-only graph extraction; ordinary data cycles remain refused.
use super::*;
use lash_kernel_doc::{CaptureValue, FunctionRepresentation, FunctionValue};

/// Capture discovery follows authored adapter bodies as well as the library.
struct LinkedCatalog<'a> {
    document: &'a lash_kernel_doc::Document,
    base: &'a dyn FunctionCatalog,
}

impl FunctionCatalog for LinkedCatalog<'_> {
    fn definition(
        &self,
        function: &lash_vocabulary::FunctionId,
    ) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.document
            .functions
            .get(function)
            .or_else(|| self.base.definition(function))
    }
}

#[derive(Default)]
struct Graph {
    closures: BTreeMap<EntityId, u32>,
    functions: BTreeMap<CallableId, u32>,
    count: u32,
}

pub(super) fn extract_function(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &PageValue,
    budget: usize,
    path: &mut Vec<EntityId>,
) -> Result<FunctionValue, CopyError> {
    let mut graph = Graph::default();
    let CaptureValue::Function(function) = graph.function(heap, run, value, budget, path)? else {
        return Err(raised("not_data", "root callable is not a new environment").into());
    };
    function
        .check(
            run.catalog,
            lash_kernel_doc::FunctionValueLimits::standard(),
            run.data,
        )
        .map_err(|error| raised("not_data", error.to_string()))?;
    Ok(*function)
}

impl Graph {
    fn function(
        &mut self,
        heap: &mut CrossingHeap<'_>,
        run: &mut CrossingRun<'_>,
        value: &PageValue,
        budget: usize,
        path: &mut Vec<EntityId>,
    ) -> Result<CaptureValue, CopyError> {
        let root = function_root(heap, run, value)?
            .ok_or_else(|| raised("not_data", "no callable root"))?;
        let original = value;
        let value = &root;
        let callable =
            callable(heap, value)?.ok_or_else(|| raised("not_data", "no callable identity"))?;
        let seen = match value {
            PageValue::Closure(id) => self.closures.get(id),
            _ => self.functions.get(&callable),
        };
        if let Some(index) = seen {
            return Ok(CaptureValue::Reference(*index));
        }
        let index = self.count;
        self.count += 1;
        if self.count > FunctionValue::MAX_ENVIRONMENT_NODES {
            return Err(raised("not_data", "callable environment has too many nodes").into());
        }
        match value {
            PageValue::Closure(id) => {
                self.closures.insert(*id, index);
            }
            _ => {
                self.functions.insert(callable, index);
            }
        }
        let document = defining_document(run, callable)?;
        let catalog = LinkedCatalog {
            document,
            base: run.catalog,
        };
        let definition = catalog
            .definition(&callable.function)
            .ok_or_else(|| raised("not_data", "callable definition is not loaded"))?;
        let mut signature = definition.signature.clone();
        let mut convention = lash_kernel_doc::StartConvention::Direct;
        let mut representation = FunctionRepresentation::Bare;
        let mut captured = BTreeMap::new();
        let mut creation = None;
        let mut token = None;
        if let PageValue::Closure(id) = value {
            let closure = heap.heap.closure(*id)?;
            let site = closure.site.resolve(heap.heap.pages())?;
            creation = Some(site.as_ref().clone());
            let start = lash_kernel_doc::snapshot::closure_at(document, &site)
                .and_then(|c| c.presentation.as_ref())
                .ok_or_else(|| raised("not_data", "the function states no start signature"))?;
            {
                signature = start.signature.clone();
                convention = start.convention.clone();
                representation = start.representation.clone();
            }
            for index in 0..closure.captures.len() {
                let (_, (name, cell)) = closure
                    .captures
                    .rank_get(index, heap.heap.pages())?
                    .ok_or_else(|| HeapFault::Malformed("closure capture missing".into()))?;
                captured.insert(
                    name.resolve(heap.heap.pages())?,
                    heap.heap.variable_get(cell)?,
                );
            }
            if site.unit == lash_vocabulary::sites::Unit::Main {
                if let Some(body) = definition.body() {
                    let params = definition
                        .signature
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>();
                    for name in
                        lash_kernel_doc::snapshot::CodeReads::of(&params, &body.block, &catalog)
                            .free
                    {
                        if !captured.contains_key(&name) {
                            if let Some(value) = heap.heap.session_binding(&name)? {
                                captured.insert(name, value);
                            }
                        }
                    }
                }
            }
        } else if let Some(export) = document.exports.values().find_map(|export| match export {
            lash_kernel_doc::Export::Function(export) if export.function == callable.function => {
                Some(export)
            }
            _ => None,
        }) {
            signature = export.signature.clone();
            convention = export.convention.clone();
        }
        if let (PageValue::Tuple(tuple), FunctionRepresentation::Token { callable: at, .. }) =
            (original, &representation)
        {
            let values = members(heap.heap, tuple)?;
            token = Some(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        if i == *at as usize {
                            Ok(Datum::Null)
                        } else {
                            crate::data::copy_page_out(heap.heap, v)
                        }
                    })
                    .collect::<Result<_, _>>()?,
            );
        } else {
            representation = FunctionRepresentation::Bare;
        }
        let mut captioned = std::collections::BTreeSet::new();
        let mut pending = vec![callable.function];
        while let Some(function) = pending.pop() {
            if !captioned.insert(function) {
                continue;
            }
            if let Some(body) = document
                .functions
                .get(&function)
                .and_then(|definition| definition.body())
            {
                pending.extend(lash_kernel_doc::Node::Block(&body.block).functions_within());
            }
        }
        let mut captures = BTreeMap::new();
        for (name, value) in captured {
            captures.insert(name, self.value(heap, run, &value, budget, path)?);
        }
        Ok(CaptureValue::Function(Box::new(FunctionValue {
            callable,
            captures,
            signature,
            convention,
            representation,
            creation,
            token,
            captions: lash_kernel_doc::Captions::new(
                defining_document(run, callable)?,
                run.captions.iter().filter(|entry| entry.target.site().is_some_and(|site| matches!(&site.unit, lash_vocabulary::sites::Unit::Library(function) if captioned.contains(function)))).cloned().collect(),
            ).map_err(|error| raised("not_data", error.to_string()))?,
        })))
    }

    fn value(
        &mut self,
        heap: &mut CrossingHeap<'_>,
        run: &mut CrossingRun<'_>,
        value: &PageValue,
        budget: usize,
        path: &mut Vec<EntityId>,
    ) -> Result<CaptureValue, CopyError> {
        let budget = budget
            .checked_sub(1)
            .ok_or_else(|| raised("too_deep", "callable captures exceed the value depth bound"))?;
        if function_root(heap, run, value)?.is_some() {
            return self.function(heap, run, value, budget, path);
        }
        Ok(match value {
            PageValue::Task(_) | PageValue::Ref(lash_kernel_doc::heap::Identity::Task(_)) => {
                return Err(
                    raised("not_data", "the function holds a task or a pending promise").into(),
                );
            }
            PageValue::Tuple(tuple) => CaptureValue::Tuple(
                members(heap.heap, tuple)?
                    .iter()
                    .map(|v| self.value(heap, run, v, budget, path))
                    .collect::<Result<_, _>>()?,
            ),
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id) => {
                if path.contains(id) {
                    return Err(raised("cycle", "captured data contains itself").into());
                }
                path.push(*id);
                let mut elements = Vec::new();
                heap.heap.visit(*id, &mut |e| {
                    elements.push(match e {
                        lash_kernel_doc::heap::Element::Item(v) => (None, None, v.clone()),
                        lash_kernel_doc::heap::Element::Entry { key, value } => {
                            (Some(key.clone()), None, value.clone())
                        }
                        lash_kernel_doc::heap::Element::Field { name, value } => {
                            (None, Some(name.clone()), value.clone())
                        }
                    });
                    std::ops::ControlFlow::Continue(())
                })?;
                let mut items = Vec::new();
                let mut entries = Vec::new();
                let mut fields = Vec::new();
                for element in elements {
                    match element {
                        (None, None, v) => items.push(self.value(heap, run, &v, budget, path)?),
                        (Some(key), _, value) => entries.push((
                            self.value(heap, run, &key, budget, path)?,
                            self.value(heap, run, &value, budget, path)?,
                        )),
                        (_, Some(name), value) => {
                            fields.push((
                                text_of(heap.heap, &name)?,
                                self.value(heap, run, &value, budget, path)?,
                            ));
                        }
                    }
                }
                path.pop();
                match value {
                    PageValue::List(_) => CaptureValue::List(items),
                    PageValue::Set(_) => CaptureValue::Set(items),
                    PageValue::Map(_) => CaptureValue::Map(entries),
                    _ => CaptureValue::Record(fields),
                }
            }
            PageValue::Error(error) => {
                let data = heap.heap.error_data(error)?;
                CaptureValue::Error {
                    kind: text_of(heap.heap, &error.kind)?,
                    message: text_of(heap.heap, &error.message)?,
                    data: Box::new(self.value(heap, run, &data, budget, path)?),
                }
            }
            _ => CaptureValue::Data(crate::data::copy_page_out(heap.heap, value)?),
        })
    }
}
