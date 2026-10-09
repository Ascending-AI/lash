use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{WorkflowNodeId, WorkflowOwnership, WorkflowSlotPath};
use crate::ast::{AstPath, TypeExpr};
use crate::linker::{LinkError, WorkflowLinkAnalysis};

/// Version of the optional, derived workflow type-facet contract. Version 4
/// (FIG-4038) drops `incompatible_binary_operands`: the retired surface
/// dialect's operand check has no ECMA-262 equivalent; v3 facet documents
/// are refused. The pre-1.0 freeze changes this shape in place.
///
/// version_guard(
///     shapes(cover(WorkflowNodeTypeFacets)),
///     roots(
///         path = "crates/lash-vm/src/ast.rs", ProcessSignature, ProcessTypeKind, ProcessTypeWire,
///         ProcessParam,
///     ),
/// )
/// version_surface = "migrate"
/// format_manifest = "WorkflowTypeFacet"
/// version_unguarded = "a derived projection facet: a reader strips it and regenerates it from its module rather than decoding it, so it has no range to read"
pub const WORKFLOW_TYPE_FACET_SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowNodeTypeFacets {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_variables: Vec<WorkflowTypedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_arguments: Vec<WorkflowExpectedArgument>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkflowTypeDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTypedVariable {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowExpectedArgument {
    /// The argument's expression, from the node's statement.
    pub slot: WorkflowSlotPath,
    pub ty: TypeExpr,
}

/// A semantic diagnostic at a node and optional argument slot. A source view
/// supplies text coordinates when a host shows the diagnostic in source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowTypeDiagnostic {
    pub node_id: WorkflowNodeId,
    pub kind: WorkflowDiagnosticKind,
    pub classification: WorkflowDiagnosticClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<WorkflowSlotPath>,
    pub message: String,
}

/// Whether a diagnostic establishes an admission failure for the analyzed
/// program and host environment or gives advice without establishing a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticClassification {
    Definite,
    Advisory,
}

/// Closed host-facing vocabulary for linker diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDiagnosticKind {
    DuplicateDeclaration,
    DuplicateProcessParam,
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
    ProcessLiteralOutsideProcessSlot,
    UnresolvedReceiver,
    UnknownResourceOperation,
    AmbiguousModuleOperation,
    BareToolCall,
    IncompatibleProcessArgument,
    FeatureDisabled,
    ProcessLifecycleOutsideProcess,
    OpaqueHostDescriptorAccess,
    UnknownObjectField,
    IncompatibleBuiltinOperands,
    IncompatibleIterationTarget,
    ModuleHash,
    InvalidAst,
}

impl WorkflowDiagnosticKind {
    pub const ALL: [Self; 33] = [
        Self::DuplicateDeclaration,
        Self::DuplicateProcessParam,
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
        Self::ProcessLiteralOutsideProcessSlot,
        Self::UnresolvedReceiver,
        Self::UnknownResourceOperation,
        Self::AmbiguousModuleOperation,
        Self::BareToolCall,
        Self::IncompatibleProcessArgument,
        Self::FeatureDisabled,
        Self::ProcessLifecycleOutsideProcess,
        Self::OpaqueHostDescriptorAccess,
        Self::UnknownObjectField,
        Self::IncompatibleBuiltinOperands,
        Self::IncompatibleIterationTarget,
        Self::ModuleHash,
        Self::InvalidAst,
    ];

    /// The classification of diagnostics produced for this kind.
    ///
    /// Every current kind reports a linker refusal. Advisory diagnostics do
    /// not establish an admission failure and have no current producer.
    pub fn classification(self) -> WorkflowDiagnosticClassification {
        match self {
            Self::DuplicateDeclaration
            | Self::DuplicateProcessParam
            | Self::UnknownProcess
            | Self::UnknownName
            | Self::UnknownBuiltin
            | Self::UnknownResource
            | Self::UnknownType
            | Self::IncompatibleConstructorInput
            | Self::IncompatibleOperationInput
            | Self::AwaitedSettledExpression
            | Self::IncompatibleExpectedLiteral
            | Self::IncompatibleProcessReturn
            | Self::IncompatibleFunctionReturn
            | Self::DuplicateFunctionParam
            | Self::FunctionArgumentCount
            | Self::IncompatibleFunctionArgument
            | Self::ForbiddenInFunction
            | Self::FunctionNameIsNotAValue
            | Self::FunctionShadowsBuiltin
            | Self::ProcessLiteralOutsideProcessSlot
            | Self::UnresolvedReceiver
            | Self::UnknownResourceOperation
            | Self::AmbiguousModuleOperation
            | Self::BareToolCall
            | Self::IncompatibleProcessArgument
            | Self::FeatureDisabled
            | Self::ProcessLifecycleOutsideProcess
            | Self::OpaqueHostDescriptorAccess
            | Self::UnknownObjectField
            | Self::IncompatibleBuiltinOperands
            | Self::IncompatibleIterationTarget
            | Self::ModuleHash
            | Self::InvalidAst => WorkflowDiagnosticClassification::Definite,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateDeclaration => "duplicate_declaration",
            Self::DuplicateProcessParam => "duplicate_process_param",
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
            Self::ProcessLiteralOutsideProcessSlot => "process_literal_outside_process_slot",
            Self::UnresolvedReceiver => "unresolved_receiver",
            Self::UnknownResourceOperation => "unknown_resource_operation",
            Self::AmbiguousModuleOperation => "ambiguous_module_operation",
            Self::BareToolCall => "bare_tool_call",
            Self::IncompatibleProcessArgument => "incompatible_process_argument",
            Self::FeatureDisabled => "feature_disabled",
            Self::ProcessLifecycleOutsideProcess => "process_lifecycle_outside_process",
            Self::OpaqueHostDescriptorAccess => "opaque_host_descriptor_access",
            Self::UnknownObjectField => "unknown_object_field",
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
            LinkError::ProcessLiteralOutsideProcessSlot { .. } => {
                Self::ProcessLiteralOutsideProcessSlot
            }
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
/// `lash_vm` while the projector that calls it lives in `lash-typescript`.
/// An argument is addressed as every other expression of the node is: by the
/// slot path `ownership` gives it from the node's statement.
pub fn projected_node_type_facets(
    analysis: Option<&WorkflowLinkAnalysis>,
    path: &AstPath,
    ownership: &WorkflowOwnership,
    available_variables: &[String],
    id: &WorkflowNodeId,
) -> Option<WorkflowNodeTypeFacets> {
    let facts = analysis?.facts_for(path)?;
    let slot = |argument: &AstPath| {
        ownership
            .site_for_ast(argument)
            .map(|(_, slots)| WorkflowSlotPath::new(slots.iter().copied()))
    };
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
            .filter_map(|argument| {
                Some(WorkflowExpectedArgument {
                    slot: slot(&argument.path)?,
                    ty: argument.ty.clone(),
                })
            })
            .collect(),
        diagnostics: facts
            .diagnostics
            .iter()
            .map(|diagnostic| WorkflowTypeDiagnostic {
                node_id: id.clone(),
                kind: WorkflowDiagnosticKind::from_link_error(&diagnostic.error),
                classification: diagnostic.classification,
                slot: facts
                    .expected_arguments
                    .iter()
                    .find(|argument| argument.path == diagnostic.path)
                    .and_then(|argument| slot(&argument.path)),
                message: diagnostic.error.to_string(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
