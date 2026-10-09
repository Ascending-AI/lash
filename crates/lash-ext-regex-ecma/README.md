# lash-ext-regex-ecma

ECMAScript regular expressions for the lash kernel: six extension functions
(`docs/kernel/design.md` §2.4) with only native implementations, built on
`lash-regress`. The crate depends on `lash-kernel-doc` and `lash-regress` and
on no other lash crate.

An embedder creates one `Engine`, stating how many compiled patterns it may
keep, and calls `register` to add the six functions to its
`FunctionRegistry`. A front end calls `Engine::check` on a regex literal.

## Values

A regex is the record `{brand, pattern, flags, lastIndex}`: `brand` is the
text `regex.ecma`, `flags` any of `g`, `i`, `m`, `s`, `u`, `y` at most once
each, and `lastIndex` an integer of zero or more. Every index counts UTF-16
code units. A function is strict: any other record, or an operand of another
kind, raises `type_error`, and a negative index raises `number_range`.

A match is the record `{index, groups, named}`: where it begins; the text of
the whole match followed by each capturing group's, `null` for a group that
took no part; and a record of the named groups by name, or `null` when the
pattern names none.

No function changes its arguments. Where ECMAScript writes `lastIndex`, the
function returns the new value and the caller's dialect writes it to the
record, so that aliases of the regex see it.

## Functions

| Function | Returns | Meaning |
| --- | --- | --- |
| `regex.ecma.compile_check(pattern, flags)` | regex | Checks the pattern and flags. Returns the record with the flags in the order `gimsuy` and a `lastIndex` of 0. |
| `regex.ecma.exec(regex, input)` | `{match, lastIndex}` | `RegExpBuiltinExec`. A regex that is neither global nor sticky searches from the start and keeps its `lastIndex`. Otherwise the search starts at `lastIndex` (sticky: matches only exactly there), which becomes the match's end, or 0 when nothing matches or it lies past the input. `match` is `null` when nothing matches. |
| `regex.ecma.test(regex, input)` | `{matched, lastIndex}` | `exec` without the match. |
| `regex.ecma.match_all(regex, input)` | list of matches | Every match of a global regex from `lastIndex` on; after an empty match the search moves on one code unit, or one code point under `u`. For any other regex, the one match `exec` finds, or none. |
| `regex.ecma.replace(regex, input, replacement)` | `{text, lastIndex}` | `RegExp.prototype[@@replace]` with a replacement text. A global regex replaces every match from the start and returns a `lastIndex` of 0; any other replaces the match `exec` finds. The replacement expands `$$`, `$&`, `` $` ``, `$'`, `$n`, `$nn` and `$<name>`. |
| `regex.ecma.split(regex, input, limit?)` | list of text or `null` | `RegExp.prototype[@@split]`: the pieces between matches, each followed by that match's captures, at most `limit` entries. It reads neither `lastIndex` nor `g` and `y`. |

Errors a function declares:

- `regex.syntax`: the flags or the pattern are refused. `d` and `v` are not
  supported. A pattern longer than 4,096 UTF-16 code units, or with groups
  nested deeper than 32, is refused before it is compiled.
- `regex.lone_surrogate`: a result would hold half of a surrogate pair, which
  kernel text cannot. Only a regex without `u` can produce one.

A replacement that is a function has no native route (§2.4 rule 4): the
dialect's helper calls `match_all` or `exec`, calls the function per match
and joins the pieces in kernel code.

## Charge

Every definition states its charge, and it is in the function's identity:

- `compile_check`: `100 + 8 × size(pattern) + size(flags)`.
- the others: `100 + 8 × deep_size(regex) + size(input) + deep_size(result)`,
  and `replace` adds `size(replacement)`.

The pattern term is the price of compiling, charged on every call. A call is
charged the same whether the engine compiled the pattern for it or already
held the program.

## Guard

Every function but `compile_check` states a guard. Its unit is one step of
the backtracking matcher, or one UTF-16 code unit of text written to the
result. Its limit is `1,000,000 + 64 × size(input)`.

The matcher counts its own steps: one per instruction it dispatches and one
per backtrack entry it pops. The count is a function of the compiled program,
the input and the start offset, and the program is a function of the pattern
and the flags `i`, `m`, `s`, `u`. A call is granted exactly what its counter
has left, so the call that would pass the limit is the same call at the same
count with the cache cold, warm or absent. Compiling spends no guard unit:
its work is bounded by the pattern's length and priced by the charge.

A call runs the matcher to the end, then spends the text of its result, then
builds the result. A guard failure is therefore decided before anything is
allocated.

## Test262 routes

How the `built-ins/RegExp` families that main's record marks as passing, and
the `String.prototype` methods that take a regex, reach these functions. The
TypeScript dialect's helpers own the coercions (`ToString`, `ToLength`,
`ToUint32`), the receiver checks, the write of `lastIndex`, and the shape of
the JavaScript match array.

| Test262 family (passing on main) | Route |
| --- | --- |
| `RegExp` constructor and literals; `property-escapes`, `named-groups`, `lookBehind`, `regexp-modifiers`, `CharacterClassEscapes`, the pattern-syntax tests directly under `RegExp/` | `compile_check` for `new RegExp` and `RegExp()` (`SyntaxError` from `regex.syntax`); `Engine::check` for a literal at lowering; matching through `exec` or `test` |
| `RegExp/prototype/exec` | `exec`; the helper writes `lastIndex` |
| `RegExp/prototype/test` | `test`; the helper writes `lastIndex` |
| `RegExp/prototype/{global,ignoreCase,multiline,dotAll,unicode,sticky}`, `flags`, `source`, `toString` | No call: the helper reads `flags` and `pattern` from the record |
| `String/prototype/match` | Not global: `exec`. Global: `match_all` on a copy of the record with `lastIndex` 0, the helper keeps each match's `groups[0]` and writes `lastIndex` 0 |
| `String/prototype/matchAll` | `match_all` (the helper raises `TypeError` for a regex that is not global) |
| `String/prototype/search` | `exec` on a copy of the record with `lastIndex` 0; `lastIndex` is not written |
| `String/prototype/replace`, `replaceAll` | A replacement text: `replace` (the helper raises `TypeError` for `replaceAll` on a regex that is not global). A replacement function: `match_all` or `exec`, then kernel code |
| `String/prototype/split` | `split` |
