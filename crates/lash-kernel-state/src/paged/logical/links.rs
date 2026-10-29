//! Empty and single-identity occurrences need no index allocation. Sealing
//! uses the existing canonical nested tree, including its grant handling.
use super::{Edge, Tree, identity_key};
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
    Many(Rc<Tree<Edge>>),
}
impl Links {
    pub fn len(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::One { .. } => 1,
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
        }
    }
    pub fn get(&self, key: &[u8], access: &Access<'_>) -> Result<Option<Edge>, HeapFault> {
        match self {
            Self::Empty => Ok(None),
            Self::One { edge, .. } => {
                Ok((identity_key(edge.target).as_slice() == key).then(|| edge.clone()))
            }
            Self::Many(index) => index.get(key, access),
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
        }
    }
    pub fn same_version(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Empty, Self::Empty) => true,
            (Self::One { edge: a, .. }, Self::One { edge: b, .. }) => {
                a.target == b.target && a.count == b.count
            }
            (Self::Many(a), Self::Many(b)) => a.same_version(b),
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
        }
    }
    pub fn children(&self, out: &mut Vec<PageRef>) {
        match self {
            Self::Empty => (),
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
            Self::Empty | Self::One { sealed: None, .. } => Ok(()),
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
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self::Many(Rc::new(Tree::decode(input)?)))
    }
}
