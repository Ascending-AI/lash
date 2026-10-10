# Kernel numeric library

`lash-kernel-lib::numbers()` returns definitions beside their native implementations;
`register_numbers` adds them to the embedder's registry. This library depends only
on `lash-kernel-doc` and third-party crates. `num-bigint` is its arbitrary-precision
integer implementation. It implements `K-NUM-001..008`, `K-VAL-020..033`, and
`K-KEY-001/002/004` without dialect conversions.

## Domains and names

`add`, `sub`, `mul`, `div`, `div_floor`, `div_trunc`, `rem_floor`, `rem_trunc`,
`pow`, `min`, `max` take two numbers. `neg`, `abs`, `floor`, `ceil`, `trunc`,
`round_even`, `round_away`, `round_up`, `sign`, `is_finite`, `is_infinite`,
`is_nan`, `is_integer` take one number. Each has a `num.`, `int.`, and `float.`
variant with exactly that operand domain. Integers produce integers, float or
mixed arithmetic produces floats, `div` always produces a float, predicates
produce bools, and min/max select an operand. No bool, text, null or absent is
converted. Every operand is checked, including operands a mathematical shortcut
would otherwise ignore. A wrong kind raises `type_error`; wrong arity raises `arity`.

`eq`, `same` accept any two values. `lt`, `le`, `gt`, `ge`, `compare` accept the
orderable pairs of `K-VAL-029`. `num.`, `int.`, `float.` variants of `eq`, `lt`,
`le`, `gt`, `ge`, `compare` have exactly those narrower domains. `compare`
returns integer -1, 0 or 1 and raises `unordered` on NaN. Ordered predicates
return false on NaN, also when met inside a sequence. List ordering skips equal
prefix members; repeated active cyclic list pairs count as equal. Structural
map/set/record equality ignores insertion order; a map's keys and a set's members
use key equality, including tuple NaNs. Comparing an object with itself still
visits its contents, so a list containing NaN is unequal to itself.

`ref(Any)` accepts only an object or task, produces its ref, and raises
`type_error` otherwise. `same` and `ref` are ordinary natives; the machine owns
`deref` and `tasks.unfinished`. `Key::new` validates the immutable domain of
`K-KEY-001`; its `Eq` and `Hash` implement `K-KEY-002`. The key retains its
original value while hashing finite numbers as an odd signed integer times a
power of two. Thus equal numeric keys, including large integral floats, have
one hash; NaNs have one hash, including in tuples. No guest function exposes it.

`kind(Any)` gives one of `null`, `absent`, `bool`, `integer`, `float`, `text`,
`bytes`, `timestamp`, `tuple`, `list`, `map`, `set`, `record`, `closure`, `error`,
`task`, `function`, `handle`, `ref`. It inspects only the value kind and raises
nothing for any kind. These strings include all rows of the value table and ref.

## Additional numeric edges

These are rules of the content-addressed library definitions, not new kernel forms.

- **N-POW.** Two integers with a non-negative exponent produce an exact integer;
  `0^0` is 1. A negative integer exponent raises `number_range`; a caller wanting
  a floating reciprocal explicitly converts first. Floating/mixed power is
  software binary64 power: zero exponent gives 1 even for NaN, base 1 gives 1
  even for NaN, a negative finite base with non-integral exponent gives NaN,
  negative zero to a negative odd integer gives negative infinity, and overflow
  gives infinity. All other NaNs propagate.
- **N-ABS-NEG.** Abs removes a float's sign; neg reverses it. They preserve the
  numeric kind, and both return the canonical NaN for NaN.
- **N-MIN-MAX.** Min/max compare exactly and select an original operand, the left
  on a mathematical tie. NaN propagates. Two floating zeros select negative zero
  for min and positive zero for max. This is selection, so it never converts a
  large integer just to compare it with a float.
- **N-ROUND.** Floor rounds down, ceil up, trunc toward zero, round_even to the
  nearest integer with even ties, round_away to nearest with ties away from zero,
  round_up to nearest with ties toward positive infinity. Results preserve kind,
  NaN/infinity, and the sign of a zero result. Integers are unchanged.
- **N-SIGN-PREDICATES.** Sign produces same-kind -1, 0, or 1, preserving float
  zero's sign and propagating NaN. Integers are finite and integral, never NaN
  or infinite. Float predicates inspect IEEE state; integral excludes non-finite.
- **N-CONVERT.** `int.to_float` and `num.to_float` round once, ties to even, and
  raise `number_range` at infinity; a float input remains unchanged, including
  non-finite. `float.to_int` and `num.to_int` accept finite integral numbers
  only, raising `number_range` otherwise; their result is exact with no fixed
  integer range. Both float zeros become integer zero. There is no saturation.
- **N-PARSE.** `int.parse(text, radix)` accepts ASCII radix digits (case-insensitive)
  with an optional leading `+` or `-`, and leading zeros. `int.to_text(int, radix)`
  writes lower-case digits, optional `-`, no leading zeros. Radix is an integer
  in 2..36; otherwise `number_range`. Signs without digits, prefixes, separators,
  whitespace, or digits outside the radix raise `number_parse`.
  `float.parse(text)` accepts ASCII decimal digits with optional sign, decimal
  point and `e`/`E` exponent with optional sign and at least one exponent digit;
  at least one mantissa digit is required. `nan`, `inf`, `+inf`, `-inf` are the
  only non-finite spellings. Whitespace, separators and invalid grammar raise
  `number_parse`. Decimal overflow raises `number_range`; underflow rounds to a
  signed subnormal or signed zero. Parsing rounds ties to even. `num.to_text`
  writes canonical decimal integer or shortest-round-trip float text;
  `float.to_text` is float-only. Float layout is exactly `K-VAL-031`.

## Software math

Every `math.*` operand is Number; integers convert by `K-NUM-003`. Every result
is Float. The implementation is pinned `libm` 0.2.16, default features disabled;
**host libm is never used**. Its software binary64 algorithms fix the edges and
last-bit behavior independently of a platform's C math library. A dialect that
raises on a mathematical domain failure or overflow emits its own explicit check.
There is no random function; random is a host-read form.

**N-MATH.** All functions propagate NaN, except `pow(NaN, 0)` and `pow(1, NaN)`
are 1, `hypot(NaN, infinity)` is positive infinity, `copysign` takes only the
second operand's sign (NaN included), and `nextafter` with NaN is NaN.

| Function | Meaning and pinned edge |
| --- | --- |
| acos | Inverse cosine, NaN outside [-1,1]. |
| acosh | Inverse hyperbolic cosine, NaN below 1, positive infinity at infinity. |
| asin | Inverse sine, NaN outside [-1,1], preserves zero's sign. |
| asinh | Inverse hyperbolic sine, preserves signed zero and infinity. |
| atan | Inverse tangent, preserves signed zero; infinities approach signed pi/2. |
| atanh | Inverse hyperbolic tangent, signed infinity at +/-1, NaN outside [-1,1]. |
| cbrt | Real cube root, preserves signed zero and infinity, defined for negatives. |
| cos | Cosine in radians, 1 at zero, NaN at infinity. |
| cosh | Hyperbolic cosine, 1 at zero, positive infinity at either infinity/overflow. |
| erf | Error integral, preserves signed zero, +/-1 at signed infinities. |
| erfc | Complementary error integral, 0 at positive infinity, 2 at negative infinity. |
| exp | Base-e exponential, 0 at negative infinity, positive infinity at overflow. |
| exp2 | Base-2 exponential, ties-to-even underflow, infinity at overflow. |
| expm1 | exp(x)-1 with small-x precision, preserves signed zero, -1 at negative infinity. |
| gamma | Gamma, signed infinity at signed zero, NaN at negative integer poles/negative infinity, infinity at overflow. |
| lgamma | log(abs(gamma)), positive infinity at integer poles/either infinity. |
| log | Natural logarithm, negative infinity at either zero, NaN at negative inputs. |
| log2 | Base-2 logarithm, same zero/negative edges as log. |
| log10 | Base-10 logarithm, same zero/negative edges as log. |
| log1p | log(1+x) with small-x precision, preserves signed zero, -infinity at -1, NaN below -1. |
| sin | Sine in radians, preserves signed zero, NaN at infinity. |
| sinh | Hyperbolic sine, preserves signed zero/infinity, signed infinity at overflow. |
| sqrt | Positive square root, preserves signed zero, NaN for negative nonzero values. |
| tan | Tangent in radians, preserves signed zero, NaN at infinity. |
| tanh | Hyperbolic tangent, preserves signed zero, +/-1 at signed infinity. |
| atan2(y,x) | Quadrant-aware angle: signed zero for y=+/-0,x=+0; signed pi for x=-0; signed pi/4 or 3pi/4 for pairs of infinities. |
| hypot(x,y) | Scaled sqrt(x*x+y*y), positive zero for two zeros; infinity wins over NaN. |
| copysign(x,y) | Magnitude of x with sign of y; NaN payload/sign is canonicalized. |
| nextafter(x,y) | Next representable float toward y; returns y when numerically equal, crosses zero by the smallest subnormal. |
| remainder(x,y) | x-n*y with n nearest integer, ties even; zero has x's sign; zero divisor/infinite x gives NaN, infinite y returns finite x. |
| pow(x,y) | Binary64 power with N-POW's floating edges. |
| fma(x,y,z) | x*y+z rounded once; infinity times zero and opposing infinite addends give NaN. |

## Charges and work guard

For a call, let S be the sum of deep sizes of its arguments and R the result's
deep size, or zero on a raise (`K-CHG-003..005`). Each definition carries its
formula: 1+S+R for unary/conversion/text functions; 1+S*S+R for equality,
ordering, multiplication and division/remainders; 1+S*magnitude(exponent)+R for
`pow` and `num.pow`, whose integer power grows with the exponent, and 1+S+R for
`float.pow`, one libm call; 65+S+R for software math. `kind` is 1+R: it reads a tag. `same` is
1+min(size(a), size(b))+nested(a)+nested(b)+R: two heap objects are compared by
identity, never by what they hold, and two immutable values member by member,
so the nested sizes count every member it may walk, down to the heap objects
(`K-CHG-005`).
These formulas describe logical work, independent of object layout, hash tables,
caches, or whether a future body is substituted.

Integer power has a native guard of 1,048,576 upper-bound 64-bit output magnitude
words produced by its exponentiation products. Before each multiply/square it
spends max(1,ceil((bits(left)+bits(right))/64)); binary exponentiation visits low
exponent bits first. The attempted product that passes the limit is refused
before allocation. The guard is in the definition's identity. A floating power
spends no guard units. Other natives have no guard and spend no work units.

## Admission boundary

Each function above has a meaning independent of a language and is directly
usable by multiple dialects or their expansions. Truthiness, coercing addition,
prefix-tolerant parseInt/parseFloat, fixed-width wrapping conversions, language
exception policies, and random Math functions are deliberately excluded. They
belong to dialect helpers or the host. No old runtime tests are ported or deleted
here: the old runtime is owned by the later cutover lanes.
