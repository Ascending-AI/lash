// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * Every supported canvas operation, admitted atomically through [`crate::Draft::apply`].
 * Structural operations accept only separable perform/tool steps. While loops
 * support condition editing; creating or removing one requires source.
 */
export type Operation = ({
  "rename_binding": {
    "base": DocumentId;
    "declaration": Site;
    "name": Name;
  };
} | {
  "insert_binding": {
    "at": Position;
    "base": DocumentId;
    "name": Name;
    "value": DataValue;
  };
} | {
  "remove_binding": {
    "base": DocumentId;
    "declaration": Site;
  };
} | {
  "rename_export": {
    "base": DocumentId;
    "from": Name;
    "to": Name;
  };
} | {
  "replace_document": {
    "base": DocumentId;
    "document": Document;
  };
} | {
  "set_value": {
    "base": DocumentId;
    "target": ValueTarget;
    "value": DataValue;
  };
} | {
  "insert_step": {
    "args": Array<DataValue>;
    "at": Position;
    "base": DocumentId;
    "binding"?: (Name | null);
    "result": Type;
    "slot": ImportSlot;
  };
} | {
  "remove_step": {
    "action": Site;
    "base": DocumentId;
  };
} | {
  "move_step": {
    "action": Site;
    "base": DocumentId;
    "to": Position;
  };
} | {
  "wrap_steps": {
    "base": DocumentId;
    "first": Site;
    "last": Site;
    "wrapper": Wrapper;
  };
} | {
  "unwrap_steps": {
    "base": DocumentId;
    "branch": Branch;
    "wrapper": Site;
  };
} | {
  "change_condition": {
    "base": DocumentId;
    "condition": Expr;
    "statement": Site;
  };
});

/**
 * An action inside an expression: the action forms that may nest, with
 * expression arguments.
 */
export type ActExpr = {
  "args"?: Array<Expr>;
  "kind": ActKind;
};

/**
 * What an [`Expr::Act`] does. `yield` and `cancel` stay statement
 * actions.
 */
export type ActKind = ({
  "call": Callee;
} | {
  "perform": {
    "result": Type;
    "slot": ImportSlot;
    /**
     * As [`Action::Perform`]'s.
     */
    "then_yield"?: boolean;
  };
} | {
  "spawn": Callee;
} | "join" | {
  "join_many": JoinMode;
} | "yield" | "cancel" | "cancel_effect" | {
  "await": FunctionTarget;
});

/**
 * An action as the whole right-hand side of its own statement, its
 * arguments atoms (`K-FORM-001`); [`Expr::Act`] nests one in an
 * expression.
 */
export type Action = ({
  "call": {
    "args": Array<Atom>;
    "callee": Callee;
  };
} | {
  "perform": {
    "args": Array<Atom>;
    "result": Type;
    "slot": ImportSlot;
    /**
     * Whether the task takes one more turn of the ready queue once
     * the outcome is in, before it continues (`K-TASK-005`).
     */
    "then_yield"?: boolean;
  };
} | {
  "join": {
    "task": Atom;
  };
} | {
  "join_many": {
    "mode": JoinMode;
    "tasks": Atom;
  };
} | "yield" | {
  "spawn": {
    "args": Array<Atom>;
    "callee": Callee;
  };
} | {
  "cancel": {
    "task": Atom;
  };
} | {
  "cancel_effect": {
    "task": Atom;
  };
} | {
  "await": {
    "policy": FunctionTarget;
    "value": Atom;
  };
});

/**
 * The order an assignment to a member evaluates its parts in
 * (`K-EVAL-003`): each dialect's own, written in the document.
 */
export type AssignOrder = ("value_first" | "place_first");

/**
 * An argument of an action: a variable or a literal, never a computation.
 */
export type Atom = ({
  "variable": Name;
} | {
  "literal": Literal;
});

export type Branch = ("then" | "else" | "body");

/**
 * Nominal type or error identity: its defining package and canonical name, independent of representation.
 */
export type Brand = {
  "name": Segment;
  "package": PackagePath;
};

export type Bytes = string;

/**
 * How a function a closure makes is started from outside its run: the
 * signature it is started under, how the dialect holds the function as a
 * value, and how the entry calls it.
 */
export type CallablePresentation = {
  "convention": StartConvention;
  "representation": FunctionRepresentation;
  "signature": Signature;
};

/**
 * What a call or a `spawn` runs.
 */
export type Callee = ({
  "local": FunctionId;
} | {
  "import": ImportSlot;
} | {
  "value": Name;
});

export type Catch = {
  /**
   * The variable the caught error value is bound to.
   */
  "binding": Name;
  "body": Array<Stmt>;
};

/**
 * A function value written in place. It shares the variables of the scope
 * that defines it (`K-CLO-001`); which ones is derived, never written.
 */
export type Closure = {
  /**
   * The anonymous body's own identity, local to the defining Document.
   */
  "function": FunctionId;
  "presentation"?: (CallablePresentation | null);
};

/**
 * Semantic call convention. Compatibility compares this variant, not another document's helper IDs.
 */
export type Convention = ({
  "kind": "direct";
} | {
  "kind": "py_args_list";
} | {
  "kind": "ts_direct";
} | {
  "kind": "ts_args_list";
} | {
  "kind": "py_direct";
});

export type DataMapEntry = {
  "key": DataValue;
  "value": DataValue;
};

export type DataRecordEntry = {
  "field": string;
  "value": DataValue;
};

/**
 * Data that can be connected to a value slot; no executable syntax.
 */
export type DataValue = ({
  "timestamp": {
    "constructor": FunctionId;
    "nanoseconds": Integer;
  };
} | {
  "error": {
    "constructor": FunctionId;
    "data": DataValue;
    "kind": string;
    "message": string;
  };
} | {
  "literal": Literal;
} | {
  "variable": Name;
} | {
  "tuple": Array<DataValue>;
} | {
  "list": Array<DataValue>;
} | {
  "set": Array<DataValue>;
} | {
  "map": Array<DataMapEntry>;
} | {
  "record": Array<DataRecordEntry>;
});

export type Document = {
  "exports": {
    [key: string]: Export;
  };
  "functions": Record<string, FunctionDefinition>;
  "imports": Record<string, Import>;
  "library"?: LibraryDependencies;
  "main"?: (Array<Stmt> | null);
  "numbers": NumberPolicy;
  "private_bindings": Array<Name>;
  "requires": NodeRequirements;
};

export type DocumentId = string;

export type Export = ({
  "kind": "function";
  "value": FunctionExport;
} | {
  "kind": "type";
  "value": TypeDef;
} | {
  "kind": "error";
  "value": {
    "kind": Brand;
  };
});

/**
 * An expression, evaluated within one statement. Only an [`Expr::Act`]
 * in it may pause the task (`K-EVAL-008`).
 */
export type Expr = ({
  "literal": Literal;
} | {
  "variable": Name;
} | {
  "if": {
    "cond": Expr;
    "else": Expr;
    "then": Expr;
  };
} | {
  "let": {
    "body": Expr;
    "name": Name;
    "value": Expr;
  };
} | {
  "tuple": Array<Expr>;
} | {
  "list": Array<Expr>;
} | {
  "map": Array<MapEntry>;
} | {
  "set": Array<Expr>;
} | {
  "record": Array<RecordEntry>;
} | {
  "member": Member;
} | {
  "closure": Closure;
} | {
  "call": {
    "args": Array<Expr>;
    "function": FunctionId;
  };
} | {
  "act": ActExpr;
});

export type Float = string;

/**
 * An amount computed from the sizes of a call's arguments and result, in
 * saturating unsigned 64-bit arithmetic (`K-CHG-003`).
 */
export type Formula = ({
  "constant": number;
} | {
  "size": Operand;
} | {
  "deep_size": Operand;
} | {
  "nested_size": Operand;
} | {
  "magnitude": Operand;
} | {
  "sum": Array<Formula>;
} | {
  "product": Array<Formula>;
} | {
  "max": Array<Formula>;
} | {
  "min": Array<Formula>;
} | {
  "ceil_log2": Formula;
});

/**
 * A library function's kernel-code body. Its parameters are the
 * signature's, by name.
 */
export type FunctionBody = {
  "block": Array<Stmt>;
  /**
   * Every library function the body calls, by identity, with the name
   * its definition carries.
   */
  "functions"?: Record<string, FunctionName>;
  /**
   * Every operation the body performs itself, with what it requires of
   * it (`K-LIB-004`). A body beside a native implementation performs
   * nothing. What the functions it calls perform is theirs to list.
   */
  "imports"?: Record<string, Import>;
};

/**
 * A library function: its name, its typed signature over kernel values,
 * its error kinds, what a call is charged, the guard that bounds its work,
 * and how it is implemented. Its identity is the hash of all of it, so a
 * change to any part is a new function.
 */
export type FunctionDefinition = {
  /**
   * The kinds of error a call may raise, beyond the kernel's own.
   */
  "errors"?: Array<string>;
  /**
   * What a call is charged, whichever implementation runs.
   * The work bound of a native implementation that can run away.
   */
  "guard"?: (Guard | null);
  "implementation": Implementation;
  /**
   * The kernel version the definition is written for.
   */
  "kernel": number;
  "name": FunctionName;
  /**
   * The version of the native implementation (`K-LIB-011`): its code is
   * not part of the definition, so a native that answers otherwise is
   * stated anew under the next version, which is a new function. Only a
   * definition that states a native implementation states one other
   * than the first.
   */
  "native_version"?: number;
  "projection"?: ProjectionRole;
  "signature": Signature;
};

/**
 * Public call signature, settled result and defining convention. Startability is exactly function kind plus data_only.
 */
export type FunctionExport = {
  "asynchronous": boolean;
  "convention": StartConvention;
  "function": FunctionId;
  "rest": boolean;
  "signature": Signature;
};

export type FunctionId = string;

export type FunctionName = string;

/**
 * How a dialect holds a function as a value.
 */
export type FunctionRepresentation = ("bare" | {
  "token": {
    "callable": number;
    "tag": string;
  };
});

/**
 * A convention helper local to this document or named by an explicit Import slot.
 */
export type FunctionTarget = ({
  "kind": "local";
  "value": FunctionId;
} | {
  "kind": "import";
  "value": ImportSlot;
});

/**
 * A native implementation's work bound: the unit it counts and the most
 * units one call may spend. A call that would pass the limit ends with a
 * typed bound error (`K-LIB-008`).
 */
export type Guard = {
  /**
   * The limit, over the arguments.
   */
  "limit": Formula;
  /**
   * What one unit of work is, in words a second implementation can
   * count by: `"backtracking step"`.
   */
  "unit": string;
};

export type HelperLibraryId = string;

/**
 * One generation floor per archive lineage, with exact code outside those archives.
 */
export type HelperRequirements = {
  "archives": Record<string, number>;
  "outside": Record<string, Array<FunctionId>>;
};

/**
 * How a library function is implemented.
 */
export type Implementation = ({
  "guest": {
    "body": FunctionBody;
    "metering": Metering;
    [key: string]: unknown;
  };
} | {
  "native": {
    "charge": Formula;
    [key: string]: unknown;
  };
} | {
  "both": {
    "body": FunctionBody;
    "charge": Formula;
    [key: string]: unknown;
  };
});

/**
 * A named requirement on external code, an operation, a type, or an error.
 */
export type Import = {
  "kind": ImportKind;
  "name": QualifiedName;
};

/**
 * The compatibility facts required to fill an Import. Natives and cell variables are not Imports.
 */
export type ImportKind = ({
  "kind": "operation";
  "value": {
    "signature": Signature;
  };
} | {
  "kind": "code";
  "value": {
    "convention": Convention;
    "signature": Signature;
  };
} | {
  "kind": "type";
  "value": {
    "brand": Brand;
  };
} | {
  "kind": "error";
  "value": {
    "kind": Brand;
  };
});

/**
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

export type Integer = string;

export type JoinMode = ("all" | "all_settled" | "race" | "any");

/**
 * Direct exact-ID references to immutable build library code. These are
 * disjoint from the document's own definitions and participate in its identity.
 */
export type LibraryDependencies = Array<FunctionId>;

export type Literal = (("null" | "absent") | {
  "bool": boolean;
} | {
  "int": Integer;
} | {
  "float": Float;
} | {
  "text": string;
} | {
  "bytes": Bytes;
} | {
  "function": FunctionId;
});

export type MapEntry = {
  "key": Expr;
  "value": Expr;
};

export type MapType = {
  "key": Type;
  "value": Type;
};

/**
 * A field of a record or an index into a collection.
 */
export type Member = ({
  "field": {
    "field": string;
    "target": Expr;
  };
} | {
  "index": {
    "index": Expr;
    "target": Expr;
  };
});

export type Metering = ({
  "kind": "statements";
} | {
  "kind": "formula";
  "value": Formula;
});

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * Exact helper manifests, ordinary natives and kernel versions required by selected executable reach.
 */
export type NodeRequirements = {
  /**
   * Cached availability debt of reached helpers, including interpreted bodies.
   */
  "helpers"?: HelperRequirements;
  "kernels": Array<number>;
  /**
   * Exact build-owned helper manifests, each discharging its native debt.
   */
  "libraries": Array<HelperLibraryId>;
  "natives": Array<FunctionId>;
};

/**
 * How a document's effect results decode a number whose type says only
 * "number" (`K-EFF-006`).
 */
export type NumberPolicy = ("float" | "by_spelling");

/**
 * What a [`Formula`] measures.
 */
export type Operand = ({
  "param": Name;
} | "result");

export type PackagePath = string;

export type Param = {
  "name": Name;
  /**
   * The argument may be omitted, and is then absent (`K-FN-004`). No
   * required parameter follows an optional one.
   */
  "optional"?: boolean;
  "ty": Type;
};

/**
 * Where an assignment writes.
 */
export type Place = ({
  "variable": Name;
} | {
  "member": Member;
});

/**
 * Where a statement is put: in `block`, ahead of the statement `before`,
 * or at the block's end.
 */
export type Position = {
  "before"?: (Site | null);
  "block": Site;
};

/**
 * A build helper's role in the authored workflow projection.
 */
export type ProjectionRole = ("authored" | "argument_copy" | "result_decode" | "duration_milliseconds" | "timer_seconds" | "implicit_return");

/**
 * A dialect's iteration protocol: the library functions a `for … via`
 * calls at its header, by identity (`K-ITER-005`). Each is listed in the
 * manifest as any library function a document reaches.
 */
export type Protocol = {
  /**
   * Checks a map or a set against its size when the loop opened it and
   * the items it may still give, after each step; gives that budget
   * anew.
   */
  "check"?: (FunctionTarget | null);
  /**
   * Takes what was walked and an item of it to the value bound.
   */
  "item"?: (FunctionTarget | null);
  /**
   * Steps a driver: an empty tuple when it is exhausted, else a tuple of
   * one item.
   */
  "next"?: (FunctionTarget | null);
  /**
   * Takes the iterated value to what the loop walks: a list, a tuple, a
   * map, a set or a text, which the loop's own cursor walks, or any
   * other value, a driver that `next` steps.
   */
  "open": FunctionTarget;
};

/**
 * An Import name parsed by segment, optionally addressing a type member.
 */
export type QualifiedName = {
  "member": Segment;
  "package": PackagePath;
  "type_member"?: (Segment | null);
};

export type RecordEntry = {
  "field": string;
  "value": Expr;
};

/**
 * A record either refuses, discards, or retains undeclared fields.
 */
export type RecordRest = (("closed" | "drop") | {
  "keep": Type;
});

export type RecordType = {
  "fields"?: Array<RecordTypeField>;
  /**
   * What happens to fields not named in `fields` at a boundary.
   */
  "rest"?: RecordRest;
};

export type RecordTypeField = {
  "name": string;
  /**
   * The field may be missing.
   */
  "optional"?: boolean;
  "ty": Type;
};

/**
 * The right-hand side of a `let` or an assignment.
 */
export type Rhs = ({
  "expr": Expr;
} | {
  "action": Action;
});

export type Segment = string;

/**
 * What a function, an effect or an entry takes and returns.
 */
export type Signature = {
  "params"?: Array<Param>;
  "result": Type;
};

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

export type StartConvention = ({
  "kind": "direct";
} | {
  "kind": "py_args_list";
  "value": {
    "positional": number;
  };
} | {
  "kind": "ts_direct";
  "value": {
    "await_fn": FunctionTarget;
  };
} | {
  "kind": "ts_args_list";
  "value": {
    "await_fn": FunctionTarget;
    "same_fn": FunctionTarget;
  };
} | {
  "kind": "py_direct";
  "value": {
    "cleanup": FunctionTarget;
  };
});

export type Stmt = (("break" | "continue") | {
  "let": {
    "name": Name;
    "value": Rhs;
  };
} | {
  "assign": {
    "order"?: AssignOrder;
    "place": Place;
    "value": Rhs;
  };
} | {
  "remove": {
    "member": Member;
  };
} | {
  "do": {
    "value": Expr;
  };
} | {
  "if": {
    "condition": Expr;
    "else_block"?: Array<Stmt>;
    "then_block": Array<Stmt>;
  };
} | {
  "for": {
    "binding": Name;
    "body": Array<Stmt>;
    "else_block"?: Array<Stmt>;
    "iterable": Expr;
    /**
     * Declared units at every continuation test, even in library code.
     * Zero leaves the loop covered by the call's completion formula.
     */
    "metered"?: number;
    "via"?: (Protocol | null);
  };
} | {
  "while": {
    "body": Array<Stmt>;
    "condition": Expr;
    "else_block"?: Array<Stmt>;
    /**
     * As the declared loop charge of [`Stmt::For`].
     */
    "metered"?: number;
  };
} | {
  "return": {
    "value": Expr;
  };
} | {
  "return_via": {
    "policy": FunctionTarget;
    "value": Expr;
  };
} | {
  "try": TryStmt;
} | {
  "throw": {
    "value": Expr;
  };
} | {
  "print": {
    "value": Expr;
  };
} | {
  "finish": {
    "value": Expr;
  };
} | {
  "fail": {
    "value": Expr;
  };
});

export type TryStmt = {
  "body": Array<Stmt>;
  "catch"?: (Catch | null);
  "finally"?: (Array<Stmt> | null);
};

export type Type = (("null" | "absent" | "bool" | "int" | "float" | "text" | "bytes" | "timestamp" | "error") | "any" | "json" | "number" | {
  "tuple": Array<Type>;
} | {
  "list": Type;
} | {
  "map": MapType;
} | {
  "set": Type;
} | {
  "record": RecordType;
} | {
  "enum": Array<string>;
} | {
  "function": Signature;
} | {
  "task": Type;
} | {
  "handle": string;
} | {
  "union": Array<Type>;
});

/**
 * A Brand paired with representation, executable checker and named members; validated together at admission.
 */
export type TypeDef = {
  "brand": Brand;
  "checker": FunctionId;
  "members": Record<string, TypeMember>;
  "representation": Type;
};

/**
 * A static function, explicit-receiver method, or property in its owning TypeDef.
 */
export type TypeMember = ({
  "kind": "static";
  "value": FunctionExport;
} | {
  "kind": "method";
  "value": FunctionExport;
} | {
  "kind": "property";
  "value": FunctionExport;
});

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});

/**
 * All addresses are exact sites in the operation's base document.
 */
export type ValueTarget = ({
  "literal": {
    "expression": Site;
  };
} | {
  "initializer": {
    "statement": Site;
  };
} | {
  "tool_argument": {
    "action": Site;
    "index": number;
    /**
     * Canonical child indices within the argument, empty for its root.
     */
    "path"?: Array<number>;
  };
});

export type Wrapper = ({
  "if": {
    "condition": Expr;
  };
} | {
  "for": {
    "binding": Name;
    "iterable": Expr;
  };
} | {
  "try": {
    "catch_binding": Name;
  };
});
