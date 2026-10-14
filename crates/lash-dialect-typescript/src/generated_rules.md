# TypeScript format v1 paired readable rules

The renderer in `printer.rs` chooses these spellings structurally, without
compilation. `intrinsics/readable.rs`, `expressions.rs` and `statements.rs`
decode their exact inverses. All generic forms remain available. Witnesses
below live in `tests/readable_source.rs`; every successful print is compiled
through the public operations and compared using complete canonical bytes.

| Rule | Applicability and spelling | Exact inverse | Positive / nearest fallback witness |
| --- | --- | --- | --- |
| Binding names | Original passes `legal_identifier` and is outside the reserved namespaces; otherwise use the shared injective `NamePlan` alias. | Resolve declarations, parameters and variable references through the envelope name table; explicit function references retain their original string. | `identifier_aliases_are_injective_across_shadowing_and_reserved_names`: `λ` / `return`, `k`, `helper_label`, `odd name`, and alias collisions; includes parameter shadowing and closure capture. |
| Helper expression calls | Every Expression Call has a full-ID helper alias: `helper_label(expr, ...)`. | An identifier call must resolve in the helper table and reconstructs exactly Expression Call, preserving argument nodes. | `helper_calls_keep_expression_action_and_callee_kinds_unambiguous`: `helper_foreign_same(1)` / same ID in Action Call, spelled `k.call(helper_foreign_same, [1])`. |
| Library action callees | Library callee uses its helper alias directly inside `k.call`/`k.spawn`. | Identifier callee resolves to Library; `k.declared` and `k.value` preserve the other two variants. Arguments remain atoms. | Same law: Library / Declared and Value with the same label; duplicate helper labels receive deterministic suffixes. |
| Integer literals | Magnitude takes at most 53 bits: canonical decimal number token. | Read the original numeric token, never SWC's binary64 value; canonical integer grammar without fraction/exponent reconstructs Int. Larger native tokens refuse. | `native_numbers_preserve_variants_bits_and_safe_integer_boundary`: `9007199254740991` / `k.int("9007199254740992")`; positive and negative integers. |
| Float literals | Finite: kernel canonical number token, always containing a decimal point or exponent. | Parse original token (including unary minus) with kernel Float; preserve bits, signed zero and Float variant independently of manifest NumberPolicy. | Same law: `7.0`, `-0.0`, subnormal and largest finite / `k.float("inf")`, `k.float("nan")`; absent remains `k.absent`, distinct from `null`. |
| Lists | Every List: `[expr, ...]`, including empty. | Array without holes/spread reconstructs List in source order. | `native_collections_keep_variants_order_and_record_applicability`: `[1, 2]` / Tuple and Set with the same items, `k.tuple`/`k.set`; Map remains `k.map`. |
| Records | All field names are property identifiers, unique, and not `__proto__`: `{field: expr, ...}`. Empty is `{}`. | Explicit identifier/value pairs reconstruct Record in source order; no shorthand, methods, computed keys or spread. | Same law: `{λ: 0, ready: 1}` / illegal, special or repeated field uses `k.record`. |
| Field members | Field is a property identifier: `(target).field`. Keywords are legal properties. | Dot reconstructs Field; parentheses introduce no node. Generic `k.field` handles other field names. | `native_members_preserve_field_index_and_assignment_targets`: `(page).ready` / `k.field(page, "not a field")`. Includes the field named `value` as an lvalue. |
| Index members | Every Index: `(target)[index]`. | Computed member reconstructs Index even for a string index; it never becomes Field. | Same law: `(page)["ready"]` / Field with the same text; assignments preserve target-before-value child order. |
| Structured control | If, For and While use exact structured syntax. Try uses native syntax iff catch or finally is present. Blocks retain order, bindings and statement boundaries with two-space indentation. | Decode controls directly, without authored lowering or new temporaries; catch/finally presence survives even when empty. | `readable_controls_names_and_implicit_else_preserve_statement_boundaries`: native try/catch/finally / handlerless `k.try(function() {...})`; if/for/while pinned spelling. |
| Empty else | Else block is empty: omit `else`, recording its Block Site with an implicit anchor immediately after the then block. Nonempty else is spelled. | Absent else reconstructs exactly the empty Block. No statement, action or expression is omitted. | Same law: omitted else / nonempty `else { k.yield(); }`; generic form inventory's independent map walk also checks anchors. |

`independent_native_source_has_exact_numeric_collection_and_member_inverse`
pins a hand-written reader witness. `native_near_misses_refuse_without_authored_lowering`
pins malformed native forms. The independent kernel variant/field inventory,
existing conformance and Test262 printer corpus, 240 bounded documents
(10 names × 8 literals × 3 control contexts), 64 fixed Smith inputs, and two
Python-origin documents exercise composition through the public byte law.

No helper becomes a native operator. Expression versus Action calls, declared
versus value callees, numeric and collection variants, function literals,
closure captures, every temporary and spawn/join boundaries remain visible.
Native record field uniqueness costs O(fields log fields); all other new
rule selection and rendering work is linear in nodes and emitted bytes.
