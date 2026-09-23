/* Generated from the example Rust HTTP DTOs by npm run generate:types. Do not edit directly. */

/**
 * A node's display name together with the one fact that decides whether the host may throw it away: who chose it.
 *
 * Rendering emits an `@label` annotation only for an authored name, so a payload that carries a title without saying where the name came from is a rename waiting to evaporate. The tag is therefore mandatory and carries the title inside the variant: "title present, tag absent" is unrepresentable, and an unknown tag is a decode error rather than a silent `derived`. The wire values (`label`, `derived`) mirror `WorkflowNodeNameSource`, which the browser client already writes on every node.
 */
export type NodeData = {
  availableVars?: TypedVariable[];
  binding?: string | null;
  children?: ChildGroup[];
  clauses?: EditableComprehensionClause[];
  condition?: string | null;
  diagnostics?: TypeDiagnostic[];
  effect?: string | null;
  expectedArgTypes?: ExpectedArgumentType[];
  expression?: string | null;
  fields?: {
    [k: string]: EditableValue;
  };
  iterable?: string | null;
  kind: string;
  name?: string | null;
  operation?: string | null;
  params?: EditableProcessField[];
  /**
   * The receiver the catalog entry this node came from belongs to, carried so a call node posted with no `expression` can be synthesized against the receiver it actually names (FIG-3178).
   */
  receiver?: string | null;
  signals?: EditableProcessField[];
  source?: string | null;
  subkind?: string | null;
  target?: string | null;
  terminalKind?: string | null;
  [k: string]: unknown;
} & NodeData1;
export type EditableComprehensionClause =
  | {
      binding: string;
      iterable: string;
      kind: 'for';
      [k: string]: unknown;
    }
  | {
      condition: string;
      kind: 'if';
      [k: string]: unknown;
    };
export type EditableValue = unknown;
export type NodeData1 =
  | {
      description?: string | null;
      nameSource: 'label';
      title: string;
      [k: string]: unknown;
    }
  | {
      nameSource: 'derived';
      title: string;
      [k: string]: unknown;
    };

export interface WorkflowDocument {
  /**
   * The source identity of the admitted artifact this document's graph is the view of; absent for a draft whose source does not admit. A run overlay shows a run's events only when their `definition` is this one.
   */
  definition?: string | null;
  edges: FlowEdge[];
  facetSchemaVersion?: number | null;
  nodes: FlowNode[];
  roots: GraphRoots;
  schemaVersion: number;
  source: string;
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
export interface EdgeData {
  kind: string;
  scope: string;
  variable?: string | null;
  version?: number | null;
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
  class: string;
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
