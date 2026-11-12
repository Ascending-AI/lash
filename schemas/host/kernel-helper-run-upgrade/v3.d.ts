// Derived from v3.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperRunUpgrade = {
  "outcome": HelperRunUpgradeOutcome;
  "request_id": string;
};

/**
 * A function and its operation context. Authored callbacks retain their defining
 * Link; build helpers carry the caller's real Link and an exact library ID.
 */
export type CallableId = {
  "function": FunctionId;
  "link": LinkId;
};

export type DocumentId = string;

export type EvidenceSetId = string;

export type FunctionId = string;

export type HelperChange = {
  "body": boolean;
  "charge": boolean;
  "from": FunctionId;
  "helper": HelperId;
  "native_version": boolean;
  "to": FunctionId;
};

/**
 * Permanent package-qualified contract name, used only when pairing releases.
 */
export type HelperId = string;

/**
 * A new immutable publication revision. The host chooses whether its alias moves.
 */
export type HelperPublicationUpgrade = {
  "document": DocumentId;
  "from": WorkflowReference;
  "links": Record<string, LinkId>;
  "to": WorkflowReference;
  "upgrade": HelperUpgrade;
};

/**
 * The artifact prepared by this request, coupled to the succession result.
 * A publication is not evidence that the owner has applied the successor.
 */
export type HelperRunUpgradeOutcome = ({
  "publication": HelperPublicationUpgrade;
  "status": "requested";
} | {
  "publication": HelperPublicationUpgrade;
  "status": "already_current";
} | {
  "status": "already_resolved";
});

/**
 * A validated ledger pairing, including anonymous bodies that cannot be paired by name.
 */
export type HelperUpgrade = {
  "changed": Array<FunctionId>;
  "changes": Array<HelperChange>;
  "functions": Record<string, FunctionId>;
};

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

/**
 * A selected workflow and its immutable defining Link.
 */
export type WorkflowReference = ({
  "linked": LinkedStart;
} | {
  "callable": CallableId;
});
