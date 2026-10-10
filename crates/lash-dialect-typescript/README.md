# lash-dialect-typescript

The TypeScript dialect of the lash kernel: a front end that lowers TypeScript
to kernel documents, and the helper functions its documents call.

## How an operation is lowered

Each operation takes the first tier that applies.

1. **Direct.** The operand types are known, so the operation is one kernel
   function: `a + b` on two numbers is `num.add`, `xs[i]` on an array is
   `list.get`. A direct call sits inside an expression and costs one node.
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
method call, and every read of an object's property. A read from a declared
object type or array gives a value the front end believes to be what the
type says, so a later operation on it is a row.

## Asynchronous code

An `async` function runs as a kernel task, with the interleaving Node gives.
A tool call or `sleep(ms)` awaited where it stands is one `perform` or
`sleep`; one that is not is a promise whose task performs it. `Promise.all`,
`allSettled`, `race` and `any` are helpers over the kernel's list joins. The
lowering is described in `src/lower/async_fn.rs` and
`src/helpers/promise.kernel`, and the Node witnesses are in `witness/async/`.

## Test262

Test262 is the oracle (`tests/test262/README.md`). Every test main's record
marks as passing passes on the kernel, or a row of the deviation register
names it. `scripts/check_test262_ratchet.py --kernel-outcomes` holds that.
