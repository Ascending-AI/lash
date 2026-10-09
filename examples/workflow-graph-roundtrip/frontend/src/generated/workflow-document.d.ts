/* Generated from the example Rust HTTP DTOs by npm run generate:types. Do not edit directly. */

export type EdgeData =
  | {
      kind: 'data_dependency';
      scope: string;
      variable: string;
      version: number;
    }
  | {
      kind: 'sequence';
      scope: string;
    };
export type NodeData =
  | {
      availableVars?: TypedVariable[];
      children?: ChildGroup[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'process';
      name?: string | null;
      nameSource: 'label';
      params?: EditableProcessField[];
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'data';
      nameSource: 'label';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'call';
      nameSource: 'label';
      operation?: string | null;
      receiver?: string | null;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      effect: WorkflowEffectKind;
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'effect';
      nameSource: 'label';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'state_update';
      nameSource: 'label';
      target?: string | null;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'computation';
      nameSource: 'label';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      kind: 'terminal';
      nameSource: 'label';
      terminalKind: WorkflowTerminalKind;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      kind: 'throw';
      nameSource: 'label';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      condition?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'label';
      subkind: 'if';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      children?: ChildGroup[];
      condition?: string | null;
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'label';
      subkind: 'while';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      iterable?: string | null;
      kind: 'container';
      nameSource: 'label';
      subkind: 'for';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      /**
       * The name the catch clause binds; absent when there is no clause.
       */
      catchBinding?: string | null;
      children?: ChildGroup[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      /**
       * Whether the `try` has a `finally` body.
       */
      finally?: boolean;
      kind: 'container';
      nameSource: 'label';
      subkind: 'try';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      description?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'label';
      subkind: 'scope';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      children?: ChildGroup[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'process';
      name?: string | null;
      nameSource: 'derived';
      params?: EditableProcessField[];
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'data';
      nameSource: 'derived';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'call';
      nameSource: 'derived';
      operation?: string | null;
      receiver?: string | null;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      diagnostics?: TypeDiagnostic[];
      effect: WorkflowEffectKind;
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'effect';
      nameSource: 'derived';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'state_update';
      nameSource: 'derived';
      target?: string | null;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      fields?: {
        [k: string]: EditableValue;
      };
      kind: 'computation';
      nameSource: 'derived';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      kind: 'terminal';
      nameSource: 'derived';
      terminalKind: WorkflowTerminalKind;
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      expression?: string | null;
      kind: 'throw';
      nameSource: 'derived';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      condition?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'derived';
      subkind: 'if';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      children?: ChildGroup[];
      condition?: string | null;
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'derived';
      subkind: 'while';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      iterable?: string | null;
      kind: 'container';
      nameSource: 'derived';
      subkind: 'for';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      /**
       * The name the catch clause binds; absent when there is no clause.
       */
      catchBinding?: string | null;
      children?: ChildGroup[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      /**
       * Whether the `try` has a `finally` body.
       */
      finally?: boolean;
      kind: 'container';
      nameSource: 'derived';
      subkind: 'try';
      title: string;
    }
  | {
      availableVars?: TypedVariable[];
      binding?: string | null;
      children?: ChildGroup[];
      diagnostics?: TypeDiagnostic[];
      expectedArgTypes?: ExpectedArgumentType[];
      kind: 'container';
      nameSource: 'derived';
      subkind: 'scope';
      title: string;
    };
/**
 * Whether a diagnostic establishes an admission failure for the analyzed
 * program and host environment or gives advice without establishing a failure.
 */
export type WorkflowDiagnosticClassification = 'definite' | 'advisory';
/**
 * An editable field carries its authored kind at every depth. Literal keys
 * are data, including `$expr`, `kind` and `value`.
 */
export type EditableValue =
  | {
      kind: 'null';
      value: null;
    }
  | {
      kind: 'bool';
      value: boolean;
    }
  | {
      kind: 'number';
      value: number;
    }
  | {
      kind: 'string';
      value: string;
    }
  | {
      kind: 'list';
      value: EditableValue[];
    }
  | {
      kind: 'expr';
      value: string;
    }
  | {
      kind: 'object';
      value: {
        [k: string]: EditableValue;
      };
    };
/**
 * Which effect a [`WorkflowEffect`] is, without its operands.
 */
export type WorkflowEffectKind = 'await_join' | 'sleep_for' | 'print' | 'break' | 'continue';
/**
 * Whether a [`WorkflowTerminal`] finishes or fails, without its value.
 */
export type WorkflowTerminalKind = 'finish' | 'fail';

export interface WorkflowDocument {
  /**
   * The source identity of the admitted artifact this document's graph
   * is the view of; absent for a draft whose source does not admit. A run
   * overlay shows a run's events only when their `definition` is this one.
   */
  definition?: string | null;
  edges: FlowEdge[];
  facetSchemaVersion?: number | null;
  nodes: FlowNode[];
  /**
   * Why Lash did not admit this version as a definition. It is saved as
   * a draft and cannot run until an edit makes it admissible.
   */
  notAdmitted?: string | null;
  roots: GraphRoots;
  schemaVersion: number;
  /**
   * The workflow's canonical TypeScript, a view the optional TypeScript
   * lens gives of the document. Empty when the lens has no spelling for
   * it; the document is complete and editable either way.
   */
  source: string;
  /**
   * Why the TypeScript lens has no spelling for this workflow.
   */
  sourceUnavailable?: string | null;
  version: number;
  [k: string]: unknown;
}
export interface FlowEdge {
  data: EdgeData;
  id: string;
  source: string;
  target: string;
  [k: string]: unknown;
}
export interface FlowNode {
  data: NodeData;
  id: string;
  parentId?: string | null;
  type: string;
  [k: string]: unknown;
}
export interface TypedVariable {
  name: string;
  type: string;
  [k: string]: unknown;
}
export interface ChildGroup {
  nodeIds: string[];
  scope: string;
  slot: string;
  [k: string]: unknown;
}
export interface TypeDiagnostic {
  classification: WorkflowDiagnosticClassification;
  kind: string;
  message: string;
  nodeId: string;
  slot?: string | null;
  span?: Span | null;
  [k: string]: unknown;
}
export interface Span {
  end: number;
  start: number;
  [k: string]: unknown;
}
export interface ExpectedArgumentType {
  slot: string;
  type: string;
  [k: string]: unknown;
}
export interface EditableProcessField {
  name: string;
  type: string;
  [k: string]: unknown;
}
export interface GraphRoots {
  main: string[];
  processes?: string[];
  [k: string]: unknown;
}
