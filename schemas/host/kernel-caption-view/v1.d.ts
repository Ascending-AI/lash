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
   * By binding: each `let` of the entry whose declaring statement has a
   * caption, which names the value it binds. A variable a lowering
   * declares inside an expression never has one.
   */
  "bindings": Array<BindingCaption>;
  /**
   * By callable: a declared function's caption, on its
   * [`crate::CallableView`]. The entry's is the workflow's title.
   */
  "callables": Array<CallableCaption>;
  "document": DocumentId;
  /**
   * By path: the captioned fields of the entry's parameter and result
   * types, ascending.
   */
  "fields": Array<FieldCaption>;
  /**
   * By node, then site. An action or control node has its statement's
   * caption; a statement with several actions gives it to the node of
   * its first outermost action. A code node has the caption of every
   * member, and of every clause arm of a member, that has one.
   */
  "nodes": Array<NodeCaption>;
  /**
   * By parameter: the entry's parameters that have a caption, in the
   * order the entry takes them.
   */
  "params": Array<ParamCaption>;
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

export type BindingCaption = {
  /**
   * The binding's id in [`crate::Dataflow::bindings`].
   */
  "binding": number;
  "caption": Caption;
  "name": Name;
  /**
   * The `let` statement that declares it, whose caption this is.
   */
  "site": Site;
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

export type FieldCaption = {
  "caption": Caption;
  /**
   * The field, from the entry's parameter or result.
   */
  "path": FieldPath;
};

/**
 * A walk from a parameter's or the result's type to a named record field.
 * Each step names a field of a record. A list is walked to its element
 * type, and a union whose members other than null and absent are exactly
 * one is walked to that member, so `items.title` names the `title` of each
 * record of a list `items`, and `reply.note` the `note` of an optional
 * `reply`. A map, a set, a tuple, a record's rest and a union of two
 * records are not walked.
 */
export type FieldPath = {
  /**
   * At least one field name.
   */
  "fields": Array<string>;
  "root": FieldRoot;
};

/**
 * Where a [`FieldPath`] starts.
 */
export type FieldRoot = ({
  "param": Name;
} | "result");

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

export type ParamCaption = {
  "caption": Caption;
  /**
   * The parameter's position among the entry's.
   */
  "index": number;
  "name": Name;
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
