//! The workflow contract every dialect's module mode implements (FIG-6380).
//!
//! A workflow module is source a host imports whole, in any dialect
//! ([`crate::Mode::Module`]). Each dialect reads its own syntax, lowers its
//! own bodies with its own helpers and maps its own annotations; the rules
//! below are the same in all of them, and so are the refusal codes:
//!
//! - An entry is a top-level declaration statement, not inside a block,
//!   whose value is an async function, by declaration or by `const`. A
//!   function declared anywhere else is ordinary code of the module.
//! - An entry captures nothing of the module: no module binding, no other
//!   entry and no top-level helper. A host starts it in a run of its own,
//!   which holds only its arguments
//!   ([`ModuleRefusal::CapturedModuleBinding`]).
//! - An entry's signature is the kernel meaning of the annotations its
//!   dialect wrote. An annotation with none is refused
//!   ([`ModuleRefusal::UnmappableEntryAnnotation`]), and so is a parameter
//!   default ([`ModuleRefusal::EntryDefault`]): a start passes every
//!   argument the signature names.
//! - Two entries of one name ([`ModuleRefusal::DuplicateEntry`]) and a
//!   module with no entry ([`ModuleRefusal::EmptyModule`]) are refused.
//!
//! [`Module`] holds the entries a front end lowered and assembles them into
//! the document, in name order, so every dialect's documents order and
//! collide alike. Parser errors stay each dialect's own.

use std::collections::BTreeMap;

use lash_kernel_doc::{Document, Function, Name, Signature, Span};

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
            Self::EmptyModule => {
                "declare the workflow as a top-level async function, by declaration or by const"
            }
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
}

/// One entry a front end lowered: the declared function a start calls, the
/// signature its annotations state, and the notes of its body.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: Name,
    /// The entry's declaration in the source.
    pub span: Option<Span>,
    pub function: Function,
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

    /// Adds `entry`.
    ///
    /// # Errors
    ///
    /// [`ModuleRefusal::DuplicateEntry`] at the second declaration when the
    /// module has an entry of its name.
    pub fn add(&mut self, entry: Entry) -> Result<(), ModuleError> {
        if self.entries.contains_key(&entry.name) {
            return Err(duplicate(&entry.name, entry.span));
        }
        self.entries.insert(entry.name.clone(), entry);
        Ok(())
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
        notes: &mut BTreeMap<Name, Vec<Note>>,
    ) -> Result<(), ModuleError> {
        if self.entries.is_empty() {
            return Err(ModuleRefusal::EmptyModule.at(
                "the module declares no entry: no top-level async function",
                None,
            ));
        }
        for (name, entry) in self.entries {
            if document.functions.contains_key(&name) || document.entries.contains_key(&name) {
                return Err(duplicate(&name, entry.span));
            }
            document.functions.insert(name.clone(), entry.function);
            document.entries.insert(
                name.clone(),
                lash_kernel_doc::Entry {
                    signature: entry.signature,
                    convention: entry.convention,
                },
            );
            notes.insert(name, entry.notes);
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
