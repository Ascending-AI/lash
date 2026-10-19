// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * Why a kernel process is not carried to the next kernel version.
 */
export type KernelMigrationRefusal = ({
  "document": DocumentId;
  "refused": "document_missing";
  [key: string]: unknown;
} | {
  "document": DocumentId;
  "refusal": DocumentRefusal;
  "refused": "document";
  [key: string]: unknown;
} | {
  "document": DocumentId;
  "reason": string;
  "refused": "not_admitted";
  [key: string]: unknown;
} | {
  "document": DocumentId;
  "refusal": ParkedRefusal;
  "refused": "parked";
  [key: string]: unknown;
} | {
  "message": string;
  "refused": "state";
  [key: string]: unknown;
});

export type DocumentId = string;

/**
 * Why a document is not rewritten for the next version.
 */
export type DocumentRefusal = ({
  "found": number;
  "from": number;
  "reason": "version";
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "reason": "function_not_held";
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "name": QualifiedName;
  "reason": "function_retired";
  [key: string]: unknown;
} | {
  "operation": QualifiedName;
  "reason": "operation_contract_changed";
  [key: string]: unknown;
} | {
  "detail": string;
  "reason": "form_not_carried";
  "site": Site;
  [key: string]: unknown;
} | {
  "reason": "correspondence";
  [key: string]: unknown;
});

export type FunctionId = string;

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * Why a parked run is not carried onto a rewritten document.
 */
export type ParkedRefusal = ({
  "found": number;
  "from": number;
  "reason": "version";
  [key: string]: unknown;
} | {
  "parked": DocumentId;
  "reason": "document";
  "rewritten": DocumentId;
  [key: string]: unknown;
} | {
  "held": string;
  "reason": "site_not_carried";
  "site": Site;
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "reason": "function_not_carried";
  [key: string]: unknown;
} | {
  "detail": string;
  "reason": "state_not_carried";
  [key: string]: unknown;
});

export type QualifiedName = string;

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
