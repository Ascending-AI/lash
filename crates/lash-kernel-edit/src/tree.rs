//! Writing into a document by site, and the record of where each node came
//! from.
//!
//! A [`Shadow`] mirrors a unit's tree node for node, in the child order of
//! [`lash_kernel_doc::Node::children`]. It carries what the document does
//! not: the site the node had in the base document and whether an edit
//! wrote the node. Every edit changes the document and its shadow together,
//! so a node's shadow travels with it. The document alone says which nodes
//! there are: [`Shadow::find`] and [`Shadow::walk`] follow it.

use lash_kernel_doc::{Node, Site};

/// What a site addresses, for a diagnostic.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeClass {
    Block,
    Statement,
    Action,
    Expression,
}

impl NodeClass {
    pub(crate) fn of(node: Node<'_>) -> Self {
        match node {
            Node::Block(_) => Self::Block,
            Node::Stmt(_) => Self::Statement,
            Node::Action(_) => Self::Action,
            Node::Expr(_) => Self::Expression,
        }
    }
}

/// One node's record; see the module.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shadow {
    /// The node's site in the base document. `None` for a node an edit
    /// brought in.
    pub(crate) origin: Option<Site>,
    /// An edit wrote this node itself.
    pub(crate) edited: bool,
    pub(crate) children: Vec<Shadow>,
}

impl Shadow {
    /// The shadow of a tree no part of which was in the base document.
    pub(crate) fn fresh(node: Node<'_>) -> Self {
        Self {
            children: node.children().into_iter().map(Self::fresh).collect(),
            ..Self::default()
        }
    }

    /// The shadow of a tree of the base document, rooted at `site`.
    pub(crate) fn base(node: Node<'_>, site: Site) -> Self {
        let children = (0u32..)
            .zip(node.children())
            .map(|(index, child)| Self::base(child, site.child(index)))
            .collect();
        Self {
            origin: Some(site),
            children,
            ..Self::default()
        }
    }

    /// Marks the node as written by an edit that replaced its content: what
    /// was under it is gone, and `node` is what it is now.
    pub(crate) fn replace(&mut self, node: Node<'_>) {
        self.edited = true;
        self.children = node.children().into_iter().map(Self::fresh).collect();
    }

    pub(crate) fn descend(&mut self, path: &[u32]) -> Option<&mut Shadow> {
        let mut shadow = self;
        for step in path {
            shadow = shadow.children.get_mut(usize::try_from(*step).ok()?)?;
        }
        Some(shadow)
    }

    /// The path from `node`, whose shadow this is, to the node that was at
    /// `origin`.
    pub(crate) fn find(&self, node: Node<'_>, origin: &Site) -> Option<Vec<u32>> {
        if self.origin.as_ref() == Some(origin) {
            return Some(Vec::new());
        }
        (0u32..)
            .zip(node.children().into_iter().zip(&self.children))
            .find_map(|(index, (node, child))| {
                let mut path = child.find(node, origin)?;
                path.insert(0, index);
                Some(path)
            })
    }

    /// Calls `visit` with the shadow and path of `node`, whose shadow this
    /// is, and of every node under it, parents first.
    pub(crate) fn walk(
        &self,
        node: Node<'_>,
        path: &mut Vec<u32>,
        visit: &mut dyn FnMut(&[u32], &Shadow),
    ) {
        visit(path, self);
        for (index, (node, child)) in (0u32..).zip(node.children().into_iter().zip(&self.children))
        {
            path.push(index);
            child.walk(node, path, visit);
            path.pop();
        }
    }
}
