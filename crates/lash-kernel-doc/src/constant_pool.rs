//! Derived immutable code payloads. Extraction changes neither source bytes nor
//! runtime values. Mutable constructors are templates, never pool entries.
use std::sync::Arc;

use lash_kernel_value::tokens::{encode_body, logical_digest};
use lash_kernel_value::{DocumentId, FunctionId};

use crate::{Atom, Block, Expr, FunctionDefinition, Literal, Node};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PoolOwner {
    Document(DocumentId),
    Function(FunctionId),
}

/// Atoms have no Node::children address. Keep their argument coordinate in a
/// separate namespace rather than inventing a child that could alias a node.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolCoordinate {
    pub path: Vec<u32>,
    pub atom: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolKey {
    pub owner: PoolOwner,
    pub coordinate: PoolCoordinate,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub key: PoolKey,
    /// Canonical immutable expression payload, independently authenticated by
    /// the key. This is code-owned data, not a saved guest value descriptor.
    pub payload: Arc<[u8]>,
}

#[derive(Clone, Debug, Default)]
pub struct ConstantPool {
    entries: Vec<PoolEntry>,
}

impl ConstantPool {
    /// Extract Main. Each function definition has its own FunctionId owner.
    pub fn for_document(document: &crate::Document) -> Self {
        document.main.as_ref().map_or_else(Self::default, |body| {
            Self::extract(PoolOwner::Document(document.identity()), body)
        })
    }

    pub fn for_function(definition: &FunctionDefinition) -> Self {
        definition.body().map_or_else(Self::default, |body| {
            Self::extract(PoolOwner::Function(definition.identity()), &body.block)
        })
    }

    pub fn entries(&self) -> &[PoolEntry] {
        &self.entries
    }

    pub fn at(&self, coordinate: &PoolCoordinate) -> Option<&PoolEntry> {
        self.entries
            .binary_search_by(|entry| entry.key.coordinate.cmp(coordinate))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Extract a body whose exact owner was already authenticated by code
    /// admission. Public callers without that proof use `for_document` or
    /// `for_function`, which compute the owner from the complete source.
    #[doc(hidden)]
    pub fn extract(owner: PoolOwner, body: &Block) -> Self {
        let mut pool = Self::default();
        let mut pending = vec![(0, None, Node::Block(body))];
        let mut path = Vec::new();
        let mut children = Vec::new();
        while let Some((depth, index, node)) = pending.pop() {
            path.truncate(depth);
            path.extend(index);
            match node {
                Node::Expr(expr) if closed(expr) => {
                    pool.push(owner, path.clone(), None, expr);
                }
                Node::Action(action) => {
                    for (index, atom) in action_atoms(action).iter().enumerate() {
                        if let Atom::Literal(literal) = atom
                            && !matches!(literal, Literal::Function(_))
                        {
                            pool.push(
                                owner,
                                path.clone(),
                                Some(index as u32),
                                &Expr::Literal(literal.clone()),
                            );
                        }
                    }
                }
                _ => {}
            }
            children.clear();
            node.children_into(&mut children);
            pending.extend(
                children
                    .drain(..)
                    .enumerate()
                    .rev()
                    .map(|(index, child)| (path.len(), Some(index as u32), child)),
            );
        }
        pool.entries
            .sort_by(|a, b| a.key.coordinate.cmp(&b.key.coordinate));
        pool
    }

    fn push(&mut self, owner: PoolOwner, path: Vec<u32>, atom: Option<u32>, expr: &Expr) {
        let payload = encode_body(expr);
        self.entries.push(PoolEntry {
            key: PoolKey {
                owner,
                coordinate: PoolCoordinate { path, atom },
                digest: logical_digest(&payload),
            },
            payload: payload.into(),
        });
    }

    /// Check a key against its exact source owner, coordinate and immutable
    /// payload. A digest alone cannot authorize a different archive version.
    pub fn authenticates(&self, key: &PoolKey) -> bool {
        self.at(&key.coordinate)
            .is_some_and(|entry| entry.key == *key && logical_digest(&entry.payload) == key.digest)
    }
}

fn closed(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(Literal::Function(_)) => false,
        Expr::Literal(_) => true,
        Expr::Tuple(items) => items.iter().all(closed),
        _ => false,
    }
}

fn action_atoms(action: &crate::Action) -> &[Atom] {
    use crate::Action;
    match action {
        Action::Call { args, .. } | Action::Spawn { args, .. } | Action::Perform { args, .. } => {
            args
        }
        Action::Join { task } | Action::Cancel { task } => std::slice::from_ref(task),
        Action::JoinMany { tasks, .. } => std::slice::from_ref(tasks),
        Action::Await { value, .. } => std::slice::from_ref(value),
        Action::Yield => &[],
    }
}

/// One native static-table row, bound to a closed tuple in an exact definition.
/// Generated tables retain these keys beside their physical arrays; no run
/// reads them until pool-backed values are enabled.
#[derive(Clone, Copy, Debug)]
pub struct StaticPoolKey {
    pub function: FunctionId,
    pub path: &'static [u32],
    pub digest: [u8; 32],
}

impl StaticPoolKey {
    pub fn key(&self) -> PoolKey {
        PoolKey {
            owner: PoolOwner::Function(self.function),
            coordinate: PoolCoordinate {
                path: self.path.to_vec(),
                atom: None,
            },
            digest: self.digest,
        }
    }
}
