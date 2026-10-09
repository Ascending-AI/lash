/* Generated from schemas/host by npm run generate:types. Do not edit directly. */

export type WorkflowDeclaration =
  | {
      body: WorkflowSubgraph;
      description?: string | null;
      display_name: string;
      id: WorkflowNodeId;
      kind: 'process';
      name: string;
      name_source: WorkflowNodeNameSource;
      /**
       * Whether the process was declared or lifted from an inline literal.
       */
      origin?: ProcessOrigin;
      params?: ProcessParam[];
      return_ty?: TypeExpr | null;
      /**
       * The failure wrapper around the authored run body, when the process
       * body has one ([`crate::StructuralRole::ProcessWrapper`]). `body` is
       * then the run function's body; without a wrapper it is the whole
       * process body.
       */
      wrapper?: WorkflowProcessWrapper | null;
    }
  | (FunctionDecl & {
      kind: 'function';
      [k: string]: unknown;
    });
/**
 * A deterministic node identifier minted from structural owner and AST path.
 */
export type WorkflowNodeId = string;
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
 * The IR spelling of a body whose statements are a subgraph's nodes.
 */
export type WorkflowBodyForm =
  | {
      form: 'block';
      groups?: WorkflowCompletionGroup[];
    }
  | {
      form: 'completion';
      groups?: WorkflowCompletionGroup[];
      value: Expr;
    }
  | {
      form: 'statement';
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
  | 'Absent'
  | {
      Bool: boolean;
    }
  | {
      Number: IrNumber;
    }
  | {
      String: string;
    }
  | {
      Variable: string;
    }
  | {
      List: Expr[];
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
        /**
         * The authored name of a renamed lexical element binding. Display
         * metadata only; `binding` and `bind` retain their linker identities.
         */
        authored_binding?: string | null;
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
      ResultUnwrap: Expr;
    }
  | {
      Print: Expr;
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
      MethodCall: {
        args: Expr[];
        method: MethodKey;
        receiver: Expr;
        [k: string]: unknown;
      };
    }
  | {
      ThisCall: {
        args: Expr[];
        function: Expr;
        this: Expr;
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
      FunctionReturn: Expr;
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
      CoercingUnary: {
        expr: Expr;
        op: CoercingUnaryOp;
        [k: string]: unknown;
      };
    }
  | {
      CoercingBinary: {
        left: Expr;
        op: CoercingBinaryOp;
        right: Expr;
        [k: string]: unknown;
      };
    }
  | {
      OperandLogical: {
        left: Expr;
        op: OperandLogicalOp;
        right: Expr;
        [k: string]: unknown;
      };
    };
/**
 * The stored form of an IR number literal.
 */
export type IrNumber = number | NonFiniteNumber;
/**
 * A non-finite number literal's stored spelling.
 */
export type NonFiniteNumber = 'NaN' | 'Infinity' | '-Infinity';
export type AssignPathStep =
  | {
      Field: string;
    }
  | {
      Index: Expr;
    };
/**
 * The structural roles a front end marks its generated IR with.
 *
 * Each role is language-neutral: it names what a shape does, not the source
 * construct that produced it, and each front end chooses which of its
 * constructs lower to which role.
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
      kind: 'json_traversal';
    }
  | {
      kind: 'process_wrapper';
    };
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
 * The member an [`Expr::MethodCall`] reads its callee from.
 */
export type MethodKey =
  | {
      Field: string;
    }
  | {
      Index: Expr;
    };
/**
 * Unary value operations with explicit ECMA-262 coercion rules.
 */
export type CoercingUnaryOp = 'Plus' | 'Negate' | 'Not' | 'TypeOf' | 'BitNot' | 'ToString';
/**
 * Eager binary value operations with explicit ECMA-262 coercion rules.
 *
 * Operand expressions evaluate left then right. Object-to-primitive hooks
 * run left then right when the rule requires them. Numeric results are f64.
 */
export type CoercingBinaryOp =
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
  | 'GreaterEqual'
  | 'BitAnd'
  | 'BitOr'
  | 'BitXor'
  | 'ShiftLeft'
  | 'ShiftRight'
  | 'ShiftRightUnsigned';
/**
 * Short-circuiting operations that return an operand without coercing it.
 */
export type OperandLogicalOp = 'And' | 'Or' | 'NullishCoalesce';
/**
 * Closed vocabulary of executable workflow sites.
 *
 * This describes the site, not its current observation. A workflow node may
 * expose more than one site kind. Declaration order is the canonical order:
 * a node's execution sites sort by it, so reordering the variants changes
 * the serialized workflow graph and needs a graph schema bump.
 */
export type ExecutionNodeKind =
  'resource_operation' | 'sleep' | 'wait' | 'terminal' | 'process_event' | 'branch' | 'loop' | 'call' | 'step';
/**
 * One step of a [`WorkflowSitePath`].
 */
export type WorkflowSiteSegment =
  | {
      slot: ExprSlot;
    }
  | {
      role: WorkflowSiteRole;
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
 * What a site that is not an expression of its own stands for.
 */
export type WorkflowSiteRole = 'labeled_step';
/**
 * The typed path from a workflow node's statement to one executable
 * subexpression. The empty path is the statement itself.
 *
 * Slot segments walk the statement's typed child slots; a trailing role
 * segment names a synthetic site of the expression they reach.
 */
export type WorkflowSitePath = WorkflowSiteSegment[];
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
      /**
       * The assigned value, or with `update`, the operand the update applies
       * to the target's current value (`target op= expression`).
       */
      expression: Expr;
      kind: 'state_update';
      /**
       * Set when the update is a member assignment that pins its
       * reference base before evaluating the value
       * ([`crate::StructuralRole::AttributeAssign`]): the slots it pins
       * them in. `target` is then one member step of a variable.
       */
      pinned?: WorkflowPinnedSlots | null;
      target: AssignTarget;
      update?: UpdateOperator | null;
    }
  | {
      expression: Expr;
      kind: 'terminal';
      terminal: WorkflowTerminalKind;
    }
  | {
      kind: 'throw';
      value: Expr;
    }
  | {
      binding?: AssignTarget | null;
      condition: Expr;
      container_kind: 'if';
      else_graph: WorkflowSubgraph;
      kind: 'container';
      then_graph: WorkflowSubgraph;
    }
  | {
      /**
       * The element binding's authored name, outside execution identity.
       */
      authored_element?: string | null;
      /**
       * The generated statements that bind the element into the names the
       * body reads, when the front end needs any.
       */
      bind?: Expr | null;
      binding?: AssignTarget | null;
      body: WorkflowSubgraph;
      container_kind: 'for';
      /**
       * The binding each element is assigned to.
       */
      element: string;
      iterable: Expr;
      kind: 'container';
    }
  | {
      binding?: AssignTarget | null;
      body: WorkflowSubgraph;
      condition: Expr;
      container_kind: 'while';
      kind: 'container';
    }
  | {
      binding?: AssignTarget | null;
      body: WorkflowSubgraph;
      catch?: WorkflowCatch | null;
      container_kind: 'try';
      finally?: WorkflowSubgraph | null;
      kind: 'container';
    }
  | {
      binding?: AssignTarget | null;
      body: WorkflowSubgraph;
      container_kind: 'scope';
      kind: 'container';
    };
/**
 * One call or effect argument in graph order.
 *
 * Type facets address these values with a serialized [`WorkflowSlotPath`].
 * Its typed call, argument, field, and index segments cannot collide when a
 * field contains punctuation. Nodes with several nested receiver calls add a
 * call segment in depth-first IR walk order.
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
export type WorkflowEffectKind = 'await_join' | 'sleep_for' | 'print' | 'break' | 'continue';
/**
 * An arithmetic operator a compound attribute assignment applies to the
 * attribute's current value. Named neutrally: a front end's IR decides
 * whether the operation is LashVm's or ECMA-262's.
 */
export type UpdateOperator = 'add' | 'subtract' | 'multiply' | 'divide' | 'remainder';
export type WorkflowTerminalKind = 'finish' | 'fail';
export type WorkflowNodeNameSource = 'label' | 'derived';
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
/**
 * The origin of a [`super::ProcessDecl`].
 */
export type ProcessOrigin =
  | {
      kind: 'declared';
    }
  | {
      /**
       * The authored settled output annotation. An inferred output remains
       * on the declaration, so rendering can infer it again without
       * inventing an annotation the source never declared.
       */
      declared_return_ty?: TypeExpr | null;
      hidden_params: number;
      kind: 'lifted';
      site: AstPath;
    };
/**
 * Which tree an [`AstPath`] walks down: `Program::main`, or one entry of
 * `Program::declarations`.
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
  /**
   * The interpretation of the semantic IR this document's regions and
   * expressions are written under ([`WORKFLOW_IR_VERSION`]).
   */
  ir_version: number;
  main: WorkflowSubgraph;
  /**
   * The main-level bindings that are the front end's own slots rather
   * than session-visible names ([`crate::Program::private_bindings`]).
   */
  private_bindings?: string[];
  schema_version: 21;
  /**
   * The definition identity of the admitted module artifact this graph
   * projects ([`crate::ModuleArtifact::source_identity`]), which the
   * module's traces carry too. A draft projected from source that has not
   * been admitted claims no runtime identity and carries `None`.
   * [`WORKFLOW_GRAPH_SCHEMA_VERSION`] identifies this document's wire shape,
   * `facet_schema_version` identifies optional derived facts.
   */
  source_identity?: string | null;
}
export interface WorkflowSubgraph {
  /**
   * Derived: sequence follows `nodes` order and data dependencies follow
   * the bindings the nodes' expressions read.
   */
  edges?: WorkflowEdge[];
  /**
   * How the IR spells this ordered body.
   */
  form?: WorkflowBodyForm;
  /**
   * The body's statements, in execution order.
   */
  nodes?: WorkflowNode[];
}
export interface WorkflowEdge {
  from: WorkflowNodeId;
  id: string;
  kind: WorkflowEdgeKind;
  to: WorkflowNodeId;
}
/**
 * A run of consecutive statements that the IR holds as one nested statement
 * list closed by its own completion value: the statement a front end gave a
 * value. It covers the nodes `start .. start + len` of its body; groups are
 * ordered, never overlap, and nest through `groups`, whose ranges lie inside
 * this one. A group may cover no statement at all.
 */
export interface WorkflowCompletionGroup {
  groups?: WorkflowCompletionGroup[];
  len: number;
  start: number;
  value: Expr;
}
export interface LabelMetadata {
  description?: string | null;
  title: string;
  [k: string]: unknown;
}
export interface AssignTarget {
  root: string;
  steps?: AssignPathStep[];
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
  /**
   * The ECMA-262 `name` own property the closure value carries: the
   * function's own binding name, or the name a `NamedEvaluation` /
   * `SetFunctionName` context assigned it. `None` is the anonymous `""`
   * ECMA reports for a function no naming context reached.
   */
  js_name?: string | null;
  name?: string | null;
  params?: string[];
  /**
   * The slot the call's receiver is bound to, for a function that reads
   * it. A call through [`Expr::MethodCall`] or [`Expr::ThisCall`] binds the
   * receiver it names; every other call binds `undefined`. A function
   * without one (an arrow, or one that never reads its receiver) ignores
   * the receiver entirely; an arrow reads its enclosing function's slot as
   * an ordinary capture.
   */
  receiver?: string | null;
  [k: string]: unknown;
}
/**
 * The authored shape of an inline process body, as a dialect lowers it.
 *
 * `params` carries the parameter names and their declared types, so a
 * TypeScript arrow's annotations reach the lifted declaration's signature
 * instead of widening to `Any`. `body` is the same wrapper a process literal
 * run lowers to: the authored statements inside the process-failure wrapper,
 * with the params passed through by name.
 */
export interface ProcessLiteralExpr {
  body: Expr;
  /**
   * Immutable, durably representable cell locals the body reads; each
   * becomes a hidden start argument carrying the value the variable had
   * when the process started (FIG-2998).
   */
  hidden_args?: ProcessParam[];
  params: ProcessParam[];
  /**
   * The declared settled output, or `None` to infer it from the body.
   */
  return_ty?: TypeExpr | null;
  [k: string]: unknown;
}
export interface ProcessParam {
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
export interface WorkflowNode {
  /**
   * Identifiers visible before this node executes, in stable lexical order.
   */
  available_variables?: string[];
  description?: string | null;
  execution_sites?: WorkflowExecutionSite[];
  id: WorkflowNodeId;
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
 * One execution site of a workflow node: the node (`owner` and `path`), the
 * exact executable subexpression inside its statement (`site_path`), and a
 * description of what runs there. `kind` and `label` describe the site; they
 * are not its identity.
 */
export interface WorkflowExecutionSite {
  kind: ExecutionNodeKind;
  label: string;
  owner: string;
  path?: number[];
  site_path?: WorkflowSitePath;
  [k: string]: unknown;
}
/**
 * The slots a pinned member assignment evaluates through, in evaluation
 * order: the reference base, a computed index, then the assigned value.
 */
export interface WorkflowPinnedSlots {
  base: string;
  key?: string | null;
  result: string;
}
/**
 * The catch clause of a [`WorkflowContainer::Try`]: `binding` names the
 * thrown value inside `body`.
 */
export interface WorkflowCatch {
  binding: string;
  body: WorkflowSubgraph;
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
  classification: WorkflowDiagnosticClassification;
  kind: WorkflowDiagnosticKind;
  message: string;
  node_id: WorkflowNodeId;
  slot?: WorkflowSlotPath | null;
  span?: Span | null;
  [k: string]: unknown;
}
export interface WorkflowExpectedArgument {
  slot: WorkflowSlotPath;
  ty: TypeExpr;
  [k: string]: unknown;
}
/**
 * A node's address in a `Program`: the root it hangs from plus the
 * `Expr::children()` index chain that reaches it.
 */
export interface AstPath {
  root: AstRoot;
  steps?: number[];
  [k: string]: unknown;
}
/**
 * The process failure wrapper, without the run body it wraps: the run
 * function finishes the process with its value, and the catch fails the
 * process with what the body throws.
 */
export interface WorkflowProcessWrapper {
  /**
   * The arguments the run call passes.
   */
  arguments?: Expr[];
  captures?: string[];
  /**
   * The binding the wrapper's catch fails the process with.
   */
  catch_binding: string;
  /**
   * The builtin the run call goes through, when it does.
   */
  driver?: WorkflowRunDriver | null;
  js_name?: string | null;
  /**
   * The run function's own name, when it has one.
   */
  name?: string | null;
  /**
   * The run function's parameters, bound to `arguments` in order.
   */
  params?: string[];
  receiver?: string | null;
}
/**
 * A builtin that drives a process's run function: it receives the function
 * first, then `arguments`.
 */
export interface WorkflowRunDriver {
  arguments?: Expr[];
  builtin: string;
}
/**
 * A user-defined pure synchronous function.
 *
 * A function is the language's only reusable *synchronous* abstraction:
 * `process` is durable and asynchronous, so shared pure logic previously had
 * to be inlined at every use. The declaration is deliberately narrower than
 * `process`: parameters and the return type are both mandatory, and the linker
 * rejects every effect inside the body. That ban is what keeps effect identity
 * untouched — every effect stays at a stable top-level syntactic site, so
 * call-site exactly-once identity and continuation snapshots see no new shape.
 */
export interface FunctionDecl {
  body: Expr;
  name: string;
  params?: FunctionParam[];
  return_ty: TypeExpr;
  [k: string]: unknown;
}
export interface FunctionParam {
  name: string;
  ty: TypeExpr;
  [k: string]: unknown;
}
