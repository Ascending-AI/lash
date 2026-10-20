//! Every closure a root reaches (SAVE-IN-DATA): through data, a token's
//! members, and what each closure it reaches reads, found before any is
//! gathered, so each is named once and every place that holds it refers to
//! it by that name.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use crate::heap::{Element, EntityId, HeapFault, PageValue};
use crate::{Document, Name, Unit};

use super::collect::{SnapshotHeap, closure_at, closure_held, members, text_of, token_closure};
use super::reads::CodeReads;

/// A closure a walk found to name: the root it was first found under, by
/// position, and the path from that root's (`plan.map`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub closure: EntityId,
    pub root: usize,
    pub path: String,
}

/// Every closure `roots` reach that a function must name, in walk order:
/// a closure held in data or in a token's members, through data and what
/// each closure reached reads. A closure a capture holds as its whole value
/// and no data holds is a helper, made again in the body of each function
/// that reads it, as lowering makes one. `named` are the closures already
/// named, which are walked into and never answered. Each root is a value
/// and the path it is held at.
///
/// # Errors
///
/// The heap's fault.
pub fn reach<H: SnapshotHeap>(
    heap: &mut H,
    document: &Document,
    tag: Option<&str>,
    roots: Vec<(String, PageValue)>,
    named: &BTreeMap<EntityId, Name>,
) -> Result<Vec<Found>, HeapFault> {
    let mut walk = Walk {
        heap,
        document,
        tag,
        named,
        closures: BTreeMap::new(),
        objects: BTreeSet::new(),
        found: Vec::new(),
        root: 0,
        pending: Vec::new(),
    };
    for (root, (path, value)) in roots.into_iter().enumerate() {
        walk.root = root;
        walk.pending.push(Work::Value(path, value));
        while let Some(work) = walk.pending.pop() {
            match work {
                Work::Value(path, value) => walk.value(&path, &value)?,
                Work::Closure(path, id, data) => walk.closure(path, id, data)?,
            }
        }
    }
    Ok(walk.found)
}

enum Work {
    Value(String, PageValue),
    /// A closure, and whether data holds it rather than a capture whole:
    /// only one data holds is named.
    Closure(String, EntityId, bool),
}

struct Walk<'a, 'h, H: SnapshotHeap> {
    heap: &'h mut H,
    document: &'a Document,
    tag: Option<&'a str>,
    named: &'a BTreeMap<EntityId, Name>,
    /// Each closure met, and whether it is named.
    closures: BTreeMap<EntityId, bool>,
    objects: BTreeSet<EntityId>,
    found: Vec<Found>,
    /// The root being walked.
    root: usize,
    pending: Vec<Work>,
}

impl<H: SnapshotHeap> Walk<'_, '_, H> {
    fn find(&mut self, closure: EntityId, path: String) {
        self.found.push(Found {
            closure,
            root: self.root,
            path,
        });
    }

    /// Queues `children` so the first is walked next.
    fn queue(&mut self, children: Vec<Work>) {
        self.pending.extend(children.into_iter().rev());
    }

    fn value(&mut self, path: &str, value: &PageValue) -> Result<(), HeapFault> {
        let mut children = Vec::new();
        match value {
            PageValue::Closure(id) => children.push(Work::Closure(path.to_owned(), *id, true)),
            PageValue::Tuple(tuple) => {
                let callable = token_closure(self.heap.pages(), value, self.tag)?;
                for (at, member) in members(self.heap.pages(), tuple)?.into_iter().enumerate() {
                    match callable {
                        Some((id, slot)) if slot == at => {
                            children.push(Work::Closure(path.to_owned(), id, true));
                        }
                        _ => children.push(Work::Value(format!("{path}[{at}]"), member)),
                    }
                }
            }
            PageValue::List(id)
            | PageValue::Map(id)
            | PageValue::Set(id)
            | PageValue::Record(id) => {
                if !self.objects.insert(*id) {
                    return Ok(());
                }
                let pages = self.heap.pages();
                let mut elements = Vec::new();
                pages.visit(*id, &mut |element| {
                    elements.push(match element {
                        Element::Item(item) => (None, item.clone()),
                        Element::Entry { key, value } => (Some(key.clone()), value.clone()),
                        Element::Field { name, value } => {
                            (Some(PageValue::Text(name.clone())), value.clone())
                        }
                    });
                    ControlFlow::Continue(())
                })?;
                let field = matches!(value, PageValue::Record(_));
                for (at, (key, item)) in elements.into_iter().enumerate() {
                    let path = match (&key, field) {
                        (Some(PageValue::Text(name)), true) => {
                            format!("{path}.{}", text_of(pages, name)?)
                        }
                        (Some(PageValue::Text(name)), false) => {
                            format!("{path}[{:?}]", text_of(pages, name)?)
                        }
                        (Some(_), _) => format!("{path}[…]"),
                        (None, _) => format!("{path}[{at}]"),
                    };
                    if let Some(key) = key.filter(|_| !field) {
                        children.push(Work::Value(path.clone(), key));
                    }
                    children.push(Work::Value(path, item));
                }
            }
            _ => {}
        }
        self.queue(children);
        Ok(())
    }

    fn closure(&mut self, path: String, id: EntityId, names: bool) -> Result<(), HeapFault> {
        match self.closures.get(&id) {
            Some(true) => return Ok(()),
            Some(false) if !names => return Ok(()),
            // Made again as a helper until now, and also held in data: it is
            // named from here, and what it reads was walked when it was met.
            Some(false) => {
                self.closures.insert(id, true);
                self.find(id, path);
                return Ok(());
            }
            None if self.named.contains_key(&id) => {
                self.closures.insert(id, true);
            }
            None => {
                self.closures.insert(id, names);
                if names {
                    self.find(id, path.clone());
                }
            }
        }
        let site = self.heap.closure_site(id)?;
        if let Unit::Library(_) = site.unit {
            return Ok(());
        }
        let Some(code) = closure_at(self.document, &site) else {
            return Ok(());
        };
        let reads = CodeReads::of(&code.params, &code.body);
        let mut children = Vec::new();
        for free in &reads.free {
            let value = match self.heap.closure_capture(id, free)? {
                Some(value) => value,
                None if site.unit == Unit::Main => {
                    match self.heap.unit_binding(&Unit::Main, free)? {
                        Some(value) => value,
                        None => continue,
                    }
                }
                None => continue,
            };
            let path = format!("{path}.{free}");
            match closure_held(self.heap.pages(), &value, self.tag)? {
                Some(held) => {
                    children.push(Work::Closure(path.clone(), held, false));
                    if let (PageValue::Tuple(tuple), Some((_, slot))) =
                        (&value, token_closure(self.heap.pages(), &value, self.tag)?)
                    {
                        for (at, member) in
                            members(self.heap.pages(), tuple)?.into_iter().enumerate()
                        {
                            if at != slot {
                                children.push(Work::Value(format!("{path}[{at}]"), member));
                            }
                        }
                    }
                }
                None => children.push(Work::Value(path, value)),
            }
        }
        self.queue(children);
        Ok(())
    }
}
