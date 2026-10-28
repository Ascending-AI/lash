// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

export type HelperUpgradeSafety = ({
  "status": "safe";
} | {
  "functions": Array<FunctionId>;
  "status": "deferred";
});

export type FunctionId = string;
