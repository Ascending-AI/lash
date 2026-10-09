# TypeScript strings, regular expressions and URI codecs

The TypeScript package implements the existing String, RegExp and URI surface
as kernel-code helpers. It does not add prototype mutation, descriptors,
boxed strings, locale operations or unsupported regex flags.

Every JavaScript string position and width counts UTF-16 units. Length and
numeric character-code results are converted to floats at the JavaScript
boundary. `codePointAt` combines a high and low surrogate; `charCodeAt` returns
either unit independently. Operations that would return an isolated surrogate
raise `TS_LONE_SURROGATE_UNSUPPORTED`. String iteration groups surrogate pairs,
matching JavaScript's iterator without confusing positions with scalar indexes.
Unicode case conversion uses the library's pinned Unicode 17.0.0 definitions.
Trimming and numeric parsing use ECMAScript WhiteSpace and LineTerminator
code units: BOM is included and NEL is excluded.

A guest RegExp is `{brand: "regex.ecma", pattern, flags, lastIndex}`. Its visible
`lastIndex` may hold any JavaScript value; a helper coerces it with ToLength and
constructs an integer-index snapshot for the extension. The extension never
mutates the receiver. Global and sticky exec/test write the returned index to
the original record, so aliases observe the same update. Non-stateful matching
preserves the visible value. `RegExp(r)` preserves identity when flags are
omitted; `new RegExp(r)` constructs a fresh record.

A match array is a branded record, `{brand: "regex.match", items, index, input,
groups}`. Numeric reads, length and iteration expose its items. Captures that
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
