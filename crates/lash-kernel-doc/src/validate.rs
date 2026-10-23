//! Structural validation (`K-DOC-005`): what must hold of a document or a
//! definition before anything derives from it.
//!
//! Validation is local. It reads the tree, the manifest and the definitions
//! of the functions a statement names; it resolves no variable and infers no
//! type. Scopes, linking against an environment and derived facts belong to
//! `lash-kernel-check`.

use lash_vocabulary::types::{Signature, Type};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;

use crate::ast::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Closure, Expr, Literal, Member,
    Node, Place, Protocol, Rhs, Stmt,
};

use crate::document::Document;
use crate::function::{Formula, FunctionDefinition, Implementation, Operand};

use crate::native::FunctionCatalog;
use crate::version::KernelVersion;
use crate::{Export, FunctionExport, FunctionTarget};
use lash_vocabulary::names::{FunctionId, FunctionName, Name};

use lash_vocabulary::names::{Import, ImportKind, ImportSlot};
use lash_vocabulary::sites::{Site, Unit};

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

/// An action form, as a refusal names one the body of a function with a
/// native implementation may not run (`K-LIB-004`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StatementForm {
    Call,
    Perform,
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
    #[error("invalid document: {message}")]
    Document { message: String },
    #[error("Import slot {slot:?} is missing or has the wrong kind")]
    Import { slot: ImportSlot },
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
    #[error("effect `{slot:?}` is performed but not listed in the manifest")]
    EffectNotListed { slot: ImportSlot },
    #[error("function `{name}` is not declared")]
    FunctionNotDeclared { name: Name },
    #[error("a library function's body cannot name a declared function (`{name}`)")]
    DeclaredFunctionInLibrary { name: Name },
    /// `K-LIB-004`: a library body performs only the operations its
    /// `effects` lists.
    #[error("effect `{slot:?}` is performed but not listed in the body's `effects`")]
    EffectNotDeclared { slot: ImportSlot },
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
    /// `K-EVAL-003`: an assignment evaluates its place before its
    /// right-hand side only when it writes a member, and that right-hand
    /// side is an expression, any action in it nested.
    #[error("a place-first assignment {problem}")]
    PlaceFirstAssignment { problem: Cow<'static, str> },
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
    // A registered function states its version without being decoded.
    let written_for = match catalog.registered(function) {
        Some(registered) => Some(registered.kernel()),
        None => catalog
            .definition(function)
            .map(|definition| definition.kernel),
    };
    match written_for {
        Some(written_for) if written_for != version.number() => {
            let name = catalog
                .definition(function)
                .map(|definition| definition.name.clone())
                .unwrap_or_else(|| unreachable!("a function that states a version is defined"));
            Err(invalid(
                None,
                InvalidReason::FunctionOfAnotherVersion {
                    function: *function,
                    name,
                    written_for,
                    required: version.number(),
                },
            ))
        }
        _ => Ok(()),
    }
}

/// Validates a document's structure, checking its calls by name and its
/// nested actions against the definitions in `catalog`.
pub fn validate_document(
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Result<(), Invalid> {
    validate_document_with_limits(
        document,
        catalog,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
/// Validate under a deployment's document recursion safety policy.
pub fn validate_document_with_limits(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<(), Invalid> {
    validate_document_in(document, catalog, limits, false)
}

/// Check stored document structure without consulting any installed library.
/// Exact library membership, signatures and static requirements are admission.
pub fn validate_document_structure(document: &Document) -> Result<(), Invalid> {
    validate_document_structure_with_limits(
        document,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
pub fn validate_document_structure_with_limits(
    document: &Document,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<(), Invalid> {
    validate_document_in(document, &document.functions, limits, true)
}
fn validate_document_in(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
    structural: bool,
) -> Result<(), Invalid> {
    if document.requires.kernels.len() != 1 {
        return Err(invalid(
            None,
            InvalidReason::Document {
                message: "a Document requires exactly one kernel version".into(),
            },
        ));
    }
    let kernel = *document
        .requires
        .kernels
        .first()
        .unwrap_or_else(|| unreachable!("one kernel"));
    let Some(version) = KernelVersion::of(kernel) else {
        return Err(invalid(
            None,
            InvalidReason::KernelVersion {
                found: kernel,
                supported: KernelVersion::NEWEST.number(),
            },
        ));
    };
    for function in document.library.iter() {
        if document.functions.contains_key(function)
            || (!structural && catalog.header(function).is_none())
        {
            return Err(invalid(
                None,
                InvalidReason::Document {
                    message: format!(
                        "library reference {function} is missing or overlaps a local definition"
                    ),
                },
            ));
        }
        if !structural {
            check_written_for(catalog, function, version)?;
        }
    }
    let mut library_reach = std::collections::BTreeSet::new();
    let mut pending: Vec<_> = if structural {
        Vec::new()
    } else {
        document.library.iter().copied().collect()
    };
    while let Some(function) = pending.pop() {
        if !library_reach.insert(function) {
            continue;
        }
        let (callees, imports) = if let Some(registered) = catalog.registered(&function) {
            (registered.callees().clone(), registered.imports().clone())
        } else if let Some(definition) = catalog.definition(&function) {
            definition
                .body()
                .map(|body| {
                    (
                        body.functions.keys().copied().collect(),
                        body.imports.clone(),
                    )
                })
                .unwrap_or_default()
        } else {
            return Err(invalid(
                None,
                InvalidReason::FunctionNotResolved { function },
            ));
        };
        for (slot, import) in imports {
            if document.imports.get(&slot) != Some(&import) {
                return Err(invalid(None, InvalidReason::Import { slot }));
            }
        }
        pending.extend(callees);
    }
    let context = crate::DocumentCatalog::new(document, catalog);
    let names = document
        .functions
        .iter()
        .map(|(id, f)| (*id, f.name.clone()))
        .chain(document.library.iter().filter_map(|id| {
            (!structural)
                .then(|| catalog.header(id))
                .flatten()
                .map(|header| (*id, header.name.clone()))
        }))
        .collect();
    let scope = Scope {
        limits,
        document: Some(document),
        functions: &names,
        imports: &document.imports,
        catalog,
        native_body: false,
        deferred: structural.then_some(&document.library.0),
    };
    for name in &document.private_bindings {
        check_name(name, None)?;
    }
    for import in document.imports.values() {
        check_import(import)?;
    }
    for (id, definition) in &document.functions {
        if definition.identity() != *id {
            return Err(invalid(
                None,
                InvalidReason::Document {
                    message: format!("function {id} has another identity"),
                },
            ));
        }
        if definition.kernel != kernel {
            return Err(invalid(
                None,
                InvalidReason::FunctionOfAnotherVersion {
                    function: *id,
                    name: definition.name.clone(),
                    written_for: definition.kernel,
                    required: kernel,
                },
            ));
        }
        if let Some(body) = definition.body() {
            for (slot, import) in &body.imports {
                if document.imports.get(slot) != Some(import) {
                    return Err(invalid(None, InvalidReason::Import { slot: *slot }));
                }
            }
            for (callee, name) in &body.functions {
                if !(structural && document.library.contains(callee))
                    && context.header(callee).map(|f| f.name.clone()).as_ref() != Some(name)
                {
                    return Err(invalid(
                        None,
                        InvalidReason::FunctionNotListed { function: *callee },
                    ));
                }
            }
        }
        // Use the document's local definitions, never a global same-name definition.
        validate_definition_in(
            definition,
            &context,
            limits,
            structural.then_some(&document.library.0),
        )?;
    }
    for native in document.requires.natives.iter().filter(|_| !structural) {
        check_written_for(catalog, native, version)?;
        if !context
            .header(native)
            .or_else(|| {
                library_reach
                    .contains(native)
                    .then(|| catalog.header(native))
                    .flatten()
            })
            .is_some_and(|header| header.has_native())
        {
            return Err(invalid(
                None,
                InvalidReason::FunctionNotListed { function: *native },
            ));
        }
    }
    for export in document.exports.values() {
        match export {
            Export::Function(function) => check_export(function, &scope)?,
            Export::Type(ty) => {
                check_type(&ty.representation).map_err(|reason| invalid(None, reason))?;
                let checker = scope
                    .listed(&ty.checker, &Site::new(Unit::Main, []))?
                    .ok_or_else(|| {
                        invalid(
                            None,
                            InvalidReason::FunctionNotListed {
                                function: ty.checker,
                            },
                        )
                    })?;
                if checker.signature.params.len() != 1 || checker.signature.result != Type::Bool {
                    return Err(invalid(
                        None,
                        InvalidReason::Document {
                            message: "a type checker takes one value and returns Bool".into(),
                        },
                    ));
                }
                for member in ty.members.values() {
                    check_export(member.function(), &scope)?;
                }
            }
            Export::Error { .. } => {}
        }
    }
    if let Some(main) = &document.main {
        scope.check_unit(main, Site::new(Unit::Main, []))?;
    }
    Ok(())
}

fn check_import(import: &Import) -> Result<(), Invalid> {
    match &import.kind {
        ImportKind::Operation { signature } | ImportKind::Code { signature, .. } => {
            check_signature(signature)
        }
        ImportKind::Type { .. } | ImportKind::Error { .. } => Ok(()),
    }
}
/// Check the public callable ABI against its internal body convention.
/// Parameter names and types need not be equal: source ABIs carry packed/internal parameters.
pub fn validate_callable_signature(
    signature: &Signature,
    convention: &crate::StartConvention,
    internal: &Signature,
) -> Result<(), Invalid> {
    check_signature(signature)?;
    let expected = match convention {
        crate::StartConvention::Direct | crate::StartConvention::PyDirect { .. } => {
            Some(signature.params.len())
        }
        crate::StartConvention::TsArgsList { .. } => Some(2),
        crate::StartConvention::TsDirect { .. } => None,
    };
    if let Some(signature) = expected.filter(|count| *count != internal.params.len()) {
        return Err(invalid(
            None,
            InvalidReason::StartArity {
                signature,
                declared: internal.params.len(),
            },
        ));
    }
    Ok(())
}
fn check_export(export: &FunctionExport, scope: &Scope<'_>) -> Result<(), Invalid> {
    let site = Site::new(Unit::Library(export.function), []);
    let definition = scope.listed(&export.function, &site)?.ok_or_else(|| {
        invalid(
            Some(&site),
            InvalidReason::FunctionNotListed {
                function: export.function,
            },
        )
    })?;
    validate_callable_signature(&export.signature, &export.convention, &definition.signature)?;
    if export.rest
        && !export
            .signature
            .params
            .last()
            .is_some_and(|p| matches!(p.ty, Type::List(_)))
    {
        return Err(invalid(
            Some(&site),
            InvalidReason::Document {
                message: "a rest parameter must be a final List parameter".into(),
            },
        ));
    }
    for target in export.convention.targets() {
        scope.target(target, &site)?;
    }
    Ok(())
}

/// Validates a library-function definition's structure, checking its body's
/// calls by name and nested actions against the definitions in `catalog`.
pub fn validate_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
) -> Result<(), Invalid> {
    validate_definition_with_limits(
        definition,
        catalog,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
/// Validate under a deployment's document recursion safety policy.
pub fn validate_definition_with_limits(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<(), Invalid> {
    validate_identified_definition(definition, catalog, limits).map(|_| ())
}

/// Registration keeps the identity validation used to name a body's sites.
pub(crate) fn validate_identified_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<Option<lash_vocabulary::names::FunctionId>, Invalid> {
    validate_definition_in(definition, catalog, limits, None)
}
fn validate_definition_in(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
    deferred: Option<&std::collections::BTreeSet<FunctionId>>,
) -> Result<Option<FunctionId>, Invalid> {
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
            if !deferred.is_some_and(|ids| ids.contains(function)) {
                check_written_for(catalog, function, version)?;
            }
        }
        for import in body.imports.values() {
            check_import(import)?;
        }
    }
    check_signature(&definition.signature)?;
    if let Some(charge) = definition.charge() {
        check_formula(charge, &definition.signature, true)?;
    }
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
    let native_body = matches!(definition.implementation, Implementation::Both { .. });
    // A body beside a native performs nothing (`K-LIB-004`).
    if native_body && !body.imports.is_empty() {
        return Err(invalid(
            None,
            InvalidReason::NativeBodyMayWait {
                form: StatementForm::Perform,
            },
        ));
    }
    // The site names the body by the definition's own identity.
    let function = definition.identity();
    let scope = Scope {
        limits,
        document: None,
        functions: &body.functions,
        imports: &body.imports,
        catalog,
        native_body,
        deferred,
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

fn native_call(function: &FunctionId, catalog: &dyn FunctionCatalog) -> Result<(), InvalidReason> {
    let Some(header) = catalog.header(function) else {
        return Err(InvalidReason::FunctionNotResolved {
            function: *function,
        });
    };
    if !header.has_native() {
        return Err(InvalidReason::CallNotNative {
            function: *function,
            name: header.name.clone(),
        });
    }
    Ok(())
}

struct Scope<'a> {
    limits: lash_vocabulary::data_limits::KernelDataLimits,
    /// The document being validated, or `None` inside a library body.
    document: Option<&'a Document>,
    /// The library functions this code may name.
    functions: &'a std::collections::BTreeMap<FunctionId, FunctionName>,
    /// The operations this code may perform: the document's manifest, or
    /// the body's own `effects`.
    imports: &'a std::collections::BTreeMap<ImportSlot, Import>,
    catalog: &'a dyn FunctionCatalog,
    /// The code is a body that stands beside a native implementation.
    native_body: bool,
    deferred: Option<&'a std::collections::BTreeSet<FunctionId>>,
}

impl Scope<'_> {
    fn deferred(&self, function: &FunctionId) -> bool {
        self.deferred.is_some_and(|ids| ids.contains(function))
    }
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
            Stmt::Assign {
                place,
                value,
                order,
            } => {
                match place {
                    Place::Variable(name) => {
                        check_name(name, Some(site))?;
                        if *order == AssignOrder::PlaceFirst {
                            return Err(invalid(
                                Some(site),
                                InvalidReason::PlaceFirstAssignment {
                                    problem: "writes a variable, so nothing of its place is \
                                              evaluated"
                                        .into(),
                                },
                            ));
                        }
                    }
                    Place::Member(member) => self.check_member(member, &mut child)?,
                }
                let at = child();
                match (order, value) {
                    // The place is held across a wait only by a nested
                    // action's continuation (`K-EVAL-008`).
                    (AssignOrder::PlaceFirst, Rhs::Action(_)) => Err(invalid(
                        Some(&at),
                        InvalidReason::PlaceFirstAssignment {
                            problem: "has an action as its right-hand side, which is nested \
                                      after the place"
                                .into(),
                        },
                    )),
                    (AssignOrder::PlaceFirst, Rhs::Expr(expr)) => self.check_expr(expr, &at),
                    (AssignOrder::ValueFirst, value) => self.check_rhs(value, &at),
                }
            }
            Stmt::Remove { member } => self.check_member(member, &mut child),
            Stmt::Do { value } => self.check_expr(value, &child()),
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
                ..
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
                ..
            } => {
                self.check_expr(condition, &child())?;
                self.check_block(body, &child(), true)?;
                self.check_block(else_block, &child(), in_loop)
            }
            Stmt::Break => loop_control(in_loop, "break", site),
            Stmt::Continue => loop_control(in_loop, "continue", site),
            Stmt::ReturnVia { value, policy } => {
                let Some(definition) = self.target(policy, site)? else {
                    return self.check_expr(value, &child());
                };
                if self.native_body && !definition.has_native() {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::NativeBodyMayWait {
                            form: StatementForm::Call,
                        },
                    ));
                }
                check_arity(definition.name.as_str(), 1, &definition.signature, site)?;
                self.check_expr(value, &child())
            }
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
            ActKind::Spawn(_) => StatementForm::Spawn,
            ActKind::Join | ActKind::JoinMany(_) => StatementForm::Join,
            ActKind::Await(_) => StatementForm::Await,
            ActKind::Yield => StatementForm::Yield,
            ActKind::Cancel => StatementForm::Cancel,
        };
        // As for a statement action (`K-LIB-004`): a body beside a native
        // may call functions that state one and apply a closure it holds.
        let callable = match &act.kind {
            ActKind::Call(Callee::Local(function)) => {
                self.deferred(function)
                    || self
                        .catalog
                        .header(function)
                        .is_some_and(|header| header.has_native())
            }
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
            ActKind::Perform { slot, result, .. } => {
                let signature = self.performed(slot, site)?;
                check_arity("Import", act.args.len(), signature, site)?;
                check_type(result).map_err(|reason| invalid(Some(site), reason))?;
            }
            ActKind::Join | ActKind::JoinMany(_) | ActKind::Cancel => {
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
            ActKind::Yield => {
                if !act.args.is_empty() {
                    return Err(invalid(
                        Some(site),
                        InvalidReason::CallArity {
                            name: "yield".into(),
                            given: act.args.len(),
                            required: 0,
                            accepted: 0,
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
                self.check_target(policy, 1, site)?
            }
        }
        self.check_exprs(act.args.iter(), site)
    }

    fn check_action(&self, action: &Action, site: &Site) -> Result<(), Invalid> {
        let form = match action {
            Action::Call { .. } => StatementForm::Call,
            Action::Perform { .. } => StatementForm::Perform,
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
                callee: Callee::Local(function),
                ..
            } => {
                self.deferred(function)
                    || self
                        .catalog
                        .header(function)
                        .is_some_and(|header| header.has_native())
            }
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
                slot, args, result, ..
            } => {
                let signature = self.performed(slot, site)?;
                check_arity("Import", args.len(), signature, site)?;
                check_type(result).map_err(|reason| invalid(Some(site), reason))?;
                self.check_atoms(args, site)
            }
            Action::Await { policy, value } => {
                self.check_target(policy, 1, site)?;
                self.check_atoms(std::slice::from_ref(value), site)
            }
            Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.check_atoms(std::slice::from_ref(atom), site),
            Action::Yield => Ok(()),
        }
    }

    fn check_callee(&self, callee: &Callee, given: usize, site: &Site) -> Result<(), Invalid> {
        match callee {
            Callee::Import(slot) => self.check_target(&FunctionTarget::Import(*slot), given, site),
            Callee::Value(name) => check_name(name, Some(site)),
            Callee::Local(function) => {
                let Some(definition) = self.listed(function, site)? else {
                    return Ok(());
                };
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
            let Some(definition) = self.target(function, site)? else {
                continue;
            };
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

    fn performed(&self, slot: &ImportSlot, site: &Site) -> Result<&Signature, Invalid> {
        match self.imports.get(slot).map(|i| &i.kind) {
            Some(ImportKind::Operation { signature }) => Ok(signature),
            _ => Err(invalid(Some(site), InvalidReason::Import { slot: *slot })),
        }
    }
    fn check_target(
        &self,
        target: &FunctionTarget,
        given: usize,
        site: &Site,
    ) -> Result<(), Invalid> {
        let Some(header) = self.target(target, site)? else {
            return Ok(());
        };
        check_arity(header.name.as_str(), given, &header.signature, site)
    }
    fn target(
        &self,
        target: &FunctionTarget,
        site: &Site,
    ) -> Result<Option<std::sync::Arc<crate::FunctionHeader>>, Invalid> {
        match target {
            FunctionTarget::Local(id) => self.listed(id, site),
            FunctionTarget::Import(slot) => match self.imports.get(slot) {
                Some(Import {
                    kind: ImportKind::Code { signature, .. },
                    ..
                }) => Ok(Some(std::sync::Arc::new(crate::FunctionHeader {
                    kernel: crate::KERNEL_VERSION,
                    name: FunctionName::new("import.call")
                        .unwrap_or_else(|_| unreachable!("valid name")),
                    signature: signature.clone(),
                    errors: Default::default(),
                    metering: crate::Metering::Statements,
                    guard: None,
                    implementation: crate::ImplementationKind::Guest,
                }))),
                _ => Err(invalid(Some(site), InvalidReason::Import { slot: *slot })),
            },
        }
    }

    /// The header of a library function this code lists.
    fn listed(
        &self,
        function: &FunctionId,
        site: &Site,
    ) -> Result<Option<std::sync::Arc<crate::FunctionHeader>>, Invalid> {
        if self.deferred(function)
            && (self.document.is_some() || self.functions.contains_key(function))
        {
            return Ok(None);
        }
        if !self.functions.contains_key(function) {
            return Err(invalid(
                Some(site),
                InvalidReason::FunctionNotListed {
                    function: *function,
                },
            ));
        }
        self.document
            .and_then(|d| d.functions.get(function))
            .map(|f| std::sync::Arc::new(crate::FunctionHeader::of(f)))
            .or_else(|| self.catalog.header(function))
            .map(Some)
            .ok_or_else(|| {
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
            Literal::Function(id) => self.listed(id, site).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn check_expr(&self, expr: &Expr, site: &Site) -> Result<(), Invalid> {
        match expr {
            Expr::Literal(literal) => self.check_literal(literal, site),
            Expr::Variable(name) => check_name(name, Some(site)),
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
                let Some(definition) = self.listed(function, site)? else {
                    return self.check_exprs(args.iter(), site);
                };
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
        let definition = self.listed(&closure.function, site)?;
        if let Some(start) = &closure.presentation {
            self.check_start(
                start,
                definition.map_or(start.signature.params.len(), |header| {
                    header.signature.params.len()
                }),
                site,
            )?;
        }
        Ok(())
    }
}

/// Refuses an action nested in `expr`, at `site`, which stands in
/// `position`: no frame can stand at one there. A closure's body is code of
/// its own.
impl Scope<'_> {
    /// A start adapter (`K-CLO-004`): a valid signature, a direct start's
    /// arity, a token's tag and slot, and the functions a TypeScript start
    /// calls listed in the manifest.
    fn check_start(
        &self,
        start: &crate::CallablePresentation,
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
        if matches!(
            start.convention,
            crate::StartConvention::Direct | crate::StartConvention::PyDirect { .. }
        ) && start.signature.params.len() != params
        {
            return Err(invalid(
                Some(site),
                InvalidReason::StartArity {
                    signature: start.signature.params.len(),
                    declared: params,
                },
            ));
        }
        for target in start.convention.targets() {
            self.target(target, site)?;
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
        | Type::Json
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
            record.rest.kept_type().map_or(Ok(()), check_type)
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
                || record.rest.kept_type().is_some_and(mentions_function)
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
            Unit::Main => self.main.as_ref()?,
            Unit::Function(_) => return None,
            Unit::Library(id) => &self.functions.get(id)?.body()?.block,
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
