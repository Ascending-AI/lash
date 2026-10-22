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

## Asynchronous code

An `async` function runs as a kernel task, with the interleaving Node gives.
A tool call or `sleep(ms)` awaited where it stands is one `perform` or
`sleep` marked `then yield`, which takes one more turn once its outcome is
in, so continuations run in the order outcomes arrive; one that is not is a
promise whose task performs it. `await p` is `await p via ts.await`. `Promise.all`,
`allSettled`, `race` and `any` are helpers over the kernel's list joins. The
lowering is described in `src/lower/async_fn.rs` and
`src/helpers/promise.kernel`, and the Node witnesses are in `witness/async/`.

## Test262

Test262 is the oracle (`tests/test262/README.md`). Every test main's record
marks as passing passes on the kernel, or a row of the deviation register
names it. `scripts/check_test262_ratchet.py --kernel-outcomes` holds that.
