//! Writing into a document by site, and the record of where each node came
//! from.
//!
//! A [`Shadow`] mirrors a unit's tree node for node, in the child order of
//! [`lash_kernel_doc::Node::children`]. It carries what the document does
//! not: the site the node had in the base document and whether an edit
//! wrote the node. Every edit changes the document and its shadow together,
//! so a node's shadow travels with it.

use lash_kernel_doc::{Action, Block, Expr, Member, Node, Place, Rhs, Site, Stmt};

/// Any node of the tree, for writing.
pub(crate) enum NodeMut<'a> {
    Block(&'a mut Block),
    Stmt(&'a mut Stmt),
    Action(&'a mut Action),
    Expr(&'a mut Expr),
}

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

impl<'a> NodeMut<'a> {
    pub(crate) fn class(&self) -> NodeClass {
        match self {
            Self::Block(_) => NodeClass::Block,
            Self::Stmt(_) => NodeClass::Statement,
            Self::Action(_) => NodeClass::Action,
            Self::Expr(_) => NodeClass::Expression,
        }
    }

    pub(crate) fn as_node(&self) -> Node<'_> {
        match self {
            Self::Block(block) => Node::Block(block),
            Self::Stmt(stmt) => Node::Stmt(stmt),
            Self::Action(action) => Node::Action(action),
            Self::Expr(expr) => Node::Expr(expr),
        }
    }

    /// The node's children, in the order of
    /// [`lash_kernel_doc::Node::children`] (`K-ID-004`).
    pub(crate) fn children(self) -> Vec<NodeMut<'a>> {
        let mut out = Vec::new();
        match self {
            Self::Block(block) => out.extend(block.iter_mut().map(NodeMut::Stmt)),
            Self::Action(_) => {}
            Self::Stmt(stmt) => match stmt {
                Stmt::Let { value, .. } => out.push(rhs(value)),
                Stmt::Assign { place, value } => {
                    if let Place::Member(member) = place {
                        push_member(member, &mut out);
                    }
                    out.push(rhs(value));
                }
                Stmt::Remove { member } => push_member(member, &mut out),
                Stmt::Do { value } => out.push(NodeMut::Expr(value)),
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    out.push(NodeMut::Expr(condition));
                    out.push(NodeMut::Block(then_block));
                    out.push(NodeMut::Block(else_block));
                }
                // As `Node::children`: an empty `else` is no child.
                Stmt::For {
                    iterable,
                    body,
                    else_block,
                    ..
                } => {
                    out.push(NodeMut::Expr(iterable));
                    out.push(NodeMut::Block(body));
                    if !else_block.is_empty() {
                        out.push(NodeMut::Block(else_block));
                    }
                }
                Stmt::While {
                    condition,
                    body,
                    else_block,
                } => {
                    out.push(NodeMut::Expr(condition));
                    out.push(NodeMut::Block(body));
                    if !else_block.is_empty() {
                        out.push(NodeMut::Block(else_block));
                    }
                }
                Stmt::Break | Stmt::Continue => {}
                Stmt::Return { value }
                | Stmt::Throw { value }
                | Stmt::Print { value }
                | Stmt::Finish { value }
                | Stmt::Fail { value } => out.push(NodeMut::Expr(value)),
                Stmt::Try(scope) => {
                    out.push(NodeMut::Block(&mut scope.body));
                    if let Some(catch) = &mut scope.catch {
                        out.push(NodeMut::Block(&mut catch.body));
                    }
                    if let Some(finally) = &mut scope.finally {
                        out.push(NodeMut::Block(finally));
                    }
                }
            },
            Self::Expr(expr) => match expr {
                Expr::Literal(_) | Expr::Variable(_) | Expr::Clock | Expr::Random => {}
                Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                    out.extend(items.iter_mut().map(NodeMut::Expr));
                }
                Expr::Map(entries) => {
                    for entry in entries {
                        out.push(NodeMut::Expr(&mut entry.key));
                        out.push(NodeMut::Expr(&mut entry.value));
                    }
                }
                Expr::Record(entries) => {
                    out.extend(
                        entries
                            .iter_mut()
                            .map(|entry| NodeMut::Expr(&mut entry.value)),
                    );
                }
                Expr::Member(member) => push_member(member, &mut out),
                Expr::Closure(closure) => out.push(NodeMut::Block(&mut closure.body)),
                Expr::Call { args, .. } => out.extend(args.iter_mut().map(NodeMut::Expr)),
                Expr::If { cond, then, r#else } => {
                    out.extend([
                        NodeMut::Expr(cond),
                        NodeMut::Expr(then),
                        NodeMut::Expr(r#else),
                    ]);
                }
                Expr::Let { value, body, .. } => {
                    out.extend([NodeMut::Expr(value), NodeMut::Expr(body)]);
                }
                Expr::Read(read) => {
                    out.push(NodeMut::Expr(&mut read.handle));
                    out.push(NodeMut::Expr(&mut read.request));
                }
                Expr::Act(act) => out.extend(act.args.iter_mut().map(NodeMut::Expr)),
            },
        }
        out
    }

    /// The node `path` reaches from this one.
    pub(crate) fn descend(self, path: &[u32]) -> Option<NodeMut<'a>> {
        let mut node = self;
        for step in path {
            node = node
                .children()
                .into_iter()
                .nth(usize::try_from(*step).ok()?)?;
        }
        Some(node)
    }
}

fn rhs(rhs: &mut Rhs) -> NodeMut<'_> {
    match rhs {
        Rhs::Expr(expr) => NodeMut::Expr(expr),
        Rhs::Action(action) => NodeMut::Action(action),
    }
}

fn push_member<'a>(member: &'a mut Member, out: &mut Vec<NodeMut<'a>>) {
    match member {
        Member::Field { target, .. } => out.push(NodeMut::Expr(target)),
        Member::Index { target, index } => {
            out.push(NodeMut::Expr(target));
            out.push(NodeMut::Expr(index));
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

    /// The path from this shadow to the node that was at `origin`.
    pub(crate) fn find(&self, origin: &Site) -> Option<Vec<u32>> {
        if self.origin.as_ref() == Some(origin) {
            return Some(Vec::new());
        }
        (0u32..).zip(&self.children).find_map(|(index, child)| {
            let mut path = child.find(origin)?;
            path.insert(0, index);
            Some(path)
        })
    }

    /// Calls `visit` with every shadow under this one and its path, parents
    /// first.
    pub(crate) fn walk(&self, path: &mut Vec<u32>, visit: &mut dyn FnMut(&[u32], &Shadow)) {
        visit(path, self);
        for (index, child) in (0u32..).zip(&self.children) {
            path.push(index);
            child.walk(path, visit);
            path.pop();
        }
    }
}
