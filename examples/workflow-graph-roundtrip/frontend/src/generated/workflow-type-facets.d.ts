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
 * A deterministic node identifier minted from structural owner and AST path.
 */
export type WorkflowNodeId = string;
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
    }
  | {
      expr: ExprSlot;
    };
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
 * An unambiguous address for one expression inside a workflow node.
 *
 * Two spellings share the type. A *structural* path is made only of
 * [`WorkflowSlotPathSegment::Expr`] segments and walks the typed child slots
 * of the node's statement ([`super::workflow_node_statement`]), so it reaches
 * every expression role of every IR variant; the empty path is the statement
 * itself. A *call-argument* path starts at a receiver call's argument
 * (`call`, `arg`, then record fields and list indexes) and is what type
 * facets name their expected arguments by.
 *
 * The serialized list is authoritative. [`Display`](std::fmt::Display) is a
 * derived spelling for text-only host contracts; field names use JSON string
 * quoting so they cannot collide with structural indexes or separators.
 */
export type WorkflowSlotPath = WorkflowSlotPathSegment[];

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
export interface WorkflowTypeDiagnostic {
  classification: WorkflowDiagnosticClassification;
  kind: WorkflowDiagnosticKind;
  message: string;
  node_id: WorkflowNodeId;
  slot?: WorkflowSlotPath | null;
  span?: Span | null;
  [k: string]: unknown;
}
export interface Span {
  end: number;
  start: number;
  [k: string]: unknown;
}
export interface WorkflowExpectedArgument {
  slot: WorkflowSlotPath;
  ty: TypeExpr;
  [k: string]: unknown;
}
