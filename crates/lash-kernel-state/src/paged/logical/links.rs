//! Empty and single-identity occurrences need no index allocation. Sealing
//! uses the existing canonical nested tree, including its grant handling.
use super::{Edge, Tree, identity_key, invalid};
use crate::paged::tree::{Access, SealWork, Tree as PagedTree};
use lash_kernel_doc::heap::{HeapFault, Identity, PageRef};
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use std::rc::Rc;

#[derive(Clone)]
pub(super) enum Links {
    Empty,
    One {
        edge: Edge,
        sealed: Option<Rc<Tree<Edge>>>,
    },
    Packed {
        edges: Rc<[Edge]>,
        tasks: u64,
    },
    Many(Rc<Tree<Edge>>),
}

#[derive(Default)]
pub(super) struct EdgeBatch {
    first: Option<Edge>,
    rest: Vec<Edge>,
}
impl EdgeBatch {
    pub fn push(&mut self, target: Identity, count: u64) -> Result<(), HeapFault> {
        match &mut self.first {
            None => self.first = Some(Edge { target, count }),
            Some(first) if first.target == target => {
                first.count = first
                    .count
                    .checked_add(count)
                    .ok_or_else(|| invalid("edge count overflow"))?;
            }
            Some(_) => self.rest.push(Edge { target, count }),
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<Links, HeapFault> {
        let Some(first) = self.first else {
            return Ok(Links::Empty);
        };
        if self.rest.is_empty() {
            return Ok(Links::One {
                edge: first,
                sealed: None,
            });
        }
        self.rest.push(first);
        // Identity ordering matches its canonical tagged, big-endian key.
        self.rest.sort_unstable_by_key(|edge| edge.target);
        let mut last = 0;
        for next in 1..self.rest.len() {
            if self.rest[last].target == self.rest[next].target {
                self.rest[last].count = self.rest[last]
                    .count
                    .checked_add(self.rest[next].count)
                    .ok_or_else(|| invalid("edge count overflow"))?;
            } else {
                last += 1;
                self.rest[last] = self.rest[next].clone();
            }
        }
        self.rest.truncate(last + 1);
        let tasks = self.rest.iter().try_fold(0_u64, |sum, edge| {
            sum.checked_add(if matches!(edge.target, Identity::Task(_)) {
                edge.count
            } else {
                0
            })
            .ok_or_else(|| invalid("source index aggregate overflow"))
        })?;
        Ok(Links::Packed {
            edges: Rc::from(self.rest),
            tasks,
        })
    }
}
impl Links {
    fn unpack(&mut self, access: &Access<'_>) -> Result<(), HeapFault> {
        if let Self::Packed { edges, .. } = self {
            let rows = edges
                .iter()
                .map(|edge| {
                    (
                        crate::paged::tree::Key::from(identity_key(edge.target).as_slice()),
                        edge.clone(),
                    )
                })
                .collect();
            *self = Self::Many(Rc::new(Tree::from_rows(rows, access)?));
        }
        Ok(())
    }
    pub fn len(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::One { .. } => 1,
            Self::Packed { edges, .. } => edges.len() as u64,
            Self::Many(index) => index.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn task_edges(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::One { edge, .. } => {
                if matches!(edge.target, Identity::Task(_)) {
                    edge.count
                } else {
                    0
                }
            }
            Self::Many(index) => index.task_edges(),
            Self::Packed { tasks, .. } => *tasks,
        }
    }
    pub fn get(&self, key: &[u8], access: &Access<'_>) -> Result<Option<Edge>, HeapFault> {
        match self {
            Self::Empty => Ok(None),
            Self::One { edge, .. } => {
                Ok((identity_key(edge.target).as_slice() == key).then(|| edge.clone()))
            }
            Self::Many(index) => index.get(key, access),
            Self::Packed { edges, .. } => Ok(edges
                .binary_search_by(|edge| identity_key(edge.target).as_slice().cmp(key))
                .ok()
                .map(|at| edges[at].clone())),
        }
    }
    pub fn write(
        &mut self,
        key: impl AsRef<[u8]>,
        edge: Edge,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let key = key.as_ref();
        match self {
            Self::Empty => *self = Self::One { edge, sealed: None },
            Self::One { edge: old, .. } if old.target == edge.target => {
                *self = Self::One { edge, sealed: None }
            }
            Self::One { edge: old, .. } => {
                let mut index = Tree::new(false);
                index.write(identity_key(old.target), old.clone(), access)?;
                index.write(key, edge, access)?;
                *self = Self::Many(Rc::new(index));
            }
            Self::Many(index) => Rc::make_mut(index).write(key, edge, access)?,
            Self::Packed { .. } => {
                self.unpack(access)?;
                self.write(key, edge, access)?;
            }
        }
        Ok(())
    }
    pub fn write_with<E: From<HeapFault>>(
        &mut self,
        key: &[u8],
        access: &Access<'_>,
        write: impl FnOnce(Option<&Edge>) -> Result<Option<Edge>, E>,
    ) -> Result<bool, E> {
        let old = self.get(key, access)?;
        let Some(value) = write(old.as_ref())? else {
            return Ok(false);
        };
        self.write(key, value, access)?;
        Ok(true)
    }
    pub fn remove(&mut self, key: &[u8], access: &Access<'_>) -> Result<(), HeapFault> {
        match self {
            Self::Empty => (),
            Self::One { edge, .. } if identity_key(edge.target).as_slice() == key => {
                *self = Self::Empty
            }
            Self::One { .. } => (),
            Self::Many(index) => {
                Rc::make_mut(index).remove(key, access)?;
            }
            Self::Packed { .. } => {
                self.unpack(access)?;
                self.remove(key, access)?;
            }
        }
        Ok(())
    }
    pub fn each(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&Edge) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match self {
            Self::Empty => Ok(()),
            Self::One { edge, .. } => visit(edge),
            Self::Many(index) => super::visit_tree(index.as_ref(), access, visit),
            Self::Packed { edges, .. } => edges.iter().try_for_each(visit),
        }
    }
    pub fn same_version(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Empty, Self::Empty) => true,
            (Self::One { edge: a, .. }, Self::One { edge: b, .. }) => {
                a.target == b.target && a.count == b.count
            }
            (Self::Many(a), Self::Many(b)) => a.same_version(b),
            (Self::Packed { edges: a, .. }, Self::Packed { edges: b, .. }) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
    pub fn seal_nested(
        &mut self,
        access: &Access<'_>,
        work: &mut SealWork,
    ) -> Result<bool, HeapFault> {
        match self {
            Self::Empty => Ok(true),
            Self::One { edge, sealed } => {
                if sealed.is_none() {
                    *sealed = Some(Rc::new(Tree::single(
                        &identity_key(edge.target),
                        edge.clone(),
                        access,
                    )?));
                }
                Ok(true)
            }
            Self::Many(index) => Rc::make_mut(index).seal_nested(access, work),
            Self::Packed { .. } => {
                self.unpack(access)?;
                self.seal_nested(access, work)
            }
        }
    }
    pub fn children(&self, out: &mut Vec<PageRef>) {
        match self {
            Self::Empty | Self::Packed { .. } => (),
            Self::One {
                sealed: Some(index),
                ..
            }
            | Self::Many(index) => index.children(out),
            Self::One { sealed: None, .. } => (),
        }
    }
    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match self {
            Self::Empty | Self::Packed { .. } | Self::One { sealed: None, .. } => Ok(()),
            Self::One {
                sealed: Some(index),
                ..
            }
            | Self::Many(index) => Rc::make_mut(index).grants(children),
        }
    }
}
impl TokenCodec for Links {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Empty => PagedTree::<Edge>::new(false).encode(out),
            Self::One {
                sealed: Some(index),
                ..
            }
            | Self::Many(index) => index.encode(out),
            Self::One { sealed: None, .. } => {
                PagedTree::<Edge>::new(false).encode_pending(1, 0, self.task_edges(), 0, out)
            }
            Self::Packed { edges, tasks } => {
                PagedTree::<Edge>::new(false).encode_pending(edges.len() as u64, 0, *tasks, 0, out)
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self::Many(Rc::new(Tree::decode(input)?)))
    }
}
