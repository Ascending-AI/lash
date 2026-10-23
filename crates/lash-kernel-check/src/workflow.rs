//! Checked document and host start selection. Static eligibility is kind plus data_only.
use crate::{Graph, Refusal};
use lash_kernel_doc::{Document, Export, FunctionDeclarations, FunctionExport};

use lash_vocabulary::names::DocumentId;
use lash_vocabulary::names::Name;
use lash_vocabulary::names::{ExportName, LinkedStart, StartSelector};
use lash_vocabulary::sites::Unit;
use lash_vocabulary::types::Signature;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartRefusal {
    #[error(transparent)]
    SignatureRequired(#[from] Box<lash_kernel_doc::SignatureRequired>),
    #[error("the Document has no main")]
    MissingMain,
    #[error("Export {export:?} is not a function")]
    NotFunction { export: ExportName },
    #[error("Export {export:?} has non-data parameters or settled result")]
    NotDataOnly { export: ExportName },
}
pub fn startable_export<'a>(
    document: &'a Document,
    name: &Name,
) -> Result<&'a FunctionExport, StartRefusal> {
    let export = ExportName {
        name: name.clone(),
        member: None,
    };
    let Some(Export::Function(function)) = document.exports.get(name) else {
        return Err(StartRefusal::NotFunction { export });
    };
    lash_kernel_doc::require_entry_signature(
        &function.signature,
        lash_kernel_doc::FunctionLocation {
            name: Some(name.to_string()),
            site: Some(lash_vocabulary::sites::Site::new(
                Unit::Library(function.function),
                [],
            )),
        },
    )?;
    if !function
        .signature
        .params
        .iter()
        .all(|param| lash_vocabulary::type_relations::data_only(&param.ty))
        || !lash_vocabulary::type_relations::data_only(&function.signature.result)
    {
        return Err(StartRefusal::NotDataOnly { export });
    }
    Ok(function)
}
#[derive(Clone, Debug)]
pub struct WorkflowDocument {
    document: Arc<Document>,
    graph: Arc<Graph>,
    reference: lash_kernel_doc::WorkflowReference,
    callable: Option<lash_kernel_doc::FunctionValue>,
}
#[derive(Debug, thiserror::Error)]
pub enum WorkflowDocumentError {
    #[error(transparent)]
    Callable(#[from] lash_kernel_doc::FunctionValueRefusal),
    #[error("{0}")]
    Refused(#[from] Refusal),
    #[error("{0}")]
    Start(#[from] StartRefusal),
}
impl WorkflowDocument {
    pub fn entered(
        document: Document,
        reference: LinkedStart,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<Self, WorkflowDocumentError> {
        crate::selected_reach(&document, &reference.selector, catalog)?;
        let graph = crate::derive(&document, catalog)?;
        Ok(Self {
            document: Arc::new(document),
            graph: Arc::new(graph),
            reference: reference.into(),
            callable: None,
        })
    }
    pub fn captured(
        document: Document,
        function: lash_kernel_doc::FunctionValue,
        helpers: &lash_kernel_doc::BuildHelpers,
    ) -> Result<Self, WorkflowDocumentError> {
        function.check(
            helpers,
            lash_kernel_doc::FunctionValueLimits::standard(),
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )?;
        function.entry_export(&document, helpers, &lash_kernel_doc::Selection::Fresh)?;
        let definition = document
            .functions
            .get(&function.callable.function)
            .ok_or(lash_kernel_doc::FunctionValueRefusal::Identity)?;
        lash_kernel_doc::validate_callable_signature(
            &function.signature,
            &function.convention,
            &definition.signature,
        )
        .map_err(|error| lash_kernel_doc::FunctionValueRefusal::Invalid {
            problem: error.to_string(),
        })?;
        let catalog = lash_kernel_doc::DocumentCatalog::new(&document, helpers);
        let graph = crate::derive_captured_definition(definition, &catalog, &function.captures)?;
        Ok(Self {
            reference: lash_kernel_doc::WorkflowReference::Callable(function.callable),
            document: Arc::new(document),
            graph: Arc::new(graph),
            callable: Some(function),
        })
    }
    pub fn callable(&self) -> Option<&lash_kernel_doc::FunctionValue> {
        self.callable.as_ref()
    }
    pub fn captures(
        &self,
    ) -> Option<&std::collections::BTreeMap<Name, lash_kernel_doc::CaptureValue>> {
        self.callable().map(|function| &function.captures)
    }
    pub fn reference(&self) -> &lash_kernel_doc::WorkflowReference {
        &self.reference
    }
    pub fn identity(&self) -> DocumentId {
        self.document.identity()
    }
    pub fn document(&self) -> &Document {
        &self.document
    }
    pub fn graph(&self) -> &Graph {
        &self.graph
    }
    pub fn declarations(&self) -> &FunctionDeclarations {
        &self.document.functions
    }
    pub fn selector(&self) -> Option<&StartSelector> {
        match &self.reference {
            lash_kernel_doc::WorkflowReference::Linked(start) => Some(&start.selector),
            lash_kernel_doc::WorkflowReference::Callable(_) => None,
        }
    }
    pub fn entry_unit(&self) -> Unit {
        match &self.reference {
            lash_kernel_doc::WorkflowReference::Callable(callable) => {
                Unit::Library(callable.function)
            }
            lash_kernel_doc::WorkflowReference::Linked(start) => match &start.selector {
                StartSelector::Main => Unit::Main,
                StartSelector::Export(name) => match self.document.exports.get(name) {
                    Some(Export::Function(f)) => Unit::Library(f.function),
                    _ => unreachable!("checked function Export"),
                },
            },
        }
    }
    pub fn entry_signature(&self) -> Option<&Signature> {
        if let Some(function) = self.callable() {
            return Some(&function.signature);
        }
        match self.selector()? {
            StartSelector::Main => None,
            StartSelector::Export(name) => startable_export(&self.document, name)
                .ok()
                .map(|f| &f.signature),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartBoundaryRefusal {
    #[error(transparent)]
    Start(#[from] StartRefusal),
    #[error("unexpected start argument {name}")]
    UnexpectedArgument { name: Name },
    #[error("missing start argument {name}")]
    MissingArgument { name: Name },
    #[error(transparent)]
    TypeMismatch(#[from] Box<lash_vocabulary::EntryTypeMismatch>),
    #[error(transparent)]
    Decode(#[from] lash_kernel_doc::DecodeRefusal),
}
/// Runtime fitting is separate from static host eligibility, and runs before the function body.
pub fn validate_start_input(
    document: &Document,
    selector: &StartSelector,
    link: lash_vocabulary::LinkId,
    args: &std::collections::BTreeMap<Name, lash_kernel_doc::Datum>,
) -> Result<(), StartBoundaryRefusal> {
    let signature = match selector {
        StartSelector::Main => {
            if document.main.is_none() {
                return Err(StartRefusal::MissingMain.into());
            }
            None
        }
        StartSelector::Export(name) => Some(&startable_export(document, name)?.signature),
    };
    for name in args.keys() {
        if !signature.is_some_and(|s| s.params.iter().any(|p| p.name == *name)) {
            return Err(StartBoundaryRefusal::UnexpectedArgument { name: name.clone() });
        }
    }
    if let Some(signature) = signature {
        let decoder = lash_kernel_doc::Decoder {
            policy: document.numbers,
            max_depth: 64,
            declared: &|_| false,
        };
        for param in &signature.params {
            let Some(value) = args.get(&param.name) else {
                if param.optional {
                    continue;
                }
                return Err(StartBoundaryRefusal::MissingArgument {
                    name: param.name.clone(),
                });
            };
            if param.optional && matches!(value, lash_kernel_doc::Datum::Absent) {
                continue;
            }
            decoder.decode(value, &param.ty).map_err(|mismatch| {
                entry_decode_refusal(
                    document,
                    selector,
                    link,
                    mismatch.within(param.name.as_str()).within("input"),
                )
            })?;
        }
    }
    Ok(())
}
/// Fit the settled value, after the convention has awaited/cleaned up the function's result.
pub fn validate_start_result(
    document: &Document,
    selector: &StartSelector,
    link: lash_vocabulary::LinkId,
    result: &lash_kernel_doc::Datum,
) -> Result<(), StartBoundaryRefusal> {
    let StartSelector::Export(name) = selector else {
        return Ok(());
    };
    let function = startable_export(document, name)?;
    lash_kernel_doc::Decoder {
        policy: document.numbers,
        max_depth: 64,
        declared: &|_| false,
    }
    .decode(result, &function.signature.result)
    .map(|_| ())
    .map_err(|mismatch| entry_decode_refusal(document, selector, link, mismatch.within("output")))
}

fn entry_decode_refusal(
    document: &Document,
    selector: &StartSelector,
    link: lash_vocabulary::LinkId,
    error: lash_kernel_doc::DecodeRefusal,
) -> StartBoundaryRefusal {
    let StartSelector::Export(name) = selector else {
        return error.into();
    };
    let Some(Export::Function(function)) = document.exports.get(name) else {
        return error.into();
    };
    match error {
        lash_kernel_doc::DecodeRefusal::Type { mismatch } => {
            Box::new(lash_vocabulary::EntryTypeMismatch {
                callable: lash_vocabulary::CallableId {
                    link,
                    function: function.function,
                },
                mismatch,
            })
            .into()
        }
        other => other.into(),
    }
}
