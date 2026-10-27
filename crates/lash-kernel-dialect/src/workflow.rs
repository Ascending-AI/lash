//! Module Exports share the document function convention; sync and async functions both qualify.
//!
//! An entry has a private materialized environment for reachable module helper
//! declarations and static data. It never reads main's bindings (K-FN-002).
//! Calls, effects, writes and dynamic module initialization must be moved into
//! the entry or supplied as arguments; they are not run during import.
//!
//! Starts validate required public arguments before execution. An omitted
//! optional argument is `Absent`; explicit null (Python `None`) is a value,
//! and does not select a default. Entry adapters evaluate only omitted
//! defaults, in parameter definition order. TypeScript defaults see preceding
//! parameters, as ordinary calls do. Python workflow defaults see the
//! materialized module environment; unlike cell functions, workflows have no
//! module execution phase at which to evaluate and retain defaults. Static
//! data is fresh for each start, shared by that start's helper closures.
use std::collections::BTreeMap;

use lash_kernel_doc::{Document, FunctionDefinition, Span};

use lash_kernel_value::names::FunctionId;
use lash_kernel_value::names::Name;
use lash_kernel_value::types::Signature;

use crate::{Diagnostic, DiagnosticKind, Note};

/// A rule of the workflow contract a module broke. Its code is the same in
/// every dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModuleRefusal {
    /// Two entries have one name.
    DuplicateEntry,
    /// The module declares no entry.
    EmptyModule,
    /// A module binding needs dynamic initialization outside the entry.
    DynamicModuleInitialization,
}

impl ModuleRefusal {
    /// Every rule, for a caller that lists them.
    pub const ALL: [Self; 3] = [
        Self::DuplicateEntry,
        Self::EmptyModule,
        Self::DynamicModuleInitialization,
    ];

    /// The stable code a diagnostic of this rule carries, in every dialect.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DuplicateEntry => "WORKFLOW_DUPLICATE_ENTRY",
            Self::EmptyModule => "WORKFLOW_EMPTY_MODULE",
            Self::DynamicModuleInitialization => "WORKFLOW_DYNAMIC_MODULE_INITIALIZATION",
        }
    }

    /// Whose fault it is: every rule is the contract's, however the module
    /// is written, except two entries of one name, which is a slip.
    #[must_use]
    pub const fn kind(self) -> DiagnosticKind {
        match self {
            Self::DuplicateEntry => DiagnosticKind::ProgramDefect,
            Self::EmptyModule | Self::DynamicModuleInitialization => DiagnosticKind::Refusal,
        }
    }

    /// What to write instead.
    #[must_use]
    pub const fn repair(self) -> &'static str {
        match self {
            Self::DuplicateEntry => "rename one of the entries",
            Self::EmptyModule => "declare the workflow as a top-level function",
            Self::DynamicModuleInitialization => {
                "pass dynamic state as an entry parameter, or initialize it inside the entry"
            }
        }
    }

    /// This rule, broken at `span`.
    #[must_use]
    pub fn at(self, message: impl Into<String>, span: Option<Span>) -> ModuleError {
        ModuleError {
            refusal: self,
            message: message.into(),
            span,
        }
    }
}

/// A module the contract refuses: the rule, what broke it and where. A
/// dialect reports it under [`ModuleRefusal::code`], in its own diagnostic
/// type.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", refusal.code())]
pub struct ModuleError {
    pub refusal: ModuleRefusal,
    pub message: String,
    pub span: Option<Span>,
}

impl ModuleError {
    /// The refusal as a dialect-neutral diagnostic, under the shared code.
    #[must_use]
    pub fn into_diagnostic(self) -> Diagnostic {
        let mut diagnostic = Diagnostic::dialect(
            crate::DialectCode::from_static(self.refusal.code()),
            self.refusal.kind(),
            self.span,
            self.message,
        );
        diagnostic.repairs.push(self.refusal.repair().to_owned());
        diagnostic
    }
}

/// A host's admission policy for one imported module's source: at most
/// `max_bytes` of text, nested at most `max_nesting` deep. Each dialect
/// holds these within its own parser's safety ceilings and refuses a policy
/// beyond them; a host that names none imports under the dialect's
/// standard policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceLimits {
    pub max_bytes: usize,
    pub max_nesting: usize,
    /// TypeScript implicit text conversion admission. The default is `allow`.
    pub implicit_text_conversion: ImplicitTextConversionPolicy,
}

/// Admission of implicit text conversions in saved TypeScript workflows.
/// Cell execution and explicit conversions keep JavaScript semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplicitTextConversionPolicy {
    /// Refuse records, collections and functions; accept unconstrained operands.
    Refuse,
    /// Accept all implicit conversions.
    #[default]
    Allow,
    /// Also refuse unconstrained (`Any`) operands.
    Strict,
}

impl ImplicitTextConversionPolicy {
    /// The first retained source fact refused by this host's admission policy.
    pub fn check(
        self,
        facts: &[lash_kernel_doc::ImplicitTextConversion],
    ) -> Result<(), Diagnostic> {
        if self == Self::Allow {
            return Ok(());
        }
        if let Some(fact) = facts.iter().find(|fact| fact.refused(self == Self::Strict)) {
            return Err(Diagnostic::new(
                crate::DiagnosticReason::ImplicitTextConversion(Box::new(fact.clone())),
                "implicit text conversion refused by source admission policy",
            ));
        }
        Ok(())
    }
}

/// One entry a front end lowered: the declared function a start calls, the
/// signature its annotations state, and the notes of its body.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: Name,
    /// The entry's declaration in the source.
    pub span: Option<Span>,
    pub function: FunctionDefinition,
    pub asynchronous: bool,
    pub rest: bool,
    pub signature: Signature,
    pub convention: lash_kernel_doc::StartConvention,
    pub notes: Vec<Note>,
}

/// The entries of one module, as a front end lowers them, and their
/// assembly into its document.
#[derive(Debug, Default)]
pub struct Module {
    entries: BTreeMap<Name, Entry>,
}

impl Module {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the module already has an entry named `name`.
    #[must_use]
    pub fn contains(&self, name: &Name) -> bool {
        self.entries.contains_key(name)
    }

    /// The exact body identity of the Export already lowered under `name`.
    #[must_use]
    pub fn function(&self, name: &Name) -> Option<FunctionId> {
        self.entries
            .get(name)
            .map(|entry| entry.function.identity())
    }

    /// Adds `entry`.
    ///
    /// # Errors
    ///
    /// [`ModuleRefusal::DuplicateEntry`] at the second declaration when the
    /// module has an entry of its name.
    pub fn add(&mut self, entry: Entry) -> Result<FunctionId, ModuleError> {
        if self.entries.contains_key(&entry.name) {
            return Err(duplicate(&entry.name, entry.span));
        }
        let id = entry.function.identity();
        self.entries.insert(entry.name.clone(), entry);
        Ok(id)
    }

    /// Puts every entry into `document`, as a declared function and an
    /// entry under its signature, and its body's notes into `notes`.
    ///
    /// # Errors
    ///
    /// [`ModuleRefusal::EmptyModule`] when the module has no entry, and
    /// [`ModuleRefusal::DuplicateEntry`] when the document already declares
    /// a function of an entry's name.
    pub fn assemble(
        self,
        document: &mut Document,
        notes: &mut BTreeMap<FunctionId, Vec<Note>>,
    ) -> Result<(), ModuleError> {
        if self.entries.is_empty() {
            return Err(ModuleRefusal::EmptyModule
                .at("the module declares no entry: no top-level function", None));
        }
        for (name, entry) in self.entries {
            if document.exports.contains_key(&name) {
                return Err(duplicate(&name, entry.span));
            }
            let function = entry.function.identity();
            document.functions.insert(function, entry.function);
            document.exports.insert(
                name.clone(),
                lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                    function,
                    signature: entry.signature,
                    convention: entry.convention,
                    asynchronous: entry.asynchronous,
                    rest: entry.rest,
                }),
            );
            notes.insert(function, entry.notes);
        }
        Ok(())
    }
}

fn duplicate(name: &Name, span: Option<Span>) -> ModuleError {
    ModuleRefusal::DuplicateEntry.at(
        format!("the module declares more than one entry named `{name}`"),
        span,
    )
}
