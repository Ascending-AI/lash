// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * One host authoring operation, lowered by [`plan`] to canonical edits.
 *
 * Lash owns this operation data and its serialized representation. Hosts own
 * their transport, revision and authentication envelope, embedding batches as
 * `Vec<Operation>`. Save the expanded [`Transaction`] as the exact edit record
 * to replay against its base and admission context. Re-planning operations
 * later is a new plan, not a replay: generated temporary names are not part of
 * the operation contract.
 */
export type Operation = ({
  "edit": Edit;
} | {
  "insert_effect": {
    /**
     * Arguments in evaluation order. Computed expressions become `let`s.
     */
    "args": Array<Expr>;
    /**
     * Insertion position in the transaction's base document.
     */
    "at": Position;
    /**
     * Caller-selected output name. It is preserved, not made fresh.
     */
    "binding": Name;
    /**
     * The effect to perform, checked against the admission environment.
     */
    "effect": QualifiedName;
    /**
     * The result type stated by the canonical `perform` action.
     */
    "result": Type;
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
 * What an [`Expr::Act`] does. `join`, `yield` and `cancel` stay statement
 * actions.
 */
export type ActKind = ({
  "call": Callee;
} | {
  "perform": {
    "effect": QualifiedName;
    "result": Type;
  };
} | "sleep" | {
  "spawn": Callee;
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
  };
} | {
  "sleep": {
    "duration": Atom;
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
});

/**
 * An argument of an action: a variable or a literal, never a computation.
 */
export type Atom = ({
  "variable": Name;
} | {
  "literal": Literal;
});

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
};

/**
 * One typed change to a document.
 */
export type Edit = ({
  "insert_statement": {
    "at": Position;
    "statement": Stmt;
  };
} | {
  "remove_statement": {
    "statement": Site;
  };
} | {
  "move_statement": {
    "statement": Site;
    "to": Position;
  };
} | {
  "clone_statement": {
    "statement": Site;
    "to": Position;
  };
} | {
  "replace_statement": {
    "statement": Site;
    "with": Stmt;
  };
} | {
  "replace_expression": {
    "expression": Site;
    "with": Expr;
  };
} | {
  "replace_action": {
    "action": Site;
    "with": Action;
  };
} | {
  "set_argument": {
    "action": Site;
    "argument": Atom;
    "index": number;
  };
} | {
  "set_condition": {
    "condition": Expr;
    "statement": Site;
  };
} | {
  "set_catch": {
    "catch"?: (Catch | null);
    "statement": Site;
  };
} | {
  "set_finally": {
    "finally"?: (Array<Stmt> | null);
    "statement": Site;
  };
} | {
  "rename_variable": {
    "declared_at": Site;
    "name": Name;
    "to": Name;
  };
} | {
  "insert_function": {
    "function": Function;
    "name": Name;
  };
} | {
  "remove_function": {
    "name": Name;
  };
} | {
  "replace_function": {
    "function": Function;
    "name": Name;
  };
} | {
  "rename_function": {
    "from": Name;
    "to": Name;
  };
} | {
  "set_private_bindings": {
    "names": Array<Name>;
  };
} | {
  "insert_entry": {
    "function": Name;
    "signature": Signature;
  };
} | {
  "remove_entry": {
    "function": Name;
  };
} | {
  "rename_entry": {
    "from": Name;
    "to": Name;
  };
} | {
  "set_entry_signature": {
    "function": Name;
    "signature": Signature;
  };
} | {
  "set_effect_signature": {
    "effect": QualifiedName;
    "signature": Signature;
  };
} | {
  "set_number_policy": {
    "numbers": NumberPolicy;
  };
} | {
  "replace_function_identity": {
    "from": FunctionId;
    "to": FunctionId;
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
} | "clock" | "random" | {
  "read": ProjectionRead;
} | {
  "act": ActExpr;
});

export type Float = string;

/**
 * A declared function's parameters and body.
 */
export type Function = {
  "body": Array<Stmt>;
  "params"?: Array<Name>;
};

export type FunctionId = string;

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

/**
 * How a document's effect results decode a number whose type says only
 * "number" (`K-EFF-006`).
 */
export type NumberPolicy = ("float" | "by_spelling");

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

export type Stmt = (("break" | "continue") | {
  "let": {
    "name": Name;
    "value": Rhs;
  };
} | {
  "assign": {
    "place": Place;
    "value": Rhs;
  };
} | {
  "remove": {
    "member": Member;
  };
} | {
  "do": {
    "action": Action;
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
    "iterable": Expr;
  };
} | {
  "while": {
    "body": Array<Stmt>;
    "condition": Expr;
  };
} | {
  "return": {
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
