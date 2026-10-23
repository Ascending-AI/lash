# TypeScript strings, regular expressions and URI codecs

The TypeScript package implements the existing String, RegExp and URI surface
as kernel-code helpers. It does not add prototype mutation, descriptors,
boxed strings or unsupported regex flags.

`toLocaleString()` and `toLocaleString("en-US")` (also `undefined`) use one
fixed en-US format. Numbers use grouping and half-expand rounding to at most
three fractional digits; arrays call each element's locale method and join
with commas, with empty null/undefined slots. Dates render in UTC. Other
locales, options objects and `Intl.*` are refused as `TS_METHOD_UNSUPPORTED`.
`toLocaleDateString` and `toLocaleTimeString` use the same fixed en-US/UTC
calendar and clock fields. Date local-named readers equal their UTC readers,
and date/time strings use UTC with zero offset. Parsing accepts ISO and its
own exact whole-second `toString`/`toUTCString` formats, without legacy parsing.
`toLocaleLowerCase` and `toLocaleUpperCase` use pinned Unicode 17 root/en-US
mapping with no locale, undefined or "en-US"; other locales are named refusals.
`localeCompare` still needs a pinned collator. Method-call refusals name the
method and receiver kind.

Every JavaScript string position and width counts UTF-16 units. Length and
numeric character-code results are converted to floats at the JavaScript
boundary. `codePointAt` combines a high and low surrogate; `charCodeAt` returns
either unit independently. Operations that would return an isolated surrogate
raise `TS_LONE_SURROGATE_UNSUPPORTED`. String iteration groups surrogate pairs,
matching JavaScript's iterator without confusing positions with scalar indexes.
Unicode case conversion uses the library's pinned Unicode 17.0.0 definitions.
Trimming and numeric parsing use ECMAScript WhiteSpace and LineTerminator
code units: BOM is included and NEL is excluded.

Every package value carries one canonical brand, `__lash_brand: (package,
type)`, and `__lash_built: Bytes`, the identity of its constructor
(`brand.built()`).
`brand.of` reads the tuple; `ts.receiver` derives a dispatch tag from it.
A brand selects members and is not a capability: a typed decode can create
one, so package functions still check their arguments. Dialect readers hide
internal fields. Guest data named `brand` remains ordinary data. The
[host-package guide](../host-packages.md#sessions-use-the-cells-current-set)
describes representation checks and stale-value diagnostics.

A guest RegExp and the extension snapshot both carry
`__lash_brand: ("regex.ecma", "reg_exp")`. Their state is pattern, flags
and lastIndex; neither carries a second dialect brand. The visible
`lastIndex` may hold any JavaScript value; a helper coerces it with ToLength and
constructs an integer-index snapshot for the extension. The extension never
mutates the receiver. Global and sticky exec/test write the returned index to
the original record, so aliases observe the same update. Non-stateful matching
preserves the visible value. `RegExp(r)` preserves identity when flags are
omitted; `new RegExp(r)` constructs a fresh record.

A match array is an internal record with
`__lash_brand: ("typescript", "regex_match")`, its building digest, items,
index, input and groups. Numeric reads, length and iteration expose its items. Captures that
did not participate are `absent`, including named captures. The extension's
`regex.lone_surrogate` cause remains available in the typed dialect refusal's
data. Native charge and guard formulas remain those of the extension.

The method table generates receiver dispatch, reads of unbound methods and
computed-name dispatch. Borrowing an existing prototype method through
`call`/`apply` uses its original receiver and argument convention. It creates
no prototype object. Own record members still resolve through the record.

The four URI codecs use kernel-code bodies. Encoding computes UTF-8 octets
from UTF-16 pairs; decoding accumulates octets and calls the strict UTF-8
library decoder. `decodeURI` preserves reserved escapes verbatim, including
hexadecimal letter case. Invalid escapes and invalid UTF-8 raise `URIError`.

The Test262 runner uses the production kernel machine and one real registry
for both lowering and execution. The family selector is
`TEST262_KERNEL_FILTER=test/built-ins/String/`. The runner fails when any test
recorded as passing does not pass, and prints each such test's observation.
