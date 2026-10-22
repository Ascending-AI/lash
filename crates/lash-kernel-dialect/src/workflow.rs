//! Module Exports share the document function convention; sync and async functions both qualify.
use std::collections::BTreeMap;

use lash_kernel_doc::{Document, FunctionDefinition, Span};

use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::Name;
use lash_vocabulary::types::Signature;

use crate::{Diagnostic, DiagnosticKind, Note};

/// A rule of the workflow contract a module broke. Its code is the same in
/// every dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModuleRefusal {
    /// Two entries have one name.
    DuplicateEntry,
    /// The module declares no entry.
    EmptyModule,
    /// An entry reads a binding of the module: another entry, a top-level
    /// helper or a top-level value.
    CapturedModuleBinding,
    /// An entry's parameter or result annotation has no kernel type.
    UnmappableEntryAnnotation,
    /// An entry's parameter has a default.
    EntryDefault,
}

impl ModuleRefusal {
    /// Every rule, for a caller that lists them.
    pub const ALL: [Self; 5] = [
        Self::DuplicateEntry,
        Self::EmptyModule,
        Self::CapturedModuleBinding,
        Self::UnmappableEntryAnnotation,
        Self::EntryDefault,
    ];

    /// The stable code a diagnostic of this rule carries, in every dialect.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::DuplicateEntry => "WORKFLOW_DUPLICATE_ENTRY",
            Self::EmptyModule => "WORKFLOW_EMPTY_MODULE",
            Self::CapturedModuleBinding => "WORKFLOW_CAPTURED_MODULE_BINDING",
            Self::UnmappableEntryAnnotation => "WORKFLOW_UNMAPPABLE_ENTRY_ANNOTATION",
            Self::EntryDefault => "WORKFLOW_ENTRY_DEFAULT",
        }
    }

    /// Whose fault it is: every rule is the contract's, however the module
    /// is written, except two entries of one name, which is a slip.
    #[must_use]
    pub const fn kind(self) -> DiagnosticKind {
        match self {
            Self::DuplicateEntry => DiagnosticKind::ProgramDefect,
            Self::EmptyModule
            | Self::CapturedModuleBinding
            | Self::UnmappableEntryAnnotation
            | Self::EntryDefault => DiagnosticKind::Refusal,
        }
    }

    /// What to write instead.
    #[must_use]
    pub const fn repair(self) -> &'static str {
        match self {
            Self::DuplicateEntry => "rename one of the entries",
            Self::EmptyModule => "declare the workflow as a top-level function",
            Self::CapturedModuleBinding => {
                "pass the value as a parameter of the entry, or define the helper inside its body"
            }
            Self::UnmappableEntryAnnotation => {
                "annotate with a primitive, a list, a record or dict, a union of those, or nothing"
            }
            Self::EntryDefault => {
                "remove the default; a start passes every argument the entry declares"
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
        Diagnostic {
            code: self.refusal.code().to_owned(),
            message: self.message,
            span: self.span,
            kind: self.refusal.kind(),
            repairs: vec![self.refusal.repair().to_owned()],
            operand_type: None,
            namespace_shadowing: None,
        }
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
    /// TypeScript implicit text conversion admission. The default is `refuse`.
    pub implicit_text_conversion: ImplicitTextConversionPolicy,
}

/// Admission of implicit text conversions in saved TypeScript workflows.
/// Cell execution and explicit conversions keep JavaScript semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplicitTextConversionPolicy {
    /// Refuse records, collections and functions; accept unconstrained operands.
    #[default]
    Refuse,
    /// Accept all implicit conversions.
    Allow,
    /// Also refuse unconstrained (`Any`) operands.
    Strict,
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
