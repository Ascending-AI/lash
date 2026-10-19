// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * The captions of one document, by site: strictly ascending and unique,
 * each a captionable site of the document of identity `document`. It is
 * separate from the [`crate::SourceMap`]: captions outlive the source bytes
 * a map is keyed to. A host holds it; lash stores none.
 *
 * Decoding checks the order; validate a decoded value against its document
 * before using its sites.
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
 * One site's caption.
 */
export type CaptionEntry = {
  "caption": Caption;
  "site": Site;
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
