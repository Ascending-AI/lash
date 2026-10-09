/* Generated from schemas/host by npm run generate:types. Do not edit directly. */

/**
 * A serialized value-type expression.
 *
 * Host decoders must refuse unknown variants. `TypeExpr` is decoded only
 * after its graph or facet carrier version is accepted; adding a variant
 * therefore requires the owning carrier version to advance.
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
      Union: UnionMembers;
    };
export type ProcessType =
  | {
      kind: 'unknown';
    }
  | {
      kind: 'known';
      output: TypeExpr;
      params: ProcessParamWire[];
    };
/**
 * @minItems 2
 */
export type UnionMembers = [TypeExpr, TypeExpr, ...TypeExpr[]];
/**
 * Whether a diagnostic establishes an admission failure for the analyzed
 * program and host environment or gives advice without establishing a failure.
 */
export type WorkflowDiagnosticClassification = 'definite' | 'advisory';
/**
 * Closed host-facing vocabulary for linker diagnostics.
 */
export type WorkflowDiagnosticKind =
  | 'duplicate_declaration'
  | 'duplicate_process_param'
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
  | 'process_literal_outside_process_slot'
  | 'unresolved_receiver'
  | 'unknown_resource_operation'
  | 'ambiguous_module_operation'
  | 'bare_tool_call'
  | 'incompatible_process_argument'
  | 'feature_disabled'
  | 'process_lifecycle_outside_process'
  | 'opaque_host_descriptor_access'
  | 'unknown_object_field'
  | 'incompatible_builtin_operands'
  | 'incompatible_iteration_target'
  | 'module_hash'
  | 'invalid_ast';
/**
 * A deterministic node identifier of a workflow document, minted from the
 * node's structural owner and AST path. It is never empty.
 */
export type WorkflowNodeId = string;
/**
 * The role one child expression plays in its parent expression of the
 * shared workflow IR.
 */
export type ExprSlot =
  | (
      | 'condition'
      | 'then'
      | 'else'
      | 'iterable'
      | 'receiver'
      | 'callee'
      | 'this'
      | 'catch'
      | 'finally'
      | 'target'
      | 'index'
      | 'left'
      | 'right'
    )
  | {
      item: number;
    }
  | {
      entry: number;
    }
  | 'inner'
  | {
      assign_index: number;
    }
  | 'value'
  | 'bind'
  | 'body'
  | 'input'
  | {
      arg: number;
    }
  | 'operand'
  | 'method_key'
  | 'items'
  | 'function';
/**
 * The typed path from a workflow node's statement to one expression inside
 * it: the child slot taken at each step. The empty path is the statement
 * itself.
 *
 * The serialized list is authoritative. [`Display`](std::fmt::Display) is a
 * derived spelling for text-only host contracts.
 */
export type WorkflowSlotPath = ExprSlot[];

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
export interface ProcessParamWire {
  name: string;
  ty: TypeExpr;
}
/**
 * A semantic diagnostic at a node and optional argument slot. A source view
 * supplies text coordinates when a host shows the diagnostic in source.
 */
export interface WorkflowTypeDiagnostic {
  classification: WorkflowDiagnosticClassification;
  kind: WorkflowDiagnosticKind;
  message: string;
  node_id: WorkflowNodeId;
  slot?: WorkflowSlotPath | null;
  [k: string]: unknown;
}
export interface WorkflowExpectedArgument {
  /**
   * The argument's expression, from the node's statement.
   */
  slot: WorkflowSlotPath;
  ty: TypeExpr;
  [k: string]: unknown;
}
