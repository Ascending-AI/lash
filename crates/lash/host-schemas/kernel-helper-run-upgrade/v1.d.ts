// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperRunUpgrade = {
  /**
   * A newly prepared revision. An already settled id returns no new publication.
   */
  "publication"?: (HelperPublicationUpgrade | null);
  "request_id": string;
  "state": HelperRunUpgradeState;
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

export type HelperRunUpgradeState = ("requested" | "already_current" | "already_resolved");

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

/**
 * A selected workflow and its immutable defining Link.
 */
export type WorkflowReference = ({
  "linked": LinkedStart;
} | {
  "callable": CallableId;
});
