/* Generated from schemas/host by npm run generate:types. Do not edit directly. */

/**
 * A serialized value-type expression.
 *
 * Host decoders must refuse unknown variants. `TypeExpr` is decoded only after its graph or facet carrier version is accepted; adding a variant therefore requires the owning carrier version to advance.
 */
export type TypeExpr =
  | ('Any' | 'Str' | 'Int' | 'Float' | 'Bool' | 'Dict')
  | 'Null'
  | {
      Enum: string[];
    }
  | {
      List: TypeExpr;
    }
  | {
      Object: TypeField[];
    }
  | {
      Ref: string;
    }
  | {
      Process: ProcessType;
    }
  | {
      TriggerHandle: TypeExpr;
    }
  | {
      Union: UnionMembers;
    };
export type ProcessType =
  | {
      kind: 'unknown';
    }
  | {
      kind: 'known';
      output: TypeExpr;
      params: ProcessParam[];
    };
export type UnionMembers = TypeExpr[];
/**
 * Whether a diagnostic blocks save under ADR 0073's gradual typing rule.
 */
export type WorkflowDiagnosticClass = 'definite' | 'advisory';
/**
 * Closed host-facing vocabulary for linker diagnostics.
 */
export type WorkflowDiagnosticKind =
  | 'duplicate_declaration'
  | 'duplicate_process_param'
  | 'duplicate_process_signal'
  | 'unknown_process'
  | 'unknown_name'
  | 'unknown_builtin'
  | 'unknown_resource'
  | 'unknown_type'
  | 'incompatible_constructor_input'
  | 'incompatible_operation_input'
  | 'awaited_settled_expression'
  | 'incompatible_expected_literal'
  | 'incompatible_process_return'
  | 'incompatible_function_return'
  | 'duplicate_function_param'
  | 'function_argument_count'
  | 'incompatible_function_argument'
  | 'forbidden_in_function'
  | 'function_name_is_not_a_value'
  | 'function_shadows_builtin'
  | 'invalid_trigger_registration'
  | 'invalid_trigger_subscription_key'
  | 'process_literal_outside_process_slot'
  | 'conflicting_signal_payload'
  | 'invalid_trigger_inputs'
  | 'duplicate_trigger_input'
  | 'missing_trigger_input'
  | 'unknown_trigger_input'
  | 'missing_trigger_event_input'
  | 'trigger_target_takes_no_event'
  | 'ambiguous_omitted_trigger_inputs'
  | 'trigger_event_outside_inputs'
  | 'trigger_event_projection'
  | 'invalid_trigger_list'
  | 'unknown_trigger_event_type'
  | 'invalid_trigger_target'
  | 'trigger_event_mismatch'
  | 'unresolved_receiver'
  | 'unknown_resource_operation'
  | 'ambiguous_module_operation'
  | 'bare_tool_call'
  | 'incompatible_process_argument'
  | 'feature_disabled'
  | 'process_lifecycle_outside_process'
  | 'opaque_host_descriptor_access'
  | 'unknown_object_field'
  | 'incompatible_binary_operands'
  | 'incompatible_builtin_operands'
  | 'incompatible_iteration_target'
  | 'module_hash'
  | 'invalid_ast';
/**
 * One structural step in a [`WorkflowSlotPath`].
 */
export type WorkflowSlotPathSegment =
  | {
      call: number;
    }
  | {
      arg: number;
    }
  | {
      field: string;
    }
  | {
      index: number;
    };

export interface WorkflowNodeTypeFacets {
  available_variables?: WorkflowTypedVariable[];
  diagnostics?: WorkflowTypeDiagnostic[];
  expected_arguments?: WorkflowExpectedArgument[];
  [k: string]: unknown;
}
export interface WorkflowTypedVariable {
  name: string;
  ty: TypeExpr;
  [k: string]: unknown;
}
export interface TypeField {
  name: string;
  optional: boolean;
  ty: TypeExpr;
}
export interface ProcessParam {
  name: string;
  ty: TypeExpr;
}
export interface WorkflowTypeDiagnostic {
  class: WorkflowDiagnosticClass;
  kind: WorkflowDiagnosticKind;
  message: string;
  node_id: string;
  slot?: WorkflowSlotPathSegment[] | null;
  span?: Span | null;
  [k: string]: unknown;
}
export interface Span {
  end: number;
  start: number;
}
export interface WorkflowExpectedArgument {
  slot: WorkflowSlotPathSegment[];
  ty: TypeExpr;
  [k: string]: unknown;
}
