//! Type facets and the signature checks built on them (`K-ADM-005`,
//! `K-ADM-006`).
//!
//! A facet is what the document alone says of a value: the type of a
//! literal, of a construction, of a library call's or an effect's stated
//! result. A document's variables are untyped, so most facets are `Any`, and
//! a check refuses only what can never be right.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    EffectName, Function, FunctionCatalog, JoinMode, Literal, MapType, Name, RecordType,
    RecordTypeField, Signature, Type,
};

use crate::graph::{
    ActionNode, AtomNode, CalleeNode, ExprNode, Graph, MemberNode, NodeId, NodeKind, PlaceNode,
    Reference, StmtNode,
};
use crate::refusal::{RefusalReason, Refused, refused};
use crate::types::{common, disjoint, function_of, stable, union};

pub(crate) struct Typer<'a> {
    pub(crate) graph: &'a mut Graph,
    pub(crate) catalog: &'a dyn FunctionCatalog,
    /// The effects the code may perform; none inside a library body.
    pub(crate) effects: Option<&'a BTreeMap<EffectName, Signature>>,
    /// The declared functions a literal may reference.
    pub(crate) declared: Option<&'a BTreeMap<Name, Function>>,
    pub(crate) errors: &'a mut Vec<Refused>,
}

impl Typer<'_> {
    pub(crate) fn run(&mut self) {
        let bodies: Vec<NodeId> = self.graph.units.iter().map(|unit| unit.body).collect();
        for body in bodies {
            self.block(body);
        }
    }

    fn block(&mut self, id: NodeId) {
        if let NodeKind::Block(statements) = self.graph.node(id).kind.clone() {
            for stmt in statements {
                self.stmt(stmt);
            }
        }
    }

    fn bind(&mut self, binding: crate::graph::BindingId, facet: &Type) {
        let binding = &mut self.graph.bindings[binding.index()];
        if !binding.assigned {
            binding.facet = stable(facet);
        }
    }

    fn stmt(&mut self, id: NodeId) {
        let NodeKind::Stmt(stmt) = self.graph.node(id).kind.clone() else {
            return;
        };
        match stmt {
            StmtNode::Let { binding, value } => {
                let facet = self.value(value);
                self.bind(binding, &facet);
            }
            StmtNode::Assign { place, value } => {
                if let PlaceNode::Member(member) = &place {
                    self.member_parts(member);
                }
                self.value(value);
            }
            StmtNode::Remove { member } => {
                self.member_parts(&member);
            }
            StmtNode::Do { action } => {
                self.value(action);
            }
            StmtNode::If {
                condition,
                then_block,
                else_block,
            } => {
                self.value(condition);
                self.block(then_block);
                self.block(else_block);
            }
            StmtNode::For {
                binding,
                iterable,
                body,
            } => {
                let element = match self.value(iterable) {
                    Type::List(item) | Type::Set(item) => *item,
                    Type::Map(map) => map.key,
                    Type::Tuple(members) => union(members),
                    _ => Type::Any,
                };
                self.bind(binding, &element);
                self.block(body);
            }
            StmtNode::While { condition, body } => {
                self.value(condition);
                self.block(body);
            }
            StmtNode::Break { .. } | StmtNode::Continue { .. } => {}
            StmtNode::Return { value }
            | StmtNode::Throw { value }
            | StmtNode::Print { value }
            | StmtNode::Finish { value }
            | StmtNode::Fail { value } => {
                self.value(value);
            }
            StmtNode::Try {
                body,
                catch,
                finally,
            } => {
                self.block(body);
                if let Some(catch) = catch {
                    self.block(catch.body);
                }
                if let Some(finally) = finally {
                    self.block(finally);
                }
            }
        }
    }

    /// Types the expressions of a member and returns their facets.
    fn member_parts(&mut self, member: &MemberNode) -> (Type, Option<Type>) {
        match member {
            MemberNode::Field { target, .. } => (self.value(*target), None),
            MemberNode::Index { target, index } => {
                let target = self.value(*target);
                (target, Some(self.value(*index)))
            }
        }
    }

    /// Types an expression or an action, records its facet and returns it.
    fn value(&mut self, id: NodeId) -> Type {
        let facet = match self.graph.node(id).kind.clone() {
            NodeKind::Expr(expr) => self.expr(expr),
            NodeKind::Action(action) => self.action(id, &action),
            NodeKind::Block(_) | NodeKind::Stmt(_) => return Type::Any,
        };
        self.graph.nodes[id.index()].facet = Some(facet.clone());
        facet
    }

    fn reference(&self, reference: &Reference) -> Type {
        match reference {
            Reference::Binding(binding) => self.graph.binding(*binding).facet.clone(),
            Reference::Session(_) => Type::Any,
        }
    }

    fn literal(&self, literal: &Literal) -> Type {
        match literal {
            Literal::Null => Type::Null,
            Literal::Absent => Type::Absent,
            Literal::Bool(_) => Type::Bool,
            Literal::Int(_) => Type::Int,
            Literal::Float(_) => Type::Float,
            Literal::Text(_) => Type::Text,
            Literal::Bytes(_) => Type::Bytes,
            Literal::Function(name) => function_of(
                self.declared
                    .and_then(|declared| declared.get(name))
                    .map_or(&[][..], |function| &function.params),
            ),
        }
    }

    fn atom(&self, atom: &AtomNode) -> Type {
        match atom {
            AtomNode::Variable(reference) => self.reference(reference),
            AtomNode::Literal(literal) => self.literal(literal),
        }
    }

    fn values(&mut self, ids: &[NodeId]) -> Vec<Type> {
        ids.iter().map(|id| self.value(*id)).collect()
    }

    fn expr(&mut self, expr: ExprNode) -> Type {
        match expr {
            ExprNode::Literal(literal) => self.literal(&literal),
            ExprNode::Variable(reference) => self.reference(&reference),
            ExprNode::Tuple(items) => Type::Tuple(self.values(&items)),
            ExprNode::List(items) => Type::List(Box::new(common(self.values(&items)))),
            ExprNode::Set(items) => Type::Set(Box::new(common(self.values(&items)))),
            ExprNode::Map(entries) => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for entry in entries {
                    keys.push(self.value(entry.key));
                    values.push(self.value(entry.value));
                }
                Type::Map(Box::new(MapType {
                    key: common(keys),
                    value: common(values),
                }))
            }
            ExprNode::Record(entries) => Type::Record(RecordType {
                fields: entries
                    .into_iter()
                    .map(|entry| RecordTypeField {
                        ty: self.value(entry.value),
                        name: entry.field,
                        optional: false,
                    })
                    .collect(),
                rest: None,
            }),
            ExprNode::Member(member) => {
                let (target, _) = self.member_parts(&member);
                match &member {
                    MemberNode::Field { field, .. } => field_facet(&target, field),
                    MemberNode::Index { .. } => index_facet(&target),
                }
            }
            ExprNode::Closure { params, body, .. } => {
                self.block(body);
                let names: Vec<Name> = params
                    .iter()
                    .map(|param| self.graph.binding(*param).name.clone())
                    .collect();
                function_of(&names)
            }
            ExprNode::Call { function, args } => {
                let found = self.values(&args);
                let Some(definition) = self.catalog.definition(&function) else {
                    return Type::Any;
                };
                let signature = definition.signature.clone();
                let callee = definition.name.to_string();
                for (index, found) in found.iter().enumerate() {
                    let site = self.graph.node(args[index]).site.clone();
                    self.argument(&callee, &signature, index, found, &site);
                }
                signature.result
            }
            ExprNode::Clock => Type::Timestamp,
            ExprNode::Random => Type::Float,
            ExprNode::Read { handle, request } => {
                self.value(handle);
                self.value(request);
                Type::Any
            }
        }
    }

    /// Refuses an argument that can never be of its parameter's type. An
    /// `absent` given for an optional parameter is the omitted argument
    /// (`K-FN-004`).
    fn argument(
        &mut self,
        callee: &str,
        signature: &Signature,
        index: usize,
        found: &Type,
        site: &lash_kernel_doc::Site,
    ) {
        let Some(param) = signature.params.get(index) else {
            return;
        };
        if param.optional && found == &Type::Absent {
            return;
        }
        if disjoint(found, &param.ty) {
            self.errors.push(refused(
                Some(site),
                RefusalReason::ArgumentType {
                    callee: callee.to_string(),
                    param: param.name.clone(),
                    expected: param.ty.clone(),
                    found: found.clone(),
                },
            ));
        }
    }

    fn arguments(&mut self, id: NodeId, callee: &str, signature: &Signature, args: &[AtomNode]) {
        let site = self.graph.node(id).site.clone();
        for (index, arg) in args.iter().enumerate() {
            let found = self.atom(arg);
            self.argument(callee, signature, index, &found, &site);
        }
    }

    /// What a call of `callee` yields, after checking a library callee's
    /// arguments.
    fn call(&mut self, id: NodeId, callee: &CalleeNode, args: &[AtomNode]) -> Type {
        let CalleeNode::Library(function) = callee else {
            return Type::Any;
        };
        let Some(definition) = self.catalog.definition(function) else {
            return Type::Any;
        };
        let signature = definition.signature.clone();
        let name = definition.name.to_string();
        self.arguments(id, &name, &signature, args);
        signature.result
    }

    fn action(&mut self, id: NodeId, action: &ActionNode) -> Type {
        match action {
            ActionNode::Call { callee, args } => self.call(id, callee, args),
            ActionNode::Spawn { callee, args } => {
                Type::Task(Box::new(stable(&self.call(id, callee, args))))
            }
            ActionNode::Perform {
                effect,
                args,
                result,
            } => {
                if let Some(signature) = self.effects.and_then(|effects| effects.get(effect)) {
                    let signature = signature.clone();
                    self.arguments(id, effect.as_str(), &signature, args);
                    if disjoint(result, &signature.result) {
                        let site = self.graph.node(id).site.clone();
                        self.errors.push(refused(
                            Some(&site),
                            RefusalReason::PerformResult {
                                effect: effect.clone(),
                                declared: signature.result,
                                stated: result.clone(),
                            },
                        ));
                    }
                }
                result.clone()
            }
            ActionNode::Join { task } => match self.atom(task) {
                Type::Task(result) => *result,
                _ => Type::Any,
            },
            ActionNode::JoinMany { mode, .. } => match mode {
                JoinMode::All | JoinMode::AllSettled => Type::List(Box::new(Type::Any)),
                JoinMode::Race | JoinMode::Any => Type::Any,
            },
            ActionNode::Cancel { .. } => Type::Null,
            ActionNode::Sleep { .. } | ActionNode::Yield => Type::Any,
        }
    }
}

/// What reading `field` of a value of type `target` gives (`K-FORM-009`).
fn field_facet(target: &Type, field: &str) -> Type {
    match target {
        Type::Record(record) => match record.fields.iter().find(|other| other.name == field) {
            Some(found) if found.optional => union([found.ty.clone(), Type::Absent]),
            Some(found) => found.ty.clone(),
            None => match &record.rest {
                Some(rest) => union([(**rest).clone(), Type::Absent]),
                None => Type::Absent,
            },
        },
        Type::Error => match field {
            "kind" | "message" => Type::Text,
            "data" => Type::Any,
            _ => Type::Absent,
        },
        _ => Type::Any,
    }
}

/// What reading an index of a value of type `target` gives (`K-FORM-010`).
fn index_facet(target: &Type) -> Type {
    match target {
        Type::List(item) => (**item).clone(),
        Type::Tuple(members) => union(members.iter().cloned()),
        Type::Map(map) => map.value.clone(),
        Type::Set(_) => Type::Bool,
        _ => Type::Any,
    }
}
