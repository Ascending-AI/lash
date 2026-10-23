# lash-dialect-typescript

The TypeScript dialect of the lash kernel: a front end that lowers TypeScript
to kernel documents, and the helper functions its documents call.

Hosts install code and operations as [packages](../../docs/host-packages.md).
Programs call their namespaces directly, without imports: `dataframe.col(x)`
calls code, `q.reduceByKey(keys, values)` calls a value member and
`await inbox.sendItem(input)` performs the canonical `inbox.send_item`
operation. Package values expose only declared members and check their
representation on use. A new cell uses its round's Current snapshot; a
parked run keeps its exact Link and Execution Environment. A session relinks its
saved functions against each cell's Namespaces while preserving resolved pins; see the guide for saving limitations.

## BigInt

`1n` and `BigInt(value)` produce mathematical kernel integers, distinct from
binary64 Numbers. BigInt arithmetic and bit operations preserve integer
precision; mixing BigInt and Number in arithmetic raises `TypeError`.
Comparisons work across numeric kinds without rounding the integer. Use
`Number(value)` for an explicit binary64 conversion. BigInt property keys are
textual, and `JSON.stringify` raises `TypeError` unless a replacer converts
them. `bigint` entry annotations map exactly to the kernel `Int` boundary.
Integer work and amplifying allocations obey the run's charge and memory bounds.
Primitive BigInts support `valueOf`, `toString(radix)` and `BigInt.asIntN/asUintN`;
boxed primitive objects remain outside the dialect.

## Builtin values and guest reflection

Builtin functions and constructors retain identity when aliased, borrowed or
read through computed keys. Supported constructors also accept aliased and
computed `new` and `instanceof`. `globalThis` is a guest namespace containing
only the dialect's declared builtins; it exposes no host globals.

`in`, `hasOwnProperty`, `Object.hasOwn`, `propertyIsEnumerable`, own-property
names, descriptors and descriptor collections inspect guest values and builtin
metadata. String property order is deterministic; own symbols are an empty list.
`Reflect.get`, `Reflect.has`, `Reflect.getOwnPropertyDescriptor` and
`Reflect.ownKeys` use that same metadata. Descriptor/prototype mutation,
prototype traversal, symbols and boxed primitive construction remain outside
the object model.

`Object.assign` writes through the ordinary guest setter, including array
indices and length, RegExp `lastIndex` and writable URL fields. Match-array
mutators preserve aliases and the surrounding match metadata.

`Function.prototype.toString` returns native spelling for builtin and bound
functions. Authored-source reflection remains refused until retained text and
its lexical type context can survive faithful saved-function source printing. At a tool
boundary, a builtin callable becomes a source-owned forwarding wrapper whose
start signature has optional `arg0`, `arg1`, … parameters through the builtin's
standard arity. Only its declared path is captured. Function slots accept the
wrapper; Json slots still reject it. Arbitrary library closures retain the
kernel's crossing restrictions.

## Name resolution

Lexical locals, including persistent bindings from earlier cells, shadow package
namespace roots. Discovering a tool never changes the meaning of existing locals
or code. A call such as `jev.decide(...)` whose root resolves to a local `jev`
is refused at compile time when the namespace offers that operation, including
through a deferred grant. `NAMESPACE_SHADOWED_BY_LOCAL` carries the binding name,
its origin when available, the canonical namespace, the requested call path and
the source span. This rule is independent of the local's type; without a matching
namespace operation, ordinary local member resolution applies.

Tool discovery returns namespace collisions separately from free call paths.
`Namespaces::collisions` projects the same typed namespace and binding facts for
consumers rendering discovery. Presentation and any repair hints belong to the
consumer.

## How an operation is lowered

Each operation takes the first tier that applies.

1. **Direct.** The operand types are known, so the operation is one kernel
   function: `a + b` on two numbers is `num.add`, `xs[i]` on an array is
   `list.get`, `xs.push(v)` on an array is the kernel's append and
   `for (x of xs)` its loop over a list. A direct call sits inside an
   expression and costs one node. A method call on a value whose type is one
   kind (an array, a string, a number, a boolean) calls the helper of that
   kind's method row itself, without the dispatch on the receiver.
2. **Helper.** The operand types are not known, so the operation is a call to
   a `ts.*` helper, kernel code that does what JavaScript does: `ts.add`
   converts its operands, then concatenates or adds.

A type is known by proof or by trust.

- **Proof** changes nothing. A literal, the result of an operator that gives
  one type whatever it is given, a `const`, a `let` whose initialiser and
  every assignment give one type, and a value under a `typeof`, `=== null` or
  `=== undefined` test are what JavaScript itself would hold.
- **Trust** is an annotation: a variable's, a parameter's, a function's
  return type, an array's element type, an object type's property, an `as`
  assertion. The front end believes it and does not check it.

`any`, `unknown` and code with no annotation are never trusted, so they keep
JavaScript's behaviour. `x as any` makes the front end forget what it
believed of `x`.

## Dates and locale

Date uses UTC for construction, local-named and UTC readers, timezone offset
and date/time text on every host. Valid Dates remain immutable timestamps;
construct a new Date instead of calling a setter. Parsing accepts ECMA ISO
and the exact whole-second `toString()`/`toUTCString()` formats it emits;
other legacy grammars remain refused.

Locale formatting uses fixed en-US and UTC. `toLocaleDateString()` and
`toLocaleTimeString()` accept no locale, `undefined` or `"en-US"`, with no
options. Locale case methods use pinned Unicode 17 root/en-US mappings and
accept the same locale choices. Other locales/options name the refused
method. `localeCompare` still requires a pinned collator and remains refused.

## Deviations

Trusting an annotation makes the dialect stricter than JavaScript: a kernel
function takes exactly the types it is chosen for, so when a believed type is
wrong at run time the operation raises a typed error where JavaScript would
have converted the value. It never gives a different answer. Every such
place is a `TS_TYPED_*` row of [`deviations.md`](deviations.md), the
dialect's one deviation register, and each row has a law.

What no row covers keeps JavaScript's behaviour: `**`, the bitwise operators,
`<` on two strings, `===` on anything but two numbers, a write `xs[i] = v`, a
method call on a value whose type is not one of the kinds above, and every
read of an object's property. A read from a declared object type or array
gives a value the front end believes to be what the type says, so a later
operation on it is a row.

## RegExp and Set protocols

RegExp accepts `d`, `g`, `i`, `m`, `s`, `u`, `y`. With `d`, match results
from `exec`, `match` and `matchAll` include `indices` in UTF-16 code units;
`indices.groups` aliases the numbered spans for named captures. Replacement
routes accept `d` and keep the standard replacement callback arguments.
`v` remains unsupported. Pattern length/group nesting limits and refusals
for lone-surrogate text results still apply.

Set algebra accepts Set, Map and guest set-like objects with `size`, `has`
and `keys`. It reads that protocol once in order, selects the specified
size-dependent algorithm, and steps guest iterators in resumable kernel
bodies. Guest callbacks and unbounded iterator tests receive ordinary charge.

## Source forms

Labels, comma expressions, `debugger` (a no-op), and tagged templates are
supported. A tag receives the same frozen cooked/raw arrays each time its
source site is evaluated, with invalid cooked escapes represented by
`undefined`. Member tags retain their receiver. Top-level `this` is
`undefined`, as in a module; `arguments` requires a function scope and
`new.target` is `undefined` in ordinary function calls.

Ambient declarations erase without creating runtime bindings. Overload
signatures require an implementation in the same scope. Local export aliases
create no local binding; workflow callable aliases export the same function.
Legal function redeclarations share the winning function binding;
lexical early errors still apply. Strict `delete` evaluates non-reference
operands and returns true; optional property deletion short-circuits normally.

Workflows allow ordinary implicit text conversion by default. Hosts may opt
into `ImplicitTextConversionPolicy::Refuse` or `Strict` through source limits.

## Asynchronous code

An `async` function runs as a kernel task, with the interleaving Node gives.
A tool call or `sleep(ms)` awaited where it stands is one `perform` or
`sleep` marked `then yield`, which takes one more turn once its outcome is
in, so continuations run in the order outcomes arrive; one that is not is a
promise whose task performs it. Tool and sleep arguments accept spreads,
evaluated once before signature and arity validation. Discarded tool promises
are admitted; outstanding tasks and unobserved errors are judged at cell end.
`await p` is `await p via ts.await`. `Promise.all`,
`allSettled`, `race` and `any` are helpers over the kernel's list joins. The
lowering is described in `src/lower/async_fn.rs` and
`src/helpers/promise.kernel`, and the Node witnesses are in `witness/async/`.

## Test262

Test262 is the oracle (`tests/test262/README.md`). Every test main's record
marks as passing passes on the kernel, or a row of the deviation register
names it. `scripts/check_test262_ratchet.py --kernel-outcomes` holds that.

Workflow entries materialize reachable module helpers and static data into a
private environment. Helpers keep their module bindings when an entry parameter
shadows a module name. Effects and dynamic module initialization belong inside
an entry or in its arguments. Each start constructs fresh static data.

An omitted optional argument is absent and selects its default. Explicit
`null` does not. Defaults run in parameter order and can read preceding
parameters. Boundary annotations resolve fixed tuples, structural intersections,
generic aliases and qualified admitted package types exactly, under the existing
expansion limits. Unknown or unrepresentable annotations retain typed refusals.
