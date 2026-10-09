use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{
    HostRequirements, ModuleArtifact, host_requirements_for_program_with_catalog,
};
use crate::ast::{
    AssignPathStep, AstPath, AstRoot, AstString, Declaration, Expr, ProcessDecl, ProcessOrigin,
    ProcessParam, Program, ResourceRefExpr, TypeExpr, TypeField, format_type_expr,
};
use crate::span::Span;

mod catalog;
pub use catalog::{LashVmHostCatalog, OperationContract};
mod host;
use host::module_path_key;
pub use host::{
    LashVmHostCatalogError, LashVmHostEnvironment, LashVmLanguageFeatures, LinkedModule,
    ModuleInstanceCatalog, ModuleOperationBinding, NamedDataType, NamedDataTypeError,
    OutputFromInputBinding, ResolvedOperation, ResourceOperationBinding, ResourceTypeCatalog,
    ValueConstructorBinding,
};
mod errors;
pub use errors::LinkError;
mod pass_setup;
use pass_setup::{Binding, Linker, Rederived, function_signature};
mod lower_calls;
mod lower_expr;
mod lower_javascript;
mod module_resolution;
mod open_shapes;
use open_shapes::OpenPlaces;
mod pass_validation;
mod process_literal;
mod type_helpers;
use type_helpers::{
    Completion, Scope, any_binding, binding_type, call_input_type, direct_call_input_field,
    expected_call_arg_type, expr_has_label_annotation, field_type, index_type, iterable_item_type,
    label_annotation_path, literal_type, module_path_for_expr, process_input_record_type,
    process_input_type, process_type_for_decl, process_unknown_type, shaping_builtin_return_type,
    shaping_comparable_type, shaping_list_item, shaping_number_type, shaping_record_type,
    shaping_text_type, strip_label_annotation, union_type,
};
mod facets;
pub use facets::analyze_workflow_program;
use facets::{declaration_span, recover_workflow_binding, workflow_diagnostic_owner_key};
#[cfg(test)]
mod tests;

/// Facts the workflow projector reads back for one node, keyed by the node's
/// [`AstPath`] in the program the walk ran on. Paths — not addresses — are
/// what survive a clone of the tree.
#[derive(Clone, Debug, Default)]
pub struct WorkflowLinkAnalysis {
    nodes: BTreeMap<AstPath, WorkflowLinkNodeFacts>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowLinkNodeFacts {
    pub(crate) available_variables: BTreeMap<String, TypeExpr>,
    pub(crate) expected_arguments: Vec<WorkflowLinkExpectedArgument>,
    pub(crate) diagnostics: Vec<WorkflowLinkDiagnostic>,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkflowLinkDiagnostic {
    pub(crate) classification: crate::WorkflowDiagnosticClassification,
    pub(crate) error: LinkError,
    pub(crate) path: AstPath,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkflowLinkExpectedArgument {
    pub(crate) slot: crate::WorkflowSlotPath,
    pub(crate) ty: TypeExpr,
    pub(crate) path: AstPath,
}

#[derive(Debug, Default)]
struct ExpectedTypeFacts {
    by_expression: BTreeMap<AstPath, TypeExpr>,
}
