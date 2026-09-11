use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::artifact::{
    HostRequirements, ModuleArtifact, host_requirements_for_program_with_catalog,
};
use crate::ast::{
    AssignPathStep, AstString, Declaration, Expr, ListComprehensionClause, ProcessDecl,
    ProcessParam, Program, ResourceRefExpr, TypeExpr, TypeField, format_type_expr,
};
use crate::lexer::Span;

mod catalog;
pub use catalog::LashlangHostCatalog;
mod host;
use host::module_path_key;
pub use host::{
    LashlangAbilities, LashlangHostCatalogError, LashlangHostEnvironment, LashlangLanguageFeatures,
    LinkedModule, ModuleInstanceCatalog, ModuleOperationBinding, NamedDataType, NamedDataTypeError,
    OutputFromInputBinding, ResolvedOperation, ResourceOperationBinding, ResourceTypeCatalog,
    TriggerSourceBinding, ValueConstructorBinding,
};
mod errors;
pub use errors::LinkError;
mod pass_setup;
use pass_setup::{Binding, Linker, function_signature};
mod lower_expr;
mod pass_validation;
use pass_validation::{
    StaticTriggerBinding, TriggerKeyCollector, materialize_default_trigger_keys,
    semantic_trigger_source_key, validate_trigger_operation_subscription_key,
};
mod type_helpers;
use type_helpers::{
    Completion, Scope, any_binding, binary_op_source, binary_operands_compatible,
    binary_return_type, binding_type, call_input_type, direct_call_input_field,
    expected_call_arg_type, expr_has_label_annotation, field_type, index_type,
    is_trigger_event_expr, is_trigger_event_projection_expr, iterable_item_type,
    label_annotation_path, literal_type, membership_key_type, module_path_for_expr,
    process_input_record_type, process_input_type, process_type_for_decl,
    shaping_builtin_return_type, shaping_comparable_type, shaping_list_item, shaping_number_type,
    shaping_record_type, shaping_text_type, strip_label_annotation, trigger_target_process_label,
    union_type,
};
mod facets;
pub(crate) use facets::analyze_workflow_program;
use facets::{expression_spans_by_pointer, recover_workflow_binding};
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowLinkAnalysis {
    nodes: BTreeMap<usize, WorkflowLinkNodeFacts>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowLinkNodeFacts {
    pub(crate) available_variables: BTreeMap<String, TypeExpr>,
    pub(crate) expected_arguments: Vec<WorkflowLinkExpectedArgument>,
    pub(crate) diagnostics: Vec<LinkError>,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkflowLinkExpectedArgument {
    pub(crate) slot: String,
    pub(crate) ty: TypeExpr,
}

#[derive(Debug, Default)]
struct ExpectedTypeFacts {
    by_expression: BTreeMap<usize, TypeExpr>,
}

#[cfg(test)]
use pass_validation::semantic_trigger_subscription_key;
