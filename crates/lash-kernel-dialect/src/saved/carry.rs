//! Save data-held closures once by identity, preserving their defining Link.
use super::{HeldAt, Left, NotSaved, SaveError, SavedFunction, Saving, Token};
use lash_kernel_doc::heap::{Element, EntityId, HeapFault, Identity, PageReader, PageValue};
use lash_kernel_doc::snapshot::{
    closure_held, freeze_token, gather, members, reach, token_closure,
};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{ObjectKind, SemanticHeap};
use lash_vocabulary::Name;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
struct LinkedCatalog<'a> {
    document: &'a lash_kernel_doc::Document,
    documents: &'a BTreeMap<lash_vocabulary::LinkId, lash_kernel_doc::Document>,
    base: &'a dyn lash_kernel_doc::FunctionCatalog,
}
impl lash_kernel_doc::FunctionCatalog for LinkedCatalog<'_> {
    fn definition(
        &self,
        function: &lash_vocabulary::FunctionId,
    ) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.document
            .functions
            .get(function)
            .or_else(|| {
                self.documents
                    .values()
                    .find_map(|document| document.functions.get(function))
            })
            .or_else(|| self.base.definition(function))
    }
    fn registered(
        &self,
        function: &lash_vocabulary::FunctionId,
    ) -> Option<&lash_kernel_doc::RegisteredFunction> {
        self.base.registered(function)
    }
}
#[derive(Debug, Default)]
pub struct Saved {
    pub functions: BTreeMap<Name, SavedFunction>,
    pub bindings: BTreeMap<Name, PageValue>,
    pub refused: BTreeMap<Name, NotSaved>,
}
pub fn save(
    heap: &mut PagedHeap,
    left: Left<'_>,
    held: &BTreeSet<Name>,
) -> Result<Saved, HeapFault> {
    let catalog = LinkedCatalog {
        document: left.document,
        documents: left.documents,
        base: left.catalog,
    };
    let left = Left {
        catalog: &catalog,
        ..left
    };
    let roots = left
        .not_carried
        .iter()
        .filter_map(|name| {
            left.bindings
                .get(name)
                .map(|value| (name.to_string(), value.clone()))
        })
        .collect::<Vec<_>>();
    let root_names = roots
        .iter()
        .map(|(name, _)| Name::new(name))
        .collect::<Vec<_>>();
    let mut found = reach(heap, roots)?;
    let mut walked = found.iter().map(|f| f.closure).collect::<BTreeSet<_>>();
    // Captures are graph edges too. Discover every environment before the
    // first freeze, including a callback held only by another closure.
    let mut at = 0;
    while at < found.len() {
        let current = found[at].clone();
        at += 1;
        let callable = heap.closure(current.closure)?.callable;
        let definition = left
            .catalog
            .definition(&callable.function)
            .or_else(|| left.document.functions.get(&callable.function))
            .or_else(|| {
                left.documents
                    .get(&callable.link)
                    .and_then(|d| d.functions.get(&callable.function))
            })
            .ok_or_else(|| HeapFault::Malformed("saved closure definition is not loaded".into()))?;
        let Some(body) = definition.body() else {
            continue;
        };
        let params = definition
            .signature
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        let reads = lash_kernel_doc::snapshot::CodeReads::of(&params, &body.block, left.catalog);
        let site = heap.closure(current.closure)?.site.resolve(heap.pages())?;
        let mut saving = Saving {
            heap: &mut *heap,
            left: &left,
        };
        for name in reads.free {
            use lash_kernel_doc::snapshot::SnapshotHeap as _;
            let value = saving.closure_capture(current.closure, &name)?;
            let value = match value {
                Some(v) => Some(v),
                None => saving.unit_binding(&site.unit, &name)?,
            };
            if let Some(value) = value {
                for mut capture in reach(
                    saving.heap,
                    vec![(format!("{}.captures.{}", current.path, name), value)],
                )? {
                    if walked.insert(capture.closure) {
                        capture.root = current.root;
                        found.push(capture);
                    }
                }
            }
        }
    }
    // Membership is fixed before any capture freezes: mutual references
    // and traversal order cannot decide which environments are admitted.
    let candidates = found
        .iter()
        .map(|found| found.closure)
        .collect::<BTreeSet<_>>();
    let mut named = BTreeSet::<EntityId>::new();
    let mut names = BTreeMap::<EntityId, Name>::new();
    for (name, value) in left.bindings {
        if let Some(id) = closure_held(heap, value, left.values.tag())? {
            names.entry(id).or_insert_with(|| name.clone());
        }
    }
    let first = held
        .iter()
        .filter_map(Name::saved_index)
        .max()
        .map_or(0, |index| index + 1);
    let mut saved = Saved::default();
    let mut refusals = BTreeMap::new();
    for (next, found) in (first..).zip(found) {
        let name = names
            .entry(found.closure)
            .or_insert_with(|| Name::saved(next))
            .clone();
        let result = (|| -> Result<SavedFunction, SaveError> {
            let callable = heap.closure(found.closure)?.callable;
            let defining = if callable.link == left.link {
                Some(left.document)
            } else {
                left.documents.get(&callable.link)
            };
            if defining.is_none_or(|document| document.main.is_none()) {
                return Err(SaveError::Refused(NotSaved::MadeByLibrary));
            }
            let gathered = gather(
                &mut Saving {
                    heap: &mut *heap,
                    left: &left,
                },
                left.catalog,
                found.closure,
                &candidates,
            )?;
            let callable = gathered.callable;
            let document = if callable.link == left.link {
                left.document.clone()
            } else {
                left.documents
                    .get(&callable.link)
                    .ok_or_else(|| super::unreadable("the defining linked document is not loaded"))?
                    .clone()
            };
            super::check_controls(&left, callable, &document)?;
            let mut function = SavedFunction {
                name: name.clone(),
                link: callable.link,
                callable,
                environment: found.closure,
                document: lash_kernel_doc::IdentifiedDocument::new(document),
                captures: gathered.captures,
                closure: left.closure,
                references: gathered.references,
                written: left.written.get(&gathered.site).cloned(),
                token: None,
                held: Some(HeldAt {
                    binding: root_names[found.root].clone(),
                    path: found.path.clone(),
                }),
            };
            if let Some(value @ PageValue::Tuple(tuple)) = left.bindings.get(&name)
                && let Some((id, at)) = token_closure(heap, value, left.values.tag())?
                && id == found.closure
            {
                let values = members(heap, tuple)?;
                function.token = Some(Token::frozen(freeze_token(
                    &mut Saving {
                        heap: &mut *heap,
                        left: &left,
                    },
                    &values,
                    at,
                    left.values.tag().unwrap_or_default(),
                    &candidates,
                )?));
            }
            Ok(function)
        })();
        match result {
            Ok(function) => {
                named.insert(found.closure);
                saved.functions.insert(name, function);
            }
            Err(SaveError::Refused(why)) => {
                refusals.insert(found.closure, why.clone());
                saved.refused.insert(name, why);
            }
            Err(SaveError::Heap(fault)) => return Err(fault),
        }
    }
    // A refused capture invalidates every owner that reaches it. Iterate
    // by environment identity until no successful row names a missing row.
    loop {
        let available = saved
            .functions
            .values()
            .map(|f| f.environment)
            .collect::<BTreeSet<_>>();
        let refused = saved
            .functions
            .iter()
            .filter_map(|(name, function)| {
                function
                    .references
                    .iter()
                    .find(|id| !available.contains(id))
                    .map(|id| (name.clone(), *id))
            })
            .collect::<Vec<_>>();
        if refused.is_empty() {
            break;
        }
        for (name, needed) in refused {
            if let Some(function) = saved.functions.remove(&name) {
                named.remove(&function.environment);
            }
            saved.refused.insert(
                name,
                NotSaved::Needs {
                    function: names
                        .get(&needed)
                        .cloned()
                        .unwrap_or_else(|| Name::new(format!("{needed:?}"))),
                },
            );
        }
    }
    for name in left.not_carried {
        let Some(value) = left.bindings.get(name) else {
            saved.refused.insert(name.clone(), NotSaved::Task);
            continue;
        };
        match holds(heap, value, &named, &refusals)? {
            Holds::Refused(why) => {
                saved.refused.insert(name.clone(), why);
            }
            Holds::Task => {
                saved.refused.insert(name.clone(), NotSaved::Task);
            }
            Holds::Keyed => {
                saved.refused.insert(name.clone(), NotSaved::Keyed);
            }
            Holds::Closures => {
                saved.bindings.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(saved)
}
enum Holds {
    Refused(NotSaved),
    Task,
    Keyed,
    Closures,
}
fn holds(
    heap: &PagedHeap,
    value: &PageValue,
    named: &BTreeSet<EntityId>,
    refusals: &BTreeMap<EntityId, NotSaved>,
) -> Result<Holds, HeapFault> {
    let mut keyed = false;
    let mut objects = BTreeSet::new();
    // Each value, and whether a reference cannot stand where it is.
    let mut pending = vec![(value.clone(), false)];
    while let Some((value, fixed)) = pending.pop() {
        match &value {
            PageValue::Task(_) | PageValue::Ref(Identity::Task(_)) => return Ok(Holds::Task),
            PageValue::Closure(id) if refusals.contains_key(id) => {
                return Ok(Holds::Refused(refusals[id].clone()));
            }
            PageValue::Closure(id) if fixed || !named.contains(id) => keyed = true,
            PageValue::Closure(_) => {}
            PageValue::Ref(Identity::Object(id)) => {
                if heap.kind(*id)? == Some(ObjectKind::Closure) {
                    keyed = true;
                } else if let Some(object) = heap.value_of(*id)? {
                    pending.push((object, true));
                }
            }
            PageValue::Tuple(tuple) => {
                pending.extend(members(heap, tuple)?.into_iter().map(|v| (v, fixed)));
            }
            PageValue::Error(error) => pending.push((heap.error_data(error)?, true)),
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id) => {
                if !objects.insert(*id) {
                    continue;
                }
                let set = matches!(value, PageValue::Set(_));
                heap.visit(*id, &mut |element| {
                    match element {
                        Element::Item(item) => pending.push((item.clone(), fixed)),
                        Element::Entry { key, value } => {
                            pending.push((key.clone(), true));
                            pending.push((value.clone(), fixed || set));
                        }
                        Element::Field { value, .. } => pending.push((value.clone(), fixed)),
                    }
                    ControlFlow::Continue(())
                })?;
            }
            _ => {}
        }
    }
    Ok(if keyed { Holds::Keyed } else { Holds::Closures })
}
