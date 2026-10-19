// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * The captions of one document, by target: strictly ascending and unique,
 * each a target the document of identity `document` lets a caption name. It is
 * separate from the [`crate::SourceMap`]: captions outlive the source bytes
 * a map is keyed to. A host holds it; lash stores none.
 *
 * Decoding checks the order; validate a decoded value against its document
 * before using its targets.
 */
export type Captions = {
  "document": DocumentId;
  "entries"?: Array<CaptionEntry>;
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

/**
 * One caption: its text and what it names, by exactly one of `site`,
 * `param` and `field`.
 */
export type CaptionEntry = (SiteEntry | ParamEntry | FieldEntry);

export type DocumentId = string;

/**
 * A field's caption.
 */
export type FieldEntry = {
  "caption": Caption;
  "field": FieldTarget;
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

/**
 * A named field of a type an entry's signature states.
 */
export type FieldTarget = {
  /**
   * The entry.
   */
  "function": Name;
  "path": FieldPath;
};

export type FunctionId = string;

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * A parameter's caption.
 */
export type ParamEntry = {
  "caption": Caption;
  "param": ParamTarget;
};

/**
 * A declared function's parameter, by name. An entry's parameters are
 * its signature's, the names a host starts it with; another function's are
 * its own.
 */
export type ParamTarget = {
  "function": Name;
  "param": Name;
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
 * A site's caption.
 */
export type SiteEntry = {
  "caption": Caption;
  "site": Site;
};

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});
