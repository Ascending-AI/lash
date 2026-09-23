/* Generated from schemas/host by npm run generate:types. Do not edit directly. */

export type WorkflowDeclaration =
  | {
      kind: 'type';
      name: string;
      ty: TypeExpr;
      [k: string]: unknown;
    }
  | {
      body: WorkflowSubgraph;
      description?: string | null;
      display_name: string;
      id: string;
      kind: 'process';
      name: string;
      name_source: WorkflowNodeNameSource;
      /**
       * Whether the process was declared or lifted from an inline literal.
       */
      origin?: ProcessOrigin;
      params?: ProcessParam[];
      return_ty?: TypeExpr | null;
      signals?: ProcessSignalDecl[];
    }
  | {
      body: Expr;
      kind: 'function';
      name: string;
      params?: FunctionParam[];
      return_ty: TypeExpr;
      [k: string]: unknown;
    };
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
      params: ProcessParamWire[];
    };
/**
 * @minItems 2
 */
export type UnionMembers = [TypeExpr, TypeExpr, ...TypeExpr[]];
export type WorkflowEdgeKind =
  | {
      kind: 'data_dependency';
      variable: string;
      version: number;
    }
  | {
      kind: 'sequence';
    };
/**
 * Closed vocabulary of executable workflow sites.
 *
 * This describes the site, not its current observation. A workflow node may expose more than one site kind. Declaration order is the canonical order: a node's execution sites sort by it, so reordering the variants changes the serialized workflow graph and needs a graph schema bump.
 */
export type ExecutionNodeKind =
  'resource_operation' | 'sleep' | 'wait' | 'terminal' | 'process_event' | 'branch' | 'loop' | 'call' | 'step';
export type WorkflowNodeKind =
  | {
      binding?: AssignTarget | null;
      expression: Expr;
      kind: 'data';
    }
  | {
      arguments?: WorkflowArgument[];
      binding?: AssignTarget | null;
      kind: 'call';
      operation: string;
      receiver: Expr;
      result_steps?: WorkflowResultStep[];
    }
  | {
      arguments?: WorkflowArgument[];
      binding?: AssignTarget | null;
      effect: WorkflowEffectKind;
      kind: 'effect';
      result_steps?: WorkflowResultStep[];
    }
  | {
      binding?: AssignTarget | null;
      expression: Expr;
      kind: 'computation';
    }
  | {
      expression: Expr;
      kind: 'state_update';
      target: AssignTarget;
    }
  | {
      expression: Expr;
      kind: 'terminal';
      terminal: WorkflowTerminalKind;
    }
  | {
      binding?: AssignTarget | null;
      condition: Expr;
      container_kind: 'if';
      else_graph: WorkflowSubgraph;
      /**
       * Whether the source's else branch is a block rather than a direct value or `else if`.
       */
      else_is_block: boolean;
      kind: 'container';
      then_graph: WorkflowSubgraph;
      /**
       * Whether the source's then branch is a statement block rather than a value expression.
       */
      then_is_block: boolean;
    }
  | {
      bind?: Expr | null;
      binding: string;
      body: WorkflowSubgraph;
      container_kind: 'for';
      iterable: Expr;
      kind: 'container';
    }
  | {
      body: WorkflowSubgraph;
      condition: Expr;
      container_kind: 'while';
      kind: 'container';
    }
  | {
      binding?: AssignTarget | null;
      clauses: WorkflowListComprehensionClause[];
      container_kind: 'list_comprehension';
      element: WorkflowSubgraph;
      kind: 'container';
    }
  | {
      kind: 'opaque';
      source: string;
    };
export type AssignPathStep =
  | {
      Field: string;
    }
  | {
      Index: Expr;
    };
export type Expr =
  | ('Null' | 'Break' | 'Continue')
  | {
      Block: Expr[];
    }
  | {
      LabelAnnotated: {
        expr: Expr;
        label: LabelMetadata;
        [k: string]: unknown;
      };
    }
  | 'Undefined'
  | {
      Bool: boolean;
    }
  | {
      Number: number;
    }
  | {
      String: string;
    }
  | {
      Variable: string;
    }
  | {
      Tuple: Expr[];
    }
  | {
      List: Expr[];
    }
  | {
      ListComprehension: {
        clauses: ListComprehensionClause[];
        element: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Record: [string, Expr][];
    }
  | {
      Assign: {
        expr: Expr;
        target: AssignTarget;
        [k: string]: unknown;
      };
    }
  | {
      If: {
        condition: Expr;
        else_block: Expr;
        then_block: Expr;
        [k: string]: unknown;
      };
    }
  | {
      For: {
        bind?: Expr | null;
        binding: string;
        body: Expr;
        iterable: Expr;
        [k: string]: unknown;
      };
    }
  | {
      While: {
        body: Expr;
        condition: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Role: {
        expr: Expr;
        role: StructuralRole;
        [k: string]: unknown;
      };
    }
  | {
      ProcessRef: {
        process: string;
        [k: string]: unknown;
      };
    }
  | {
      HostDescriptorConstructor: {
        input: Expr;
        type_name: string;
        [k: string]: unknown;
      };
    }
  | {
      ResourceRef: ResourceRefExpr;
    }
  | {
      ReceiverCall: {
        args: Expr[];
        operation: string;
        receiver: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Await: Expr;
    }
  | {
      SleepFor: Expr;
    }
  | {
      SleepUntil: Expr;
    }
  | {
      WaitSignal: {
        name: string;
        [k: string]: unknown;
      };
    }
  | {
      ResultUnwrap: Expr;
    }
  | {
      Print: Expr;
    }
  | {
      Yield: Expr;
    }
  | {
      Finish: Expr;
    }
  | {
      Fail: Expr;
    }
  | {
      BuiltinCall: {
        args: Expr[];
        name: string;
        [k: string]: unknown;
      };
    }
  | {
      Function: FunctionExpr;
    }
  | {
      ProcessLiteral: ProcessLiteralExpr;
    }
  | {
      Call: {
        args: Expr[];
        function: Expr;
        [k: string]: unknown;
      };
    }
  | {
      FunctionCall: {
        args: Expr[];
        function: string;
        [k: string]: unknown;
      };
    }
  | {
      Map: {
        function: Expr;
        items: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Try: TryExpr;
    }
  | {
      Throw: Expr;
    }
  | {
      Return: Expr;
    }
  | {
      Field: {
        field: string;
        target: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Index: {
        index: Expr;
        target: Expr;
        [k: string]: unknown;
      };
    }
  | {
      Unary: {
        expr: Expr;
        op: UnaryOp;
        [k: string]: unknown;
      };
    }
  | {
      Binary: {
        left: Expr;
        op: BinaryOp;
        right: Expr;
        [k: string]: unknown;
      };
    }
  | {
      JavaScriptUnary: {
        expr: Expr;
        op: JavaScriptUnaryOp;
        [k: string]: unknown;
      };
    }
  | {
      JavaScriptBinary: {
        left: Expr;
        op: JavaScriptBinaryOp;
        right: Expr;
        [k: string]: unknown;
      };
    }
  | {
      JavaScriptLogical: {
        left: Expr;
        op: JavaScriptLogicalOp;
        right: Expr;
        [k: string]: unknown;
      };
    }
  | {
      TypeLiteral: TypeExpr;
    };
export type ListComprehensionClause =
  | {
      For: {
        binding: string;
        iterable: Expr;
        [k: string]: unknown;
      };
    }
  | {
      If: {
        condition: Expr;
        [k: string]: unknown;
      };
    };
/**
 * The structural roles a front end marks its generated IR with.
 *
 * Each role is language-neutral: it names what a shape does, not the source construct that produced it, and each front end chooses which of its constructs lower to which role.
 */
export type StructuralRole =
  | {
      kind: 'scope';
    }
  | {
      kind: 'completion';
    }
  | {
      kind: 'attribute_assign';
    }
  | {
      kind: 'collection_transform';
      operation: string;
    }
  | {
      kind: 'process_wrapper';
    };
export type UnaryOp = 'Negate' | 'Not';
export type BinaryOp =
  | 'Add'
  | 'Subtract'
  | 'Multiply'
  | 'Divide'
  | 'Modulo'
  | 'Equal'
  | 'NotEqual'
  | 'Less'
  | 'LessEqual'
  | 'Greater'
  | 'GreaterEqual'
  | 'In'
  | 'And'
  | 'Or';
export type JavaScriptUnaryOp = 'Plus' | 'Negate' | 'Not' | 'TypeOf';
export type JavaScriptBinaryOp =
  | 'Add'
  | 'Subtract'
  | 'Multiply'
  | 'Divide'
  | 'Remainder'
  | 'StrictEqual'
  | 'StrictNotEqual'
  | 'LooseEqual'
  | 'LooseNotEqual'
  | 'Less'
  | 'LessEqual'
  | 'Greater'
  | 'GreaterEqual';
export type JavaScriptLogicalOp = 'And' | 'Or' | 'NullishCoalesce';
/**
 * One call or effect argument in graph order.
 *
 * Type facets address these values with a serialized [`WorkflowSlotPath`]. Its typed call, argument, field, and index segments cannot collide when a field contains punctuation. Nodes with several nested receiver calls add a call segment in depth-first IR walk order.
 */
export type WorkflowArgument =
  | {
      kind: 'positional';
      value: Expr;
    }
  | {
      fields: [string, Expr][];
      kind: 'named';
    };
/**
 * Ordered wrappers around a call or effect, from the operation outwards.
 */
export type WorkflowResultStep = 'await' | 'unwrap_result';
export type WorkflowEffectKind =
  'await_join' | 'wait_signal' | 'sleep_for' | 'sleep_until' | 'print' | 'yield' | 'break' | 'continue';
export type WorkflowTerminalKind = 'finish' | 'fail';
/**
 * One editable list-comprehension clause.
 */
export type WorkflowListComprehensionClause =
  | {
      binding: string;
      iterable: Expr;
      kind: 'for';
    }
  | {
      condition: Expr;
      kind: 'if';
    };
export type WorkflowNodeNameSource = 'label' | 'derived';
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
/**
 * The origin of a [`super::ProcessDecl`].
 */
export type ProcessOrigin =
  | {
      kind: 'declared';
    }
  | {
      hidden_params: number;
      kind: 'lifted';
      site: AstPath;
    };
/**
 * Which tree an [`AstPath`] walks down: `Program::main`, or one entry of `Program::declarations`.
 */
export type AstRoot =
  | 'main'
  | {
      declaration: number;
    };

/**
 * The single serializable graph document used for editing and run overlays.
 */
export interface WorkflowGraph {
  declarations?: WorkflowDeclaration[];
  facet_schema_version?: number | null;
  main: WorkflowSubgraph;
  schema_version: 15;
  /**
   * The definition identity of the admitted module artifact this graph projects ([`crate::ModuleArtifact::source_identity`]), which the module's traces carry too. A draft projected from source that has not been admitted claims no runtime identity and carries `None`. [`WORKFLOW_GRAPH_SCHEMA_VERSION`] identifies this document's wire shape, `facet_schema_version` identifies optional derived facts.
   */
  source_identity?: string | null;
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
export interface WorkflowSubgraph {
  edges?: WorkflowEdge[];
  nodes?: WorkflowNode[];
}
export interface WorkflowEdge {
  from: string;
  id: string;
  kind: WorkflowEdgeKind;
  to: string;
}
export interface WorkflowNode {
  /**
   * Identifiers visible before this node executes, in stable lexical order.
   */
  available_variables?: string[];
  description?: string | null;
  execution_sites?: WorkflowExecutionSite[];
  id: string;
  kind: WorkflowNodeKind;
  name: string;
  name_source: WorkflowNodeNameSource;
  outputs?: VariableVersion[];
  source_span?: Span | null;
  /**
   * Optional host-derived type information. It is never used to render source.
   */
  type_facets?: WorkflowNodeTypeFacets | null;
}
/**
 * Stable source-level location of one runtime site under a workflow node.
 */
export interface WorkflowExecutionSite {
  kind: ExecutionNodeKind;
  label: string;
  owner: string;
  path?: number[];
  [k: string]: unknown;
}
export interface AssignTarget {
  root: string;
  steps?: AssignPathStep[];
  [k: string]: unknown;
}
export interface LabelMetadata {
  description?: string | null;
  title: string;
  [k: string]: unknown;
}
export interface ResourceRefExpr {
  alias: string;
  path?: string[];
  resource_type: string;
  [k: string]: unknown;
}
export interface FunctionExpr {
  body: Expr;
  captures?: string[];
  name?: string | null;
  params?: string[];
  [k: string]: unknown;
}
/**
 * The authored shape of an inline process body, as a dialect lowers it.
 *
 * `params` carries the parameter names and their declared types, so a TypeScript arrow's annotations reach the lifted declaration's signature instead of widening to `Any`. `body` is the same wrapper a process literal run lowers to: the authored statements inside the process-failure wrapper, with the params passed through by name.
 */
export interface ProcessLiteralExpr {
  body: Expr;
  /**
   * Immutable, durably representable cell locals the body reads; each becomes a hidden start argument carrying the value the variable had when the process started (FIG-2998).
   */
  hidden_args?: ProcessParam[];
  params: ProcessParam[];
  [k: string]: unknown;
}
export interface ProcessParam {
  name: string;
  ty: TypeExpr;
  [k: string]: unknown;
}
export interface TryExpr {
  body: Expr;
  catch?: CatchClause | null;
  finally?: Expr | null;
  [k: string]: unknown;
}
export interface CatchClause {
  binding: string;
  body: Expr;
  [k: string]: unknown;
}
export interface VariableVersion {
  variable: string;
  version: number;
}
export interface Span {
  end: number;
  start: number;
  [k: string]: unknown;
}
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
export interface WorkflowTypeDiagnostic {
  class: WorkflowDiagnosticClass;
  kind: WorkflowDiagnosticKind;
  message: string;
  node_id: string;
  slot?: WorkflowSlotPathSegment[] | null;
  span?: Span | null;
  [k: string]: unknown;
}
export interface WorkflowExpectedArgument {
  slot: WorkflowSlotPathSegment[];
  ty: TypeExpr;
  [k: string]: unknown;
}
/**
 * A node's address in a `Program`: the root it hangs from plus the `Expr::children()` index chain that reaches it.
 */
export interface AstPath {
  root: AstRoot;
  steps?: number[];
  [k: string]: unknown;
}
export interface ProcessSignalDecl {
  name: string;
  ty: TypeExpr;
  [k: string]: unknown;
}
export interface FunctionParam {
  name: string;
  ty: TypeExpr;
  [k: string]: unknown;
}
