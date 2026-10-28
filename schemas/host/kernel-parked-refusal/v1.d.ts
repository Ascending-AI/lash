// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * Why a parked run is not carried onto a rewritten document.
 */
export type ParkedRefusal = ({
  "function": FunctionId;
  "kind": "helper_operation_context";
  "slot": ImportSlot;
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "kind": "helper_unavailable";
  "reason": HelperUnavailableReason;
  [key: string]: unknown;
} | {
  "found": number;
  "from": number;
  "kind": "version";
  [key: string]: unknown;
} | {
  "kind": "document";
  "parked": DocumentId;
  "rewritten": DocumentId;
  [key: string]: unknown;
} | {
  "held": string;
  "kind": "site_not_carried";
  "site": Site;
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "kind": "function_not_carried";
  [key: string]: unknown;
} | {
  "detail": string;
  "kind": "state_not_carried";
  [key: string]: unknown;
});

export type DocumentId = string;

export type FunctionId = string;

/**
 * Why an exact helper cannot execute on this node.
 */
export type HelperUnavailableReason = ("retired" | "unknown" | "corrupt");

/**
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

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
