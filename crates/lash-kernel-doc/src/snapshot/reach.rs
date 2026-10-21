//! Find closures held in data, preserving their defining callable identities.
use super::members;
use crate::heap::{Element, EntityId, HeapFault, PageReader, PageValue};

use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub closure: EntityId,
    pub root: usize,
    pub path: String,
}

pub fn reach(
    heap: &dyn PageReader,
    roots: Vec<(String, PageValue)>,
) -> Result<Vec<Found>, HeapFault> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for (root, (path, value)) in roots.into_iter().enumerate() {
        let mut pending = vec![(path, value)];
        while let Some((path, value)) = pending.pop() {
            match value {
                PageValue::Closure(id) => {
                    if seen.insert(id) {
                        found.push(Found {
                            closure: id,
                            root,
                            path,
                        });
                    }
                }
                PageValue::Tuple(tuple) => {
                    for (index, value) in members(heap, &tuple)?.into_iter().enumerate().rev() {
                        pending.push((format!("{path}[{index}]"), value));
                    }
                }
                PageValue::List(id)
                | PageValue::Map(id)
                | PageValue::Set(id)
                | PageValue::Record(id) => {
                    if !seen.insert(id) {
                        continue;
                    }
                    let mut children = Vec::new();
                    heap.visit(id, &mut |element| {
                        match element {
                            Element::Item(v) => {
                                children.push((format!("{path}[{}]", children.len()), v.clone()))
                            }
                            Element::Entry { key, value } => {
                                children.push((format!("{path}[key]"), key.clone()));
                                children.push((format!("{path}[value]"), value.clone()));
                            }
                            Element::Field { value, .. } => {
                                children.push((path.clone(), value.clone()))
                            }
                        }
                        std::ops::ControlFlow::Continue(())
                    })?;
                    pending.extend(children.into_iter().rev());
                }
                _ => {}
            }
        }
    }
    Ok(found)
}
