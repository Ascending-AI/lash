//! Dialect-owned construction and recognition recipes used by canvas admission.
mod admission;
pub use admission::verify_lowering;
use lash_kernel_doc::{
    self as k, ActExpr, ActKind, Callee, Expr, FunctionCatalog, FunctionTarget, Literal, Protocol,
};
use lash_kernel_value::FunctionId;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug)]
pub struct Iteration {
    pub open: &'static str,
    pub next: Option<&'static str>,
    pub item: Option<&'static str>,
    pub check: Option<&'static str>,
}
impl Iteration {
    pub fn resolve<E>(
        &self,
        mut resolve: impl FnMut(&str) -> Result<FunctionTarget, E>,
    ) -> Result<Protocol, E> {
        Ok(Protocol {
            open: resolve(self.open)?,
            next: self.next.map(&mut resolve).transpose()?,
            item: self.item.map(&mut resolve).transpose()?,
            check: self.check.map(&mut resolve).transpose()?,
        })
    }
    pub fn matches(
        &self,
        protocol: &Protocol,
        mut is: impl FnMut(&FunctionTarget, &str) -> bool,
    ) -> bool {
        let mut hook = |target: Option<&FunctionTarget>, name: Option<&str>| match (target, name) {
            (Some(target), Some(name)) => is(target, name),
            (None, None) => true,
            _ => false,
        };
        hook(Some(&protocol.open), Some(self.open))
            && hook(protocol.next.as_ref(), self.next)
            && hook(protocol.item.as_ref(), self.item)
            && hook(protocol.check.as_ref(), self.check)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Identity,
    /// Call the selected helper on the value, followed by fixed operands.
    Call {
        helper: &'static str,
        role: Option<k::ProjectionRole>,
        tail: &'static [BoundaryOperand],
        literals_passthrough: bool,
    },
}
#[derive(Clone, Copy, Debug)]
pub enum BoundaryOperand {
    Absent,
    /// Optional source-owned factory, whose evaluation must only construct values.
    Forwarder,
}
impl BoundaryOperand {
    fn expression(self) -> Expr {
        match self {
            Self::Absent | Self::Forwarder => Expr::Literal(Literal::Absent),
        }
    }
    fn matches(self, found: &Expr) -> bool {
        *found == self.expression() || matches!((self, found), (Self::Forwarder, Expr::Closure(_)))
    }
}
impl Boundary {
    pub fn wrap(
        &self,
        value: Expr,
        mut resolve: impl FnMut(&str) -> Option<FunctionId>,
    ) -> Option<Expr> {
        match self {
            Self::Identity => Some(value),
            Self::Call {
                literals_passthrough: true,
                ..
            } if matches!(value, Expr::Literal(_)) => Some(value),
            Self::Call { helper, tail, .. } => {
                let mut args = vec![value];
                args.extend(tail.iter().copied().map(BoundaryOperand::expression));
                Some(Expr::Act(Box::new(ActExpr {
                    kind: ActKind::Call(Callee::Local(resolve(helper)?)),
                    args,
                })))
            }
        }
    }
    pub fn unwrap<'a>(
        &self,
        value: &'a Expr,
        mut is: impl FnMut(&FunctionId, &str) -> bool,
    ) -> Option<&'a Expr> {
        match (self, value) {
            (Self::Identity, value) => Some(value),
            (Self::Call { helper, tail, .. }, Expr::Act(act)) => {
                let ActKind::Call(Callee::Local(id)) = &act.kind else {
                    return None;
                };
                (is(id, helper)
                    && act.args.len() == tail.len() + 1
                    && act.args[1..]
                        .iter()
                        .zip(*tail)
                        .all(|(found, expected)| expected.matches(found)))
                .then(|| &act.args[0])
            }
            _ => None,
        }
    }
    pub fn resolve(&self, catalog: &dyn FunctionCatalog, name: &str) -> Option<FunctionId> {
        match self {
            Self::Call {
                role: Some(role), ..
            } => catalog.projection_helper(*role),
            _ => catalog.named_helper(name),
        }
    }
    pub fn helper(&self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Call { helper, .. } => Some(helper),
        }
    }
}

/// An authored action consumes its result and returns a fixed dialect value.
#[derive(Clone, Copy, Debug)]
pub struct DiscardResult {
    pub result: BoundaryOperand,
}
impl DiscardResult {
    pub fn wrap(&self, name: lash_kernel_value::Name, value: Expr) -> Expr {
        Expr::Let {
            name,
            value: Box::new(value),
            body: Box::new(self.result.expression()),
        }
    }

    pub fn unwrap<'a>(&self, expression: &'a Expr) -> Option<&'a Expr> {
        let Expr::Let { value, body, .. } = expression else {
            return None;
        };
        (**body == self.result.expression()).then_some(value)
    }

    pub fn matches_result(&self, body: &Expr) -> bool {
        *body == self.result.expression()
    }
}

/// Where an authored assignment declares a new local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingScope {
    Block,
    Callable,
}
impl BindingScope {
    /// The dialect's sentinel for a local declared before its authored initializer.
    pub fn uninitialized(&self) -> Expr {
        match self {
            Self::Block => Expr::Tuple(Vec::new()),
            Self::Callable => Expr::Literal(Literal::Absent),
        }
    }

    /// Scope predeclarations precede every authored insertion point.
    pub fn prelude_end(&self, block: &[k::Stmt]) -> usize {
        block.iter().take_while(|statement| matches!(statement,
            k::Stmt::Let { value: k::Rhs::Expr(value), .. } if *value == self.uninitialized()
        )).count()
    }

    pub fn predeclaration(&self, name: lash_kernel_value::Name) -> k::Stmt {
        k::Stmt::Let {
            name,
            value: k::Rhs::Expr(self.uninitialized()),
        }
    }

    pub fn statement(
        &self,
        name: lash_kernel_value::Name,
        value: k::Rhs,
        nested: bool,
    ) -> (k::Stmt, Option<k::Stmt>) {
        if *self == Self::Callable && nested {
            let declaration = self.predeclaration(name.clone());
            (
                k::Stmt::Assign {
                    place: k::Place::Variable(name),
                    value,
                    order: k::AssignOrder::ValueFirst,
                },
                Some(declaration),
            )
        } else {
            (k::Stmt::Let { name, value }, None)
        }
    }
}

/// The return statement emitted by lowering and recognized by source recovery.
#[derive(Clone, Copy, Debug)]
pub struct Returns {
    pub implicit: Option<BoundaryOperand>,
    pub asynchronous_policy: Option<&'static str>,
}

/// Why a lowering-owned return cannot be recovered as authored source.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReturnRefusal {
    #[error("implicit return must terminate its function body; canvas work follows it")]
    NotTerminating,
    #[error("implicit return does not match the dialect's return value and policy")]
    WrongForm,
}

impl Returns {
    pub const DIRECT: Self = Self {
        implicit: None,
        asynchronous_policy: None,
    };

    pub fn statement<E>(
        &self,
        value: Expr,
        asynchronous: bool,
        mut resolve: impl FnMut(&str) -> Result<FunctionTarget, E>,
    ) -> Result<k::Stmt, E> {
        Ok(match self.asynchronous_policy.filter(|_| asynchronous) {
            Some(policy) => k::Stmt::ReturnVia {
                value,
                policy: resolve(policy)?,
            },
            None => k::Stmt::Return { value },
        })
    }

    pub fn implicit_value(&self) -> Option<Expr> {
        self.implicit.map(BoundaryOperand::expression)
    }

    pub fn is_statement(
        &self,
        statement: &k::Stmt,
        is: impl Fn(&FunctionTarget, &str) -> bool,
    ) -> bool {
        match statement {
            k::Stmt::Return { .. } => true,
            k::Stmt::ReturnVia { policy, .. } => self
                .asynchronous_policy
                .is_some_and(|name| is(policy, name)),
            _ => false,
        }
    }

    /// The end of authored work, excluding the implicit lowering-owned return.
    pub fn body_end(
        &self,
        function: &k::FunctionDefinition,
        is: impl Fn(&FunctionTarget, &str) -> bool,
    ) -> Result<Option<usize>, ReturnRefusal> {
        if function.projection != k::ProjectionRole::ImplicitReturn {
            return Ok(None);
        }
        let body = function.body().ok_or(ReturnRefusal::WrongForm)?;
        let Some(last) = body.block.last() else {
            return Err(ReturnRefusal::NotTerminating);
        };
        let (value, valid_policy) = match last {
            k::Stmt::Return { value } => (value, true),
            k::Stmt::ReturnVia { value, policy } => (
                value,
                self.asynchronous_policy
                    .is_some_and(|name| is(policy, name)),
            ),
            _ => return Err(ReturnRefusal::NotTerminating),
        };
        if !valid_policy || self.implicit_value().as_ref() != Some(value) {
            return Err(ReturnRefusal::WrongForm);
        }
        Ok(Some(body.block.len() - 1))
    }
}

/// A source dialect supplies its inverse, or explicitly refuses source edits.
#[derive(Clone, Copy, Debug)]
pub enum Inverse {
    Checked(fn(&k::Document, &dyn crate::Library) -> Result<k::Document, crate::PrintError>),
    Unavailable,
}

/// A helper-owned operation has authored syntax, but no raw tool-call form.
#[derive(Clone, Copy, Debug)]
pub struct SourceOnlyOperation {
    pub package: &'static str,
    pub member: &'static str,
    pub construct: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Conventions {
    pub language: Option<k::SourceLanguage>,
    pub matches_start: fn(&k::StartConvention) -> bool,
    pub helper_prefix: &'static str,
    pub iteration: Option<Iteration>,
    pub argument: Boundary,
    pub reply: Boundary,
    pub then_yield: bool,
    pub discard_result: Option<DiscardResult>,
    /// Whether a statement is independent of its enclosing source construct.
    pub separable_step:
        fn(&k::Document, &lash_kernel_value::sites::Site, &dyn FunctionCatalog) -> bool,
    pub returns: Returns,
    pub bindings: BindingScope,
    pub inverse: Inverse,
    pub source_only: &'static [SourceOnlyOperation],
}
impl Conventions {
    pub const KERNEL: Self = Self {
        language: None,
        matches_start: |_| false,
        helper_prefix: "",
        iteration: None,
        argument: Boundary::Identity,
        reply: Boundary::Identity,
        then_yield: false,
        discard_result: None,
        separable_step: |_, _, _| true,
        returns: Returns::DIRECT,
        bindings: BindingScope::Block,
        inverse: Inverse::Unavailable,
        source_only: &[],
    };
    pub fn operation_refusal(
        &self,
        name: &lash_kernel_value::QualifiedName,
    ) -> Option<crate::UnprintableReason> {
        self.source_only
            .iter()
            .find(|entry| {
                entry.package == name.package.to_string() && entry.member == name.member.as_str()
            })
            .map(|entry| {
                crate::UnprintableReason::Operation(Box::new(crate::OperationRefusal {
                    operation: name.clone(),
                    construct: entry.construct.into(),
                    detail: entry.reason.into(),
                }))
            })
    }

    /// Resolve authoring helpers against the exact release this document pins.
    pub fn authoring_catalog<'a>(
        document: &'a k::Document,
        catalog: &'a dyn FunctionCatalog,
    ) -> impl FunctionCatalog + 'a + use<'a> {
        admission::CatalogLibrary::new(catalog, document)
    }

    /// Remove abandoned boundary factories, preserving all authored guest declarations.
    fn argument_factories(
        &self,
        document: &k::Document,
        catalog: &dyn FunctionCatalog,
    ) -> BTreeSet<FunctionId> {
        let Boundary::Call { helper, tail, .. } = self.argument else {
            return BTreeSet::new();
        };
        let mut factories = Vec::new();
        let mut nodes: Vec<_> = document
            .units()
            .map(|(_, block)| k::Node::Block(block))
            .collect();
        while let Some(node) = nodes.pop() {
            if let k::Node::Expr(expression @ Expr::Act(act)) = node
                && self
                    .argument
                    .unwrap(expression, |id, name| {
                        name == helper && self.argument.resolve(catalog, name) == Some(*id)
                    })
                    .is_some()
            {
                factories.extend(tail.iter().zip(&act.args[1..]).filter_map(
                    |(kind, value)| match (kind, value) {
                        (BoundaryOperand::Forwarder, Expr::Closure(closure)) => {
                            Some(closure.function)
                        }
                        _ => None,
                    },
                ));
            }
            nodes.extend(node.children());
        }
        let mut owned = BTreeSet::new();
        while let Some(id) = factories.pop() {
            if document.functions.contains_key(&id)
                && owned.insert(id)
                && let Some(body) = document.functions[&id].body()
            {
                factories.extend(k::Node::Block(&body.block).functions_within());
            }
        }
        owned
    }

    /// Refresh source-owned artifacts after an edit. Unreferenced authored
    /// declarations remain data; only abandoned boundary-owned code is removed.
    pub fn refresh_document(
        &self,
        before: &k::Document,
        document: &mut k::Document,
        catalog: &dyn FunctionCatalog,
    ) {
        if self.language.is_none() {
            return;
        }
        let owned = self.argument_factories(before, catalog);
        let mut pending: Vec<_> = document
            .functions
            .keys()
            .filter(|id| !owned.contains(*id))
            .copied()
            .collect();
        pending.extend(document.library.iter().copied());
        pending.extend(document.requires.natives.iter().copied());
        if let Some(main) = &document.main {
            pending.extend(k::Node::Block(main).functions_within());
        }
        for export in document.exports.values() {
            let functions = match export {
                k::Export::Function(function) => vec![function],
                k::Export::Type(ty) => {
                    pending.push(ty.checker);
                    ty.members
                        .values()
                        .map(|member| member.function())
                        .collect()
                }
                k::Export::Error { .. } => continue,
            };
            for function in functions {
                pending.push(function.function);
                pending.extend(function.convention.functions().into_iter().copied());
            }
        }
        let mut retained = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if retained.insert(id)
                && let Some(body) = document
                    .functions
                    .get(&id)
                    .and_then(|function| function.body())
            {
                pending.extend(k::Node::Block(&body.block).functions_within());
            }
        }
        document
            .functions
            .retain(|id, _| !owned.contains(id) || retained.contains(id));
        let used: BTreeSet<_> = document
            .units()
            .flat_map(|(_, block)| k::Node::Block(block).imports_within())
            .collect();
        document.imports.retain(|slot, import| {
            !matches!(import.kind, lash_kernel_value::ImportKind::Operation { .. })
                || used.contains(slot)
        });
    }

    /// Verify the dialect's inverse before publishing any edited source document.
    ///
    /// # Errors
    /// The same typed refusal ordinary source printing or lowering returns.
    pub fn verify_inverse(
        &self,
        document: &k::Document,
        catalog: &dyn FunctionCatalog,
    ) -> Result<(k::Document, BTreeMap<FunctionId, FunctionId>), crate::PrintError> {
        let library = admission::CatalogLibrary::new(catalog, document);
        let canonical = if let Some(language) = self.language {
            match self.inverse {
                Inverse::Checked(verify) => verify(document, &library)?,
                Inverse::Unavailable => {
                    return Err(crate::PrintError::Unprintable {
                        site: lash_kernel_value::sites::Site::new(
                            lash_kernel_value::sites::Unit::Main,
                            vec![],
                        ),
                        reason: crate::UnprintableReason::InverseUnavailable { language },
                    });
                }
            }
        } else {
            document.clone()
        };
        let identities = if canonical.canonical_bytes() == document.canonical_bytes() {
            document.functions.keys().map(|id| (*id, *id)).collect()
        } else {
            crate::normalization::module(document, &canonical, &library)
                .ok_or_else(|| crate::PrintError::Unprintable {
                    site: lash_kernel_value::sites::Site::new(
                        lash_kernel_value::sites::Unit::Main,
                        vec![],
                    ),
                    reason: crate::UnprintableReason::ReadbackMismatch,
                })?
                .into_iter()
                .map(|(new, old)| (old, new))
                .collect()
        };
        Ok((canonical, identities))
    }

    /// Validate source-owned structure before printing or admitting an edit.
    ///
    /// # Errors
    /// A typed missing inverse at the damaged callable.
    pub fn check_document(
        &self,
        document: &k::Document,
        catalog: &dyn FunctionCatalog,
    ) -> Result<(), crate::PrintError> {
        if self.language.is_none() {
            return Ok(());
        }
        for (id, function) in &document.functions {
            self.returns
                .body_end(function, |target, name| target_is(catalog, target, name))
                .map_err(|reason| crate::PrintError::Unprintable {
                    site: lash_kernel_value::sites::Site::new(
                        lash_kernel_value::sites::Unit::Library(*id),
                        vec![],
                    ),
                    reason: crate::UnprintableReason::ImplicitReturn(reason),
                })?;
        }
        Ok(())
    }

    /// The first authored position follows the dialect's scope predeclarations.
    pub fn canvas_start(
        &self,
        document: &k::Document,
        block: &lash_kernel_value::sites::Site,
    ) -> usize {
        if self.language.is_none() {
            return 0;
        }
        match document.node(block) {
            Some(k::Node::Block(block)) => self.bindings.prelude_end(block),
            _ => 0,
        }
    }

    /// Resolve a canvas end through the same return declaration as lowering.
    ///
    /// # Errors
    /// A malformed implicit return, which an edit must repair before admission.
    pub fn canvas_end(
        &self,
        document: &k::Document,
        block: &lash_kernel_value::sites::Site,
        catalog: &dyn FunctionCatalog,
    ) -> Result<Option<usize>, ReturnRefusal> {
        if self.language.is_none() {
            return Ok(None);
        }
        if block.path.is_empty()
            && let lash_kernel_value::sites::Unit::Library(id) = &block.unit
            && let Some(function) = document.functions.get(id)
        {
            self.returns
                .body_end(function, |target, name| target_is(catalog, target, name))?;
        }
        let Some(k::Node::Block(statements)) = document.node(block) else {
            return Ok(None);
        };
        Ok(statements
            .last()
            .filter(|last| {
                self.returns
                    .is_statement(last, |target, name| target_is(catalog, target, name))
            })
            .map(|_| statements.len() - 1))
    }

    /// Export evidence has priority over helper evidence, in document order.
    pub fn of<'a>(
        document: &k::Document,
        catalog: &dyn FunctionCatalog,
        installed: &'a [Self],
    ) -> &'a Self {
        for export in document.exports.values() {
            if let k::Export::Function(function) = export
                && let Some(conventions) = installed
                    .iter()
                    .find(|entry| (entry.matches_start)(&function.convention))
            {
                return conventions;
            }
        }
        for id in document.library.iter() {
            if let Some(header) = catalog.header(id)
                && let Some(conventions) = installed.iter().find(|entry| {
                    !entry.helper_prefix.is_empty()
                        && header.name.as_str().starts_with(entry.helper_prefix)
                })
            {
                return conventions;
            }
        }
        &Self::KERNEL
    }
}

fn target_is(catalog: &dyn FunctionCatalog, target: &FunctionTarget, name: &str) -> bool {
    matches!(target, FunctionTarget::Local(id) if catalog.named_helper(name) == Some(*id)
        || catalog.header(id).is_some_and(|header| header.name.as_str() == name))
}
