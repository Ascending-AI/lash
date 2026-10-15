// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * One entry of a document as a canvas reads it.
 */
export type WorkflowView = {
  "dataflow": Dataflow;
  /**
   * Every action of the document a run can report an occurrence at, in
   * node order. An entry may call and spawn any declared function, so
   * these are the sites of every unit.
   */
  "execution_sites": Array<ExecutionSiteView>;
  /**
   * Every block of the entry, including empty ones, in document order.
   */
  "scopes": Array<ScopeView>;
  /**
   * The entry's statements in document order, nested blocks after the
   * statement that holds them.
   */
  "statements": Array<StatementView>;
};

/**
 * A form that is the whole right-hand side of its own statement
 * (`K-STMT-001`).
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
 * One action.
 */
export type ActionView = {
  "args": Array<ArgumentView>;
  "available": Array<number>;
  "block": Site;
  /**
   * The statically named target of a call, spawn or perform.
   * Calls through values have no static callee name.
   */
  "callee"?: (CalleeView | null);
  /**
   * What an assignment of the action's result writes.
   */
  "destination"?: (Place | null);
  "kind": SiteKind;
  /**
   * A `perform`'s declared result, else the checker's facet.
   */
  "result_type": Type;
  "site": Site;
  "statement": Site;
};

/**
 * One argument of an action.
 */
export type ArgumentView = {
  "atom": Atom;
  /**
   * The binding a variable argument reads.
   */
  "binding"?: (number | null);
  /**
   * The parameter or builtin operand the kernel names here.
   */
  "name"?: (Name | null);
  /**
   * That parameter's type; `any` for an untyped declared parameter or
   * when no signature names one.
   */
  "type": Type;
};

/**
 * One assignment.
 */
export type AssignmentView = {
  "destination": Place;
  "statement": Site;
  "value_site": Site;
};

/**
 * An argument of an action: a variable or a literal, never a computation.
 */
export type Atom = ({
  "variable": Name;
} | {
  "literal": Literal;
});

/**
 * How a variable is declared.
 */
export type BindingKind = ("param" | "let" | "for" | "catch");

/**
 * One variable declaration.
 */
export type BindingView = {
  /**
   * Some statement assigns it after its declaration.
   */
  "assigned": boolean;
  /**
   * The block it is declared in.
   */
  "block": Site;
  /**
   * A closure shares it.
   */
  "captured": boolean;
  /**
   * The binding's id in this derivation.
   */
  "id": number;
  "kind": BindingKind;
  "name": Name;
  /**
   * The node that declares it.
   */
  "site": Site;
  /**
   * The checker's facet of what it holds.
   */
  "type": Type;
  /**
   * Every node that reads it, in document order.
   */
  "uses": Array<Use>;
  /**
   * The site of a `let`'s value.
   */
  "value_site"?: (Site | null);
};

export type Bytes = string;

/**
 * What a statement-position call or a `spawn` runs.
 */
export type Callee = ({
  "declared": Name;
} | {
  "value": Name;
} | {
  "library": FunctionId;
});

/**
 * A statically named call target. Declared names are arbitrary kernel names;
 * library names are qualified [`FunctionName`]s, and effects use their
 * manifest names.
 */
export type CalleeView = ({
  "library": {
    "id": FunctionId;
    "name": QualifiedName;
    [key: string]: unknown;
  };
} | {
  "declared": Name;
} | {
  "effect": QualifiedName;
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
 * What a variable is and where values flow in the entry.
 */
export type Dataflow = {
  /**
   * Every action of the entry, in document order.
   */
  "actions": Array<ActionView>;
  /**
   * Every assignment of the entry.
   */
  "assignments": Array<AssignmentView>;
  /**
   * Every variable the entry declares, in declaration order.
   */
  "bindings": Array<BindingView>;
  /**
   * Every expression of the entry, in document order.
   */
  "expressions": Array<ExpressionView>;
  /**
   * The entry's parameters, by binding id.
   */
  "parameters": Array<number>;
  /**
   * Every `return` and `finish` of the entry and the callable it ends.
   */
  "returns": Array<ReturnView>;
};

/**
 * One action and the coordinates it runs under.
 */
export type ExecutionSiteView = {
  "kind": SiteKind;
  /**
   * The loops enclosing it inside its own function body, outermost
   * first.
   */
  "loops": Array<Site>;
  /**
   * The action's site.
   */
  "site": Site;
  /**
   * The statement whose right-hand side it is: where its task stands
   * while it waits.
   */
  "statement": Site;
};

/**
 * An expression: evaluated within one statement, it never pauses a task.
 */
export type Expr = ({
  "literal": Literal;
} | {
  "variable": Name;
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
});

/**
 * One expression.
 */
export type ExpressionView = {
  /**
   * The bindings a name at this site resolves to, by id: one per name,
   * as lexical lookup finds it. A binding's own initializer cannot read
   * it.
   */
  "available": Array<number>;
  "block": Site;
  "expression": Expr;
  /**
   * The bindings it reads, by id. A closure's body is its own.
   */
  "references": Array<number>;
  "site": Site;
  /**
   * The statement the expression belongs to.
   */
  "statement": Site;
  /**
   * The checker's facet of its value.
   */
  "type": Type;
};

export type Float = string;

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
 * One `return` or `finish`.
 */
export type ReturnView = {
  /**
   * What it ends: the enclosing closure, or the entry (a `finish`
   * always ends the entry).
   */
  "callable": Site;
  /**
   * The result type that callable declares.
   */
  "expected_type": Type;
  "expression_site": Site;
  "statement": Site;
  /**
   * The checker's facet of the value.
   */
  "type": Type;
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
 * What a block is the body of.
 */
export type ScopeKind = ("entry" | "closure" | "then" | "else" | "for" | "while" | "try" | "catch" | "finally");

/**
 * One block of the entry and what opens it.
 */
export type ScopeView = {
  "block": Site;
  /**
   * How many statements enclose this block inside the entry's body:
   * the same depth its statements carry.
   */
  "depth": number;
  "kind": ScopeKind;
  /**
   * The document ordinal of this block, including nonempty blocks.
   */
  "ordinal": number;
  /**
   * The statement that opens this block; absent for the entry body.
   */
  "statement"?: (Site | null);
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

/**
 * What an action does.
 */
export type SiteKind = ("call" | "perform" | "sleep" | "join" | "join_many" | "yield" | "spawn" | "cancel");

/**
 * One statement of the entry.
 */
export type StatementView = {
  /**
   * The site of the statement's action, when it has one: where a run
   * reports it.
   */
  "action"?: (Site | null);
  /**
   * The block the statement is in.
   */
  "block": Site;
  /**
   * How many statements enclose this one inside the entry's body.
   */
  "depth": number;
  /**
   * The document ordinal shared with run counts and replay paths.
   */
  "ordinal": number;
  "site": Site;
  /**
   * The statement as the document writes it.
   */
  "statement": Stmt;
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

/**
 * One read of a binding and the statement that contains it.
 */
export type Use = {
  "site": Site;
  "statement": Site;
};
