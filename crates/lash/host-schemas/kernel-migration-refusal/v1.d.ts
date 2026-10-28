// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * Why a kernel process is not carried to the next kernel version.
 */
export type KernelMigrationRefusal = ({
  "callable": CallableId;
  "refused": "callable_origin_required";
  [key: string]: unknown;
} | {
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
  "refusal": Invalid;
  "refused": "not_admitted";
  [key: string]: unknown;
} | {
  "document": DocumentId;
  "refused": "explicit_successor_required";
  [key: string]: unknown;
} | {
  "document": DocumentId;
  "reason": string;
  "refused": "succession";
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

/**
 * A function and its operation context. Authored callbacks retain their defining
 * Link; build helpers carry the caller's real Link and an exact library ID.
 */
export type CallableId = {
  "function": FunctionId;
  "link": LinkId;
};

export type DocumentId = string;

/**
 * Why a document is not rewritten for the next version.
 */
export type DocumentRefusal = ({
  "found": number;
  "from": number;
  "kind": "version";
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "kind": "function_not_held";
  [key: string]: unknown;
} | {
  "function": FunctionId;
  "kind": "function_retired";
  "name": FunctionName;
  [key: string]: unknown;
} | {
  "kind": "operation_contract_changed";
  "operation": FunctionName;
  [key: string]: unknown;
} | {
  "detail": string;
  "kind": "form_not_carried";
  "site": Site;
  [key: string]: unknown;
} | {
  "kind": "correspondence";
  [key: string]: unknown;
});

export type FunctionId = string;

export type FunctionName = string;

/**
 * Why an exact helper cannot execute on this node.
 */
export type HelperUnavailableReason = ("retired" | "unknown" | "corrupt");

/**
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

/**
 * Why a document or a definition is refused, and the node at fault.
 */
export type Invalid = {
  "reason": InvalidReason;
  /**
   * The node at fault. `None` when the fault is in no node: the
   * manifest, an entry, a signature.
   */
  "site"?: (Site | null);
};

export type InvalidReason = (("empty_name" | "limit_measures_result" | "empty_formula" | "guard_without_native" | "empty_guard_unit") | {
  "document": {
    "message": string;
  };
} | {
  "import": {
    "slot": ImportSlot;
  };
} | {
  "kernel_version": {
    "found": number;
    "supported": number;
  };
} | {
  "too_deep": {
    "limit": number;
  };
} | {
  "duplicate_param": {
    "name": Name;
  };
} | {
  "duplicate_field": {
    "field": string;
  };
} | {
  "loop_control_outside_loop": {
    "keyword": string;
  };
} | {
  "call_not_native": {
    "function": FunctionId;
    "name": FunctionName;
  };
} | {
  "function_of_another_version": {
    "function": FunctionId;
    "name": FunctionName;
    "required": number;
    "written_for": number;
  };
} | {
  "function_not_listed": {
    "function": FunctionId;
  };
} | {
  "function_not_resolved": {
    "function": FunctionId;
  };
} | {
  "helper_unavailable": {
    "function": FunctionId;
    "reason": HelperUnavailableReason;
  };
} | {
  "call_arity": {
    "accepted": number;
    "given": number;
    "name": string;
    "required": number;
  };
} | {
  "effect_not_listed": {
    "slot": ImportSlot;
  };
} | {
  "function_not_declared": {
    "name": Name;
  };
} | {
  "declared_function_in_library": {
    "name": Name;
  };
} | {
  "effect_not_declared": {
    "slot": ImportSlot;
  };
} | {
  "entry_not_declared": {
    "entry": Name;
  };
} | {
  "entry_arity": {
    "declared": number;
    "entry": Name;
    "signature": number;
  };
} | {
  "invalid_type": {
    "problem": string;
  };
} | {
  "required_after_optional": {
    "name": Name;
  };
} | {
  "formula_unknown_param": {
    "name": Name;
  };
} | {
  "native_version_without_native": {
    "version": number;
  };
} | "native_version_zero" | {
  "native_takes_function": {
    "name": Name;
  };
} | {
  "native_body_may_wait": {
    "form": StatementForm;
  };
} | {
  "place_first_assignment": {
    "problem": string;
  };
} | "action_as_expression" | {
  "start_arity": {
    "declared": number;
    "signature": number;
  };
} | "start_token");

export type LinkId = string;

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
 * An action form, as a refusal names one the body of a function with a
 * native implementation may not run (`K-LIB-004`).
 */
export type StatementForm = ("call" | "perform" | "join" | "yield" | "spawn" | "cancel" | "await");

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});
