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
    "effect": QualifiedName;
    "result": Type;
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
    "effect": QualifiedName;
    "result": Type;
    /**
     * As [`Action::Perform`]'s.
     */
    "then_yield"?: boolean;
  };
} | {
  "sleep": {
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
  "await": FunctionId;
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
    "effect": QualifiedName;
    "result": Type;
    /**
     * Whether the task takes one more turn of the ready queue once
     * the outcome is in, before it continues (`K-TASK-005`).
     */
    "then_yield"?: boolean;
  };
} | {
  "sleep": {
    "duration": Atom;
    /**
     * As [`Action::Perform`]'s.
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
  "await": {
    "policy": FunctionId;
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

export type Bytes = string;

/**
 * What a call or a `spawn` runs.
 */
export type Callee = ({
  "declared": Name;
} | {
  "value": Name;
} | {
  "library": FunctionId;
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
  "body": Array<Stmt>;
  "params"?: Array<Name>;
  /**
   * How a value this closure makes is started when it crosses to a tool
   * as a [`crate::FunctionValue`] (`K-CLO-004`); `None` for one that
   * cannot be started. Executable behaviour: it is part of the
   * document's identity.
   */
  "start"?: (StartAdapter | null);
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
} | "clock" | "random" | {
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
  "function": Name;
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

export type ProjectionRead = {
  "handle": Expr;
  /**
   * What is asked of the projection: kernel data the host interprets.
   */
  "request": Expr;
};

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
  "check"?: (FunctionId | null);
  /**
   * Takes what was walked and an item of it to the value bound.
   */
  "item"?: (FunctionId | null);
  /**
   * Steps a driver: an empty tuple when it is exhausted, else a tuple of
   * one item.
   */
  "next"?: (FunctionId | null);
  /**
   * Takes the iterated value to what the loop walks: a list, a tuple, a
   * map, a set or a text, which the loop's own cursor walks, or any
   * other value, a driver that `next` steps.
   */
  "open": FunctionId;
};

export type QualifiedName = string;

export type RecordEntry = {
  "field": string;
  "value": Expr;
};

export type RecordType = {
  "fields"?: Array<RecordTypeField>;
  /**
   * The type of every field not named in `fields`. `None` closes the
   * record: it has no other field.
   */
  "rest"?: (Type | null);
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

/**
 * How a function a closure makes is started from outside its run: the
 * signature it is started under, how the dialect holds the function as a
 * value, and how the entry calls it.
 */
export type StartAdapter = {
  "convention": StartConvention;
  "representation": FunctionRepresentation;
  "signature": Signature;
};

/**
 * How a host starts an entry. A start adapter uses the same convention
 * when it extracts a function value from a dialect callable.
 */
export type StartConvention = ("direct" | {
  "ts_direct": {
    "await_fn": FunctionId;
  };
} | {
  "ts_args_list": {
    "await_fn": FunctionId;
    "same_fn": FunctionId;
  };
} | {
  "py_direct": {
    "cleanup": FunctionId;
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
    "policy": FunctionId;
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

export type Type = (("null" | "absent" | "bool" | "int" | "float" | "text" | "bytes" | "timestamp" | "error") | "any" | "number" | {
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
