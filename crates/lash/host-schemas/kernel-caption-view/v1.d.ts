// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * A view's captions, each keyed by where the view draws it. Wording for a
 * step without a caption is the host's.
 */
export type CaptionView = {
  /**
   * By node, then arm: a control node's [`crate::ControlArm`] captions.
   */
  "arms": Array<ArmCaption>;
  /**
   * By callable: a declared function's caption, on its
   * [`crate::CallableView`]. The entry's is the workflow's title.
   */
  "callables": Array<CallableCaption>;
  "document": DocumentId;
  /**
   * By node, then site. An action or control node has its carrier
   * statement's caption; a statement with several actions gives it to
   * the node of its first. A code node has the caption of every member,
   * and of every clause arm of a member, that has one.
   */
  "nodes": Array<NodeCaption>;
};

export type ArmCaption = {
  /**
   * The arm's index in that node's `arms`.
   */
  "arm": number;
  "block": Site;
  "caption": Caption;
  /**
   * The control node's index in [`WorkflowView::nodes`].
   */
  "node": number;
};

export type CallableCaption = {
  "block": Site;
  /**
   * The callable's index in [`WorkflowView::callables`].
   */
  "callable": number;
  "caption": Caption;
};

/**
 * A step's title and optional description, in the one text domain every
 * dialect can spell and read back exactly (`K-CAP-005`). A value exists only
 * once [`Caption::new`] accepted its text; decoding checks it the same way.
 */
export type Caption = {
  /**
   * At most [`CAPTION_DESCRIPTION_BYTES`] of trimmed lines separated by
   * `\n`, with no blank first or last line and no two blank lines in a
   * row. Never empty: no description is `None`.
   */
  "description"?: (string | null);
  /**
   * One non-empty, trimmed line of at most [`CAPTION_TITLE_BYTES`].
   */
  "title": string;
};

export type DocumentId = string;

export type FunctionId = string;

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

export type NodeCaption = {
  "caption": Caption;
  /**
   * The node's index in [`WorkflowView::nodes`].
   */
  "node": number;
  /**
   * The captioned statement or arm.
   */
  "site": Site;
};

/**
 * A node's address: the code it belongs to and the chain of
 * [`Node::children`] indexes that reaches it from that code's body.
 *
 * A site is derived from the document and is never written by a host
 * (`K-ID-004`). An empty chain addresses the body itself.
 */
export type Site = {
  "path"?: Array<number>;
  "unit": Unit;
};

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});
