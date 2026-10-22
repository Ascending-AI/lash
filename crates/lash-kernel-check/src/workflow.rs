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
    reference: LinkedStart,
}
#[derive(Debug, thiserror::Error)]
pub enum WorkflowDocumentError {
    #[error("{0}")]
    Refused(#[from] Refusal),
    #[error("{0}")]
    Start(#[from] StartRefusal),
}
impl WorkflowDocument {
    pub fn entered(
        document: Document,
        reference: LinkedStart,
    ) -> Result<Self, WorkflowDocumentError> {
        crate::selected_reach(&document, &reference.selector)?;
        let graph = crate::derive(&document, &document.functions)?;
        Ok(Self {
            document: Arc::new(document),
            graph: Arc::new(graph),
            reference,
        })
    }
    pub fn reference(&self) -> &LinkedStart {
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
    pub fn selector(&self) -> &StartSelector {
        &self.reference.selector
    }
    pub fn entry_unit(&self) -> Unit {
        match &self.reference.selector {
            StartSelector::Main => Unit::Main,
            StartSelector::Export(name) => match self.document.exports.get(name) {
                Some(Export::Function(f)) => Unit::Library(f.function),
                _ => unreachable!("checked function Export"),
            },
        }
    }
    pub fn entry_signature(&self) -> Option<&Signature> {
        match &self.reference.selector {
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
