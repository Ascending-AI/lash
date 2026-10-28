// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperUpgradeRefusal = ({
  "from": FunctionId;
  "helper": HelperId;
  "refused": "helper_contract_changed";
  "to": FunctionId;
} | {
  "helper": HelperId;
  "refused": "helper_tombstoned";
} | {
  "helper": HelperId;
  "refused": "helper_missing";
} | {
  "function": FunctionId;
  "refused": "helper_unpaired";
} | {
  "message": string;
  "refused": "archive";
} | {
  "message": string;
  "refused": "document";
} | {
  "function": FunctionId;
  "refused": "retained_callable";
} | {
  "function": FunctionId;
  "refused": "branded_data";
});

export type FunctionId = string;

/**
 * Permanent package-qualified contract name, used only when pairing releases.
 */
export type HelperId = string;
