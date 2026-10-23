//! Type facets and the signature checks built on them (`K-ADM-005`,
//! `K-ADM-006`).
//!
//! A facet is what the document alone says of a value: the type of a
//! literal, of a construction, of a library call's or an effect's stated
//! result. A document's variables are untyped, so most facets are `Any`, and
//! a check refuses only what can never be right.

use lash_kernel_doc::FunctionTarget;
use lash_vocabulary::names::{Import, ImportKind, ImportSlot};
use std::collections::BTreeMap;

use lash_kernel_doc::{FunctionCatalog, JoinMode, Literal};

use lash_vocabulary::names::Name;
use lash_vocabulary::types::MapType;
use lash_vocabulary::types::RecordType;
use lash_vocabulary::types::RecordTypeField;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

use crate::graph::{
    ActNode, ActionNode, AtomNode, CalleeNode, ExprNode, Graph, MemberNode, NodeId, NodeKind,
    PlaceNode, Reference, StmtNode,
};
use crate::refusal::{RefusalReason, Refused, refused};
use crate::types::{common, disjoint, function_of, stable, union};

pub(crate) struct Typer<'a> {
    pub(crate) graph: &'a mut Graph,
    pub(crate) presentations: &'a BTreeMap<NodeId, lash_kernel_doc::CallablePresentation>,
    pub(crate) catalog: &'a dyn FunctionCatalog,
    /// The operations the code may perform: the document's manifest, or a
    /// library body's own `effects`.
    pub(crate) imports: &'a BTreeMap<ImportSlot, Import>,
    /// The declared functions a literal may reference.
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
            StmtNode::Assign { place, value, .. } => {
                if let PlaceNode::Member(member) = &place {
                    self.member_parts(member);
                }
                self.value(value);
            }
            StmtNode::Remove { member } => {
                self.member_parts(&member);
            }
            StmtNode::Do { value } => {
                self.value(value);
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
                via,
                body,
                else_block,
            } => {
                let walked = self.value(iterable);
                // A protocol's hooks decide what is bound.
                let element = match (via, walked) {
                    (Some(_), _) => Type::Any,
                    (None, Type::List(item) | Type::Set(item)) => *item,
                    (None, Type::Map(map)) => map.key,
                    (None, Type::Tuple(members)) => union(members),
                    (None, _) => Type::Any,
                };
                self.bind(binding, &element);
                self.block(body);
                if let Some(else_block) = else_block {
                    self.block(else_block);
                }
            }
            StmtNode::While {
                condition,
                body,
                else_block,
            } => {
                self.value(condition);
                self.block(body);
                if let Some(else_block) = else_block {
                    self.block(else_block);
                }
            }
            StmtNode::Break { .. } | StmtNode::Continue { .. } => {}
            StmtNode::ReturnVia { value, policy } => {
                let found = self.value(value);
                self.nested_call(&target_callee(&policy), &[value], &[found]);
            }
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
            NodeKind::Expr(expr) => self.expr(id, expr),
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

    /// Immutable lexical aliases preserve a closure's authored presentation.
    fn presentation(&self, mut id: NodeId) -> Option<&lash_kernel_doc::CallablePresentation> {
        let mut seen = std::collections::BTreeSet::new();
        loop {
            if !seen.insert(id) {
                return None;
            }
            if let Some(presentation) = self.presentations.get(&id) {
                return Some(presentation);
            }
            match &self.graph.node(id).kind {
                NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding))) => {
                    let binding = self.graph.binding(*binding);
                    if binding.assigned {
                        return None;
                    }
                    id = match &self.graph.node(binding.declared_at).kind {
                        NodeKind::Stmt(StmtNode::Let { value, .. })
                        | NodeKind::Expr(ExprNode::Let { value, .. }) => *value,
                        _ => return None,
                    };
                }
                _ => return None,
            }
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
            Literal::Function(id) => self
                .catalog
                .header(id)
                .map_or(Type::Any, |h| Type::Function(Box::new(h.signature.clone()))),
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

    fn expr(&mut self, id: NodeId, expr: ExprNode) -> Type {
        match expr {
            ExprNode::Literal(literal) => self.literal(&literal),
            ExprNode::Variable(reference) => self.reference(&reference),
            ExprNode::Tuple(items) => {
                let values = self.values(&items);
                let token = items.iter().enumerate().find_map(|(index, item)| {
                    let presentation = self.presentation(*item)?;
                    let lash_kernel_doc::FunctionRepresentation::Token { tag, callable } = &presentation.representation else { return None; };
                    let first = self.graph.node(*items.first()?).kind.clone();
                    (*callable as usize == index && matches!(first, NodeKind::Expr(ExprNode::Literal(Literal::Text(found))) if found == *tag))
                        .then(|| Type::Function(Box::new(presentation.signature.clone())))
                });
                token.unwrap_or(Type::Tuple(values))
            }
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
                rest: lash_vocabulary::RecordRest::Closed,
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
            ExprNode::If { cond, then, r#else } => {
                self.value(cond);
                let then = self.value(then);
                let otherwise = self.value(r#else);
                union(vec![then, otherwise])
            }
            ExprNode::Let {
                binding,
                value,
                body,
            } => {
                let facet = self.value(value);
                self.bind(binding, &facet);
                self.value(body)
            }
            ExprNode::Act { kind, args } => {
                let found = self.values(&args);
                match kind {
                    ActNode::Call(callee) => self.nested_call(&callee, &args, &found),
                    ActNode::Spawn(callee) => {
                        Type::Task(Box::new(stable(&self.nested_call(&callee, &args, &found))))
                    }
                    ActNode::Perform { slot, result } => {
                        if let Some(Import {
                            kind: ImportKind::Operation { signature },
                            ..
                        }) = self.imports.get(&slot)
                        {
                            let signature = signature.clone();
                            for (index, found) in found.iter().enumerate() {
                                let site = self.graph.node(args[index]).site.clone();
                                self.argument("Import", &signature, index, found, &site);
                            }
                            if disjoint(&result, &signature.result) {
                                let site = self.graph.node(id).site.clone();
                                self.errors.push(refused(
                                    Some(&site),
                                    RefusalReason::PerformResult {
                                        slot,
                                        declared: signature.result,
                                        stated: result.clone(),
                                    },
                                ));
                            }
                        }
                        result
                    }
                    ActNode::Join => match found.first() {
                        Some(Type::Task(result)) => (**result).clone(),
                        _ => Type::Any,
                    },
                    ActNode::JoinMany(mode) => join_many(mode),
                    ActNode::Await(policy) => {
                        self.nested_call(&target_callee(&policy), &args, &found)
                    }
                    ActNode::Yield | ActNode::Cancel => Type::Null,
                }
            }
        }
    }

    fn call_signature(&self, callee: &CalleeNode) -> Option<(String, Signature)> {
        match callee {
            CalleeNode::Local(id) => self
                .catalog
                .header(id)
                .map(|h| (h.name.to_string(), h.signature.clone())),
            CalleeNode::Import(slot) => match self.imports.get(slot) {
                Some(Import {
                    name,
                    kind: ImportKind::Code { signature, .. },
                }) => Some((
                    format!("{}.{}", name.package, name.member),
                    signature.clone(),
                )),
                _ => None,
            },
            CalleeNode::Value(_) => None,
        }
    }
    /// What a nested call of `callee` yields, after checking a library
    /// callee's arguments, of types `found`.
    fn nested_call(&mut self, callee: &CalleeNode, args: &[NodeId], found: &[Type]) -> Type {
        let Some((name, signature)) = self.call_signature(callee) else {
            return Type::Any;
        };
        for (index, found) in found.iter().enumerate() {
            let site = self.graph.node(args[index]).site.clone();
            self.argument(&name, &signature, index, found, &site);
        }
        signature.result
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
        site: &lash_vocabulary::sites::Site,
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
        let Some((name, signature)) = self.call_signature(callee) else {
            return Type::Any;
        };
        self.arguments(id, &name, &signature, args);
        signature.result
    }

    fn action(&mut self, id: NodeId, action: &ActionNode) -> Type {
        match action {
            ActionNode::Call { callee, args } => self.call(id, callee, args),
            ActionNode::Spawn { callee, args } => {
                Type::Task(Box::new(stable(&self.call(id, callee, args))))
            }
            ActionNode::Perform { slot, args, result } => {
                if let Some(Import {
                    kind: ImportKind::Operation { signature },
                    ..
                }) = self.imports.get(slot)
                {
                    let signature = signature.clone();
                    self.arguments(id, "Import", &signature, args);
                    if disjoint(result, &signature.result) {
                        let site = self.graph.node(id).site.clone();
                        self.errors.push(refused(
                            Some(&site),
                            RefusalReason::PerformResult {
                                slot: *slot,
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
            ActionNode::JoinMany { mode, .. } => join_many(*mode),
            ActionNode::Await { policy, value } => {
                self.call(id, &target_callee(policy), std::slice::from_ref(value))
            }
            ActionNode::Cancel { .. } => Type::Null,
            ActionNode::Yield => Type::Any,
        }
    }
}

/// What a list `join` in `mode` gives.
fn join_many(mode: JoinMode) -> Type {
    match mode {
        JoinMode::All | JoinMode::AllSettled => Type::List(Box::new(Type::Any)),
        JoinMode::Race | JoinMode::Any => Type::Any,
    }
}

/// What reading `field` of a value of type `target` gives (`K-FORM-009`).
fn field_facet(target: &Type, field: &str) -> Type {
    match target {
        Type::Record(record) => match record.fields.iter().find(|other| other.name == field) {
            Some(found) if found.optional => union([found.ty.clone(), Type::Absent]),
            Some(found) => found.ty.clone(),
            None => match &record.rest {
                lash_vocabulary::RecordRest::Keep(rest) => union([(**rest).clone(), Type::Absent]),
                lash_vocabulary::RecordRest::Closed | lash_vocabulary::RecordRest::Drop => {
                    Type::Absent
                }
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

fn target_callee(target: &FunctionTarget) -> CalleeNode {
    match target {
        FunctionTarget::Local(id) => CalleeNode::Local(*id),
        FunctionTarget::Import(slot) => CalleeNode::Import(*slot),
    }
}
