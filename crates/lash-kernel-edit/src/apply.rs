//! Applying edits to a copy of a document.
//!
//! [`Working`] is that copy and its shadows (see [`crate::tree`]). An edit
//! resolves the base sites it names to where those nodes are now, changes
//! the document and changes the shadows the same way. Nothing here admits:
//! [`Working::publish`] hands the result to the checker.

use lash_kernel_doc::{Correspondence, Survivor};
use std::collections::BTreeMap;

use lash_kernel_check::{Admitted, Environment, admit, requirements};
use lash_kernel_doc::{
    Action, Atom, Block, Document, DocumentId, Expr, Member, Name, Node, NodeMut, Place, Rhs, Site,
    Stmt, Unit,
};

use crate::edit::{Edit, Position};
use crate::refusal::{EditDiagnosticKind, EditRefusal, Location, invalid_correspondence, refused};
use crate::tree::{NodeClass, Shadow};

/// Why one edit cannot be applied.
pub(crate) enum FaultKind {
    At(Option<Location>, EditDiagnosticKind),
    /// The document the earlier edits left does not link, and this edit
    /// needs it to.
    Unlinked(lash_kernel_check::Refusal),
}

pub(crate) type Fault = Box<FaultKind>;

pub(super) fn at(base: &Site, kind: EditDiagnosticKind) -> Fault {
    Box::new(FaultKind::At(Some(Location::Base(base.clone())), kind))
}

pub(super) fn plain(kind: EditDiagnosticKind) -> Fault {
    Box::new(FaultKind::At(None, kind))
}

/// What a transaction leaves when it is admitted.
pub(crate) struct Published {
    pub(crate) document: Document,
    pub(crate) admitted: Admitted,
    pub(crate) correspondence: Correspondence,
}

pub(crate) struct Working {
    pub(super) document: Document,
    shadows: BTreeMap<Unit, Shadow>,
}

fn body<'a>(document: &'a mut Document, unit: &Unit) -> Option<&'a mut Block> {
    match unit {
        Unit::Main => Some(&mut document.main),
        Unit::Function(name) => document
            .functions
            .get_mut(name)
            .map(|function| &mut function.body),
        Unit::Library(_) => None,
    }
}

pub(super) fn node<'a>(document: &'a mut Document, site: &Site) -> Option<NodeMut<'a>> {
    NodeMut::Block(body(document, &site.unit)?).descend(&site.path)
}

fn shadow<'a>(shadows: &'a mut BTreeMap<Unit, Shadow>, site: &Site) -> Option<&'a mut Shadow> {
    shadows.get_mut(&site.unit)?.descend(&site.path)
}

/// The arguments of an action, in the order it writes them.
fn arguments(action: &mut Action) -> Vec<&mut Atom> {
    match action {
        Action::Call { args, .. } | Action::Perform { args, .. } | Action::Spawn { args, .. } => {
            args.iter_mut().collect()
        }
        Action::Sleep { duration: atom, .. }
        | Action::Join { task: atom }
        | Action::JoinMany { tasks: atom, .. }
        | Action::Cancel { task: atom }
        | Action::Await { value: atom, .. } => vec![atom],
        Action::Yield => Vec::new(),
    }
}

/// The right-hand side of the `let` or assignment whose right-hand side is
/// the node at `site`.
fn rhs_at<'a>(document: &'a mut Document, site: &Site) -> Option<&'a mut Rhs> {
    let (index, parent) = site.path.split_last()?;
    let parent = Site::new(site.unit.clone(), parent);
    let (place, value) = match node(document, &parent)? {
        NodeMut::Stmt(Stmt::Let { value, .. }) => (None, value),
        NodeMut::Stmt(Stmt::Assign { place, value, .. }) => (Some(&*place), value),
        _ => return None,
    };
    // The right-hand side is the statement's last child, after the place's
    // target and index (`K-ID-004`).
    let at = match place {
        None | Some(Place::Variable(_)) => 0,
        Some(Place::Member(Member::Field { .. })) => 1,
        Some(Place::Member(Member::Index { .. })) => 2,
    };
    (*index == at).then_some(value)
}

impl Working {
    /// A copy of `document`, every node of which is a node of the base.
    pub(crate) fn open(document: &Document) -> Self {
        let mut shadows = BTreeMap::new();
        let main = Site::new(Unit::Main, []);
        shadows.insert(Unit::Main, Shadow::base(Node::Block(&document.main), main));
        for (name, function) in &document.functions {
            let unit = Unit::Function(name.clone());
            let site = Site::new(unit.clone(), []);
            shadows.insert(unit, Shadow::base(Node::Block(&function.body), site));
        }
        Self {
            document: document.clone(),
            shadows,
        }
    }

    /// Each unit's body and its shadow.
    fn units(&self) -> impl Iterator<Item = (Unit, Node<'_>, &Shadow)> {
        let functions = self
            .document
            .functions
            .iter()
            .map(|(name, function)| (Unit::Function(name.clone()), &function.body));
        std::iter::once((Unit::Main, &self.document.main))
            .chain(functions)
            .filter_map(|(unit, body)| {
                let root = self.shadows.get(&unit)?;
                Some((unit, Node::Block(body), root))
            })
    }

    /// Every site of the document.
    pub(crate) fn sites(&self) -> Vec<Site> {
        let mut sites = Vec::new();
        for (unit, body, root) in self.units() {
            root.walk(body, &mut Vec::new(), &mut |path, _| {
                sites.push(Site::new(unit.clone(), path));
            });
        }
        sites
    }

    /// The base identity of a surviving node at an edited address. New
    /// wrappers have no base identity and cannot be offered as base targets.
    pub(super) fn origin(&self, site: &Site) -> Option<Site> {
        let (_, _, mut shadow) = self.units().find(|(unit, _, _)| unit == &site.unit)?;
        for index in &site.path {
            shadow = shadow.children.get(*index as usize)?;
        }
        shadow.origin.clone()
    }

    /// Where the node that was at `base` is now.
    pub(super) fn locate(&self, base: &Site) -> Result<Site, Fault> {
        self.units()
            .find_map(|(unit, body, root)| Some(Site::new(unit, root.find(body, base)?)))
            .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))
    }

    /// Where the node that was at `base` is now, when it is a node of
    /// `class`.
    pub(super) fn locate_as(&mut self, base: &Site, class: NodeClass) -> Result<Site, Fault> {
        let site = self.locate(base)?;
        let found = NodeClass::of(
            node(&mut self.document, &site)
                .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))?
                .as_node(),
        );
        if found == class {
            Ok(site)
        } else {
            Err(at(
                base,
                EditDiagnosticKind::WrongNode {
                    expected: class,
                    found,
                },
            ))
        }
    }

    /// The block that holds the statement that was at `base`, and the
    /// statement's index in it.
    pub(super) fn statement(&mut self, base: &Site) -> Result<(Site, usize), Fault> {
        let mut block = self.locate_as(base, NodeClass::Statement)?;
        let index = block
            .path
            .pop()
            .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))?;
        Ok((block, index as usize))
    }

    /// The block a position names, and the index it puts a statement at.
    pub(super) fn position(&mut self, position: &Position) -> Result<(Site, usize), Fault> {
        let block = self.locate_as(&position.block, NodeClass::Block)?;
        let index = match &position.before {
            Some(before) => {
                let (holder, index) = self.statement(before)?;
                if holder != block {
                    return Err(at(before, EditDiagnosticKind::NotInBlock));
                }
                index
            }
            None => self.block(&block).len(),
        };
        Ok((block, index))
    }

    /// The statements of the block at `site`, which a `locate_as` has shown
    /// to be one.
    pub(super) fn block(&mut self, site: &Site) -> &Block {
        match node(&mut self.document, site) {
            Some(NodeMut::Block(block)) => block,
            _ => unreachable!("the site was resolved to a block"),
        }
    }

    pub(super) fn shadow(&mut self, site: &Site) -> &mut Shadow {
        match shadow(&mut self.shadows, site) {
            Some(shadow) => shadow,
            None => unreachable!("every node of the document has a shadow"),
        }
    }

    /// Changes the statements of the block at `site`, which a `locate_as`
    /// has shown to be one, and their shadows through `change`, which holds
    /// both for the whole change. A loop's `else` the change leaves empty is
    /// no longer a node (`K-ID-004`), and its shadow goes with it.
    pub(super) fn splice<R>(
        &mut self,
        site: &Site,
        change: impl FnOnce(&mut Block, &mut Vec<Shadow>) -> R,
    ) -> R {
        let Some(NodeMut::Block(block)) = node(&mut self.document, site) else {
            unreachable!("the site was resolved to a block");
        };
        let Some(shadow) = shadow(&mut self.shadows, site) else {
            unreachable!("every node of the document has a shadow");
        };
        let changed = change(block, &mut shadow.children);
        if block.is_empty()
            && let Some((_, parent)) = site.path.split_last()
        {
            let parent = Site::new(site.unit.clone(), parent);
            let Some(holder) = node(&mut self.document, &parent) else {
                unreachable!("a block's parent is a node");
            };
            let children = holder.as_node().children().len();
            self.shadow(&parent).children.truncate(children);
        }
        changed
    }

    pub(super) fn insert(&mut self, block: &Site, index: usize, statement: Stmt, shadow: Shadow) {
        self.splice(block, |statements, shadows| {
            statements.insert(index, statement);
            shadows.insert(index, shadow);
        });
    }

    pub(super) fn take(&mut self, block: &Site, index: usize) -> (Stmt, Shadow) {
        self.splice(block, |statements, shadows| {
            (statements.remove(index), shadows.remove(index))
        })
    }

    /// Replaces the node at `site`'s content through `write`, which returns
    /// a fault to leave it as it was.
    pub(super) fn write(
        &mut self,
        site: &Site,
        write: impl FnOnce(&mut NodeMut<'_>) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        let Some(mut node) = node(&mut self.document, site) else {
            unreachable!("the site was resolved to a node");
        };
        write(&mut node)?;
        let Some(shadow) = shadow(&mut self.shadows, site) else {
            unreachable!("every node of the document has a shadow");
        };
        shadow.replace(node.as_node());
        Ok(())
    }

    pub(crate) fn edit(&mut self, edit: &Edit) -> Result<(), Fault> {
        match edit {
            Edit::InsertStatement { at, statement } => {
                let (block, index) = self.position(at)?;
                let shadow = Shadow::fresh(Node::Stmt(statement));
                self.insert(&block, index, statement.clone(), shadow);
            }
            Edit::RemoveStatement { statement } => {
                let (block, index) = self.statement(statement)?;
                self.take(&block, index);
            }
            Edit::MoveStatement { statement, to } => self.move_statement(statement, to)?,
            Edit::ReplaceExpression { expression, with } => {
                let site = self.locate_as(expression, NodeClass::Expression)?;
                self.write(&site, |node| {
                    if let NodeMut::Expr(expr) = node {
                        **expr = with.clone();
                    }
                    Ok(())
                })?;
            }
            Edit::SetArgument {
                action,
                index,
                argument,
            } => self.set_argument(action, *index, argument)?,
            Edit::SetCondition {
                statement,
                condition,
            } => {
                let site = self.locate_as(statement, NodeClass::Statement)?;
                let Some(NodeMut::Stmt(
                    Stmt::If {
                        condition: slot, ..
                    }
                    | Stmt::While {
                        condition: slot, ..
                    },
                )) = node(&mut self.document, &site)
                else {
                    return Err(at(statement, EditDiagnosticKind::NoCondition));
                };
                *slot = condition.clone();
                // The condition is the statement's first child (`K-ID-004`).
                self.shadow(&site.child(0)).replace(Node::Expr(condition));
            }
            Edit::RemoveFunction { name } => {
                self.declared(name)?;
                self.document.functions.remove(name);
                self.document.entries.remove(name);
                self.shadows.remove(&Unit::Function(name.clone()));
            }
            Edit::SetPrivateBindings { names } => {
                self.document.private_bindings.clone_from(names);
            }
            Edit::RemoveEntry { function } => {
                self.entry(function)?;
                self.document.entries.remove(function);
            }
        }
        Ok(())
    }

    /// [`Edit::SetArgument`] of the action that was at `base`, a
    /// statement's own or one nested in an expression. The argument's node
    /// is new and the action is edited; a right-hand side changes between
    /// the statement's action and a nested one as its arguments require.
    fn set_argument(&mut self, base: &Site, index: u32, argument: &Expr) -> Result<(), Fault> {
        let site = self.locate(base)?;
        let slot = usize::try_from(index).unwrap_or(usize::MAX);
        let missing = |arguments: usize| {
            at(
                base,
                EditDiagnosticKind::NoSuchArgument {
                    index,
                    arguments: u32::try_from(arguments).unwrap_or(u32::MAX),
                },
            )
        };
        let atom = match argument {
            Expr::Variable(name) => Some(Atom::Variable(name.clone())),
            Expr::Literal(literal) => Some(Atom::Literal(literal.clone())),
            _ => None,
        };
        let rhs = match node(&mut self.document, &site) {
            Some(NodeMut::Action(action)) => {
                let count = arguments(action).len();
                if slot >= count {
                    return Err(missing(count));
                }
                if let Some(atom) = atom {
                    if let Some(written) = arguments(action).into_iter().nth(slot) {
                        *written = atom;
                    }
                    self.shadow(&site).edited = true;
                    return Ok(());
                }
                let Expr::Act(mut act) = Expr::from(action.clone()) else {
                    unreachable!()
                };
                act.args[slot] = argument.clone();
                let Some(rhs) = rhs_at(&mut self.document, &site) else {
                    unreachable!("statement actions are right-hand sides");
                };
                *rhs = Rhs::Expr(Expr::Act(act));
                rhs
            }
            Some(NodeMut::Expr(Expr::Act(act))) => {
                let count = act.args.len();
                let Some(written) = act.args.get_mut(slot) else {
                    return Err(missing(count));
                };
                *written = argument.clone();
                let action = act.as_action();
                let shadow = self.shadow(&site);
                shadow.edited = true;
                shadow.children[slot] = Shadow::fresh(Node::Expr(argument));
                match (action, rhs_at(&mut self.document, &site)) {
                    (Some(action), Some(rhs)) => {
                        *rhs = Rhs::Action(action);
                        rhs
                    }
                    _ => return Ok(()),
                }
            }
            Some(found) => {
                return Err(at(
                    base,
                    EditDiagnosticKind::WrongNode {
                        expected: NodeClass::Action,
                        found: NodeClass::of(found.as_node()),
                    },
                ));
            }
            None => return Err(at(base, EditDiagnosticKind::NoSuchNode)),
        };
        let written = match rhs {
            Rhs::Expr(expr) => Node::Expr(expr),
            Rhs::Action(action) => Node::Action(action),
        };
        let Some(shadow) = shadow(&mut self.shadows, &site) else {
            unreachable!("every node of the document has a shadow");
        };
        // The right-hand side survives; its arguments are nodes only while
        // it is nested, and an atom it had was none.
        shadow.edited = true;
        shadow.children = written.children().into_iter().map(Shadow::fresh).collect();
        Ok(())
    }

    fn declared(&self, name: &Name) -> Result<(), Fault> {
        if self.document.functions.contains_key(name) {
            Ok(())
        } else {
            Err(plain(EditDiagnosticKind::NoSuchFunction {
                name: name.clone(),
            }))
        }
    }

    fn entry(&self, name: &Name) -> Result<(), Fault> {
        if self.document.entries.contains_key(name) {
            Ok(())
        } else {
            Err(plain(EditDiagnosticKind::NoSuchEntry {
                name: name.clone(),
            }))
        }
    }

    fn move_statement(&mut self, statement: &Site, to: &Position) -> Result<(), Fault> {
        let (from, at_index) = self.statement(statement)?;
        let (mut block, mut index) = self.position(to)?;
        let moved = from.child(u32::try_from(at_index).unwrap_or(u32::MAX));
        if block.unit == moved.unit && block.path.starts_with(&moved.path) {
            return Err(at(statement, EditDiagnosticKind::IntoItself));
        }
        // Taking the statement out shifts what follows it in its block.
        if block == from {
            if index > at_index {
                index -= 1;
            }
            // One change, so the block is never empty part way through.
            self.splice(&from, |statements, shadows| {
                let taken = statements.remove(at_index);
                statements.insert(index, taken);
                let shadow = shadows.remove(at_index);
                shadows.insert(index, shadow);
            });
            return Ok(());
        }
        if block.unit == from.unit
            && block.path.starts_with(&from.path)
            && let Some(step) = block.path.get_mut(from.path.len())
            && *step as usize > at_index
        {
            *step -= 1;
        }
        let (taken, shadow) = self.take(&from, at_index);
        self.insert(&block, index, taken, shadow);
        Ok(())
    }

    /// Derives the manifest the code now requires (`CANVAS-STEP`), admits
    /// the document and publishes it with the correspondence from `base`.
    pub(crate) fn publish(
        mut self,
        base: DocumentId,
        environment: &Environment<'_>,
    ) -> Result<Published, EditRefusal> {
        let required = requirements(&self.document, environment.functions);
        let manifest = &mut self.document.manifest;
        manifest.functions = required.functions;
        manifest.effects.retain(|effect, _| {
            required.effects.contains(effect) || required.performs.contains_key(effect)
        });
        for effect in required.effects {
            if let Some(offered) = environment.effects.get(&effect) {
                manifest
                    .effects
                    .entry(effect)
                    .or_insert_with(|| offered.clone());
            }
        }
        // What the bodies the code reaches perform, it requires through
        // them (`K-DOC-002`).
        for (effect, bodies) in required.performs {
            if let Some(requirement) = bodies.into_values().next() {
                manifest.effects.entry(effect).or_insert(requirement);
            }
        }

        let admitted =
            admit(&self.document, environment).map_err(|refusal| refused(None, refusal))?;
        let correspondence = self.correspondence(base, admitted.identity)?;
        Ok(Published {
            document: self.document,
            admitted,
            correspondence,
        })
    }

    /// The correspondence the shadows hold, from `base` to the document of
    /// identity `result`.
    fn correspondence(
        &self,
        base: DocumentId,
        result: DocumentId,
    ) -> Result<Correspondence, EditRefusal> {
        let mut survivors = Vec::new();
        for (unit, body, root) in self.units() {
            root.walk(body, &mut Vec::new(), &mut |path, shadow| {
                if let Some(origin) = &shadow.origin {
                    survivors.push(Survivor {
                        from: origin.clone(),
                        to: Site::new(unit.clone(), path),
                        edited: shadow.edited,
                    });
                }
            });
        }
        Correspondence::of(base, result, survivors).ok_or_else(invalid_correspondence)
    }
}
