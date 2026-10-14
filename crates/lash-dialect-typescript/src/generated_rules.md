# TypeScript format v1 paired readable rules

`printer.rs` selects these spellings structurally; the generated reader in
`intrinsics/` decodes them directly. Authored lowering never participates.
The source has one native structured envelope, ordinary function declarations,
and `async function main()`. Maps follow the final text and actual kernel nodes.
Every successful witness in `tests/readable_source.rs` uses complete canonical
bytes; `tests/generated_source.rs` owns the independent stored-field inventory.

| Rule | Applicability and spelling | Exact inverse and nearest boundary witness |
| --- | --- | --- |
| Envelope | Full manifest, entries, private names, injective aliases and full-ID helper aliases, as native objects/arrays in `k.document`. Primitive types are strings once, such as `"any"`. | Strict literal metadata decoder rejects duplicate fields, executable operands and old escaped-JSON operands. `structured_envelope_and_types_keep_nested_optional_metadata` and independent/malformed envelope laws preserve optional fields, nested types and unused metadata. Shared structure: `docs/design/generated-source-format.md`. |
| Names | Legal, collision-free original identifiers; deterministic aliases for keywords, global intrinsic names, `main`, `helper_` and `kernel_name_`. | The envelope table resolves declarations, parameters, captures and references injectively. `identifier_aliases_are_injective_across_shadowing_and_reserved_names`: Unicode name / reserved name or alias collision. |
| Functions and main | `async function name(parameters) { ... }`; closures use anonymous `async function(parameters) { ... }`. | Declaration aliases reconstruct original names. Reserved `main` selects the main Block, without creating a stored Function. The async marker is generated syntax for the existing action boundaries; it introduces no node. Annotated/generator/named expression functions refuse. Realistic recursion, multiple entries and existing closure/capture laws pin this rule. |
| Let and Assign | `let name = rhs;` and `place = rhs;`. `const` is also accepted as Let: the kernel stores no const flag. | One binding per statement, with exact RHS kind and boundaries; no lowering or temporary fusion. A destructuring or annotated binding refuses. Authored corpus includes const and retains every lowered temporary. |
| Expression helper calls | `helper_label(expr, ...)`, for every full identity, including foreign or ambiguously labeled helpers. | Identifier must be bound in the helper table; arguments retain Expr nodes. `helper_calls_keep_expression_action_and_callee_kinds_unambiguous`: ordinary helper expression / same helper as awaited Action. |
| Action calls | `await helper_label(atoms...)`, `await declaredName(atoms...)`, `await (valueName)(atoms...)`. | Await distinguishes Action from Expr. Helper aliases, bare name aliases and parenthesized name aliases select Library, Declared and Value respectively, even under shadowing. The same helper/callee-kind law pins all three and the expression near miss. |
| Effects | `await tools.effect.path(atoms...)`, for every effect name and arity, independent of host tool roots. | The path after `tools` is the exact effect name. Equal manifest result types need no annotation; otherwise `as KernelType<structuredType>` records the exact result, including when no effect requirement exists. `effect_calls_keep_arities_result_annotations_and_atomic_boundaries`: default / mismatched or missing signature; non-atomic args and unknown annotations refuse. Admission checks requirements separately. |
| Operators | Unique `ts.*` operator labels in the preserved manifest bind native unary/binary syntax to their full IDs, for the matching arity. | Reader uses the identical envelope-bound operator table, never a live library or an assumption of JavaScript equivalence. Duplicate labels or other arities keep ordinary helper calls. Precedence preserves the exact tree; unary numeric helper calls use `-(value)` to distinguish signed literal tokens. `native_operator_aliases_preserve_helper_identity_precedence_and_ambiguity` pins grouping, helper identities, duplicate-label boundary and an independent reader witness. |
| Literals | Safe Int: decimal token; larger Int: decimal bigint token. Finite Float: canonical fraction/exponent token, retaining signed zero. Nonfinite Float: `NaN`, `Infinity`, `-Infinity`. Text/bool/null: ordinary literals; Absent: `undefined`; Bytes: `new Uint8Array([octets])`. | Decode original tokens rather than SWC binary64 values; bigint always means Int. Constructor accepts only literal octets. `native_numbers_preserve_variants_bits_and_safe_integer_boundary`: safe / large integer and finite / nonfinite Float. Existing bounded/inventory laws cover bytes and absent/null; noncanonical numeric tokens refuse. |
| Function values | `declared.alias`. | Reserved namespace plus original-name alias reconstructs Literal::Function, distinct from an ordinary variable with the same name. Callee and closure/inventory laws pin the distinction. |
| Collections | List: `[expressions]`; Tuple: `[expressions] as const`; Set: `new Set([expressions])`; Map: `new Map([[key, value], ...])`. | Exact outer syntax selects the kernel variant; tuple/map/set entry order and all expression children survive. `native_collections_keep_variants_order_and_record_applicability`: same items in each variant; holes, spreads and malformed entries refuse. |
| Records | Unique fields: `{field: expression}`, quoted keys for arbitrary names, computed text for `__proto__`. Repeated fields: `Object.fromEntries([[text, expression], ...])`. | Object fields decode in source order; the entry constructor reconstructs Record directly, retaining repeated fields instead of evaluating JavaScript. Same collection law pins ordinary / special / repeated fields; shorthand, methods and non-text computed keys refuse. |
| Members | `target.field`, `target[index]`; a non-identifier Field uses `target["field" as const]`. Parentheses are emitted only when the target's precedence requires them. | Dot and const-asserted text select Field; all other computed operands select Index, including Tuple keys spelled `as const`. `native_members_preserve_field_index_and_assignment_targets` pins field / string-index and tuple-index near misses, lvalues, and child order. The conformance corpus exposed the tuple-key bug and now passes. |
| Controls | Native if/else, while, for-of, break/continue, return, throw and try/catch/finally. | Direct structured inverse preserves blocks and optional-handler presence. `readable_controls_names_and_implicit_else_preserve_statement_boundaries`: handled native Try / handlerless explicit Try, with full byte/map laws. |
| Empty else | Omit an empty else only. | Reconstruct the empty Block and record its implicit Site anchor after the then Block. Nonempty else stays visible. Control and independent map laws pin both boundaries. |
| Terminals and printing | `finish(expression)`, `failure(expression)`, `console.log(expression)`. | Reserved call names select Finish, Fail and Print, distinct from Return, Throw and ordinary calls. Existing behavior/error/effect-trace and realistic error-handling laws pin these distinctions. |
| Task primitives | `spawn(callee, [atoms])`, `await sleep(atom)`, `await join(atom)`, `await cancel(atom)`, `await yieldTask()`, and `await Promise.all/allSettled/race/any(atom)`. | Decode only these exact APIs/arity; callee tags and atoms stay visible. No spawn/join fusion. Existing machine-observation and kernel variant laws preserve every action and all task boundaries. |
| Host primitives | `Date.now()`, `Math.random()`, `projection.read(handle, request)`. | Reserved roots select Clock, Random and Read directly; no receiver/helper nodes. Existing observation/variant laws pin no-arg / bad-arity cases and the projection's exact child order. |

## The only unavoidable body form

`k.try(async function() { ... })` represents a Try with neither catch nor
finally. TypeScript requires at least one handler; adding an empty finally or
erasing the Try would change stored nodes and Sites. Its body wrapper directly
reconstructs a Block, never a Closure. Handled Try always uses native syntax.
The complete generic expression/action/declaration paths this codec supersedes
are deleted; old generated source must be recovered with its old toolchain and
re-exported from the stored Document. Format v1 remains the single 1.0 baseline.

`ordinary_three_effect_workflow_has_no_generic_body_forms` pins the complete
header and body as an independent reader fixture. The shared realistic corpus
covers three effects, recursion with a foreign helper, nested controls, loops
over records, error handling, multiple entries and an RLM tool cell; every body
has zero `k.`. The authored corpus adds real lowering with function tokens,
arithmetic, comparison, loops, exceptions and awaited tools, with no generic
body forms. The independent form/type inventory, 240 bounded documents, 64
Smith inputs, foreign-dialect inputs, 285 admitted conformance cases and existing
Test262 printer laws apply the byte law to the same production codec.

Native record uniqueness costs O(fields log fields); ordered naming/operator
binding has bounded table costs. Rendering and inverse selection are structural
and bounded by the common byte/depth policy. Printing performs no compilation,
library lookup or admission, and map-on/off source is identical.
