//! Host-neutral construction of canonical edit transactions.
//!
//! [`plan`] lowers convenience operations into ordinary [`Edit`] values. It
//! neither mutates nor admits the draft. Apply its result once with
//! [`Draft::apply`] to commit the complete batch or refuse it unchanged.
//!
//! Every position addresses the base document, including direct edits between
//! helpers. Generated statements become targetable by site after admission,
//! in the next transaction. Within this batch, later helpers can refer to an
//! earlier helper's explicitly named output with [`Expr::Variable`]. Generated
//! temporaries are ordinary visible statements, with no persistent grouping.

use std::collections::BTreeSet;

use crate::{Draft, Edit, Position, Transaction};
use lash_kernel_doc::{
    Action, Atom, Callee, Document, EffectName, Expr, Function, Literal, Name, Node, Place, Rhs,
    Stmt, Type,
};

/// One host authoring operation, lowered by [`plan`] to canonical edits.
///
/// Operations have no serialized representation. Hosts own their request
/// contracts; the resulting [`Transaction`] uses the existing edit encoding.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Operation {
    /// The complete existing edit vocabulary, with base-document targets.
    Edit(Edit),
    /// Insert a named effect result. Computed inputs become preceding `let`s.
    ///
    /// `binding` is the caller's name, preserved verbatim. Ordinary admission
    /// decides whether the name, scope, effect and result type are valid.
    InsertEffect {
        /// Insertion position in the transaction's base document.
        at: Position,
        /// The effect to perform, checked against the admission environment.
        effect: EffectName,
        /// Arguments in evaluation order. Computed expressions become `let`s.
        args: Vec<Expr>,
        /// The result type stated by the canonical `perform` action.
        result: Type,
        /// Caller-selected output name. It is preserved, not made fresh.
        binding: Name,
    },
}

/// Build one transaction without executing effects or changing the draft.
///
/// Temporary names avoid every variable or function name declared or read in
/// the base document and the entire batch, including later direct edits.
/// Reservation is conservative across scopes. User names and references are
/// never rewritten. Literal and variable inputs need no temporary; computed
/// inputs are bound in argument order immediately before their effect.
///
/// The complete iterator is collected before names are reserved. Operations
/// lower in their supplied order; the same draft and operations produce the
/// same transaction. Generated-name spelling is not part of this contract.
/// Name reservation covers this supplied batch; callers own any additional
/// edits they append to or change in the returned transaction.
///
/// All targets address the base document. A later operation can reference an
/// earlier effect's named output when lexical scope permits, but newly inserted
/// statements have targetable sites only after applying this transaction.
/// Planning performs no admission: [`Draft::apply`] checks targets, scope and
/// types once for the complete transaction and refuses it without mutation.
/// Its diagnostic indices name the expanded canonical edits, including any
/// generated argument bindings, rather than the input operations.
#[must_use]
pub fn plan(draft: &Draft, operations: impl IntoIterator<Item = Operation>) -> Transaction {
    let operations: Vec<_> = operations.into_iter().collect();
    let mut names = Names::default();
    names.document(draft.document());
    for operation in &operations {
        match operation {
            Operation::Edit(edit) => names.edit(edit),
            Operation::InsertEffect { args, binding, .. } => {
                names.reserve(binding);
                for arg in args {
                    names.node(Node::Expr(arg));
                }
            }
        }
    }

    let mut edits = Vec::new();
    for operation in operations {
        match operation {
            Operation::Edit(edit) => edits.push(edit),
            Operation::InsertEffect {
                at,
                effect,
                args,
                result,
                binding,
            } => {
                let args = args
                    .into_iter()
                    .map(|arg| match arg {
                        Expr::Variable(name) => Atom::Variable(name),
                        Expr::Literal(value) => Atom::Literal(value),
                        expression => {
                            let name = names.fresh();
                            edits.push(Edit::InsertStatement {
                                at: at.clone(),
                                statement: Stmt::Let {
                                    name: name.clone(),
                                    value: Rhs::Expr(expression),
                                },
                            });
                            Atom::Variable(name)
                        }
                    })
                    .collect();
                edits.push(Edit::InsertStatement {
                    at,
                    statement: Stmt::Let {
                        name: binding,
                        value: Rhs::Action(Action::Perform {
                            effect,
                            args,
                            result,
                        }),
                    },
                });
            }
        }
    }
    Transaction {
        base: draft.identity(),
        edits,
    }
}

#[derive(Default)]
struct Names {
    reserved: BTreeSet<Name>,
    next: usize,
}

impl Names {
    fn reserve(&mut self, name: &Name) {
        self.reserved.insert(name.clone());
    }

    fn fresh(&mut self) -> Name {
        loop {
            let name = Name::new(format!("__authoring_arg_{}", self.next));
            self.next += 1;
            if self.reserved.insert(name.clone()) {
                return name;
            }
        }
    }

    fn document(&mut self, document: &Document) {
        self.reserved.extend(document.private_bindings.clone());
        self.reserved.extend(document.entries.keys().cloned());
        for (name, function) in &document.functions {
            self.reserve(name);
            self.function(function);
        }
        self.node(Node::Block(&document.main));
    }

    fn function(&mut self, function: &Function) {
        self.reserved.extend(function.params.iter().cloned());
        self.node(Node::Block(&function.body));
    }

    fn atom(&mut self, atom: &Atom) {
        match atom {
            Atom::Variable(name) | Atom::Literal(Literal::Function(name)) => {
                self.reserve(name);
            }
            Atom::Literal(_) => {}
        }
    }

    fn action(&mut self, action: &Action) {
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                match callee {
                    Callee::Declared(name) | Callee::Value(name) => self.reserve(name),
                    Callee::Library(_) => {}
                }
                for arg in args {
                    self.atom(arg);
                }
            }
            Action::Perform { args, .. } => {
                for arg in args {
                    self.atom(arg);
                }
            }
            Action::Sleep { duration: arg }
            | Action::Join { task: arg }
            | Action::JoinMany { tasks: arg, .. }
            | Action::Cancel { task: arg } => self.atom(arg),
            Action::Yield => {}
        }
    }

    fn node(&mut self, root: Node<'_>) {
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node {
                Node::Block(_) => {}
                Node::Action(action) => self.action(action),
                Node::Stmt(statement) => match statement {
                    Stmt::Let { name, .. }
                    | Stmt::For { binding: name, .. }
                    | Stmt::Assign {
                        place: Place::Variable(name),
                        ..
                    } => self.reserve(name),
                    Stmt::Try(scope) => {
                        if let Some(catch) = &scope.catch {
                            self.reserve(&catch.binding);
                        }
                    }
                    Stmt::Assign { .. }
                    | Stmt::Remove { .. }
                    | Stmt::Do { .. }
                    | Stmt::If { .. }
                    | Stmt::While { .. }
                    | Stmt::Break
                    | Stmt::Continue
                    | Stmt::Return { .. }
                    | Stmt::Throw { .. }
                    | Stmt::Print { .. }
                    | Stmt::Finish { .. }
                    | Stmt::Fail { .. } => {}
                },
                Node::Expr(expression) => match expression {
                    Expr::Variable(name) | Expr::Literal(Literal::Function(name)) => {
                        self.reserve(name);
                    }
                    Expr::Closure(closure) => {
                        self.reserved.extend(closure.params.iter().cloned());
                    }
                    Expr::Literal(_)
                    | Expr::Tuple(_)
                    | Expr::List(_)
                    | Expr::Map(_)
                    | Expr::Set(_)
                    | Expr::Record(_)
                    | Expr::Member(_)
                    | Expr::Call { .. }
                    | Expr::Clock
                    | Expr::Random
                    | Expr::Read(_) => {}
                },
            }
            pending.extend(node.children());
        }
    }

    fn edit(&mut self, edit: &Edit) {
        match edit {
            Edit::InsertStatement { statement, .. }
            | Edit::ReplaceStatement {
                with: statement, ..
            } => self.node(Node::Stmt(statement)),
            Edit::ReplaceExpression { with, .. }
            | Edit::SetCondition {
                condition: with, ..
            } => self.node(Node::Expr(with)),
            Edit::ReplaceAction { with, .. } => self.action(with),
            Edit::SetArgument { argument, .. } => self.atom(argument),
            Edit::SetCatch { catch, .. } => {
                if let Some(catch) = catch {
                    self.reserve(&catch.binding);
                    self.node(Node::Block(&catch.body));
                }
            }
            Edit::SetFinally { finally, .. } => {
                if let Some(finally) = finally {
                    self.node(Node::Block(finally));
                }
            }
            Edit::RenameVariable { name, to, .. }
            | Edit::RenameFunction { from: name, to }
            | Edit::RenameEntry { from: name, to } => {
                self.reserve(name);
                self.reserve(to);
            }
            Edit::InsertFunction { name, function } | Edit::ReplaceFunction { name, function } => {
                self.reserve(name);
                self.function(function);
            }
            Edit::SetPrivateBindings { names } => self.reserved.extend(names.iter().cloned()),
            Edit::RemoveFunction { name }
            | Edit::InsertEntry { function: name, .. }
            | Edit::RemoveEntry { function: name }
            | Edit::SetEntrySignature { function: name, .. } => self.reserve(name),
            Edit::RemoveStatement { .. }
            | Edit::MoveStatement { .. }
            | Edit::CloneStatement { .. }
            | Edit::SetLabel { .. }
            | Edit::SetData { .. }
            | Edit::SetEffectSignature { .. }
            | Edit::SetNumberPolicy { .. }
            | Edit::ReplaceFunctionIdentity { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use lash_kernel_check::Environment;
    use lash_kernel_doc::{FunctionRegistry, NumberPolicy, Param, Signature, Site, Unit};

    use super::*;

    fn variable(name: &str) -> Expr {
        Expr::Variable(Name::new(name))
    }

    fn binding(name: &str) -> Stmt {
        Stmt::Let {
            name: Name::new(name),
            value: Rhs::Expr(Expr::Literal(Literal::Text(name.to_owned()))),
        }
    }

    /// K-EDIT-001/002: helpers preserve base anchors and commit as one batch.
    /// Fresh temporaries must not capture a name supplied anywhere in it.
    #[test]
    fn mixed_helpers_reserve_the_whole_batch_and_refuse_atomically() {
        let mut draft = Draft::open(
            Document::new(
                NumberPolicy::Float,
                vec![
                    binding("__authoring_arg_0"),
                    Stmt::Print {
                        value: variable("__authoring_arg_0"),
                    },
                ],
            ),
            None,
        )
        .unwrap();
        let functions = FunctionRegistry::default();
        let mut environment = Environment::new(&functions);
        let effect = EffectName::new("mock.collect").unwrap();
        environment.effects.insert(
            effect.clone(),
            Signature {
                params: vec![Param {
                    name: Name::new("items"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
        let anchor = Site::new(Unit::Main, [1]);
        let at = Position::before(anchor.clone()).unwrap();
        let steps = vec![
            Operation::InsertEffect {
                at: at.clone(),
                effect: effect.clone(),
                args: vec![Expr::List(vec![variable("__authoring_arg_0")])],
                result: Type::Any,
                binding: Name::new("first"),
            },
            Operation::Edit(Edit::InsertStatement {
                at: at.clone(),
                statement: binding("__authoring_arg_1"),
            }),
            Operation::InsertEffect {
                at: at.clone(),
                effect,
                args: vec![Expr::List(vec![
                    variable("first"),
                    variable("__authoring_arg_1"),
                ])],
                result: Type::Any,
                binding: Name::new("__authoring_arg_2"),
            },
            Operation::Edit(Edit::InsertStatement {
                at: at.clone(),
                statement: binding("__authoring_arg_3"),
            }),
        ];
        let transaction = plan(&draft, steps.clone());
        let original = (draft.document().clone(), draft.annotations().clone());
        let mut invalid = steps;
        invalid.push(Operation::Edit(Edit::InsertStatement {
            at,
            statement: Stmt::Print {
                value: variable("missing_session_binding"),
            },
        }));
        let refused = plan(&draft, invalid);
        assert!(draft.apply(&refused, &environment).is_err());
        assert_eq!(
            (draft.document().clone(), draft.annotations().clone()),
            original
        );

        let applied = draft.apply(&transaction, &environment).unwrap();
        let names: Vec<_> = draft
            .document()
            .main
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Let { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            [
                "__authoring_arg_0",
                "__authoring_arg_4",
                "first",
                "__authoring_arg_1",
                "__authoring_arg_5",
                "__authoring_arg_2",
                "__authoring_arg_3",
            ]
        );
        assert_eq!(
            draft.document().main[4],
            Stmt::Let {
                name: Name::new("__authoring_arg_5"),
                value: Rhs::Expr(Expr::List(vec![
                    variable("first"),
                    variable("__authoring_arg_1"),
                ])),
            }
        );
        assert_eq!(
            draft.document().main[5],
            Stmt::Let {
                name: Name::new("__authoring_arg_2"),
                value: Rhs::Action(Action::Perform {
                    effect: EffectName::new("mock.collect").unwrap(),
                    args: vec![Atom::Variable(Name::new("__authoring_arg_5"))],
                    result: Type::Any,
                }),
            }
        );
        assert_eq!(
            applied.correspondence.successor(&anchor),
            Some(&Site::new(Unit::Main, [7]))
        );
    }
}
