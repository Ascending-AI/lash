//! Applying edits to a copy of a document.
//!
//! [`Working`] is that copy and its shadows (see [`crate::tree`]). An edit
//! resolves the base sites it names to where those nodes are now, changes
//! the document and changes the shadows the same way. Nothing here admits:
//! [`Working::publish`] hands the result to the checker.

use lash_kernel_doc::{Correspondence, Survivor};

use lash_kernel_value::names::DocumentId;

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{Admitted, Environment, admit};
use lash_kernel_doc::{
    Action, Atom, Block, Document, Expr, Member, Node, NodeMut, Place, Rhs, Stmt,
};

use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;

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
        Unit::Main => document.main.as_mut(),
        Unit::Library(id) => document
            .functions
            .get_mut(id)
            .and_then(lash_kernel_doc::FunctionDefinition::body_mut)
            .map(|body| &mut body.block),
        Unit::Function(_) => None,
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
        Action::Join { task: atom }
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
        for (unit, body) in document.units() {
            let site = Site::new(unit.clone(), []);
            shadows.insert(unit, Shadow::base(Node::Block(body), site));
        }
        Self {
            document: document.clone(),
            shadows,
        }
    }

    pub(super) fn replace_document(&mut self, document: &Document) {
        *self = Self::open(document);
        for shadow in self.shadows.values_mut() {
            shadow.clear_origins();
        }
    }

    /// Each unit's body and its shadow.
    fn units(&self) -> impl Iterator<Item = (Unit, Node<'_>, &Shadow)> {
        self.document.units().filter_map(|(unit, body)| {
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
    pub(super) fn position(
        &mut self,
        position: &Position,
        source: &lash_kernel_dialect::conventions::Conventions,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<(Site, usize), Fault> {
        let block = self.locate_as(&position.block, NodeClass::Block)?;
        let index = match &position.before {
            Some(before) => {
                let (holder, index) = self.statement(before)?;
                if holder != block {
                    return Err(at(before, EditDiagnosticKind::NotInBlock));
                }
                index
            }
            None => source
                .canvas_end(&self.document, &block, catalog)
                .map_err(|reason| {
                    at(
                        &position.block,
                        EditDiagnosticKind::Unprintable {
                            reason: lash_kernel_dialect::UnprintableReason::ImplicitReturn(reason),
                        },
                    )
                })?
                .unwrap_or_else(|| self.block(&block).len()),
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

    pub(crate) fn edit(
        &mut self,
        edit: &Edit,
        source: &lash_kernel_dialect::conventions::Conventions,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<(), Fault> {
        match edit {
            Edit::InsertStatement { at, statement } => {
                let (block, index) = self.position(at, source, catalog)?;
                let shadow = Shadow::fresh(Node::Stmt(statement));
                self.insert(&block, index, *statement.clone(), shadow);
            }
            Edit::RemoveStatement { statement } => {
                let (block, index) = self.statement(statement)?;
                self.take(&block, index);
            }
            Edit::MoveStatement { statement, to } => {
                self.move_statement(statement, to, source, catalog)?
            }
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

    fn move_statement(
        &mut self,
        statement: &Site,
        to: &Position,
        source: &lash_kernel_dialect::conventions::Conventions,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<(), Fault> {
        let (from, at_index) = self.statement(statement)?;
        let (mut block, mut index) = self.position(to, source, catalog)?;
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

    pub(super) fn reidentify(
        &mut self,
    ) -> Result<BTreeMap<lash_kernel_value::FunctionId, lash_kernel_value::FunctionId>, String>
    {
        let reidentified = self.document.reidentify_functions()?;
        self.shadows = std::mem::take(&mut self.shadows)
            .into_iter()
            .map(|(unit, shadow)| {
                let unit = match unit {
                    Unit::Library(id) => {
                        Unit::Library(reidentified.get(&id).copied().unwrap_or(id))
                    }
                    unit => unit,
                };
                (unit, shadow)
            })
            .collect();
        Ok(reidentified)
    }

    /// Derives the manifest the code now requires (`CANVAS-STEP`), admits
    /// the document and publishes it with the correspondence from `base`.
    pub(crate) fn publish(
        mut self,
        base: DocumentId,
        environment: &Environment<'_>,
    ) -> Result<Published, EditRefusal> {
        self.refresh_dependencies(environment.functions);
        self.reidentify().map_err(|message| {
            crate::refusal::refused(
                None,
                lash_kernel_check::Refusal {
                    errors: vec![lash_kernel_check::Refused {
                        site: None,
                        reason: lash_kernel_check::RefusalReason::Invalid(
                            lash_kernel_doc::InvalidReason::Document { message },
                        ),
                    }],
                },
            )
        })?;
        let admitted =
            admit(&self.document, environment).map_err(|refusal| refused(None, refusal))?;
        let correspondence = self.correspondence(base, admitted.identity)?;
        Ok(Published {
            document: self.document,
            admitted,
            correspondence,
        })
    }

    /// An edit can remove the last call of a native helper. Rebuild local
    /// body dependencies before hashing them, then release native definitions
    /// no retained code uses. Guest declarations remain authored document data,
    /// including declarations no start currently reaches.
    fn refresh_dependencies(&mut self, catalog: &dyn lash_kernel_doc::FunctionCatalog) {
        let constructors: Vec<_> = self
            .document
            .units()
            .flat_map(|(_, block)| Node::Block(block).functions_within())
            .filter(|id| {
                !self.document.functions.contains_key(id)
                    && catalog.header(id).is_some_and(|h| {
                        h.has_native()
                            && matches!(h.name.as_str(), "error.new" | "timestamp.from_nanoseconds")
                    })
            })
            .collect();
        self.document.library.0.extend(constructors);
        let mut names = self.document.function_names();
        names.extend(
            self.document
                .library
                .iter()
                .filter_map(|id| catalog.header(id).map(|header| (*id, header.name.clone()))),
        );
        for function in self.document.functions.values_mut() {
            if function.has_native() {
                continue;
            }
            if let Some(body) = function.body_mut() {
                body.functions = Node::Block(&body.block)
                    .functions_within()
                    .into_iter()
                    .filter_map(|id| names.get(&id).cloned().map(|name| (id, name)))
                    .collect();
                body.imports = Node::Block(&body.block)
                    .imports_within()
                    .into_iter()
                    .filter_map(|slot| {
                        self.document
                            .imports
                            .get(&slot)
                            .cloned()
                            .map(|import| (slot, import))
                    })
                    .collect();
            }
        }
        let mut retained: BTreeSet<_> = self
            .document
            .functions
            .iter()
            .filter(|(_, function)| !function.has_native())
            .map(|(id, _)| *id)
            .chain(
                lash_kernel_check::requirements(&self.document, catalog)
                    .functions
                    .into_keys(),
            )
            .collect();
        let mut pending: Vec<_> = retained.iter().copied().collect();
        while let Some(id) = pending.pop() {
            if let Some(body) = self
                .document
                .functions
                .get(&id)
                .and_then(|function| function.body())
            {
                for dependency in Node::Block(&body.block).functions_within() {
                    if retained.insert(dependency) {
                        pending.push(dependency);
                    }
                }
            }
        }
        self.document
            .functions
            .retain(|id, _| retained.contains(id));
        // The manifest records direct references, not the transitive helpers
        // those references happen to reach in this build.
        let mut direct: BTreeSet<_> = self
            .document
            .units()
            .flat_map(|(_, block)| Node::Block(block).functions_within())
            .collect();
        for export in self.document.exports.values() {
            let functions = match export {
                lash_kernel_doc::Export::Function(function) => vec![function],
                lash_kernel_doc::Export::Type(ty) => {
                    direct.insert(ty.checker);
                    ty.members
                        .values()
                        .map(|member| member.function())
                        .collect()
                }
                lash_kernel_doc::Export::Error { .. } => continue,
            };
            for function in functions {
                direct.insert(function.function);
                direct.extend(function.convention.functions().into_iter().copied());
            }
        }
        self.document.library.0.retain(|id| direct.contains(id));
        self.document
            .requires
            .natives
            .retain(|id| retained.contains(id));
        self.shadows.retain(|unit, _| match unit {
            Unit::Library(id) => retained.contains(id),
            _ => true,
        });
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
