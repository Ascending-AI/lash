// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * Every supported canvas operation, admitted atomically through [`crate::Draft::apply`].
 * Structural operations accept only separable perform/tool steps. While loops
 * support condition editing; creating or removing one requires source.
 */
export type Operation = ({
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
} | "yield" | "cancel" | {
  "await": FunctionTarget;
});

export type Branch = ("then" | "else" | "body");

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

export type DocumentId = string;

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
  "read": ProjectionRead;
} | {
  "act": ActExpr;
});

export type Float = string;

export type FunctionId = string;

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
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

export type Integer = string;

export type JoinMode = ("all" | "all_settled" | "race" | "any");

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

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

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
 * Where a statement is put: in `block`, ahead of the statement `before`,
 * or at the block's end.
 */
export type Position = {
  "before"?: (Site | null);
  "block": Site;
};

export type ProjectionRead = {
  "handle": Expr;
  /**
   * What is asked of the projection: kernel data the host interprets.
   */
  "request": Expr;
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
