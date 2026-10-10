# Kernel text, bytes, JSON and formatting

These native definitions live in `lash-kernel-lib::text_json` and are installed
with `register_text_json`. They perform no I/O, invoke no callbacks and mutate
no existing object (`K-LIB-006`, `K-LIB-007`). All arguments are required. Wrong
arity raises `arity`; wrong operand or collection-member kinds raise
`type_error`; nothing coerces. Except for the bounded conversion in `format.decimal_parts`, each definition charges **1 plus the deep size
of every argument plus the deep size of the result**, in the units of
`K-CHG-004` and `K-CHG-005`. No function here uses a cache or a work guard.

Function identities include names, signatures, error kinds and this charge
formula. These are the 1.0 baseline definitions; a behavioral correction after
the baseline needs a new definition identity (`K-VER-002`).

## Text

- **K-LTXT-001.** `text.len(text)` counts scalar values; `text.utf16_len(text)`
  counts UTF-16 units. `text.get(text, index)` returns one scalar as text;
  `text.utf16_get(text, index)` returns one unit as an integer, including a
  surrogate half (`K-VAL-007`). Negative indices count from the end; an index
  outside the sequence raises `index_out_of_range`.
- **K-LTXT-002.** `text.slice(text, start, end)` and
  `text.utf16_slice(text, start, end)` use half-open intervals. Negative bounds
  count from the end; bounds clamp to the sequence's length. Reversed bounds
  give empty text. Both UTF-16 boundaries must be scalar boundaries, even for
  an empty or reversed interval; splitting a pair raises `text_boundary`.
- **K-LTXT-003.** `text.find(text, needle, start)` and
  `text.utf16_find(text, needle, start)` return the first occurrence at or after
  the normalized, clamped start, in the named address space, or -1. An empty
  needle is found at start. UTF-16 search can start between a pair's units;
  it does not fabricate text from that position.
- **K-LTXT-004.** `text.compare(a, b)` compares scalar values;
  `text.utf16_compare(a, b)` compares units. Both return -1, 0 or 1 and put a
  prefix before its extension (`K-VAL-030`).
- **K-LTXT-005.** `text.concat(a, b)` concatenates.
  `text.split(text, separator, limit?)` returns a fresh list, retaining empty
  pieces for a nonempty literal separator; an empty separator gives the scalar
  values and gives an empty list for empty text. A limit keeps the first
  `limit` pieces and builds no other; a negative limit raises `number_range`,
  and an omitted one keeps every piece. `text.join(list, separator)` requires
  text members. `text.replace(text, needle, replacement)` replaces all literal,
  nonoverlapping matches from left to right. An empty needle inserts at every
  scalar boundary, including both ends. No function uses regular expressions.
- **K-LTXT-006.** `text.lower_u<major>_<minor>_<patch>` and
  `text.upper_u<major>_<minor>_<patch>` use the Unicode data shipped with the
  checksum-pinned Rust toolchain, whose version is `char::UNICODE_VERSION`.
  The baseline uses **Unicode 17.0.0**, giving the suffix `_u17_0_0`.
  They use locale-free full Unicode mappings, including multi-scalar mappings
  and contextual final sigma. `text.trim_u…`, `text.trim_start_u…` and
  `text.trim_end_u…` remove Unicode White_Space at the named ends, from those
  same versioned data; U+FEFF is not whitespace. The data version is in each
  function name and hence its content identity. No normalization or locale
  selection occurs. The exact version is recorded in the native corpus.
- **K-LTXT-007.** `text.repeat(text, count)` requires a nonnegative integer count.
  `text.pad_start(text, width, fill)` and `text.pad_end(text, width, fill)` pad
  to a nonnegative scalar width by repeating fill and truncating it at scalar
  boundaries. A shorter width or empty fill leaves text unchanged. A negative
  or machine-size-overflowing count raises `number_range`; an allocation that
  cannot fit raises the native memory bound.
- **K-LTXT-008.** `text.starts_with(text, prefix)` and
  `text.ends_with(text, suffix)` test literal boundaries. An empty pattern
  always matches.
- **K-LTXT-009.** `text.to_code_points(text)` and `text.to_utf16_units(text)`
  produce fresh integer lists. `text.from_code_points(list)` requires Unicode
  scalar integers, raising `invalid_scalar` for surrogates or out-of-range
  values. `text.from_utf16_units(list)` requires integers in 0..65535 and
  well-paired surrogates, raising `invalid_utf16` otherwise (`K-VAL-006`).
- **K-LTXT-010.** `text.normalize_u17_0_0(text, form)` applies Unicode 17
  normalization with the checksum-pinned `unicode-normalization` tables.
  `form` is exactly `NFC`, `NFD`, `NFKC` or `NFKD`; anything else raises
  `normalization_form`. Temporary decomposition and output storage are reserved
  against the native memory bound before normalization starts. The dialect
  supplies ECMAScript coercion and maps an invalid form to `RangeError`.

## Bytes

- **K-LBYTES-001.** `bytes.from_octets(list)` requires integer octets 0..255
  (`number_range` outside it). `bytes.slice(bytes, start, end)` uses the same
  negative-index, clamped half-open rule as text, without UTF-16 boundaries.
  `bytes.concat(a, b)` concatenates; `bytes.compare(a, b)` returns -1, 0 or 1
  in octet order, a prefix first (`K-VAL-008`, `K-VAL-029`).
- **K-LBYTES-002.** `bytes.utf8_encode(text)` writes UTF-8.
  `bytes.utf8_decode(bytes)` requires well-formed UTF-8 and raises `invalid_utf8`
  for invalid, overlong, surrogate or truncated encodings. It never substitutes
  replacement characters.

## JSON

- **K-LJSON-001.** JSON syntax is RFC 8259, including only its four whitespace
  characters. Bad syntax or a lone surrogate escape raises `json_syntax`;
  a paired surrogate escape becomes one scalar. Parsing and stringify refuse
  nesting deeper than `MAX_NESTING_DEPTH` (64) with `json_depth`.
- **K-LJSON-002.** `decode_number(token, expected, policy)` is the shared public
  number decoder for JSON and effect results. `Int` computes the mathematical
  integer directly from decimal digits and exponent, rejecting fractional
  values with `effect_result`. `Float` rounds once to nearest binary64, ties
  to even, rejects infinity with `effect_result`, and preserves negative zero.
  `Number`/`Any` use `NumberPolicy::BySpelling` or `NumberPolicy::Float`, exactly
  as `K-EFF-005` and `K-EFF-006` state. Under a union the first fitting member
  wins (`K-EFF-007`). Integer magnitude has no fixed precision limit.
- **K-LJSON-003.** `parse_json(text, expected, policy, heap)` validates the entire
  stated type before allocating a fresh graph. JSON arrays decode as lists
  under `Any`, or a stated list/tuple; objects decode as records under `Any`,
  or a stated record/text-keyed map. Required fields, closed records, tuple
  lengths, enum strings, unions and member types are checked; other type
  shapes raise `effect_result`. Duplicate keys keep the last value at the
  first occurrence's insertion position. Separate parses share no identity
  (`K-EFF-003`).
- **K-LJSON-004.** `stringify_json(value, heap)` and `json.stringify(value)`
  write compact JSON, with exact integer digits and canonical finite float
  spelling (`K-VAL-031`, `K-VAL-032`). Tuples/lists are arrays; records and
  text-keyed maps are objects in insertion order. Absent, bytes, timestamps,
  sets, errors, functions, closures, tasks, handles and refs raise `not_data`,
  anywhere in the graph. NaN and infinities raise `json_number`; non-text map
  keys raise `json_key`. No omission, coercion or host-resource read occurs.

`json.render_parts(value, verbatim_kinds, verbatim_field)` uses the same
strict traversal and returns a flat list of escaped text fragments and raw
finite numbers. A caller supplies number spelling and joins the fragments.
Inside a container whose kind is in the text set, or a record whose named
field holds text, numbers retain their kernel spelling in text fragments.
The selection is inherited by descendants. Depth, cycle, key and data
checks retain the strict writer's order. Text buffers and each fragment
slot are reserved before allocation; the charge uses the standard native
argument/result deep-size formula.

- **K-LJSON-005.** Stringify copies shared objects each time and refuses an
  active-path cycle with `cycle`. Shared acyclic graphs are allowed.
- **K-LJSON-006.** Native `json.parse(text, numbers, policy)` makes the number
  choice explicit as text enums: `numbers` is `int`, `float` or `number` and
  applies recursively to every number token; `policy` is `by_spelling` or
  `float`, used only for `number`. Arrays are lists and objects are records.
  The Rust entry point accepts structured `Type` for effect-site signatures.

## Neutral formatting

- **K-LFMT-001.** `format.fixed(number, precision)` writes exactly precision
  digits after the point, omitting the point at precision zero. Integers stay
  exact and acquire zero fractional digits. Floats round to nearest, ties to
  even; signed zero is preserved. `format.scientific(float, precision)` writes
  one digit before the point and exactly precision after it, with a lowercase
  `e`, no exponent plus sign or leading zeros. Non-finite values or negative
  precision raise `number_range`.
- **K-LFMT-002.** `format.radix(integer, radix)` writes an exact signed integer
  in radix 2..36 using lowercase digits, no prefix; other radices raise
  `number_range`. `format.pad(text, width, fill, side)` uses scalar padding,
  with side `start` or `end` and no dialect-specific sign or alignment policy.

- **K-LFMT-003.** `format.decimal_parts(float)` returns a tuple
  `(kind, negative, digits, exponent)`. Kind is `finite`, `infinity` or `nan`.
  For a finite value, ASCII `digits` contain its shortest round-trip decimal
  significand, with no point, leading zeros or trailing zeros. Reading
  `digits * 10^exponent` with the stated sign rounds back to the same binary64.
  Both zeros have digits `"0"` and exponent 0, retaining their sign. Infinity
  retains its sign; NaN has sign false. Both special kinds have empty digits
  and exponent 0. No notation threshold, decimal point, exponent padding or
  precision rounding is chosen here. TypeScript and Python apply their own
  layout in their helpers.
  The existing shortest algorithm writes directly into 17 bytes of scratch
  digits; the binary64 exponent is in -324..308 before digit normalization.
  Charge is **65 plus the deep sizes of the argument and result**: the bounded
  binary64 conversion has 64 fixed units, with scanning/copying proportional
  to the produced digits priced by the result size. The tuple's four values,
  its kind/digit bytes and exponent magnitude are reserved before allocation.

The named laws are in `crates/lash-kernel-lib/src/text_json/tests.rs`.
Direct native corpus shards supplement kernel-document cases; they do not
stand in for machine execution or document charge proofs.
