// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperPublicationUpgradeRequest = {
  "publication": LinkedStart;
};

export type LinkId = string;

/**
 * The immutable Link and selected start that determine process definition identity.
 */
export type LinkedStart = {
  "link": LinkId;
  "selector": StartSelector;
};

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * Main requires a main block; Export names a top-level function and never runs main.
 */
export type StartSelector = ({
  "kind": "main";
} | {
  "kind": "export";
  "value": Name;
});
