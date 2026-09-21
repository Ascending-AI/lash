use serde::{Deserialize, Serialize};

use super::{Span, WorkflowNodeId};
use crate::ast::{AstPath, TypeExpr};
use crate::linker::{LinkError, WorkflowLinkAnalysis};

/// Version of the optional, derived workflow type-facet contract.
pub const WORKFLOW_TYPE_FACET_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeTypeFacets {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_variables: Vec<WorkflowTypedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_arguments: Vec<WorkflowExpectedArgument>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkflowTypeDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTypedVariable {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowExpectedArgument {
    pub slot: String,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTypeDiagnostic {
    pub node_id: WorkflowNodeId,
    pub kind: WorkflowDiagnosticKind,
    pub class: WorkflowDiagnosticClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

/// Whether a diagnostic blocks save under ADR 0073's gradual typing rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticClass {
    Definite,
    Advisory,
}

/// Closed host-facing vocabulary for linker diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticKind {
    DuplicateDeclaration,
    DuplicateProcessParam,
    DuplicateProcessSignal,
    UnknownProcess,
    UnknownName,
    UnknownBuiltin,
    UnknownResource,
    UnknownType,
    IncompatibleConstructorInput,
    IncompatibleOperationInput,
    AwaitedSettledExpression,
    IncompatibleExpectedLiteral,
    IncompatibleProcessReturn,
    IncompatibleFunctionReturn,
    DuplicateFunctionParam,
    FunctionArgumentCount,
    IncompatibleFunctionArgument,
    ForbiddenInFunction,
    FunctionNameIsNotAValue,
    FunctionShadowsBuiltin,
    InvalidTriggerRegistration,
    InvalidTriggerSubscriptionKey,
    ProcessLiteralOutsideProcessSlot,
    ConflictingSignalPayload,
    InvalidTriggerInputs,
    DuplicateTriggerInput,
    MissingTriggerInput,
    UnknownTriggerInput,
    MissingTriggerEventInput,
    TriggerTargetTakesNoEvent,
    AmbiguousOmittedTriggerInputs,
    TriggerEventOutsideInputs,
    TriggerEventProjection,
    InvalidTriggerList,
    UnknownTriggerEventType,
    InvalidTriggerTarget,
    TriggerEventMismatch,
    UnresolvedReceiver,
    UnknownResourceOperation,
    AmbiguousModuleOperation,
    BareToolCall,
    IncompatibleProcessArgument,
    FeatureDisabled,
    ProcessLifecycleOutsideProcess,
    OpaqueHostDescriptorAccess,
    UnknownObjectField,
    IncompatibleBinaryOperands,
    IncompatibleBuiltinOperands,
    IncompatibleIterationTarget,
    ModuleHash,
    InvalidAst,
}

impl WorkflowDiagnosticKind {
    pub const ALL: [Self; 51] = [
        Self::DuplicateDeclaration,
        Self::DuplicateProcessParam,
        Self::DuplicateProcessSignal,
        Self::UnknownProcess,
        Self::UnknownName,
        Self::UnknownBuiltin,
        Self::UnknownResource,
        Self::UnknownType,
        Self::IncompatibleConstructorInput,
        Self::IncompatibleOperationInput,
        Self::AwaitedSettledExpression,
        Self::IncompatibleExpectedLiteral,
        Self::IncompatibleProcessReturn,
        Self::IncompatibleFunctionReturn,
        Self::DuplicateFunctionParam,
        Self::FunctionArgumentCount,
        Self::IncompatibleFunctionArgument,
        Self::ForbiddenInFunction,
        Self::FunctionNameIsNotAValue,
        Self::FunctionShadowsBuiltin,
        Self::InvalidTriggerRegistration,
        Self::InvalidTriggerSubscriptionKey,
        Self::ProcessLiteralOutsideProcessSlot,
        Self::ConflictingSignalPayload,
        Self::InvalidTriggerInputs,
        Self::DuplicateTriggerInput,
        Self::MissingTriggerInput,
        Self::UnknownTriggerInput,
        Self::MissingTriggerEventInput,
        Self::TriggerTargetTakesNoEvent,
        Self::AmbiguousOmittedTriggerInputs,
        Self::TriggerEventOutsideInputs,
        Self::TriggerEventProjection,
        Self::InvalidTriggerList,
        Self::UnknownTriggerEventType,
        Self::InvalidTriggerTarget,
        Self::TriggerEventMismatch,
        Self::UnresolvedReceiver,
        Self::UnknownResourceOperation,
        Self::AmbiguousModuleOperation,
        Self::BareToolCall,
        Self::IncompatibleProcessArgument,
        Self::FeatureDisabled,
        Self::ProcessLifecycleOutsideProcess,
        Self::OpaqueHostDescriptorAccess,
        Self::UnknownObjectField,
        Self::IncompatibleBinaryOperands,
        Self::IncompatibleBuiltinOperands,
        Self::IncompatibleIterationTarget,
        Self::ModuleHash,
        Self::InvalidAst,
    ];

    pub fn class(self) -> WorkflowDiagnosticClass {
        WorkflowDiagnosticClass::Definite
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateDeclaration => "duplicate_declaration",
            Self::DuplicateProcessParam => "duplicate_process_param",
            Self::DuplicateProcessSignal => "duplicate_process_signal",
            Self::UnknownProcess => "unknown_process",
            Self::UnknownName => "unknown_name",
            Self::UnknownBuiltin => "unknown_builtin",
            Self::UnknownResource => "unknown_resource",
            Self::UnknownType => "unknown_type",
            Self::IncompatibleConstructorInput => "incompatible_constructor_input",
            Self::IncompatibleOperationInput => "incompatible_operation_input",
            Self::AwaitedSettledExpression => "awaited_settled_expression",
            Self::IncompatibleExpectedLiteral => "incompatible_expected_literal",
            Self::IncompatibleProcessReturn => "incompatible_process_return",
            Self::IncompatibleFunctionReturn => "incompatible_function_return",
            Self::DuplicateFunctionParam => "duplicate_function_param",
            Self::FunctionArgumentCount => "function_argument_count",
            Self::IncompatibleFunctionArgument => "incompatible_function_argument",
            Self::ForbiddenInFunction => "forbidden_in_function",
            Self::FunctionNameIsNotAValue => "function_name_is_not_a_value",
            Self::FunctionShadowsBuiltin => "function_shadows_builtin",
            Self::InvalidTriggerRegistration => "invalid_trigger_registration",
            Self::InvalidTriggerSubscriptionKey => "invalid_trigger_subscription_key",
            Self::ProcessLiteralOutsideProcessSlot => "process_literal_outside_process_slot",
            Self::ConflictingSignalPayload => "conflicting_signal_payload",
            Self::InvalidTriggerInputs => "invalid_trigger_inputs",
            Self::DuplicateTriggerInput => "duplicate_trigger_input",
            Self::MissingTriggerInput => "missing_trigger_input",
            Self::UnknownTriggerInput => "unknown_trigger_input",
            Self::MissingTriggerEventInput => "missing_trigger_event_input",
            Self::TriggerTargetTakesNoEvent => "trigger_target_takes_no_event",
            Self::AmbiguousOmittedTriggerInputs => "ambiguous_omitted_trigger_inputs",
            Self::TriggerEventOutsideInputs => "trigger_event_outside_inputs",
            Self::TriggerEventProjection => "trigger_event_projection",
            Self::InvalidTriggerList => "invalid_trigger_list",
            Self::UnknownTriggerEventType => "unknown_trigger_event_type",
            Self::InvalidTriggerTarget => "invalid_trigger_target",
            Self::TriggerEventMismatch => "trigger_event_mismatch",
            Self::UnresolvedReceiver => "unresolved_receiver",
            Self::UnknownResourceOperation => "unknown_resource_operation",
            Self::AmbiguousModuleOperation => "ambiguous_module_operation",
            Self::BareToolCall => "bare_tool_call",
            Self::IncompatibleProcessArgument => "incompatible_process_argument",
            Self::FeatureDisabled => "feature_disabled",
            Self::ProcessLifecycleOutsideProcess => "process_lifecycle_outside_process",
            Self::OpaqueHostDescriptorAccess => "opaque_host_descriptor_access",
            Self::UnknownObjectField => "unknown_object_field",
            Self::IncompatibleBinaryOperands => "incompatible_binary_operands",
            Self::IncompatibleBuiltinOperands => "incompatible_builtin_operands",
            Self::IncompatibleIterationTarget => "incompatible_iteration_target",
            Self::ModuleHash => "module_hash",
            Self::InvalidAst => "invalid_ast",
        }
    }

    pub(crate) fn from_link_error(error: &LinkError) -> Self {
        match error {
            LinkError::DuplicateDeclaration { .. } => Self::DuplicateDeclaration,
            LinkError::DuplicateProcessParam { .. } => Self::DuplicateProcessParam,
            LinkError::DuplicateProcessSignal { .. } => Self::DuplicateProcessSignal,
            LinkError::UnknownProcess { .. } => Self::UnknownProcess,
            LinkError::UnknownName { .. } => Self::UnknownName,
            LinkError::UnknownBuiltin { .. } => Self::UnknownBuiltin,
            LinkError::UnknownResource { .. } => Self::UnknownResource,
            LinkError::UnknownType { .. } => Self::UnknownType,
            LinkError::IncompatibleConstructorInput { .. } => Self::IncompatibleConstructorInput,
            LinkError::IncompatibleOperationInput { .. } => Self::IncompatibleOperationInput,
            LinkError::AwaitedSettledExpression { .. } => Self::AwaitedSettledExpression,
            LinkError::IncompatibleExpectedLiteral { .. } => Self::IncompatibleExpectedLiteral,
            LinkError::IncompatibleProcessReturn { .. } => Self::IncompatibleProcessReturn,
            LinkError::IncompatibleFunctionReturn { .. } => Self::IncompatibleFunctionReturn,
            LinkError::DuplicateFunctionParam { .. } => Self::DuplicateFunctionParam,
            LinkError::FunctionArgumentCount { .. } => Self::FunctionArgumentCount,
            LinkError::IncompatibleFunctionArgument { .. } => Self::IncompatibleFunctionArgument,
            LinkError::ForbiddenInFunction { .. } => Self::ForbiddenInFunction,
            LinkError::FunctionNameIsNotAValue { .. } => Self::FunctionNameIsNotAValue,
            LinkError::FunctionShadowsBuiltin { .. } => Self::FunctionShadowsBuiltin,
            LinkError::InvalidTriggerRegistration { .. } => Self::InvalidTriggerRegistration,
            LinkError::InvalidTriggerSubscriptionKey { .. } => Self::InvalidTriggerSubscriptionKey,
            LinkError::ProcessLiteralOutsideProcessSlot { .. } => {
                Self::ProcessLiteralOutsideProcessSlot
            }
            LinkError::ConflictingSignalPayload { .. } => Self::ConflictingSignalPayload,
            LinkError::InvalidTriggerInputs { .. } => Self::InvalidTriggerInputs,
            LinkError::DuplicateTriggerInput { .. } => Self::DuplicateTriggerInput,
            LinkError::MissingTriggerInput { .. } => Self::MissingTriggerInput,
            LinkError::UnknownTriggerInput { .. } => Self::UnknownTriggerInput,
            LinkError::MissingTriggerEventInput { .. } => Self::MissingTriggerEventInput,
            LinkError::TriggerTargetTakesNoEvent { .. } => Self::TriggerTargetTakesNoEvent,
            LinkError::AmbiguousOmittedTriggerInputs { .. } => Self::AmbiguousOmittedTriggerInputs,
            LinkError::TriggerEventOutsideInputs { .. } => Self::TriggerEventOutsideInputs,
            LinkError::TriggerEventProjection { .. } => Self::TriggerEventProjection,
            LinkError::InvalidTriggerList { .. } => Self::InvalidTriggerList,
            LinkError::UnknownTriggerEventType { .. } => Self::UnknownTriggerEventType,
            LinkError::InvalidTriggerTarget { .. } => Self::InvalidTriggerTarget,
            LinkError::TriggerEventMismatch { .. } => Self::TriggerEventMismatch,
            LinkError::UnresolvedReceiver { .. } => Self::UnresolvedReceiver,
            LinkError::UnknownResourceOperation { .. } => Self::UnknownResourceOperation,
            LinkError::AmbiguousModuleOperation { .. } => Self::AmbiguousModuleOperation,
            LinkError::BareToolCall { .. } => Self::BareToolCall,
            LinkError::IncompatibleProcessArgument { .. } => Self::IncompatibleProcessArgument,
            LinkError::FeatureDisabled { .. } => Self::FeatureDisabled,
            LinkError::ProcessLifecycleOutsideProcess { .. } => {
                Self::ProcessLifecycleOutsideProcess
            }
            LinkError::OpaqueHostDescriptorAccess { .. } => Self::OpaqueHostDescriptorAccess,
            LinkError::UnknownObjectField { .. } => Self::UnknownObjectField,
            LinkError::IncompatibleBinaryOperands { .. } => Self::IncompatibleBinaryOperands,
            LinkError::IncompatibleBuiltinOperands { .. } => Self::IncompatibleBuiltinOperands,
            LinkError::IncompatibleIterationTarget { .. } => Self::IncompatibleIterationTarget,
            LinkError::ModuleHash { .. } => Self::ModuleHash,
            LinkError::InvalidAst { .. } => Self::InvalidAst,
        }
    }
}

/// Returns whether `value_type` may fill a slot of `expected_slot_type`.
///
/// Direction is explicit: value is the source and slot is the target. `Any`
/// is consistent in either position, as required by ADR 0073.
pub fn workflow_slot_accepts_value(value_type: &TypeExpr, expected_slot_type: &TypeExpr) -> bool {
    crate::is_resolved_type_assignable(value_type, expected_slot_type)
}

/// Derive one node's optional, non-authoritative type facets from a link
/// analysis.
///
/// The facets describe types, never syntax, so the derivation stays in
/// `lashlang` while the projector that calls it lives in `lash-typescript`.
pub fn projected_node_type_facets(
    analysis: Option<&WorkflowLinkAnalysis>,
    path: &AstPath,
    available_variables: &[String],
    id: &WorkflowNodeId,
) -> Option<WorkflowNodeTypeFacets> {
    let facts = analysis?.facts_for(path)?;
    Some(WorkflowNodeTypeFacets {
        available_variables: available_variables
            .iter()
            .map(|name| WorkflowTypedVariable {
                name: name.clone(),
                ty: facts
                    .available_variables
                    .get(name)
                    .cloned()
                    .unwrap_or(TypeExpr::Any),
            })
            .collect(),
        expected_arguments: facts
            .expected_arguments
            .iter()
            .map(|argument| WorkflowExpectedArgument {
                slot: argument.slot.clone(),
                ty: argument.ty.clone(),
            })
            .collect(),
        diagnostics: facts
            .diagnostics
            .iter()
            .map(|diagnostic| WorkflowTypeDiagnostic {
                node_id: id.clone(),
                kind: WorkflowDiagnosticKind::from_link_error(&diagnostic.error),
                class: WorkflowDiagnosticKind::from_link_error(&diagnostic.error).class(),
                slot: diagnostic_slot(
                    &diagnostic.error,
                    &diagnostic.path,
                    &facts.expected_arguments,
                ),
                message: diagnostic.error.to_string(),
                span: diagnostic.span,
            })
            .collect(),
    })
}

fn diagnostic_slot(
    error: &LinkError,
    error_path: &AstPath,
    arguments: &[crate::linker::WorkflowLinkExpectedArgument],
) -> Option<String> {
    if let Some(argument) = arguments
        .iter()
        .find(|argument| argument.path == *error_path)
    {
        return Some(argument.slot.clone());
    }
    let named = match error {
        LinkError::IncompatibleProcessArgument { arg, .. } => Some(arg.as_ref()),
        LinkError::UnknownObjectField { field, .. } => Some(field.as_str()),
        _ => None,
    };
    if let Some(name) = named
        && let Some(argument) = arguments.iter().find(|argument| {
            argument.slot.ends_with(&format!(".{name}"))
                || argument.slot.ends_with(&format!("[{name}]"))
        })
    {
        return Some(argument.slot.clone());
    }
    matches!(
        error,
        LinkError::IncompatibleConstructorInput { .. }
            | LinkError::IncompatibleOperationInput { .. }
            | LinkError::IncompatibleExpectedLiteral { .. }
            | LinkError::ProcessLiteralOutsideProcessSlot { .. }
    )
    .then(|| arguments.first().map(|argument| argument.slot.clone()))
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_kind_vocabulary_is_closed_over_current_link_errors() {
        assert_eq!(WorkflowDiagnosticKind::ALL.len(), 51);
        let spellings = WorkflowDiagnosticKind::ALL
            .into_iter()
            .map(|kind| kind.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(spellings.len(), 51);
        assert!(
            WorkflowDiagnosticKind::ALL
                .into_iter()
                .all(|kind| kind.class() == WorkflowDiagnosticClass::Definite)
        );
    }

    #[test]
    fn slot_fill_check_has_explicit_value_to_slot_direction_and_consistent_any() {
        assert!(workflow_slot_accepts_value(
            &TypeExpr::Int,
            &TypeExpr::Float
        ));
        assert!(!workflow_slot_accepts_value(
            &TypeExpr::Float,
            &TypeExpr::Int
        ));
        assert!(workflow_slot_accepts_value(&TypeExpr::Any, &TypeExpr::Str));
        assert!(workflow_slot_accepts_value(&TypeExpr::Str, &TypeExpr::Any));
    }
}
