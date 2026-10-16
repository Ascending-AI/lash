//! Structural validation (`K-DOC-005`): what must hold of a document or a
//! definition before anything derives from it.
//!
//! Validation is local. It reads the tree, the manifest and the definitions
//! of the functions a statement names; it resolves no variable and infers no
//! type. Scopes, linking against an environment and derived facts belong to
//! `lash-kernel-check`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;

use crate::ast::{
    ActExpr, ActKind, Action, Atom, Block, Callee, Closure, Expr, Literal, Member, Node, Place,
    Protocol, Rhs, Site, Stmt, Unit,
};
use crate::document::Document;
use crate::function::{Formula, FunctionDefinition, Implementation, Operand};
use crate::name::{EffectName, FunctionId, FunctionName, Name};
use crate::native::FunctionCatalog;
use crate::types::{Signature, Type};
use crate::version::KernelVersion;

/// Why a document or a definition is refused, and the node at fault.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}{}", at(.site))]
pub struct Invalid {
    /// The node at fault. `None` when the fault is in no node: the
    /// manifest, an entry, a signature.
    pub site: Option<Site>,
    pub reason: Box<InvalidReason>,
}

fn at(site: &Option<Site>) -> String {
    site.as_ref()
        .map(|site| format!(" (at {site})"))
        .unwrap_or_default()
}

/// An action form, as a refusal names it: one with no expression spelling
/// (`join`, `yield`, `cancel`) nested in an expression, or one the body of
/// a function with a native implementation may not run (`K-LIB-004`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StatementForm {
    Call,
    Perform,
    Sleep,
    Join,
    Yield,
    Spawn,
    Cancel,
    Await,
}

impl StatementForm {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Perform => "perform",
            Self::Sleep => "sleep",
            Self::Join => "join",
            Self::Yield => "yield",
            Self::Spawn => "spawn",
            Self::Cancel => "cancel",
            Self::Await => "await",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum InvalidReason {
    #[error("written for kernel version {found}; this reader supports version {supported}")]
    KernelVersion { found: u32, supported: u32 },
    #[error("nests deeper than {limit} levels; flatten the program")]
    TooDeep { limit: usize },
    #[error("a name is empty")]
    EmptyName,
    #[error("parameter `{name}` is declared twice")]
    DuplicateParam { name: Name },
    #[error("field `{field}` is written twice in one record")]
    DuplicateField { field: String },
    #[error("`{keyword}` is used outside a loop")]
    LoopControlOutsideLoop { keyword: Cow<'static, str> },
    /// A library call written as an expression's own call names a
    /// function with no native implementation: a call of it is an
    /// action, `invoke`.
    #[error("`{name}` has no native implementation, so a call to it is an `invoke` action")]
    CallNotNative {
        function: FunctionId,
        name: FunctionName,
    },
    #[error(
        "library function `{name}` ({function}) is written for kernel version {written_for}; \
         what lists it runs under version {required}"
    )]
    FunctionOfAnotherVersion {
        function: FunctionId,
        name: FunctionName,
        written_for: u32,
        required: u32,
    },
    #[error("library function {function} is called but not listed in the manifest")]
    FunctionNotListed { function: FunctionId },
    #[error(
        "library function {function} has no definition here, so a call of it cannot be checked"
    )]
    FunctionNotResolved { function: FunctionId },
    #[error("`{name}` takes at most {accepted} argument(s) and at least {required}; {given} given")]
    CallArity {
        name: String,
        given: usize,
        required: usize,
        accepted: usize,
    },
    #[error("effect `{effect}` is performed but not listed in the manifest")]
    EffectNotListed { effect: EffectName },
    #[error("function `{name}` is not declared")]
    FunctionNotDeclared { name: Name },
    #[error("a library function's body cannot name a declared function (`{name}`)")]
    DeclaredFunctionInLibrary { name: Name },
    #[error(
        "a library function's body cannot perform an effect (`{effect}`); take a function argument that performs it"
    )]
    PerformInLibrary { effect: EffectName },
    #[error("entry `{entry}` names no declared function")]
    EntryNotDeclared { entry: Name },
    #[error(
        "entry `{entry}` has {signature} parameter(s) in its signature and {declared} in its function"
    )]
    EntryArity {
        entry: Name,
        signature: usize,
        declared: usize,
    },
    #[error("invalid type: {problem}")]
    InvalidType { problem: Cow<'static, str> },
    #[error("a required parameter (`{name}`) follows an optional one")]
    RequiredAfterOptional { name: Name },
    #[error("a formula names `{name}`, which is not a parameter")]
    FormulaUnknownParam { name: Name },
    #[error("a guard's limit is known before the call runs, so it cannot measure the result")]
    LimitMeasuresResult,
    #[error("`max` and `min` need at least one amount")]
    EmptyFormula,
    #[error("a guard bounds a native implementation; this function states none")]
    GuardWithoutNative,
    /// `K-LIB-011`: a native version versions a native implementation.
    #[error("native version {version} is stated by a function with no native implementation")]
    NativeVersionWithoutNative { version: u32 },
    /// `K-LIB-011`: native versions count from 1.
    #[error("native version 0 is not a version; the first is 1")]
    NativeVersionZero,
    #[error("a guard's unit is empty")]
    EmptyGuardUnit,
    /// `K-LIB-003`: a function with a native implementation takes no
    /// function.
    #[error(
        "parameter `{name}` takes a function, so the function cannot have a native implementation"
    )]
    NativeTakesFunction { name: Name },
    /// `K-LIB-004`: a body that stands beside a native implementation
    /// cannot wait.
    #[error("the body stands beside a native implementation, so it cannot `{}`", .form.keyword())]
    NativeBodyMayWait { form: StatementForm },
    /// An action nested where no frame can stand at it: a `while`
    /// condition, or an assignment's place.
    #[error("an action cannot be nested in {position}")]
    NestedActionPosition { position: Cow<'static, str> },
    /// A right-hand side that is one action with variable and literal
    /// arguments is the statement's action, not an expression: the one
    /// spelling kernel text reads back.
    #[error(
        "an action that is the whole right-hand side with atom arguments is the statement's action"
    )]
    ActionAsExpression,
    /// `K-CLO-004`: a direct start passes the signature's arguments as the
    /// function's parameters, one for one.
    #[error("a start adapter's signature has {signature} parameter(s) and its function {declared}")]
    StartArity { signature: usize, declared: usize },
    /// `K-CLO-004`: a token holds its tag first and its callable after it.
    #[error("a start adapter's token has an empty tag or holds its callable in slot 0")]
    StartToken,
}

fn invalid(site: Option<&Site>, reason: InvalidReason) -> Invalid {
    Invalid {
        site: site.cloned(),
        reason: Box::new(reason),
    }
}

/// A document or a body runs under one kernel version, so every library
/// function it lists is written for that version (`K-VER-003`). A function
/// the catalog does not hold is refused where it is called.
fn check_written_for(
    catalog: &dyn FunctionCatalog,
    function: &FunctionId,
    version: KernelVersion,
) -> Result<(), Invalid> {
    match catalog.definition(function) {
        Some(definition) if definition.kernel != version.number() => Err(invalid(
            None,
            InvalidReason::FunctionOfAnotherVersion {
                function: *function,
                name: definition.name.clone(),
                written_for: definition.kernel,
                required: version.number(),
            },
        )),
        _ => Ok(()),
    }
}

/// Validates a document's structure, checking its calls by name and its
/// nested actions against the definitions in `catalog`.
pub fn validate_document(
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Result<(), Invalid> {
    validate_document_with_limits(document, catalog, crate::KernelDataLimits::standard())
}
/// Validate under a deployment's document recursion safety policy.
pub fn validate_document_with_limits(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    limits: crate::KernelDataLimits,
) -> Result<(), Invalid> {
    let Some(version) = KernelVersion::of(document.manifest.kernel) else {
        return Err(invalid(
            None,
            InvalidReason::KernelVersion {
                found: document.manifest.kernel,
                supported: KernelVersion::NEWEST.number(),
            },
        ));
    };
    for function in document.manifest.functions.keys() {
        check_written_for(catalog, function, version)?;
    }
    for signature in document.manifest.effects.values() {
        check_signature(signature)?;
    }
    for name in &document.private_bindings {
        check_name(name, None)?;
    }
    for (entry, signature) in &document.entries {
        check_signature(signature)?;
        let Some(function) = document.functions.get(entry) else {
            return Err(invalid(
                None,
                InvalidReason::EntryNotDeclared {
                    entry: entry.clone(),
                },
            ));
        };
        if function.params.len() != signature.params.len() {
            return Err(invalid(
                None,
                InvalidReason::EntryArity {
                    entry: entry.clone(),
                    signature: signature.params.len(),
                    declared: function.params.len(),
                },
            ));
        }
    }
    let scope = Scope {
        limits,
        document: Some(document),
        functions: &document.manifest.functions,
        catalog,
        native_body: false,
    };
    for (name, function) in &document.functions {
        let site = Site::new(Unit::Function(name.clone()), []);
        check_name(name, Some(&site))?;
        check_params(&function.params, &site)?;
        if let Some(start) = &function.start {
            scope.check_start(start, function.params.len(), &site)?;
        }
        scope.check_unit(&function.body, site)?;
    }
    scope.check_unit(&document.main, Site::new(Unit::Main, []))
}

/// Validates a library-function definition's structure, checking its body's
/// calls by name and nested actions against the definitions in `catalog`.
pub fn validate_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
) -> Result<(), Invalid> {
    validate_definition_with_limits(definition, catalog, crate::KernelDataLimits::standard())
}
/// Validate under a deployment's document recursion safety policy.
pub fn validate_definition_with_limits(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    limits: crate::KernelDataLimits,
) -> Result<(), Invalid> {
    validate_identified_definition(definition, catalog, limits).map(|_| ())
}

/// Registration keeps the identity validation used to name a body's sites.
pub(crate) fn validate_identified_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    limits: crate::KernelDataLimits,
) -> Result<Option<crate::FunctionId>, Invalid> {
    let Some(version) = KernelVersion::of(definition.kernel) else {
        return Err(invalid(
            None,
            InvalidReason::KernelVersion {
                found: definition.kernel,
                supported: KernelVersion::NEWEST.number(),
            },
        ));
    };
    if let Some(body) = definition.body() {
        for function in body.functions.keys() {
            check_written_for(catalog, function, version)?;
        }
    }
    check_signature(&definition.signature)?;
    check_formula(&definition.charge, &definition.signature, true)?;
    if definition.native_version == 0 {
        return Err(invalid(None, InvalidReason::NativeVersionZero));
    }
    if definition.native_version != crate::FIRST_NATIVE_VERSION && !definition.has_native() {
        return Err(invalid(
            None,
            InvalidReason::NativeVersionWithoutNative {
                version: definition.native_version,
            },
        ));
    }
    if let Some(guard) = &definition.guard {
        if !definition.has_native() {
            return Err(invalid(None, InvalidReason::GuardWithoutNative));
        }
        if guard.unit.is_empty() {
            return Err(invalid(None, InvalidReason::EmptyGuardUnit));
        }
        check_formula(&guard.limit, &definition.signature, false)?;
    }
    if definition.has_native() {
        for param in &definition.signature.params {
            if mentions_function(&param.ty) {
                return Err(invalid(
                    None,
                    InvalidReason::NativeTakesFunction {
                        name: param.name.clone(),
                    },
                ));
            }
        }
    }
    let Some(body) = definition.body() else {
        return Ok(None);
    };
    // The site names the body by the definition's own identity.
    let function = definition.identity();
    let scope = Scope {
        limits,
        document: None,
        functions: &body.functions,
        catalog,
        native_body: matches!(definition.implementation, Implementation::Both(_)),
    };
    scope.check_unit(&body.block, Site::new(Unit::Library(function), []))?;
    Ok(Some(function))
}

/// A call by name inside an expression that `K-LIB-002` refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason} (at child path {path:?} of the statement)")]
pub struct StatementRuleViolation {
    /// The offending call, as [`Node::children`] indexes from the
    /// statement.
    pub path: Vec<u32>,
    pub reason: Box<InvalidReason>,
}

/// Checks one statement, and it alone, for `K-LIB-002`: every call by name
/// inside one of its expressions names a library function that `catalog`
/// defines with a native implementation.
///
/// The check stops at a nested block. The statements of an `if` arm, a loop
/// body or a closure body are statements of their own.
pub fn check_statement(
    stmt: &Stmt,
    catalog: &dyn FunctionCatalog,
) -> Result<(), StatementRuleViolation> {
    let mut pending: Vec<(Node<'_>, Vec<u32>)> = vec![(Node::Stmt(stmt), Vec::new())];
    while let Some((node, path)) = pending.pop() {
        if let Node::Expr(Expr::Call { function, .. }) = node {
            native_call(function, catalog).map_err(|reason| StatementRuleViolation {
                path: path.clone(),
                reason: Box::new(reason),
            })?;
        }
        for (index, child) in (0u32..).zip(node.children()) {
            if matches!(child, Node::Block(_)) {
                continue;
            }
            let mut child_path = path.clone();
            child_path.push(index);
            pending.push((child, child_path));
        }
    }
    Ok(())
}

fn native_call<'c>(
    function: &FunctionId,
    catalog: &'c dyn FunctionCatalog,
) -> Result<&'c FunctionDefinition, InvalidReason> {
    let Some(definition) = catalog.definition(function) else {
        return Err(InvalidReason::FunctionNotResolved {
            function: *function,
        });
    };
    if !definition.has_native() {
        return Err(InvalidReason::CallNotNative {
            function: *function,
            name: definition.name.clone(),
        });
    }
    Ok(definition)
}

struct Scope<'a> {
    limits: crate::KernelDataLimits,
    /// The document being validated, or `None` inside a library body.
    document: Option<&'a Document>,
    /// The library functions this code may name.
    functions: &'a std::collections::BTreeMap<FunctionId, FunctionName>,
    catalog: &'a dyn FunctionCatalog,
    /// The code is a body that stands beside a native implementation.
    native_body: bool,
}

impl Scope<'_> {
    fn check_unit(&self, body: &Block, site: Site) -> Result<(), Invalid> {
        check_depth(body, &site, self.limits.document_depth())?;
        self.check_block(body, &site, false)
    }

    fn check_block(&self, block: &Block, site: &Site, in_loop: bool) -> Result<(), Invalid> {
        for (index, stmt) in (0u32..).zip(block) {
            self.check_stmt(stmt, &site.child(index), in_loop)?;
        }
        Ok(())
    }

    fn check_stmt(&self, stmt: &Stmt, site: &Site, in_loop: bool) -> Result<(), Invalid> {
        let mut next = 0u32;
        let mut child = || {
            let index = next;
            next += 1;
            site.child(index)
        };
        match stmt {
            Stmt::Let { name, value } => {
                check_name(name, Some(site))?;
                self.check_rhs(value, &child())
            }
            Stmt::Assign { place, value } => {
                match place {
                    Place::Variable(name) => check_name(name, Some(site))?,
                    Place::Member(member) => {
                        let mut first = 0;
                        let mut place = |expr: &Expr| {
                            let at = site.child(first);
                            first += 1;
                            nested_action(expr, &at, "an assignment's place")
                        };
                        match member {
                            Member::Field { target, .. } => place(target)?,
                            Member::Index { target, index } => {
                                place(target)?;
                                place(index)?;
                            }
                        }
                        self.check_member(member, &mut child)?
                    }
                }
                self.check_rhs(value, &child())
            }
            Stmt::Remove { member } => self.check_member(member, &mut child),
            Stmt::Do { action } => self.check_action(action, &child()),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.check_expr(condition, &child())?;
                self.check_block(then_block, &child(), in_loop)?;
                self.check_block(else_block, &child(), in_loop)
            }
            Stmt::For {
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                check_name(binding, Some(site))?;
                if let Some(via) = via {
                    self.check_protocol(via, site)?;
                }
                self.check_expr(iterable, &child())?;
                self.check_block(body, &child(), true)?;
                self.check_block(else_block, &child(), in_loop)
            }
            Stmt::While {
                condition,
                body,
                else_block,
            } => {
                let at = child();
                nested_action(condition, &at, "a `while` condition")?;
                self.check_expr(condition, &at)?;
                self.check_block(body, &child(), true)?;
                self.check_block(else_block, &child(), in_loop)
            }
            Stmt::Break => loop_control(in_loop, "break", site),
            Stmt::Continue => loop_control(in_loop, "continue", site),
            Stmt::Return { value }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.check_expr(value, &child()),
            Stmt::Try(scope) => {
                self.check_block(&scope.body, &child(), in_loop)?;
                if let Some(catch) = &scope.catch {
                    check_name(&catch.binding, Some(site))?;
                    self.check_block(&catch.body, &child(), in_loop)?;
                }
                if let Some(finally) = &scope.finally {
                    self.check_block(finally, &child(), in_loop)?;
                }
                Ok(())
            }
        }
    }

    fn check_rhs(&self, rhs: &Rhs, site: &Site) -> Result<(), Invalid> {
        match rhs {
            Rhs::Expr(Expr::Act(act)) if act.as_action().is_some() => {
                Err(invalid(Some(site), InvalidReason::ActionAsExpression))
            }
            Rhs::Expr(expr) => self.check_expr(expr, site),
            Rhs::Action(action) => self.check_action(action, site),
        }
    }

    fn check_member(
        &self,
        member: &Member,
        child: &mut dyn FnMut() -> Site,
    ) -> Result<(), Invalid> {
        match member {
            Member::Field { target, .. } => self.check_expr(target, &child()),
            Member::Index { target, index } => {
                self.check_expr(target, &child())?;
                self.check_expr(index, &child())
            }
        }
    }

    /// An action nested in an expression, checked as its statement form
    /// is (`K-EVAL-008`), its arguments as expressions.
    fn check_act(&self, act: &ActExpr, site: &Site) -> Result<(), Invalid> {
        let form = match &act.kind {
            ActKind::Call(_) => StatementForm::Call,
            ActKind::Perform { .. } => StatementForm::Perform,
            ActKind::Sleep { .. } => StatementForm::Sleep,
            ActKind::Spawn(_) => StatementForm::Spawn,
            ActKind::Join | ActKind::JoinMany(_) => StatementForm::Join,
            ActKind::Await(_) => StatementForm::Await,
        };
        // As for a statement action (`K-LIB-004`): a body beside a native
        // may call functions that state one and apply a closure it holds.
        let callable = match &act.kind {
            ActKind::Call(Callee::Library(function)) => self
                .catalog
                .definition(function)
                .is_some_and(FunctionDefinition::has_native),
            ActKind::Call(Callee::Value(_)) => true,
            _ => false,
        };
        if self.native_body && !callable {
            return Err(invalid(
                Some(site),
                InvalidReason::NativeBodyMayWait { form },
            ));
        }
        match &act.kind {
            ActKind::Call(callee) | ActKind::Spawn(callee) => {
                self.check_callee(callee, act.args.len(), site)?
            }
            ActKind::Perform { effect, result, .. } => {
                let Some(document) = self.document else {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::PerformInLibrary {
                            effect: effect.clone(),
                        },
                    ));
                };
                let Some(signature) = document.manifest.effects.get(effect) else {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::EffectNotListed {
                            effect: effect.clone(),
                        },
                    ));
                };
                check_arity(effect.as_str(), act.args.len(), signature, site)?;
                check_type(result).map_err(|reason| invalid(Some(site), reason))?;
            }
            ActKind::Sleep { .. } | ActKind::Join | ActKind::JoinMany(_) => {
                if act.args.len() != 1 {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::CallArity {
                            name: form.keyword().to_owned(),
                            given: act.args.len(),
                            required: 1,
                            accepted: 1,
                        },
                    ));
                }
            }
            ActKind::Await(policy) => {
                if act.args.len() != 1 {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::CallArity {
                            name: form.keyword().to_owned(),
                            given: act.args.len(),
                            required: 1,
                            accepted: 1,
                        },
                    ));
                }
                self.check_callee(&Callee::Library(*policy), 1, site)?
            }
        }
        self.check_exprs(act.args.iter(), site)
    }

    fn check_action(&self, action: &Action, site: &Site) -> Result<(), Invalid> {
        let form = match action {
            Action::Call { .. } => StatementForm::Call,
            Action::Perform { .. } => StatementForm::Perform,
            Action::Sleep { .. } => StatementForm::Sleep,
            Action::Join { .. } | Action::JoinMany { .. } => StatementForm::Join,
            Action::Yield => StatementForm::Yield,
            Action::Spawn { .. } => StatementForm::Spawn,
            Action::Cancel { .. } => StatementForm::Cancel,
            Action::Await { .. } => StatementForm::Await,
        };
        // A body beside a native implementation may call functions that
        // state one, and apply a closure it holds: one it reaches through
        // its arguments or one it made. It cannot wait itself (`K-LIB-004`).
        let callable = match action {
            Action::Call {
                callee: Callee::Library(function),
                ..
            } => self
                .catalog
                .definition(function)
                .is_some_and(FunctionDefinition::has_native),
            Action::Call {
                callee: Callee::Value(_),
                ..
            } => true,
            _ => false,
        };
        if self.native_body && !callable {
            return Err(invalid(
                Some(site),
                InvalidReason::NativeBodyMayWait { form },
            ));
        }
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                self.check_callee(callee, args.len(), site)?;
                self.check_atoms(args, site)
            }
            Action::Perform {
                effect,
                args,
                result,
                ..
            } => {
                let Some(document) = self.document else {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::PerformInLibrary {
                            effect: effect.clone(),
                        },
                    ));
                };
                let Some(signature) = document.manifest.effects.get(effect) else {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::EffectNotListed {
                            effect: effect.clone(),
                        },
                    ));
                };
                check_arity(effect.as_str(), args.len(), signature, site)?;
                check_type(result).map_err(|reason| invalid(Some(site), reason))?;
                self.check_atoms(args, site)
            }
            Action::Await { policy, value } => {
                self.check_callee(&Callee::Library(*policy), 1, site)?;
                self.check_atoms(std::slice::from_ref(value), site)
            }
            Action::Sleep { duration: atom, .. }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.check_atoms(std::slice::from_ref(atom), site),
            Action::Yield => Ok(()),
        }
    }

    fn check_callee(&self, callee: &Callee, given: usize, site: &Site) -> Result<(), Invalid> {
        match callee {
            Callee::Declared(name) => {
                let function = self.declared(name, site)?;
                if given > function {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::CallArity {
                            name: name.to_string(),
                            given,
                            required: 0,
                            accepted: function,
                        },
                    ));
                }
                Ok(())
            }
            Callee::Value(name) => check_name(name, Some(site)),
            Callee::Library(function) => {
                let definition = self.listed(function, site)?;
                check_arity(definition.name.as_str(), given, &definition.signature, site)
            }
        }
    }

    /// A loop protocol's functions: each listed and taking its hook's
    /// arguments, and, in a body beside a native implementation, native
    /// (`K-ITER-005`, `K-LIB-004`).
    fn check_protocol(&self, via: &Protocol, site: &Site) -> Result<(), Invalid> {
        let hooks = [
            (Some(&via.open), 1),
            (via.next.as_ref(), 1),
            (via.item.as_ref(), 2),
            (via.check.as_ref(), 4),
        ];
        for (function, given) in hooks {
            let Some(function) = function else { continue };
            let definition = self.listed(function, site)?;
            if self.native_body && !definition.has_native() {
                return Err(invalid(
                    Some(site),
                    InvalidReason::NativeBodyMayWait {
                        form: StatementForm::Call,
                    },
                ));
            }
            check_arity(definition.name.as_str(), given, &definition.signature, site)?;
        }
        Ok(())
    }

    /// The parameter count of the declared function `name`.
    fn declared(&self, name: &Name, site: &Site) -> Result<usize, Invalid> {
        let Some(document) = self.document else {
            return Err(invalid(
                Some(site),
                InvalidReason::DeclaredFunctionInLibrary { name: name.clone() },
            ));
        };
        document
            .functions
            .get(name)
            .map(|function| function.params.len())
            .ok_or_else(|| {
                invalid(
                    Some(site),
                    InvalidReason::FunctionNotDeclared { name: name.clone() },
                )
            })
    }

    /// The definition of a library function this code lists.
    fn listed(&self, function: &FunctionId, site: &Site) -> Result<&FunctionDefinition, Invalid> {
        if !self.functions.contains_key(function) {
            return Err(invalid(
                Some(site),
                InvalidReason::FunctionNotListed {
                    function: *function,
                },
            ));
        }
        self.catalog.definition(function).ok_or_else(|| {
            invalid(
                Some(site),
                InvalidReason::FunctionNotResolved {
                    function: *function,
                },
            )
        })
    }

    fn check_atoms(&self, atoms: &[Atom], site: &Site) -> Result<(), Invalid> {
        for atom in atoms {
            match atom {
                Atom::Variable(name) => check_name(name, Some(site))?,
                Atom::Literal(literal) => self.check_literal(literal, site)?,
            }
        }
        Ok(())
    }

    fn check_literal(&self, literal: &Literal, site: &Site) -> Result<(), Invalid> {
        match literal {
            Literal::Function(name) => self.declared(name, site).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn check_expr(&self, expr: &Expr, site: &Site) -> Result<(), Invalid> {
        match expr {
            Expr::Literal(literal) => self.check_literal(literal, site),
            Expr::Variable(name) => check_name(name, Some(site)),
            Expr::Clock | Expr::Random => Ok(()),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                self.check_exprs(items.iter(), site)
            }
            Expr::Map(entries) => self.check_exprs(
                entries.iter().flat_map(|entry| [&entry.key, &entry.value]),
                site,
            ),
            Expr::Record(entries) => {
                let mut seen = BTreeSet::new();
                for entry in entries {
                    if !seen.insert(entry.field.as_str()) {
                        return Err(invalid(
                            Some(site),
                            InvalidReason::DuplicateField {
                                field: entry.field.clone(),
                            },
                        ));
                    }
                }
                self.check_exprs(entries.iter().map(|entry| &entry.value), site)
            }
            Expr::Member(member) => match member.as_ref() {
                Member::Field { target, .. } => self.check_expr(target, &site.child(0)),
                Member::Index { target, index } => {
                    self.check_expr(target, &site.child(0))?;
                    self.check_expr(index, &site.child(1))
                }
            },
            Expr::Closure(closure) => self.check_closure(closure, site),
            Expr::Call { function, args } => {
                let definition = self.listed(function, site)?;
                if !definition.has_native() {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::CallNotNative {
                            function: *function,
                            name: definition.name.clone(),
                        },
                    ));
                }
                check_arity(
                    definition.name.as_str(),
                    args.len(),
                    &definition.signature,
                    site,
                )?;
                self.check_exprs(args.iter(), site)
            }
            Expr::If { cond, then, r#else } => self.check_exprs(
                [cond.as_ref(), then.as_ref(), r#else.as_ref()].into_iter(),
                site,
            ),
            Expr::Let { name, value, body } => {
                check_name(name, Some(site))?;
                self.check_exprs([value.as_ref(), body.as_ref()].into_iter(), site)
            }
            Expr::Read(read) => {
                self.check_expr(&read.handle, &site.child(0))?;
                self.check_expr(&read.request, &site.child(1))
            }
            Expr::Act(act) => self.check_act(act, site),
        }
    }

    fn check_exprs<'e>(
        &self,
        exprs: impl Iterator<Item = &'e Expr>,
        site: &Site,
    ) -> Result<(), Invalid> {
        for (index, expr) in (0u32..).zip(exprs) {
            self.check_expr(expr, &site.child(index))?;
        }
        Ok(())
    }

    fn check_closure(&self, closure: &Closure, site: &Site) -> Result<(), Invalid> {
        check_params(&closure.params, site)?;
        if let Some(start) = &closure.start {
            self.check_start(start, closure.params.len(), site)?;
        }
        // A closure body is a function body: an enclosing loop does not
        // reach into it.
        self.check_block(&closure.body, &site.child(0), false)
    }
}

/// Refuses an action nested in `expr`, at `site`, which stands in
/// `position`: no frame can stand at one there. A closure's body is code of
/// its own.
fn nested_action(expr: &Expr, site: &Site, position: &'static str) -> Result<(), Invalid> {
    let mut pending = vec![(Node::Expr(expr), site.clone())];
    while let Some((node, at)) = pending.pop() {
        match node {
            Node::Expr(Expr::Act(_)) => {
                return Err(invalid(
                    Some(&at),
                    InvalidReason::NestedActionPosition {
                        position: position.into(),
                    },
                ));
            }
            Node::Expr(Expr::Closure(_)) => {}
            node => pending.extend(
                (0u32..)
                    .zip(node.children())
                    .map(|(index, child)| (child, at.child(index))),
            ),
        }
    }
    Ok(())
}

impl Scope<'_> {
    /// A start adapter (`K-CLO-004`): a valid signature, a direct start's
    /// arity, a token's tag and slot, and the functions a TypeScript start
    /// calls listed in the manifest.
    fn check_start(
        &self,
        start: &crate::StartAdapter,
        params: usize,
        site: &Site,
    ) -> Result<(), Invalid> {
        check_signature(&start.signature).map_err(|error| Invalid {
            site: Some(site.clone()),
            reason: error.reason,
        })?;
        if let crate::FunctionRepresentation::Token { tag, callable } = &start.representation
            && (tag.is_empty() || *callable == 0)
        {
            return Err(invalid(Some(site), InvalidReason::StartToken));
        }
        match &start.convention {
            crate::StartConvention::Direct => {
                if start.signature.params.len() != params {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::StartArity {
                            signature: start.signature.params.len(),
                            declared: params,
                        },
                    ));
                }
            }
            crate::StartConvention::TsArgsList { await_fn, same_fn } => {
                for function in [await_fn, same_fn] {
                    if !self.functions.contains_key(function) {
                        return Err(invalid(
                            Some(site),
                            InvalidReason::FunctionNotListed {
                                function: *function,
                            },
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
fn loop_control(in_loop: bool, keyword: &'static str, site: &Site) -> Result<(), Invalid> {
    if in_loop {
        Ok(())
    } else {
        Err(invalid(
            Some(site),
            InvalidReason::LoopControlOutsideLoop {
                keyword: keyword.into(),
            },
        ))
    }
}

fn check_name(name: &Name, site: Option<&Site>) -> Result<(), Invalid> {
    if name.as_str().is_empty() {
        Err(invalid(site, InvalidReason::EmptyName))
    } else {
        Ok(())
    }
}

fn check_params(params: &[Name], site: &Site) -> Result<(), Invalid> {
    let mut seen = BTreeSet::new();
    for name in params {
        check_name(name, Some(site))?;
        if !seen.insert(name) {
            return Err(invalid(
                Some(site),
                InvalidReason::DuplicateParam { name: name.clone() },
            ));
        }
    }
    Ok(())
}

fn check_arity(
    name: &str,
    given: usize,
    signature: &Signature,
    site: &Site,
) -> Result<(), Invalid> {
    let required = signature.required();
    let accepted = signature.params.len();
    if (required..=accepted).contains(&given) {
        Ok(())
    } else {
        Err(invalid(
            Some(site),
            InvalidReason::CallArity {
                name: name.to_string(),
                given,
                required,
                accepted,
            },
        ))
    }
}

/// Refuses a body that nests deeper than [`MAX_NESTING_DEPTH`]. The walk is
/// iterative: a recursive one would overflow on the input it exists to
/// refuse.
fn check_depth(body: &Block, site: &Site, limit: usize) -> Result<(), Invalid> {
    let mut pending: Vec<(Node<'_>, usize)> = vec![(Node::Block(body), 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > limit {
            return Err(invalid(Some(site), InvalidReason::TooDeep { limit }));
        }
        pending.extend(node.children().into_iter().map(|child| (child, depth + 1)));
    }
    Ok(())
}

fn check_signature(signature: &Signature) -> Result<(), Invalid> {
    let mut seen = BTreeSet::new();
    let mut optional = false;
    for param in &signature.params {
        check_name(&param.name, None)?;
        if !seen.insert(&param.name) {
            return Err(invalid(
                None,
                InvalidReason::DuplicateParam {
                    name: param.name.clone(),
                },
            ));
        }
        if optional && !param.optional {
            return Err(invalid(
                None,
                InvalidReason::RequiredAfterOptional {
                    name: param.name.clone(),
                },
            ));
        }
        optional |= param.optional;
        check_type(&param.ty).map_err(|reason| invalid(None, reason))?;
    }
    check_type(&signature.result).map_err(|reason| invalid(None, reason))
}

fn check_type(ty: &Type) -> Result<(), InvalidReason> {
    let problem = |problem: &'static str| {
        Err(InvalidReason::InvalidType {
            problem: problem.into(),
        })
    };
    match ty {
        Type::Any
        | Type::Null
        | Type::Absent
        | Type::Bool
        | Type::Int
        | Type::Float
        | Type::Number
        | Type::Text
        | Type::Bytes
        | Type::Timestamp
        | Type::Error
        | Type::Handle(_) => Ok(()),
        Type::Tuple(members) => members.iter().try_for_each(check_type),
        Type::List(item) | Type::Set(item) | Type::Task(item) => check_type(item),
        Type::Map(map) => {
            check_type(&map.key)?;
            check_type(&map.value)
        }
        Type::Record(record) => {
            let mut seen = BTreeSet::new();
            for field in &record.fields {
                if !seen.insert(field.name.as_str()) {
                    return problem("a record type names a field twice");
                }
                check_type(&field.ty)?;
            }
            record.rest.as_deref().map_or(Ok(()), check_type)
        }
        Type::Enum(members) => {
            let unique: BTreeSet<&String> = members.iter().collect();
            if members.is_empty() {
                problem("an enum has no member")
            } else if unique.len() != members.len() {
                problem("an enum names a member twice")
            } else {
                Ok(())
            }
        }
        Type::Function(signature) => check_signature(signature).map_err(|invalid| *invalid.reason),
        Type::Union(members) => {
            if members.len() < 2 {
                return problem("a union has fewer than two members");
            }
            if members
                .iter()
                .any(|member| matches!(member, Type::Union(_)))
            {
                return problem("a union holds a union; write its members in place");
            }
            for (index, member) in members.iter().enumerate() {
                if members[..index].contains(member) {
                    return problem("a union names a member twice");
                }
                check_type(member)?;
            }
            Ok(())
        }
    }
}

fn mentions_function(ty: &Type) -> bool {
    match ty {
        Type::Function(_) => true,
        Type::Tuple(members) | Type::Union(members) => members.iter().any(mentions_function),
        Type::List(item) | Type::Set(item) | Type::Task(item) => mentions_function(item),
        Type::Map(map) => mentions_function(&map.key) || mentions_function(&map.value),
        Type::Record(record) => {
            record
                .fields
                .iter()
                .any(|field| mentions_function(&field.ty))
                || record.rest.as_deref().is_some_and(mentions_function)
        }
        _ => false,
    }
}

fn check_formula(formula: &Formula, signature: &Signature, result: bool) -> Result<(), Invalid> {
    match formula {
        Formula::Constant(_) => Ok(()),
        Formula::CeilLog2(term) => check_formula(term, signature, result),
        Formula::Size(operand)
        | Formula::DeepSize(operand)
        | Formula::NestedSize(operand)
        | Formula::Magnitude(operand) => match operand {
            Operand::Result if result => Ok(()),
            Operand::Result => Err(invalid(None, InvalidReason::LimitMeasuresResult)),
            Operand::Param(name) => {
                if signature.params.iter().any(|param| &param.name == name) {
                    Ok(())
                } else {
                    Err(invalid(
                        None,
                        InvalidReason::FormulaUnknownParam { name: name.clone() },
                    ))
                }
            }
        },
        Formula::Max(terms) | Formula::Min(terms) if terms.is_empty() => {
            Err(invalid(None, InvalidReason::EmptyFormula))
        }
        Formula::Sum(terms)
        | Formula::Product(terms)
        | Formula::Max(terms)
        | Formula::Min(terms) => terms
            .iter()
            .try_for_each(|term| check_formula(term, signature, result)),
    }
}

impl Document {
    /// The node a site addresses, when the site is in this document.
    pub fn node(&self, site: &Site) -> Option<Node<'_>> {
        let body = match &site.unit {
            Unit::Main => &self.main,
            Unit::Function(name) => &self.functions.get(name)?.body,
            Unit::Library(_) => return None,
        };
        let mut node = Node::Block(body);
        for step in &site.path {
            node = node
                .children()
                .into_iter()
                .nth(usize::try_from(*step).ok()?)?;
        }
        Some(node)
    }
}
