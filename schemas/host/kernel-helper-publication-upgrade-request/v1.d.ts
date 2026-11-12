// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperPublicationUpgradeRequest = {
  "publication": LinkedStart;
};

export type EvidenceSetId = string;

export type LinkId = string;

/**
 * The immutable Link and selected start that determine process definition
 * identity, and the publication's source evidence a run of it reads.
 */
export type LinkedStart = {
  /**
   * The publication's source evidence: absent when the published document
   * carried none, and then not serialized, so an evidence-less start keeps
   * its encoding. Evidence never changes what runs.
   */
  "evidence"?: (EvidenceSetId | null);
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
