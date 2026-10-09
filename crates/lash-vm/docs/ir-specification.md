# The lash_vm IR specification

This is the normative specification of the lash_vm intermediate
representation: the tree a module artifact stores, the tree the VM compiles
and executes, and the tree every module's identity is computed over. It is
written in dialect-independent terms. A dialect is a front end that lowers
its source into this IR; it does not change what any node here means.
TypeScript is one front end, covered only in
[the front-end mapping](#the-typescript-front-end).

The normative sources are `crates/lash-vm/src/ast.rs` (the tree),
`crates/lash-vm/src/ast_roles.rs` (structural roles and origins),
`crates/lash-vm/src/ast_number.rs` (the number rule),
`crates/lash-vm/src/builtins.rs` (the builtin and intrinsic registry),
`crates/lash-vm/src/runtime/value.rs` and
`crates/lash-vm/src/runtime/heap/object.rs` (the value and heap model),
`crates/lash-vm/src/runtime/javascript.rs` and
`crates/lash-vm/src/runtime/ops.rs` (operator and coercion semantics), and
`crates/lash-vm/src/artifact.rs` with `artifact_identity.rs` and
`artifact_hash_writer.rs` (identity and the stored envelope).

Every node-variant name, operator, builtin, intrinsic, value kind and heap
kind in those sources appears in this document, and a repository check
(`crates/lash-vm/tests/ir_spec.rs`) fails when one does not. Adding a
variant or intrinsic without specifying it here fails that check.

Relevant decisions: [ADR 0060](adr/0060-the-lash-vm-is-a-heap-substrate-with-dialect-lowered-value-semantics.md)
(the VM is a heap substrate under dialect-lowered value semantics),
[ADR 0096](adr/0096-typescript-is-the-sole-rlm-dialect.md) (one dialect, and
a later one is authored against the IR of its day),
[ADR 0100](adr/0100-the-run-observation-contract.md) (node identity and
source identity),
[ADR 0106](adr/0106-durable-formats-upgrade-by-migration-or-drain.md) and
[ADR 0113](adr/0113-artifacts-are-kept-alive-only-by-their-referrers.md)
(stored format and liveness), and
[ADR 0115](adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
(the envelope's coexist rule).

## 1. Programs

A program is a `Program`: a list of `declarations`, a `main` expression that
is the program's entry point and result, a set of `private_bindings` naming
main-level bindings that belong to the front end rather than the session's
globals, and a `spans` table of source spans addressed by `AstPath`. Spans
are diagnostics only: the durable program is span-free, and no identity or
hash ever reads them.

There is one node sort, `Expr`, for statements and expressions. Every node
evaluates to a value; a statement list is a `Block` whose completion is the
value of its last element, or `null` when the block is empty. A node used in
statement position has its value discarded.

A `Program` must satisfy the admission checks (`validate_ast` plus the
artifact checks) before it can link, compile or store:

- Nesting may not exceed `MAX_AST_NESTING_DEPTH` of `64` AST levels.
- `Break` and `Continue` appear only inside a loop body of the same function
  body; `FunctionReturn` appears only inside a function body.
- Declared process signatures are checked: parameter names must be valid
  process parameter names and unique (`ProcessSignatureError`).
- A `TypeExpr::Process` of unknown signature is refused in program-owned IR
  (it is a host-schema shape only).
- Every `Role` wraps an expression of its role's shape, every `ProcessOrigin`
  agrees with its declaration's name and parameters, and declaration names
  are unique within their kind. Violations are `InvalidAst`.
- A stored program additionally carries no spans, no unlifted
  `ProcessLiteral`, and no process declaration without a `return_ty`.

### Declarations

A `Declaration` is one of:

| variant | meaning |
|---|---|
| `Process` | A `ProcessDecl`: a named durable process with `params` (each a `ProcessParam` of `name` and `ty`), an optional `return_ty` inferred by the linker when unwritten, an optional `label` (`LabelMetadata`: `title` plus optional `description`), an `origin` (`ProcessOrigin`), and a `body`. |
| `Function` | A `FunctionDecl`: a named pure synchronous function with `params` (each a `FunctionParam`), a mandatory `return_ty`, and a `body`. The linker rejects every effect inside the body. |

A `ProcessOrigin` records where a process declaration came from:

| variant | meaning |
|---|---|
| `Declared` | Authored as a module declaration. |
| `Lifted` | Lifted by the linker out of an inline `ProcessLiteral`; carries the literal's `site` (`AstPath`), its `hidden_params` count, and an optional `declared_return_ty`. The declaration's name is `LIFTED_PROCESS_NAME_PREFIX` (`__process_`) plus a digest of the canonical body and site under the `LASH_LIFTED_PROCESS_NAME_DOMAIN_VERSION` domain (`lash-lifted-process-name/v2`), and its last `hidden_params` parameters are the literal's hidden start arguments. |

`main` bindings named in `private_bindings` have `BindingVisibility`
`Private` — they are the front end's own slots, never imported from or
exported to the session's globals; every other main-level binding is
`SessionVisible`.

### Node addresses

An `AstPath` addresses a node inside a program: an `AstRoot` (`Main`, or
`Declaration(index)` into `declarations`) plus a `steps` list of
`Expr::children()` indices. Spans, linker side tables, lifted-process sites
and execution sites are all keyed by `AstPath`. The flat `legacy_steps`
encoding (bare steps for main; `u32::MAX`, the declaration index, then the
steps for a declaration) survives only as a durable hash input.

## 2. Types

A `TypeExpr` is a serialized value-type expression used in signatures,
resource catalogs and validation:

| variant | meaning |
|---|---|
| `Any` | Any value. |
| `Str` | Text. |
| `Int` | A number with integral value. |
| `Float` | A number. |
| `Bool` | A boolean. |
| `Dict` | A record of unknown fields. |
| `Null` | The literal `null` type. |
| `Enum` | One of the listed string literals. |
| `List` | A list of the member type. |
| `Object` | A record with named `TypeField`s (each `name`, `ty`, `optional`). |
| `Ref` | A named host data type, resolved against the artifact's requirements. |
| `Process` | A `ProcessType`: `Unknown` (host schemas only), or `Known` with a checked `ProcessSignature` of ordered params and one output. |
| `Union` | Two or more alternative member types; `UnionMembers` makes smaller unions unrepresentable — construction deduplicates, flattens nested unions and refuses a remainder under two members, and `TypeExpr::union` collapses one member to itself and zero to `Null`. |

`ResourceRefExpr` names a host resource: a `path` of segments plus the
`resource_type` and `alias` fields a resolved reference carries.

## 3. Nodes

The `Expr` sort has these variants. "Evaluates" states evaluation order;
every operand is evaluated left to right.

### Literals and reads

| variant | shape and semantics |
|---|---|
| `Null` | Produces `null`. |
| `Absent` | Produces the absent-value sentinel (`Value::Undefined`), distinct from `null`. It coerces to `NaN` as a number and to `"undefined"` as text, is false by truthiness, and answers `"undefined"` to `TypeOf`. |
| `Bool` | A boolean literal. |
| `Number` | An IEEE-754 double literal under the one IR number rule: `0` and `-0` are distinct values and every NaN is the one canonical NaN. Its stored form is an `IrNumber` — `Finite` (a JSON number) or `NonFinite`, where a `NonFiniteNumber` is `NaN`, `Infinity` or `NegativeInfinity`, stored as the string `"NaN"`, `"Infinity"` or `"-Infinity"` so a literal decodes to exactly the value that was hashed. |
| `String` | A text literal. |
| `Variable` | Reads the named binding. |
| `List` | Evaluates its items and produces a list. |
| `Record` | Evaluates its values and produces a record of the named fields. |
| `Field` | Evaluates `target`, then reads its named member. |
| `Index` | Evaluates `target` then `index`, then reads the member the index names. |

### Binding and control

| variant | shape and semantics |
|---|---|
| `Assign` | Evaluates `expr` (after any `Index` steps of `target`, in path order), writes it through the `AssignTarget`, and completes `null`. An `AssignTarget` is a `root` binding plus `steps` of `AssignPathStep`: `Field` (a named member) or `Index` (a computed member). A path target writes through a heap reference. |
| `Block` | Evaluates its elements in order, discarding every value but the last, which is the block's completion. An empty block completes `null`. |
| `If` | Evaluates `condition`; completes with `then_block` when it is true by truthiness, else `else_block`. Both arms are always present. |
| `For` | Evaluates `iterable` once, then iterates it: per element, binds the element to `binding`, evaluates the optional generated `bind` expression (the front end's element-destructuring glue), then runs `body`. `authored_binding` is display metadata only. Completes `null`. |
| `While` | Evaluates `condition` on each iteration and runs `body` while it is true by truthiness. Completes `null`. |
| `Break` | Exits the enclosing loop; admission refuses it outside one. |
| `Continue` | Skips to the enclosing loop's next iteration; admission refuses it outside one. |
| `LabelAnnotated` | Attaches `LabelMetadata` to `expr` for observation: a labelled effect step records the label as its execution site, otherwise an observation step is emitted around the inner node. Executes as `expr`. |
| `Role` | A `StructuralRole` marker around front-end-generated IR. A role never changes what runs — the node links, compiles and executes exactly as its `expr` — but tells structural consumers what the shape is. Admission refuses a role whose expression lacks the role's shape. |
| `Try` | Structured exception scope: runs `body`; on a thrown value runs `catch` (`CatchClause`: `binding`, `body`) with the value bound, then always runs `finally`. This node is AST-only; no source grammar produces it. |
| `Throw` | Evaluates its operand and raises it unchanged as an exception. AST-only. |
| `FunctionReturn` | Evaluates its operand and returns it from the enclosing function, running every intervening `finally`. Admission refuses it outside a function body. |

### Functions and calls

| variant | shape and semantics |
|---|---|
| `Function` | A closure value (`FunctionExpr`): optional `name`, optional `js_name` (the `name` own property the value reports), an optional `receiver` slot the callee's receiver binds to, `params`, `captures` (copied at creation), and `body`. AST-only. |
| `Call` | Evaluates `function`, then `args`, then calls it; the callee's receiver slot binds the absent sentinel. |
| `MethodCall` | Evaluates `receiver` once, then the `MethodKey` (`Field` for a named member, `Index` for a computed key), reads the member, evaluates `args`, and calls with the receiver bound into the callee's receiver slot. |
| `ThisCall` | Evaluates `this`, then `function`, then `args`, and calls with `this` as the explicit receiver. Generated code only — a front end uses it where a builtin passes a receiver of its choosing. |
| `FunctionCall` | Calls the declared `FunctionDecl` named `function` with `args`. The parser never produces this node; the linker resolves a `BuiltinCall` it recognises into one, so the compiler can emit a static callee and later passes can tell a pure declared call from a first-class closure call. |
| `Map` | Evaluates `items` and `function`, then applies the function to each element. AST-only; it exists to exercise builtin-to-VM callbacks. |
| `ProcessLiteral` | An inline process body (`ProcessLiteralExpr`: `params`, `hidden_args`, optional `return_ty`, `body`) written where a `Process`-typed slot is expected. AST-only and never survives the link: the linker lifts it to a hoisted `ProcessDecl` (`ProcessOrigin::Lifted`), and a literal in a non-process slot is a type error. |
| `ProcessRef` | A reference to the declared process `process`: produces that process's descriptor value. |

### Effects and host values

| variant | shape and semantics |
|---|---|
| `ResourceRef` | A `ResourceRefExpr` resolved by the linker: produces a `Value::Resource` handle naming `resource_type` and `alias`. |
| `ReceiverCall` | Evaluates `receiver`, then `args`, then calls the named host `operation` on the receiver. Produces the operation's result record (see `ResultUnwrap`). |
| `Await` | Evaluates its operand and waits for the pending value it names. |
| `SleepFor` | Evaluates its operand as a duration and suspends the process for it. |
| `ResultUnwrap` | Evaluates its operand and unwraps a host result record: `{ok: true, value}` produces `value`; `{ok: false, error, cause}` raises the recorded failure. Nested `ResultUnwrap(Await(x))` and `ResultUnwrap(ReceiverCall)` are fused by the compiler into single await/unwrap sites. |
| `Print` | Evaluates its operand and emits it as an observation to the host. |
| `Finish` | Evaluates its operand and settles the process successfully with it. |
| `Fail` | Evaluates its operand and settles the process as failed with it. |
| `HostDescriptorConstructor` | Evaluates `input`, then wraps it in the host value of `type_name` — a host-registered value constructor producing a projected descriptor. |
| `BuiltinCall` | Calls the named builtin or intrinsic with `args` (see [builtins](#builtins) and [intrinsics](#intrinsics)). Source spells declared-function calls the same way; the linker rewrites the ones it recognises into `FunctionCall`. |

### Operators

| variant | shape and semantics |
|---|---|
| `CoercingUnary` | Evaluates `expr` once, then applies the `CoercingUnaryOp` rule. |
| `CoercingBinary` | Evaluates `left`, then `right`, then applies the `CoercingBinaryOp` rule. |
| `OperandLogical` | Evaluates `left`, then conditionally evaluates `right`, returning the selected operand uncoerced, per the `OperandLogicalOp` rule. |

## 4. Operator semantics

The operator names describe what they compute; their rules are the ECMA-262
coercion semantics the IR's value model defines, stated here so a second
dialect can target or map them.

`CoercingUnaryOp`:

| variant | semantics |
|---|---|
| `Plus` | ECMA `ToNumber` of the operand (through `ToPrimitive` for an object), preserving NaN, infinities and signed zero. |
| `Negate` | Negates `ToNumber` of the operand, including flipping signed zero. |
| `Not` | Negates truthiness: `null`, absent, `false`, `0`, `-0`, `NaN` and empty text are false; lists, records, images, resources and all heap references are true; an inline tuple is true iff nonempty; a projected value delegates to its host's truthiness read. |
| `TypeOf` | `"undefined"` for absent, `"boolean"`, `"number"` or `"string"` for those primitives, `"function"` for callable heap references, `"object"` for `null` and every other value. |
| `BitNot` | Complement of `ToInt32(ToNumber(x))`, returned as a number. |
| `ToString` | ECMA `ToString`: an object is asked for its primitive with the string hint (`toString` before `valueOf`); `null` and absent become `"null"` and `"undefined"`; numbers use ECMA's decimal spelling. |

`CoercingBinaryOp` — both operands evaluate left then right, and
object-to-primitive hooks run left then right:

| variant | semantics |
|---|---|
| `Add` | Default `ToPrimitive` on both operands; concatenates the `ToString` results if either primitive is text, otherwise adds the `ToNumber` results. |
| `Subtract` | Subtracts the operands' `ToNumber` results. |
| `Multiply` | Multiplies the operands' `ToNumber` results. |
| `Divide` | Divides the operands' `ToNumber` results, retaining infinities and NaN. |
| `Remainder` | Remainder of the `ToNumber` results, with the dividend's sign. |
| `StrictEqual` | No coercion: compares primitive values or object identity. NaN differs from itself, `0` and `-0` compare equal, `null` differs from absent, resources compare their reference values, and inline compounds (`Tuple`, `List`, `Record`, `Image`) compare by identity. A projected value compares the value it represents; unavailable placeholders compare equal only when their placeholder identities agree. |
| `StrictNotEqual` | The negation of `StrictEqual`. |
| `LooseEqual` | ECMA abstract equality: `null` and absent compare equal to each other; a boolean first becomes a number; text compared to a number becomes a number; an object compared to a primitive uses default `ToPrimitive`. |
| `LooseNotEqual` | The negation of `LooseEqual`. |
| `Less` | Number-hint `ToPrimitive` on both; UTF-16 code-unit order when both are text, otherwise numeric order after `ToNumber`. A NaN operand yields `false`. |
| `LessEqual` | The inclusive order of `Less`; NaN yields `false`. |
| `Greater` | The reversed order of `Less`; NaN yields `false`. |
| `GreaterEqual` | The inclusive reversed order of `Less`; NaN yields `false`. |
| `BitAnd` | Bitwise AND of `ToInt32(ToNumber)` operands, as a number. |
| `BitOr` | Bitwise OR of `ToInt32(ToNumber)` operands, as a number. |
| `BitXor` | Bitwise XOR of `ToInt32(ToNumber)` operands, as a number. |
| `ShiftLeft` | Wrapping 32-bit left shift of `ToInt32(left)` by `ToUint32(right)` modulo 32. |
| `ShiftRight` | Sign-extending right shift of `ToInt32(left)` by `ToUint32(right)` modulo 32. |
| `ShiftRightUnsigned` | Zero-filling right shift of `ToUint32(left)` by `ToUint32(right)` modulo 32, as a number. |

`OperandLogicalOp` — `right` may not evaluate at all; the selected operand is
returned uncoerced:

| variant | semantics |
|---|---|
| `And` | Returns `left` when it is false by `Not`'s truthiness; otherwise evaluates and returns `right`. |
| `Or` | Returns `left` when it is true by truthiness; otherwise evaluates and returns `right`. |
| `NullishCoalesce` | Returns `left` unless it is `null` or absent; otherwise evaluates and returns `right`. `false`, `0`, `NaN` and empty text retain `left`. |

`UpdateOperator` names the arithmetic operators a compound attribute
assignment (`StructuralRole::AttributeAssign`) applies to the attribute's
current value; each maps to the `CoercingBinaryOp` of the same name:

| variant | semantics |
|---|---|
| `Add` | `base.step = base.step + operand` under `Add`. |
| `Subtract` | `base.step = base.step - operand` under `Subtract`. |
| `Multiply` | `base.step = base.step * operand` under `Multiply`. |
| `Divide` | `base.step = base.step / operand` under `Divide`. |
| `Remainder` | `base.step = base.step % operand` under `Remainder`. |

### Coercions

- `ToPrimitive` (default or number hint): a `Tuple` or `List` joins its
  members' `ToString` results with `,` (`null` and absent join as empty
  text); a `Record`, `Image`, `Resource` or heap reference produces
  `"[object Object]"`; a projected value coerces the value it materializes
  to (an unrestorable placeholder stands in as absent, and the VM's async
  coercion path refuses it with a typed error first).
- `ToNumber`: absent is `NaN`; `null` is `0`; a boolean is `0` or `1`; a
  number is itself; text is parsed under the ECMA string-to-number grammar
  (ECMA `StrWhiteSpace` — a fixed set that differs from Unicode White_Space —
  is trimmed, then `Infinity`, radix-prefixed `0x`/`0b`/`0o` integers and
  decimal literals with optional exponent are accepted, and anything else is
  `NaN`); every other value coerces through `ToPrimitive` first.
- `ToInt32`/`ToUint32`: truncate toward zero and reduce modulo 2^32; NaN,
  the infinities and both zeros are `0`.
- `ToString`: `"undefined"`, `"null"`, the boolean's spelling, ECMA's
  `Number::toString` (with `"NaN"`, `"Infinity"`, `"-Infinity"`, and `"0"`
  for both zeros), or the operand's own text; every other value coerces
  through `ToPrimitive` first.
- Text is measured in UTF-16 code units for ordering; strings have a size
  bound of `8 * 1024 * 1024` bytes enforced on construction.

## 5. Values and heap kinds

A `Value` is one of:

| variant | meaning | truthy |
|---|---|---|
| `Null` | The null literal. | never |
| `Undefined` | The absent-value sentinel (`Expr::Absent`). | never |
| `Bool` | A boolean. | its own value |
| `Number` | An IEEE-754 double. | false for `0`, `-0`, `NaN` |
| `String` | Text. | false when empty |
| `Image` | An image descriptor (`id`, `mime`, `label`, `size`, `width`, `height`). | always |
| `Resource` | A `ResourceHandle` (`resource_type`, `alias`) naming a host resource. | always |
| `Ref` | A `HeapId` reference into the VM heap. | always |
| `Tuple` | An inline immutable sequence. | iff nonempty |
| `List` | An inline sequence. | always |
| `Record` | An inline string-keyed map. | always |
| `Projected` | A host descriptor: a projected value the host reads through the `ProjectedReadRequest`/`ProjectedReadResponse` algebra. Truthiness, reads and coercions delegate to the host; materializing it yields the value it represents, and an unavailable placeholder materializes to nothing. | host-defined |

The heap object kinds — every kind a durable session can hold, the
`HEAP_OBJECT_KINDS` inventory — are:

| kind | `HeapObject` variant | contents |
|---|---|---|
| `tuple` | `Tuple` | A fixed sequence of values. |
| `list` | `List` | `items` plus a sorted set of `holes` (sparse-array elision slots). |
| `record` | `Record` | A string-keyed field map. |
| `function` | `Closure` | A compiled `function` index, copied `captures`, and the `name` and `length` own-property slots (configurable, not writable). |
| `built-in function` | `BuiltinFunction` | One heap object per built-in function, so every read answers the same reference. |
| `RegExp` | `RegExp` | A compiled regular expression and its `last_index`. |
| `RegExp match array` | `RegExpMatch` | A match result: `items`, `index`, `input`, `groups`. |
| `Map` | `Map` | An insertion-ordered key/value entry set. |
| `Set` | `Set` | An insertion-ordered member set. |
| `Date` | `Date` | A millisecond timestamp. |
| `Error` | `Error` | An error object of an `ErrorKind` class with optional `message`, `cause` and `errors`. |
| `URL` | `Url` | A parsed URL and its `search_params`. |
| `URLSearchParams` | `UrlSearchParams` | An ordered list of name/value pairs. |
| `binding cell` | `Cell` | One shared storage location for a captured binding that something assigns (FIG-3707); never a guest-visible value. |

## 6. Structural roles

A `StructuralRole` is language-neutral metadata a front end marks generated
IR with; each role names what the wrapped shape does, and admission refuses
a role whose `expr` lacks the role's shape. Serialized `kind` spellings are
snake_case.

| variant | serialized | required shape |
|---|---|---|
| `Scope` | `scope` | A `Block` whose every element is a statement. |
| `Completion` | `completion` | A non-empty `Block` of statements closed by a pure completion value. |
| `AttributeAssign` | `attribute_assign` | A member assignment that pins its base (and key) before evaluating the value: `Block([base = object, (key = index)?, result = value, base.step = result, result])`, where `value` may read the pinned base only as the left operand of an `UpdateOperator`. |
| `CollectionTransform` | `collection_transform` | A callback-driven collection transform: `Block` binding receiver, callback and operands, then a driver function capturing receiver and callback, then the one or two expressions that run it; `operation` carries the front end's name for the transform. |
| `JsonTraversal` | `json_traversal` | A recursive JSON value traversal: a block binding its input and options, then a traversal or dispatch. Display metadata only — it contributes no wrapper node to children or hash paths. |
| `ProcessWrapper` | `process_wrapper` | The process failure wrapper around an authored run body: `Try { body: Finish(Call { function: run, args }), catch e: Fail(e) }`, where `run` is a `Function` or a builtin call whose first argument is one. |

## 7. Builtins

`Expr::BuiltinCall` names a function from the `SOURCE_BUILTINS` registry.
These are IR facilities with no dialect spelling of their own; arities are
checked at compile time.

| name | arity | semantics |
|---|---|---|
| `len` | 1 | Element, member or character count of a list, tuple, record or text. |
| `empty` | 1 | Whether a list, tuple, record or text has no members; `null` is empty. |
| `keys` | 1 | The field names of a record as a list. |
| `values` | 1 | The field values of a record as a list. |
| `trim` | 1 | Text with leading and trailing whitespace removed. |
| `to_string` | 1 | The value as text. |
| `to_int` | 1 | The value as an integral number; fails on non-integral input. |
| `to_float` | 1 | The value as a number. |
| `json_parse` | 1 | Parses JSON text into a value. |
| `contains` | 2 | Whether the haystack (text, sequence or record) holds the needle. |
| `grep_text` | 2 | The lines of a text that match a pattern. |
| `starts_with` | 2 | Whether a text begins with a prefix. |
| `ends_with` | 2 | Whether a text ends with a suffix. |
| `split` | 2 | A text split on a separator into a list of parts. |
| `join` | 2 | A sequence joined into text with a separator. |
| `validate` | 2 | Validates a value against a type/schema argument; compiles to a precompiled-schema instruction when the schema folds at compile time. |
| `ceil_div` | 2 | Integer ceiling division of two numbers. |
| `floor_div` | 2 | Integer floor division of two numbers. |
| `push` | 2 | Appends a value to a list. |
| `slice` | 3 | The subrange of a sequence or text between two bounds (either may be `null`). |
| `find` | 2–3 | The position of a needle in a haystack, from an optional start. |
| `format` | ≥1 | Interpolates arguments into a template; compiles to a precompiled-template instruction. |
| `range` | 1–3 | A numeric range: `range(end)`, `range(start, end)`, or `range(start, end, step)`. |
| `sort` | 1 | A sorted copy of a list. |
| `sort_by` | 2 | A list sorted by a key function. |
| `sum` | 1 | The numeric total of a list. |
| `min` | 1 | The smallest element of a list. |
| `max` | 1 | The largest element of a list. |
| `replace` | 3 | A text with occurrences of a pattern replaced. |
| `lower` | 1 | Text in lowercase. |
| `upper` | 1 | Text in uppercase. |
| `unique` | 1 | A list with duplicate elements removed. |
| `reverse` | 1 | A list in reverse order. |

## 8. Intrinsics

The `IR_INTRINSICS` are reserved names generated code may call; authored
source cannot spell them and they are never advertised as builtins. A name
carries what it does, not the dialect that emits it.

| name | arity | semantics |
|---|---|---|
| `__lash_vm_split` | 2 | ECMA `String.prototype.split`: text split on a separator (absent separator yields the whole text; empty separator splits into characters and refuses lone surrogates). |
| `__lash_vm_join` | 2 | ECMA `Array.prototype.join`: sequence members' `ToString` results joined on a separator (absent separator is `,`; `null` and absent join as empty text). |
| `__lash_vm_stdlib` | ≥1 | Dispatches one standard-library operation by its string selector: the instance and static method surface, plus the reserved selectors (`Lash.Apply`, `Lash.OwnMethod`, `Lash.ToPropertyKey`, `__jsonContainerKind`, `__jsonHasOwnToJSON`). |
| `__lash_vm_heap_new` | ≥1 | Constructs a heap object by constructor-name discriminator: the error kinds, `URL`, `URLSearchParams`, `RegExp`, `Map`, `Set`, `Date`. |
| `__lash_vm_heap_instanceof` | 2 | Whether a value is an instance of the named constructor's heap kind. |
| `__lash_vm_heap_delete_member` | 2 | Deletes a named member of a heap object; answers whether anything was deleted. |
| `__lash_vm_regexp` | ≥1 | Dispatches a regular-expression operation (compile, exec, test, replace and their flag handling). |
| `__lash_vm_global_delete` | 1 | Removes a session-global binding by name; answers whether anything was removed. |
| `__lash_vm_global_get` | 1 | Reads a session-global binding by name; absent globals read the absent sentinel. |
| `__lash_vm_global_has` | 1 | Whether a session-global binding holds a value. |
| `__lash_vm_global_set` | 2 | Writes a session-global binding to a value; produces the value written. |
| `__lash_vm_call_dynamic` | 2 | Calls a function value with a list of arguments (`CallDynamic`). |
| `__lash_vm_call_method_dynamic` | 3 | Calls a function value with an explicit receiver and a list of arguments (`CallMethodDynamic`). |
| `__lash_vm_pending_tool` | 1 | Wraps a `ReceiverCall` so the tool call issues as a pending handle the caller awaits explicitly. |
| `__lash_vm_pending_timer` | 1 | Starts a timer for a duration and produces its pending handle. |
| `__lash_vm_await_array` | 2 | Awaits a list of pending values under an aggregate consumer mode named by a literal (`all`, `allSettled`, `race`, `any`). |
| `__lash_vm_await_pending` | 1 | Awaits a pending value (`AwaitPending`). |
| `__lash_vm_async_map` | 2 | Maps an async function over a sequence, running one call per element. |
| `__lash_vm_closure` | 3 | Creates a closure from a `Function` node with an explicit permissive parameter model (`required_count`, `accepts_rest`). |
| `__lash_vm_cell_new` | 1 | Creates a binding cell holding a value. |
| `__lash_vm_cell_get` | 1 | Reads the value a binding cell holds. |
| `__lash_vm_cell_set` | 2 | Stores a value in a binding cell; produces the stored value. |
| `__lash_vm_encode_uri_component` | 1 | `encodeURIComponent` of a text. |
| `__lash_vm_decode_uri_component` | 1 | `decodeURIComponent` of a text. |
| `__lash_vm_encode_uri` | 1 | `encodeURI` of a text. |
| `__lash_vm_decode_uri` | 1 | `decodeURI` of a text. |

## 9. Module identity

A module's identity is its IR. The `ModuleRef` spells
`LASH_VM_PREFIX_VERSION` (`lash_vm:v2:blake3:`) followed by a BLAKE3 hex
digest computed under the `LASH_VM_CONTENT_DOMAIN_VERSION` domain
(`lash-vm-content/v2`); the same digest under the
`LASH_WORKFLOW_SOURCE_DOMAIN_VERSION` domain (`lash-workflow-source/v4`),
over the same atom stream prefixed by `source`, is the module's
`source_identity` (ADR 0100 R6). The companion `HostRequirementsRef` spells
`LASH_VM_HOST_REQUIREMENTS_PREFIX_VERSION`
(`lash-vm-host-requirements:v2:blake3:`) plus a digest of the requirements
snapshot.

The hash input is a stream of atoms. An atom is `len:value;` — the byte
count of `value` in decimal, a colon, the value, a semicolon. An integer or
boolean is the atom of its decimal (`true`/`false`) rendering; a binding
name is written as one prefixed atom `name:<text>`.

The module-ref preimage is, in order:

1. `atom(family)` — the semantic-hash version `LASH_VM_SEMANTIC_HASH_VERSION`,
   currently `lash-vm-semantic-v25`.
2. `atom("module")`.
3. `atom("lash-vm-ir")` — `IR_ATOM` (`lash-vm-ir`), the dialect-neutral
   atom that stands where a front end's name would: two dialects that lower
   to the same program share one module ref.
4. `atom(host_requirements_ref)`.
5. The exports: `atom("exports")`, the process count, then per exported
   process in name order `atom("process-export")`, the process name, its
   component digest hex, and its declaration position. A process's
   `ProcessRef` component is itself a digest of `family`, `atom("process")`
   and the process declaration's write stream.
6. The program: `atom("program")`, the declaration count, each declaration,
   then `main`, then `atom("private-bindings")` and the names.

The host-requirements preimage is `atom(family)`,
`atom("host-requirements")`, then `abilities`, the language features
(`label-annotations`), `globals`, and `resources` (module instances with
their operations, resource types with their operation signatures,
named data types and value constructors), every collection
prefixed by its count and every entry sorted.

Inside a program, each node writes its variant atom then its fields in
declaration order:

| variant | head atom |
|---|---|
| `Block` | `block` |
| `LabelAnnotated` | `label-annotated` |
| `Null` | `null` |
| `Absent` | `ir:absent` |
| `Bool` | `bool` |
| `Number` | `number` (the literal's canonical bits — distinct `-0`, one canonical NaN) |
| `String` | `string` |
| `Variable` | `variable` |
| `List` | `list` |
| `Record` | `record` |
| `Assign` | `assign` (root name, then `field`/`index` steps, then the value) |
| `If` | `if` |
| `For` | `for` (binding name, iterable, `bind` or `no-bind`, body; `authored_binding` is not hashed) |
| `While` | `while` |
| `Role` | `role` with the role's name — except `JsonTraversal`, which writes only its `expr` |
| `Break` | `break` |
| `Continue` | `continue` |
| `ProcessRef` | `process-ref` |
| `HostDescriptorConstructor` | `host-value-constructor` |
| `ResourceRef` | `resource-ref` |
| `ReceiverCall` | `receiver-call` |
| `Await` | `await` |
| `SleepFor` | `sleep-for` |
| `ResultUnwrap` | `unwrap` |
| `Print` | `print` |
| `Finish` | `finish` |
| `Fail` | `fail` |
| `BuiltinCall` | `builtin-call` |
| `FunctionCall` | `declared-function-call` |
| `Function` | `function` (name or `anonymous`, `js_name` or `unnamed`, `receiver` or `no-receiver`, params, captures, body) |
| `ProcessLiteral` | `process-literal` |
| `Call` | `function-call` |
| `MethodCall` | `method-call` (`field` or `index` key) |
| `ThisCall` | `this-call` |
| `Map` | `function-map` |
| `Try` | `try` (`catch`/`no-catch`, `finally`/`no-finally`) |
| `Throw` | `throw` |
| `FunctionReturn` | `ir:function-return` |
| `Field` | `field-access` |
| `Index` | `index-access` |
| `CoercingUnary` | `ir:coercing-unary` (the operator's `Debug` name) |
| `CoercingBinary` | `ir:coercing-binary` (the operator's `Debug` name) |
| `OperandLogical` | `ir:operand-logical` (the operator's `Debug` name) |

Declarations write `process-decl` or `function-decl` followed by name,
params, `return`/`no-return` type, label,
`ProcessOrigin` and body; a function writes `return` type then body. A
`TypeExpr` writes a `type:*` atom per variant (`type:any`, `type:str`,
`type:int`, `type:float`, `type:bool`, `type:dict`, `type:null`, `type:enum`,
`type:list`, `type:object`, `type:ref`, `type:process-signature`,
`type:process-unknown`, `type:union`) then its
members.

Two programs that differ only in a binding's spelling are two modules: names
are hashed verbatim.

## 10. The stored envelope

The store keeps a `ModuleArtifact`: `module_ref`, `host_requirements_ref`,
`host_requirements` (the `HostRequirements` snapshot the linker collected:
resources, globals, abilities, language features), `exports` (the
`ModuleExports` process table), and the span-free `ir`. An artifact is
admitted by construction — the linker and the validating builders are the
only producers, and the store decoder is the only consumer of the stored
shape.

The stored bytes are a JSON `ModuleArtifactEnvelope`:

```json
{
  "family": "lash-vm-semantic-v25",
  "encoding": 2,
  "artifact": { "module_ref": "...", "host_requirements_ref": "...",
    "host_requirements": {...}, "exports": {...}, "ir": {...} }
}
```

`family` is `LASH_VM_SEMANTIC_HASH_VERSION`; `encoding` is
`MODULE_ARTIFACT_ENVELOPE_VERSION`, `2`. Under the `synthetic-next` feature
(the N+1 upgrade build, ADR 0115 §6) the constant is `3` but that build
still writes `2` and admits both, so a module published during a roll forward
reads after a rollback. Decoding refuses an unknown family or encoding,
obsolete shapes (the retired `compilation_dialect`,
the old anonymous process-type shape), IR that fails admission, and content
whose re-derived refs do not match the recorded ones — the decode verifies
`module_ref`, `host_requirements_ref` and `exports` against the artifact's
own bytes.

On disk the envelope is a store artifact blob: SQLite keeps it in the
content-addressed `blobs`/`refs` pair of the artifact family; PostgreSQL
keeps the bytes inline in `lash_lash_vm_artifacts`. Liveness is the
referrer-edge set of ADR 0113 — an artifact exists exactly while a referrer
edge holds it — and a read both decodes and verifies.

The envelope's sibling version is `LASH_VM_ABI_VERSION`
(`lash-vm-abi-v14`), the compiled-program ABI the VM's compiled-module
cache is keyed on. It is not part of the stored module shape — a compiled
program is a drain-and-recompile surface (ADR 0115), so the constant changes
in place with the instruction set rather than coexisting as envelope
encodings do.

## 11. The TypeScript front end

TypeScript is the sole authored dialect (ADR 0096, ADR 0062): the adapter in
`crates/lash-typescript` parses an exact ECMA-262 subset and lowers it into
this IR. The mapping that matters to the IR:

- `undefined` lowers to `Expr::Absent`; declarations and bindings lower to
  `Variable`/`Assign` plus `private_bindings`; `let`/`const`/`var` are
  front-end roles, not IR.
- The operator spellings lower to the coercing operators (`===` →
  `StrictEqual`, `+` → `Add`, `!` → `Not`, `&&`/`||`/`??` → `And`/`Or`/
  `NullishCoalesce`, `typeof` → `TypeOf`, `String(x)` → `ToString`).
- `console.log` lowers to `Print(__lash_vm_stdlib("__consoleObservationText", …))`;
  `Promise.all`/`allSettled`/`race`/`any` lower to
  `__lash_vm_await_array`; `sleep` lowers to `__lash_vm_pending_timer`;
  `await` on a pending handle lowers to `Await`/`__lash_vm_await_pending`.
- The ECMA method surface lowers to `__lash_vm_stdlib` selector calls,
  `new X(…)` to `__lash_vm_heap_new`, `instanceof` to
  `__lash_vm_heap_instanceof`, `delete` to `__lash_vm_heap_delete_member`
  or `__lash_vm_global_delete`, `RegExp` operations to
  `__lash_vm_regexp`, `globalThis` access to the `__lash_vm_global_*`
  intrinsics, assigned-capture bindings to the `__lash_vm_cell_*`
  intrinsics, and spread/dynamic calls to the `__lash_vm_call_*` intrinsics.
- Lowered statement shapes are marked with `StructuralRole`s (scope,
  completion, attribute assign, collection transform, JSON traversal,
  process wrapper) and generated names carry the front end's own prefix so
  no consumer infers structure from spelling. A function body that captures
  an assigned binding shares it through a `binding cell`.
- An `async` process body lowers to an inline `ProcessLiteral` inside a
  `ProcessWrapper` role, which the linker lifts to a `Lifted`
  `ProcessDecl`.

**Printing.** The workflow lens's canonical TypeScript printer
(`crates/lash-typescript/src/workflow_graph/printer.rs`) turns lowered IR
back into authored-looking source. It is not a straight walk: it re-sugars
the shapes the lowerer generates (process wrappers to `async` arrows,
`__lash_vm_stdlib("__consoleObservationText", …)` prints to `console.log`,
await arrays to `Promise.*`, timers to `sleep`, collection transforms and
attribute assignments to member syntax, sparse arrays to literals with
elisions, the default JSON traversal to `JSON.stringify`, `for`/`for-of`
loops) and falls back to the structural spelling for the rest.

On main the printer is partial. It refuses `ThisCall` and a bare `Map`
intrinsic; statement-position nodes (`Block`, `Role`, `LabelAnnotated`,
`Assign`, `For`, `While`, `Break`, `Continue`, `Try`, `Throw`,
`FunctionReturn`) are refused in expression position and vice versa;
a `HostDescriptorConstructor` is refused without a constructor path; a
generated binding that reaches the printer un-re-sugared is refused as
`GeneratedBinding`; and a label with no one-line comment spelling is
refused. Making the printer total over the IR is open work
([FIG-4846](https://linear.app/ascending-ai/issue/FIG-4846)); this
specification describes printer behaviour as it is, not as that ticket
leaves it.
