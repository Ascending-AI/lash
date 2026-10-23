// Derived from v1.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * One entry of a document as a canvas reads it.
 */
export type WorkflowView = {
  "callables": Array<CallableView>;
  "canonical_source": string;
  "dataflow": Dataflow;
  "document": DocumentId;
  /**
   * The callable the view opens: its declared signature and what a call
   * of it can end with.
   */
  "entry": EntryView;
  /**
   * Every action of the document a run can report an occurrence at, in
   * node order. An entry may call and spawn any declared function, so
   * these are the sites of every unit.
   */
  "execution_sites": Array<ExecutionSiteView>;
  "flow": Array<FlowEdge>;
  "nodes": Array<WorkflowNode>;
  "readable": ReadableLens;
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
  "await": {
    "policy": FunctionTarget;
    "value": Atom;
  };
});

export type ActionActivity = {
  "occurrences": Array<ActionObservation>;
  "site": Site;
  /**
   * The state its occurrences give it, `None` with none. A run commits
   * occurrences for its effects and its waits only (FIG-6810). A library
   * call receives its body's effects through their authored `origin`; the
   * occurrences keep their library identities. A pure call, and a `sleep`
   * that never parked, has no occurrence of its own. A
   * process's live feed may report one as entered, never as completed
   * or failed. A run that failed names the site that raised in its
   * terminal failure's `UncaughtError` cause: a wait action, or the
   * statement a library call raised in, whose actions are the nodes with
   * that `statement`.
   */
  "state"?: (ActionState | null);
};

/**
 * Outcome data stays on the exact occurrence the observer supplies, never on
 * every action of its statement. `T` can retain the host's typed result/cause.
 */
export type ActionObservation = {
  "identity": EffectId;
  /**
   * The authored statement supplied by the effect request. This is display
   * attribution; `identity` keeps the actual performing unit and occurrence.
   */
  "origin": Site;
  "outcome"?: unknown;
  "state": ActionState;
};

/**
 * Whose code or which outside party an action reaches.
 */
export type ActionRole = ("effect" | "wait" | "sub_workflow" | "library" | "task");

/**
 * What an occurrence of an action did. `Recovered` is a failure whose
 * error a `catch` took, so the run went on (FIG-6817): the process
 * overlay's `recovered` row. `Failed` is a failure nothing recovered from.
 */
export type ActionState = ("running" | "waiting" | "completed" | "failed" | "recovered" | "cancelled" | "incomplete");

/**
 * One action.
 */
export type ActionView = {
  /**
   * The may-call set of a call or `spawn` through a value, and of the
   * document callables a library callee applies, deduplicated in site
   * order. Union all writes and follow aliases, parameters from document
   * call sites, collection members selected by kernel library bodies,
   * and library results (including wrappers that apply an input).
   * A closure is named by its expression's site, a [`CallableView::site`];
   * a declared function by `Site::new(unit, [])`, opened by [`derive_callable`].
   * Host inputs, perform results and native-only library results supply
   * no known callable. Session writes in this document are followed;
   * earlier session values, including saved functions, are underived:
   * [`WorkflowDocument`] supplies no session binding-to-function identities.
   * `Promise.all` and `asyncio.gather` only await tasks; the nested actions
   * that start their members name the closures, not the aggregate.
   */
  "applies"?: Array<Site>;
  "args": Array<ArgumentView>;
  "available": Array<number>;
  "block": Site;
  /**
   * The statically named target of a call, spawn or perform.
   * Calls through values have no static callee name.
   */
  "callee"?: (CalleeView | null);
  /**
   * What an assignment of the action's result writes, when the action
   * is its whole right-hand side.
   */
  "destination"?: (Place | null);
  "kind": SiteKind;
  /**
   * Every canonical effect this action reaches, including calls through
   * library bodies and statically known declared callees. Derived from the
   * checker's effect derivation, in canonical order with duplicates removed.
   */
  "performs": Array<OperationUse>;
  /**
   * A `perform`'s declared result, else the checker's facet.
   */
  "result_type": Type;
  /**
   * Whose code or which outside party the action reaches: what a host
   * picks the steps a person recognises by, never a callee's name.
   */
  "role": ActionRole;
  "site": Site;
  "statement": Site;
  /**
   * A timer's statically known duration, after its pinned helper's conversion.
   * Whole milliseconds, represented as seconds and nanoseconds in JSON.
   * Absent for computed arguments, unrecognized helpers and invalid literals.
   * This is an authored duration, not a running timer's remaining time.
   */
  "static_duration"?: (Duration | null);
};

/**
 * One argument of an action.
 */
export type ArgumentView = {
  /**
   * The binding a variable argument reads.
   */
  "binding"?: (number | null);
  /**
   * The argument as the document writes it. A statement's own action
   * writes a variable or a literal.
   */
  "expression": Expr;
  /**
   * The parameter or builtin operand the kernel names here.
   */
  "name"?: (Name | null);
  /**
   * The argument's node, for an action nested in an expression: the
   * expression an edit replaces. A statement's own action writes its
   * arguments in place, with no node of their own.
   */
  "site"?: (Site | null);
  /**
   * That parameter's type; `any` for an untyped declared parameter or
   * when no signature names one.
   */
  "type": Type;
};

/**
 * The order an assignment to a member evaluates its parts in
 * (`K-EVAL-003`): each dialect's own, written in the document.
 */
export type AssignOrder = ("value_first" | "place_first");

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
export type BindingKind = ("capture" | "param" | "let" | "for" | "catch");

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
   * The `perform` whose reply a `let` nothing reassigns holds: the site
   * of the effect's action in [`Dataflow::actions`], for TypeScript's
   * `const r = await tools.x(...)` and Python's `r = await tools.x(...)`,
   * and for an `await` of a promise or task that only performs it.
   * Absent when the value is computed from a reply, such as one of its
   * fields: that value comes from the code. Derived, never stored.
   */
  "produced_by"?: (Site | null);
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
 * A lexical callable is projected once, independently of its invocations.
 */
export type CallableView = {
  "block": Site;
  "site": Site;
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
 * A statically named call target. Declared names are arbitrary kernel names;
 * library names are qualified [`FunctionName`]s, and effects use their
 * manifest names.
 */
export type CalleeView = ({
  "library": {
    /**
     * Every visible Export backed by this identity in the retained
     * Packages. A private definition has no Exports.
     */
    "exports": Array<ExportName>;
    "id": FunctionId;
    "link": LinkId;
    "name": FunctionName;
    [key: string]: unknown;
  };
} | {
  "import": ImportSlot;
} | {
  "timer": {
    "use_": OperationUse;
    [key: string]: unknown;
  };
} | {
  "effect": OperationUse;
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
 * A condition that is one comparison of values a host can name. A binary
 * comparison reads `left` then `right`; `contains` holds when `left`
 * holds `right`, `starts_with` and `ends_with` when `left` begins or ends
 * with `right`. Each holds under its dialect's own equality, order and
 * truth: TypeScript's `===` and Python's `==` both read as `eq`.
 */
export type ComparisonView = ({
  "left": ValueRef;
  "op": "eq";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "ne";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "lt";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "le";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "gt";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "ge";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "contains";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "starts_with";
  "right": ValueRef;
} | {
  "left": ValueRef;
  "op": "ends_with";
  "right": ValueRef;
} | {
  "op": "not";
  "operand": ValueRef;
});

/**
 * A control retains empty arms and the distinction between body entries and
 * header actions. Header actions live under `header`, not before the control.
 */
export type ControlArm = {
  "block": Site;
  "role": ScopeKind;
};

export type ControlHeader = {
  "actions": Array<Site>;
  /**
   * The comparison a condition decides, read through a variable the
   * lowering bound to it in the statement right before the control and
   * that nothing else reads or reassigns. Absent when the condition is
   * not one comparison of parameters, literals, action results and
   * their record fields; never a guess.
   */
  "comparison"?: (ComparisonView | null);
  "expression": Site;
  "role": HeaderRole;
};

/**
 * What a variable is and where values flow in the entry.
 */
export type Dataflow = {
  /**
   * Every action of the entry, a statement's own and each one nested in
   * an expression, in document order.
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

export type DocumentId = string;

export type Duration = {
  "nanos": number;
  "secs": number;
  [key: string]: unknown;
};

export type EffectId = string;

/**
 * One declared parameter.
 */
export type EntryParamView = {
  /**
   * Its binding in `dataflow`.
   */
  "binding"?: (number | null);
  "name": Name;
  /**
   * A caller may leave it out.
   */
  "optional": boolean;
  /**
   * The type the signature declares, not the binding's facet: `any` for
   * a function the document does not list as an entry.
   */
  "type": Type;
};

/**
 * The callable a view opens.
 */
export type EntryView = {
  /**
   * The declared function; absent for the document's `main`.
   */
  "name"?: (Name | null);
  /**
   * Its parameters in order, as its signature declares them.
   */
  "params": Array<EntryParamView>;
  /**
   * The result type its signature declares; `any` when it declares none.
   */
  "result": Type;
  /**
   * What a call can end with: the union of the facets of the `return`s
   * and `finish`es that end this callable, except those that only follow
   * a statement that cannot complete, with `null` when the body can
   * complete and fall off its end. A lowering that spells falling off as an
   * explicit `return` (TypeScript's `return absent`) contributes that
   * return's facet instead.
   */
  "result_facet": Type;
  "written_signature"?: (WrittenSignature | null);
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
   * The statement it is part of: where its task stands while it waits.
   */
  "statement": Site;
  /**
   * The innermost action it is an argument of, when it is nested in
   * one: in `call f(perform g() as Any)`, the call.
   */
  "within"?: (Site | null);
};

/**
 * A document Export, optionally a member of its TypeDef. Type members are not start selectors.
 */
export type ExportName = {
  "member"?: (Segment | null);
  "name": Name;
};

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

/**
 * Canonical routes at exact member Sites, including finally-unwind routes.
 * Hosts must not invent a fallthrough between coarse groups.
 */
export type FlowEdge = {
  "from": Site;
  "kind": FlowKind;
  "to": FlowTarget;
};

export type FlowKind = ("next" | "then" | "else" | "body" | "break" | "continue" | "return" | "throw" | "catch" | "finally" | "end" | "call" | "spawn" | "reference");

export type FlowTarget = (("leave" | "end") | {
  "site": Site;
});

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

export type HeaderRole = ("condition" | "iterable");

/**
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

export type Integer = string;

export type JoinMode = ("all" | "all_settled" | "race" | "any");

/**
 * An expression alternative that guards an action's evaluation. These are
 * ancestry, not scheduling edges: static issue order never implies both arms run.
 */
export type LazyArm = {
  "arm": number;
  "expression": Site;
};

/**
 * Inclusive, one-based lines in an explicitly named printed lens.
 */
export type LineRange = {
  "first": number;
  "last": number;
};

export type LinkId = string;

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

/**
 * Coverage ready for a coarse node; loop iterations remain arm entries.
 */
export type NodeCoverage = {
  "arms": Array<ScopeCoverage>;
  "condition_tests"?: (number | null);
  /**
   * Action issues, control starts, or code-group entries (first member only).
   */
  "runs"?: (number | null);
  "site": Site;
};

/**
 * One operation Import in one immutable Link. All dispatch, modes and pins use this identity.
 */
export type OperationUse = {
  "link": LinkId;
  "slot": ImportSlot;
};

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
 * A failed readable lens does not invalidate canonical membership or ranges.
 */
export type ReadableLens = ({
  "kind": "unrequested";
} | {
  "kind": "available";
  "name": string;
} | {
  "cause": ReadableRefusal;
  "kind": "refused";
  "name": string;
});

/**
 * Typed printer refusal. Projection and the canonical lens remain available.
 */
export type ReadableRefusal = ({
  "kind": "unprintable";
  "site": Site;
} | {
  "kind": "bytes";
  "limit": number;
} | {
  "kind": "depth";
  "limit": number;
} | {
  "kind": "encoding";
  "message": string;
} | {
  "kind": "invalid";
  "message": string;
});

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
 * Per-run observation. Off is unobserved, never a zero count.
 */
export type RunObservation = ("off" | "counts");

/**
 * One block of the entry and how often a run entered it.
 */
export type ScopeCoverage = {
  "block": Site;
  /**
   * Arm selections, loop iterations, caught errors or function calls.
   */
  "entries"?: (number | null);
};

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

/**
 * What an action does.
 */
export type SiteKind = (("call" | "perform" | "join" | "join_many" | "yield" | "spawn" | "cancel") | "sleep" | "await");

/**
 * The language of source, independent of a generated-source format version.
 */
export type SourceLanguage = ("type_script" | "python");

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

/**
 * One exact statement, action or while-test coordinate and its observed count.
 */
export type StatementCoverage = {
  /**
   * None is unobserved; Some(0) is an observed statement that did not run.
   */
  "runs"?: (number | null);
  "site": Site;
};

/**
 * One statement of the entry.
 */
export type StatementView = {
  /**
   * The sites of the statement's actions, its own and those nested in
   * its expressions, in the order a run issues them: an action's
   * arguments before it, left to right (`K-EVAL-001`). A run reports
   * each at its site. A closure's actions are its body's statements'.
   */
  "actions": Array<Site>;
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

/**
 * A value a comparison reads: where it comes from, then the record fields
 * read from it, outermost first.
 */
export type ValueRef = {
  "fields"?: Array<string>;
  "source": ValueSource;
};

/**
 * Where a compared value comes from.
 */
export type ValueSource = ({
  "binding": number;
  "kind": "parameter";
} | {
  "kind": "literal";
  "value": Literal;
} | {
  "kind": "result";
  "site": Site;
});

/**
 * Statement starts, scope entries, action issues and while tests of one entry.
 */
export type WorkflowCoverage = {
  /**
   * Issues at exact action sites, independent of statement starts.
   */
  "actions": Array<StatementCoverage>;
  /**
   * While header evaluations, including the final false test.
   */
  "conditions": Array<StatementCoverage>;
  /**
   * The observation the counts came under; absent is an unobserved run.
   */
  "observation"?: (RunObservation | null);
  /**
   * Each block of the entry, in document order.
   */
  "scopes": Array<ScopeCoverage>;
  /**
   * Each statement of the entry, in document order.
   */
  "statements": Array<StatementCoverage>;
};

/**
 * Identity is (view.document, kind, block, site, last member). Ranges are
 * display metadata. Supporting statements and dataflow remain addressable.
 */
export type WorkflowNode = {
  "arms": Array<ControlArm>;
  "bindings": Array<number>;
  "block": Site;
  "callable": Site;
  "canonical": LineRange;
  "condition_ordinal"?: (number | null);
  "departures": Array<Site>;
  "falls_through": boolean;
  "header"?: (ControlHeader | null);
  "kind": WorkflowNodeKind;
  "lazy": Array<LazyArm>;
  /**
   * Each statement has exactly one carrier. Additional actions in a
   * statement have no members, but retain their owning `statement`.
   */
  "members": Array<Site>;
  /**
   * The node's authoritative count coordinate in RanCounts.
   */
  "ordinal": number;
  "readable": Array<LineRange>;
  "reads": Array<number>;
  "site": Site;
  /**
   * The owning statement for actions/controls; first member for code.
   */
  "statement": Site;
  "within"?: (Site | null);
  "writes": Array<number>;
};

export type WorkflowNodeKind = ("action" | "control" | "code");

/**
 * The source spelling of a callable's public signature, without dialect state.
 */
export type WrittenSignature = {
  "language": SourceLanguage;
  "signature": string;
};
