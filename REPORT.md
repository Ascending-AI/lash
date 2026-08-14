# FIG-1304 review fix report

Branch: `samuel-fig-1304`

Implementation baseline: `e3b073423` (reviewed FIG-1303 head)

Round-1 fix baseline: `bf704079b` — a pre-rebase SHA, retained for provenance only. The branch was rebased onto `93874e275`; the reachable branch history is `218a580d7..HEAD`, and every commit cited in the ledgers below is an ancestor of the head.

Round-2 fix baseline: `13e31367e` (first decisive fresh-eyes verification, verdict BLOCK)

Round-3 fix baseline: `4dc2bd5cc` (round-2 closure verification, verdict BLOCK)

Round-4 fix baseline: `712665428` (round-3 closure verification, verdict BLOCK)

Round-5 fix baseline: `97c797c4b` (round-4 closure verification, verdict BLOCK)

Round-6 fix baseline: `e9dbf4605` (round-5 closure verification, verdict BLOCK)

Outcome: all 22 merged findings from the Opus and sol-sub adversarial reviews are closed, all ten findings from the first verification round are closed, all five findings from the round-2 closure verification are closed, all five findings from the round-3 closure verification are closed, all four findings from the round-4 closure verification are closed, and the single round-5 finding is closed. The no-abort guarantee is no longer verified shape-by-shape: it is carried by generative regressions over two axes — the recursive-production families of **the grammar SWC parses**, and the **lexical fidelity** of the preflight's own lexer against SWC's — cross-checked mechanically against SWC's AST node kinds and by a deterministic fuzzer, with both guards' power re-verified by mutation. Accepted TypeScript operations now follow the checked Node v25.2.1 oracle, unsupported operations reject with stable `TS_*` diagnostics, the durable dialect marker fails closed without becoming VM identity, and the full repository gate battery passes.

This layer is still a **pure calculator**. A TypeScript cell has no tool calls, no `await`, and no deferred tool resolution. Rendered tool signatures are synchronous descriptions for FIG-1305; they do not make tools executable here.

## Finding ledger

Each row identifies the committed failing regression and the commit that made it green. Shared commits contain separate focused tests for every item in their row. Every SHA below is reachable from the branch head; the round-1 ledger's original SHAs were pre-rebase and have been re-pointed at their rebased equivalents.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| 1 | Resume derives reference semantics from the compiled dialect; a non-aliased first suspension does not prevent later TypeScript aliasing. | `dae8b09f3` | `f3c783ac0` |
| 2 | Lashlang refuses an authored TypeScript marker and any shared graph, regardless of the wire marker; failures are typed. | `dae8b09f3` | `f3c783ac0` |
| 3 | `State::reference_semantics` is derived per program and no longer sticks historically. | `dae8b09f3` | `f3c783ac0` |
| 4 | Missing, out-of-range, and negative reads produce `undefined`; non-negative writes extend arrays with holes; negative/non-index writes reject without element corruption. | `4e5ab8591` | `497737278` |
| 5 | `typeof` distinguishes heap objects, arrays, and functions, while an unresolved operand returns `"undefined"`. | `4e5ab8591` | `497737278` |
| 6 | Loose equality follows the recursive boolean-to-number rule, including `null == false` being false. | `4e5ab8591` | `497737278` |
| 7 | TypeScript Number-to-String uses shortest round trips and ECMA exponent thresholds/formatting without changing Lashlang formatting. | `4e5ab8591` | `497737278` |
| 8 | String-to-Number rejects Rust-only spellings and handles signed prefixes and arbitrarily large hexadecimal input per ECMA. | `4e5ab8591` | `497737278` |
| 9 | TypeScript `split` and `join` use dialect-specific ECMA conversion, including empty separators and recursive array conversion. | `4e5ab8591` | `497737278` |
| 10 | String/array `.length`, `NaN`, and `Infinity` are supported with UTF-16 string length. | `4e5ab8591` | `497737278` |
| 11 | String relational comparison orders UTF-16 code units. | `4e5ab8591` | `497737278` |
| 12 | Lone-surrogate literals reject as `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`; no lossy U+FFFD conversion occurs. | `4e5ab8591` | `497737278` |
| 13 | A TypeScript `this` parameter is erased and does not affect runtime arity or binding. | `4e5ab8591` | `497737278` |
| 14 | Both reads and writes of captured mutable bindings reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED`. | `c334216ff` | `03aec52ef` |
| 15 | Catch bodies have their own mangled lexical scope and cannot overwrite enclosing slots. | `c334216ff` | `03aec52ef` |
| 16 | Unresolved identifiers and `arguments` reject statically as `TS_UNKNOWN_BINDING`; only `typeof unresolved` is legal. | `c334216ff` | `03aec52ef` |
| 17 | Assignment to an undeclared name rejects statically and cannot create an implicit durable global. | `c334216ff` | `03aec52ef` |
| 18 | Hoisted function declarations see top-level lexical bindings regardless of source order, including the README captured-object example and later-`const` captures. | `c334216ff`, `04a3dd981` | `03aec52ef`, `a0156878d` |
| 19 | Lexical bindings named `print`, `finish`, or `console` win over host interception. | `c334216ff` | `03aec52ef` |
| 20 | Source nesting is guarded before SWC/adapter recursion and reports `TS_SOURCE_NESTING_LIMIT` under the 2 MiB stack contract. | `457b6f5de` | `a5747ebc4` |
| 21 | A checked-in Node v25.2.1 differential table permanently covers both review corpora and the fix findings. | `de1b3c94d` | `3f5b2d21c` |
| 22 | The README/deviation register, structural diagnostics, `console.log` arity, and signature rendering now describe and enforce the real surface. | `7c90c79fd` | `bd72cfb63` |

### Round 2 — decisive fresh-eyes verification

The verification round returned BLOCK on two P0 defects and eight smaller findings. Each is closed below with its own red-then-green pair.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| P0-1 | Delimiter-free source nesting (`!`, unary `-`, `typeof`, `?:`, binary chains) no longer reaches SWC's recursive parser: one cumulative budget bounds every recursive source shape and returns `TS_SOURCE_NESTING_LIMIT`. Child-process regressions cover all five operator classes at 10k plus the original 10k-paren shape. | `1c51ef8fa` | `4cae4e930` |
| P0-1b | That preflight initially accumulated statement keywords across sibling statements, so 28 flat `if`/`while` statements falsely rejected. Braces are now classified as statement blocks or object literals, and a statement boundary releases the operator run it terminates. | `c5d198ae9` | `599a424d3` |
| P0-2 | Closure capture is transitive: a binding owned two or more function levels out is registered on every enclosing frame between the owner and the use site. | `2ec936167` | `c62c1089a` |
| P1-3 | Mutually recursive and nested hoisted function declarations are accepted. Each cycle routes through one generated frame record; a nested declaration may read an enclosing function's bindings regardless of source order. | `4721fe8f9`, `d4d70c372` | `e788601b9` |
| P1-4 | Source identifiers starting with `__typescript_` reject with the new `TS_RESERVED_IDENTIFIER`, so a user name can never clobber a generated binding — including a durable root global. | `bd0813712` | `e82e5e6d7` |
| P2-5 | `ToNumber(String)` trims the literal ECMA-262 `StrWhiteSpace` set rather than Rust's `White_Space` property: ZWNBSP is whitespace, NEL is not. | `06a596efb` | `978645dd5` |
| P2-6 | Both shape-dependent lone-surrogate runtime rejections (`split('')` and string indexing on astral characters, `TS_LONE_SURROGATE_UNSUPPORTED`) are registered and carry oracle rows. | `06a596efb` | `b49499b7e` |
| P3-7 | Ledger SHAs re-pointed at reachable commits. | — | this report |
| P3-8 | The exclusion list names compound assignment operators and the arity-1 restriction on mapped String methods, each backed by an executable rejection test. | — | `b49499b7e` |
| P3-10 | The oracle's distinct-expression count is stated wherever its row count is cited. | — | `b49499b7e` |

### Round 3 — round-2 closure verification

The closure verification confirmed both round-1 P0s closed and returned BLOCK on one regression the round-2 fixes introduced, plus four smaller findings.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| R2-4 | The per-cycle frame record made any session that defined mutually recursive top-level functions unpersistable: record and members formed a heap cycle reachable from a durable root, so `Vm::suspend()` and `State::snapshot()` failed after the program had already run, and `__typescript_0_frame` sat in `runtime_globals` where the bound-variables prompt would render it. The frame-record lowering is removed; declaration cycles reject statically as `TS_MUTUAL_RECURSION_UNSUPPORTED` naming the cycle. | `10f2ff326` | `883e083a3` |
| R2-1 | A newline in automatic-semicolon-insertion position releases the operator run, so semicolon-free statement sequences no longer reject at 27 statements. The release is suppressed while a statement form is open and when the next token continues the expression. | `ca1baf00d` | `4c91df3d3` |
| R2-2 | Template holes draw on the source budget, so a long template rejects as `TS_SOURCE_NESTING_LIMIT` instead of leaking the shared AST's generic `TS_INVALID_SHARED_AST`. A sweep of eight shapes at every count to 80 proves no accepted-grammar source can reach that limit. | `ca1baf00d` | `4c91df3d3` |
| R2-5 | A named function expression binds its own name inside its body, so the classic self-recursive function expression works; the generated namespace is reserved on that path too. | `250b549f0` | `0fac55e0e` |
| R2-3 | The README states the budget in units with a measured per-form cost table instead of a bare 28. | — | `c6546721c` |

### Round 4 — round-3 closure verification

Round 3 closed every round-2 finding and re-opened the round-1 no-abort guarantee for two shape families nobody had enumerated. That is the third round in which the guarantee held for the shapes under test and failed for a shape outside them, so round 4 replaces the hand-written corpora with a generative guard.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| R3-1 | The ASI release treated every operand-opening keyword as able to end a statement, so `typeof\n`, `void\n`, `new\n` and `delete\n` chains released the budget on every line and aborted the process at ~1 500 lines. Only `return` among them can end a statement; a word's statement-ending and expression-ending bits are now tracked separately. | `b3c26f11f` | `160af2346` |
| R3-2 | Postfix tails — call, subscript, tagged template — opened *and closed* a delimiter pair per link, so a chain of any length sat at depth one while the tree it produced was as deep as the chain. A single-line 20 000-link chain aborted. A postfix frame now charges a unit that survives the pair closing. Present since round 2. | `b3c26f11f` | `160af2346` |
| R3-3 | A line comment consumed its own newline, so a trailing `//` suppressed the ASI release and 27 semicolon-free statements with trailing comments falsely rejected. The rule now runs on the transition out of the comment. | `b3c26f11f` | `160af2346` |
| R3-4 | A root-level `catch` or block binding was mangled unconditionally and, at root, published a generated `__typescript_` name into the durable globals and the bound-variables prompt. Mangling now happens only where a name of the same spelling is actually visible; the residual shadowing case keeps a generated slot and the dialect filters the reserved prefix out of the prompt. | `b3c26f11f` | `3b1deab30` |
| R3-5 | The per-form cost table carries the postfix cost, the ASI rules, and the re-measured ceilings. | — | `a66f822db` |

### Round 5 — round-4 closure verification

Round 4 closed every round-3 finding and endorsed the generative method, then failed on the argument that method was derived from. The exhaustiveness claim was stated against the *accepted surface*, but the preflight runs before SWC, and SWC parses all of TypeScript: a production this crate rejects later still recurses in the parser the guard exists to protect. Walking the parsed grammar instead turned up two uncharged families and a missing continuation token.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| R4-1 | Labelled statements recurse through `Identifier ':' Statement` with neither a delimiter nor a keyword to charge, and 502 bytes of `a:` aborted the process — the smallest abort of any round. A `:` at statement level with no conditional waiting for it now charges a unit. | `dc305a631` | `8cc05714a` |
| R4-2 | `Expression as Type` and `Expression satisfies Type` are left-recursive in the parsed grammar and charged nothing; so did the type-level prefix operators (`keyof`, `readonly`, `infer`, `unique`, `asserts`, `is`). All are charged, and the cast keywords joined the continuation set so a newline-split chain is not released. | `dc305a631` | `8cc05714a` |
| R4-3 | A backtick was missing from the continuation set, so a newline before a tagged template released the budget and zeroed the per-link charge added in round 4. | `dc305a631` | `8cc05714a` |
| R4-4 | The one remaining generated-name residual — a block binding that actually shadows an outer name — is registered, and the register's numbering matches its contents. | — | `85634cb52` |

### Round 6 — round-5 closure verification

Round 5 closed every round-4 finding and judged the grammar axis genuinely closed: 53 fresh shapes, one hit. That one hit was on a third axis neither standing guard could see.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| R5-1 | The preflight's own lexer was ASCII-only, so a non-ASCII identifier character broke the word token: `previous` became a bare UTF-8 continuation byte, `can_end_expression` went false, and the label charge — the only charge with no second token to fall back on — stopped firing. 1 292 bytes of `aé:` aborted the process. Identifier scanning now treats every byte at or above `0x80` as an identifier character, walks `\uXXXX` and `\u{…}` identifier escapes, and stops at U+2028/U+2029, which end a line for automatic semicolon insertion and for line comments as they do in ECMAScript. | `1bf2a4503` | `7b2e25aaa` |

The other lexical surfaces the verification enumerated were checked in the same pass. Numeric separators are benign — `1_000` splits into `1` and `_000`, and both halves classify as expression-ending, so nothing is disarmed. The Unicode line terminators were failing in the opposite direction: SWC ends a line on them and the scanner did not, which never aborts but did reject 120 U+2028-separated declarations that are perfectly legal; that is fixed with the same change and covered in the legal direction.

The new fuzzer then found a defect nobody had reported: a `;` inside a `for` header, or a `,` between arguments, cleared the open-statement-form bookkeeping that suppresses the ASI release, so a newline could end a `for (;;)` before its body and release the budget. `8cc05714a` scopes that reset to statement level.

Fixing R3-2 initially charged a statement head (`if (…)`) as a postfix call, which cost every `if`/`while` block a third unit and dropped its documented ceiling from 13 to 9; `7879c7f23` separates ending a statement from ending an expression and restores it.

Closing R2-1 exposed one further abort shape of my own making — a newline-separated `if (1)` chain released the budget on every line and reached SWC unbounded — which the same fix closes by suppressing the release while a statement form is open; the 26-shape 2 MiB abort sweep covers it.

The optional register line on `console.log` versus Node's inspector formatting landed with `b49499b7e`. Auxiliary round-3 repair: `0e47352a5` (strict-lint style). Auxiliary repairs are `a2bdf3b3d` (strict-lint style) and, from round 1, `074b24b3e` (synchronous signature docs assertion), `db4201eb7` and `ea8ca2aeb` (strict-lint style), and `67de889dd` (remove one redundant Lashlang test so its established package count remains 461). None changes the accepted language contract.

## Review conflict resolution

Item 14 was resolved by executing the competing claim, not by compromise. The exact Opus assign-only capture repro was committed in `c334216ff` and failed before the fix: a write to an outer `let` silently targeted a function-local slot. Therefore the Opus finding was correct and the sol-sub conclusion that no hole existed was incorrect for that shape. Commit `03aec52ef` applies the same ownership check to reads and writes and returns `TS_MUTABLE_CAPTURE_UNSUPPORTED` in both paths.

## Design decisions

### Array writes

- `a[3] = 9` on `[1]` follows ECMAScript: the array extends and positions 1 and 2 are `undefined` holes.
- Negative and other non-index writes would create named array-object properties, which the v1 heap-list representation cannot encode. They reject at runtime as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`.
- Such a rejection never wraps the index or mutates an existing element.

### `console.log`

Free `console.log` accepts zero or more arguments. Each argument receives ECMA `ToString`, the results are joined with one ASCII space, and one host print effect is emitted. Zero arguments print the empty string. A lexical `console` binding is ordinary user data and takes precedence.

### Source nesting

The effective v1 TypeScript source-nesting budget is **28 levels, cumulative and shared**. It is one budget, not several independent ones: every open delimiter (`(`, `[`, `{`, and a template hole) and every nested recursive operator or statement form draws on the same 28 units. Recursive forms are the prefix operators (`!`, `~`, unary `+`/`-`, `typeof`, `void`, `delete`, `new`, `await`, `yield`), the binary, ternary and member operators, and the statement keywords (`if`, `while`, `do`, `for`, `in`, `instanceof`, `with`). Operator counts from an enclosing delimiter frame stay active while the scanner visits an inner expression, so mixed nesting can no longer reach a multiple of the nominal cap the way three independent delimiter counters allowed. A statement boundary — `;`, `,`, the `}` that closes a statement block, or a newline in automatic-semicolon-insertion position — releases the operator run it terminates, so a flat sequence of statements is one level deep however long it runs, punctuated or not. The ASI release is suppressed while a statement form is still open (`if (1)` alone on a line is not a complete statement) and when the next token continues the expression (a leading `.`, `+`, `(` and so on); both suppressions are load-bearing, since without them a newline-separated chain reaches the parser unbounded. A brace-free chain such as `if (1) if (1) …` still accumulates and rejects.

A postfix tail — `f(1)`, `a[0]`, a member step, a tagged template — charges a unit that outlives the pair it closes, because the tail leaves the tree one level deeper than it found it. Without that charge a chain of any length stayed at depth one, which is how `f(1)(1)…` reached SWC unbounded from round 2 until round 4. The charge applies only where the preceding token can end an expression, so a statement head like `if (…)` is not mistaken for a call.

Template holes draw on the budget like binary-chain terms, because a template lowers to a left-nested concatenation chain whose depth outlives each hole. That is what keeps the source budget binding *before* the shared AST's own nesting limit: a sweep of eight shapes — templates, concatenation, arrays, calls, member chains, prefix operators, ternaries and objects — at every count up to 80 confirms no accepted-grammar source can reach `TS_INVALID_SHARED_AST`. The 28 is a budget in units, not visible levels; the README carries the measured per-form cost table. The adapter enforces the same 28 with one shared counter across statement and expression conversion.

Level 28 compiles and executes on a 2 MiB child-thread stack; level 29 rejects as `TS_SOURCE_NESTING_LIMIT`. Child-process regressions across six shapes — 10,000 parentheses and 10,000 each of `!`, unary `-`, `typeof`, `?:`, and `+` chains — prove named-diagnostic-not-abort behavior; before the fix the last five aborted the process with a SIGABRT stack overflow at roughly 1.6 KB of source. Block lowering was flattened enough to make 28 the pinned source-level budget rather than exposing the shared AST's internal per-node cost.

### Mutual recursion

**v1 does not support mutually recursive function declarations.** Closures capture by value, so a cycle of hoisted declarations has no emission order: every member needs its peers' values before any of them exists.

Round 2 routed each strongly connected component through a generated frame record — members captured the record instead of their peers and rebound them on entry. It worked in memory and failed at the durability boundary: the record holds the member closures while every member closure captures the record, which is a heap cycle, and because both are root-level bindings the cycle is reachable from a durable root. Cyclic heap objects are rejected at durable capture (register item 3), so every `print`, cell boundary and between-turn snapshot in a session that defined such a group failed with an internal `UnserializableValue` — after the program had already run. Trading a compile-time diagnostic for a runtime persistence failure is strictly worse, and the frame record was also the layer's only generated **global**, so it reached `runtime_globals` and would have rendered into the model-facing bound-variables prompt.

The lowering is removed. A declaration cycle now rejects statically as `TS_MUTUAL_RECURSION_UNSUPPORTED` and names the cycle it found (`cycle: isEven -> isOdd -> isEven`), which is the honest form of the same deferral that item 3 already carries. Everything adjacent stays supported: self-recursion, named self-recursive function expressions, nested declarations reading enclosing bindings in any source order, and acyclic declaration chains, which keep the topological emission. A durability regression suspends and snapshots every accepted binding shape and asserts that no generated name ever reaches the global surface.

### Tool signatures and structural diagnostics

Rendered signatures are synchronous. Unsafe or reserved tool identifiers use collision-proof hexadecimal `__lash_tool_...` names, unsafe properties are quoted, and user names cannot collide with the generated namespace. Default parameters, rest parameters, and ambient declarations reject precisely as `TS_PARAMETER_DEFAULT_UNSUPPORTED`, `TS_PARAMETER_REST_UNSUPPORTED`, and `TS_DECLARE_UNSUPPORTED`.

## Durable dialect outcome

The continuation marker is a persistence requirement and lower bound, not VM identity. The compiled program dialect selects the VM behavior on fresh install and resume. TypeScript may persist a reachable acyclic shared graph; Lashlang continues to require its exclusive-ownership forest. Program/marker mismatch, shared Lashlang state, dangling references, cycles, unreachable objects, and invalid accounting fail closed with typed errors. Flag derivation occurs for each program, so prior TypeScript execution cannot contaminate later Lashlang execution.

## Differential oracle

`crates/lash-typescript/tests/differential/expectations.tsv` contains 310 committed rows plus its header:

| Provenance | Rows |
| --- | ---: |
| Opus review corpus | 163 |
| sol-sub review corpus | 124 |
| Combined fix findings | 23 |

Duplicates are retained to keep provenance counts executable: the 310 rows carry **237 distinct expressions**, so the table's effective corner coverage is that of 237 behaviours, not 310. Regeneration under Node v25.2.1 after the round-2 additions is byte-identical to the committed table apart from the six new rows. `generate.mjs` stamps and requires Node `v25.2.1`; regeneration is a deliberate reviewed action, never part of an ordinary test run. Accepted rows compare observable output, static rejections compare their named diagnostic, and the unrepresentable array-property write compares its named runtime rejection.

## Honest deviation register

These are the only intentional deviations for the accepted v1 surface. They are runtime-system or representation constraints, not alternate silent semantics:

1. Existing instruction, wall-clock, logical-memory, and call-frame limits may terminate execution with typed VM bound errors.
2. TypeScript source nesting is limited to 28 budget units and rejects as `TS_SOURCE_NESTING_LIMIT`, and a cell is limited to 64 KiB of source, rejecting as `TS_SOURCE_TOO_LARGE`. The size bound is a runtime-system constraint: it is what makes the parse-stack reservation finite, and so what makes stack exhaustion arithmetically unreachable.
3. Cyclic heap objects reject at durable capture. Shared acyclic identity is preserved; cycle-capable durable graph encoding is deferred.
4. Mutable lexical captures reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED` on both reads and writes until durable lexical cells exist. Immutable captures and mutation through captured object references work.
5. The host boundary is JSON-shaped: object properties holding `undefined` are omitted, array elements become `null`, and incoming JSON cannot manufacture `undefined`.
6. Lone UTF-16 surrogates are not representable by the v1 UTF-8 value model. Literals reject statically as `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`; two further shapes produce one only at runtime and reject there as `TS_LONE_SURROGATE_UNSUPPORTED` — splitting a string containing an astral character into units, and indexing into one. Both are catchable TypeScript exceptions.
7. Negative and other non-index array writes reject as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`; non-negative out-of-range writes retain ECMAScript hole-extension behavior.
8. Identifiers starting with `__typescript_` are reserved for the lowerer's generated bindings and reject as `TS_RESERVED_IDENTIFIER`.
9. `console.log` is host-defined rather than ECMA-262 and prints ECMA `ToString` of each argument, so `console.log({a: 1})` prints `[object Object]` where Node's inspector prints `{ a: 1 }`.
10. A block-scoped binding that shadows an outer name of the same spelling is lowered to a generated slot, which is the one place a `__typescript_` name can appear in persisted globals. It is dead by any turn boundary and the dialect filters the reserved prefix out of the bound-variables prompt, so it is never rendered.
11. Mutually recursive function declarations reject as `TS_MUTUAL_RECURSION_UNSUPPORTED` with the cycle named. This is item 3's deferral seen from the front end: the only v1 lowering for the shape builds a durable-rooted heap cycle, so the shape fails closed at compile time instead of at the durability boundary.

No reviewed semantic divergence was moved into this register.

## Compatibility and identity evidence

The Opus seven-program corpus was compiled independently from base `e3b073423` and fix-round head `67de889dd`. For every program, module refs, host-requirements refs, raw `ModuleArtifact::to_store_bytes()` output, and normalized first-effect continuation bytes matched. The concatenated identity records on both sides have SHA-256:

`b76f7578d37928ac4c8b044f62b7bbedd40e0079c114627b428063efa8dc603d`

The continuation comparison removes only `active_execution_elapsed` (pre-existing nondeterminism), `format_version`, and the new `reference_semantics` persistence marker, exactly as the reviewer probe does. `CompiledProgram` debug text intentionally gained `dialect: Lashlang`; this is diagnostic metadata and is not part of the artifact or continuation byte comparison. The dedicated package run remains exactly **461 Lashlang unit tests**, all passing with the pre-existing semantic expectations.

## Gate results

Commands ran unpiped from `/workspace/code/lash-fig-1304` with `CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1304` where applicable. The table below is the round-2 battery, rerun in full on the final tree.

| Gate | Result | Evidence |
| --- | --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS | Entire workspace and all targets checked. |
| `cargo test --workspace --locked` | PASS | Full unit, integration, property, UI/trybuild, simulation, conformance, and doctest suite completed with zero failures. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS | Zero warnings. |
| `cargo fmt --all --check` | PASS | No formatting drift. |
| `python3 scripts/check_included_file_formatting.py` | PASS | 37 include-assembled Rust files checked. |
| `python3 scripts/lint_docs.py` | PASS | 46 HTML pages and 42 registry pages checked. |
| `bash scripts/check-rustdoc.sh` | PASS | 599 `lash-core` public members documented, 0 missing; workspace docs built. |
| `python3 scripts/check_test_quarantines.py` | PASS | Quarantine metadata valid. |
| `python3 scripts/check_api_example_coverage.py` | PASS | 8,065 API coverage entries satisfied. |
| `just perf-guard` | PASS | 297 Lashlang perf results and 1 profile result; runtime and stack budgets passed. |
| `bash scripts/check-production-file-size.sh` | PASS | Production and test/support files remain within repository budgets. |
| `git diff --check 93874e275..HEAD` | PASS | No whitespace errors across the whole branch before the final report commit. |
| `cargo test -p lash-typescript --locked` | PASS | 210 tests across unit, depth, dialect, differential, ECMA, rejection, scoping, structural, and curated test262 suites (94 before round 2, 109 before round 3, 115 before round 4, 165 before round 5, 194 before round 6). |
| Committed Node differential table | PASS | All 310 rows match the checked Node v25.2.1 expectations or named rejection; regeneration under Node v25.2.1 reproduces the committed file byte for byte. |
| `node crates/lash-typescript/tests/differential/generate.mjs` | PASS | Deliberate regeneration, Node version stamped and enforced by the generator. |
| Base-vs-head Lashlang byte identity | PASS | Seven of seven raw artifact and normalized continuation records match; combined SHA-256 shown above. |
| `cargo test -p lashlang --locked` | PASS | 461 unit tests passed, unchanged; all package integration/property/stack tests also passed. |
| Lexical-fidelity sweep (14 units x 2 axes x 20 000 repeats, 2 MiB child processes) | PASS | Non-ASCII identifiers, identifier escapes, numeric separators and Unicode line terminators, in the nesting direction; the legal direction covers 120-statement programs of each. |
| Generative family sweep (66 units x 2 axes x 100 000 repeats, 2 MiB child processes) | PASS | Every recursive production of the grammar SWC parses, and mixed combinations, inline and one per line, returns `TS_SOURCE_NESTING_LIMIT` with a clean exit. |
| SWC AST node-kind classification | PASS | Exhaustive wildcard-free matches over `Expr`, `Stmt` and `TsType`; a new SWC variant is a compile error. |
| Deterministic parser fuzzer (4 096 sources x 4 lengths, child processes) | PASS | No source built from the charged alphabet drives SWC past the stack budget; half the corpus pairs a lexical atom with a charge-bearing tail, and the corpus is required to reach the budget on more than half its sources. |
| Mutation power-check (identifier `>= 0x80` rule deleted) | RED as designed, then restored | Both the lexical sweep and the fuzzer fail; the tree is restored and green. |
| Legal ASI corpus (trailing/block comments, CRLF, multiline templates, newline arrow bodies, 120-statement sequences) | PASS | No false rejection. |
| Shared-AST leak sweep (8 shapes x 80 counts) | PASS | No accepted-grammar source reaches `TS_INVALID_SHARED_AST`. |
| Durability corpus (suspend + snapshot) | PASS | Every accepted binding shape suspends, encodes its continuation, and snapshots; no generated name reaches the global surface. |

The first verification pass exposed a stale docs-snippet assertion that still expected `Promise` signatures and two strict-lint style findings. Commits `074b24b3e`, `db4201eb7`, and `ea8ca2aeb` corrected them; every affected gate and the full battery were rerun successfully on the corrected tree. In round 2 the workspace suite exited 0 on the first run; the `lash-sqlite-store` `sqlite_real_turn_crash_matrix` seeded flake recorded by the verifier (P3-9) did not reproduce, and it remains a pre-existing concurrency-load flake unrelated to this layer — no TypeScript or Lashlang code path is involved.

Round 6 reran the whole battery again, workspace suite green on the first run. Round 5 reran the whole battery once more on the final tree, workspace suite green on the first run. Round 4 reran the whole battery again on the final tree, with the workspace suite green on the first run. Round 3 reran the whole battery on the final tree; the workspace suite again exited 0 on the first run and the `sqlite_real_turn_crash_matrix` seeded flake did not recur.

The `ToNumber(String)` whitespace correction lives in `crates/lashlang/src/runtime/javascript.rs`, which is reached only through the `JavaScriptUnary`/`JavaScriptBinary` opcodes the TypeScript lowering emits. No Lashlang program semantics, and no semantic hash, change with it.

## Explicitly deferred

- FIG-1305 owns TypeScript tool execution and `await`; this layer deliberately disables both and also disables deferred tool resolution.
- Continuation `active_execution_elapsed` nondeterminism predates this layer and remains deferred; the byte-identity method normalizes that field explicitly.
- Cycle-capable durable graphs and durable mutable lexical cells remain deferred and fail closed as documented above.

No compatibility shim, fallback parser, dual execution path, migration adapter, silent semantic divergence, or `docs/adr/` change was added.

## Round-2 verification note

Round 6 added a third axis to the guards and, with it, a stopping rule. The first two axes are about the grammar: which productions recurse, and whether each is charged. The third is about the lexer: the preflight tokenises the source itself, so it is a second implementation of SWC's lexer, and a charge gated on a token boundary is disarmed wherever the two disagree. That axis has its own enumerable surface — Unicode identifiers, identifier escapes, numeric separators, Unicode line terminators — and it is now swept in both directions and drawn from by the fuzzer, whose lexical half pairs an atom with the charge-bearing token that follows it. Both guards were re-verified by mutation: deleting the `>= 0x80` rule turns the lexical sweep and the fuzzer red, and restoring it returns them to green.

**Escalation trigger, on the record.** Three axes is where this approach stops earning further patches. If a subsequent verification round finds an abort that is *neither* a missing grammar family *nor* within the widened lexical axis, the response is not another charge: it is to bound the recursion where it actually happens — parse in a subprocess, or on a thread with a guard page and a recovered signal — so the guarantee stops depending on this crate's lexer agreeing with SWC's at all. That is a design change with its own cost, which is why it is a trigger and not the current plan; the structure being defended against is real, though, since a hand-written scanner that must match another parser's tokenisation is a standing source of exactly this class of defect.

The round-5 fixes corrected the *premise* of the method. Round 4's argument enumerated the recursive productions of the accepted surface; the preflight, however, protects SWC, which parses the whole TypeScript grammar, so every production the dialect rejects later — labels, casts, type operators — still recurses in it and still had to be charged. `src/adapter/nesting.rs` now says so explicitly and walks the parsed grammar, and two mechanical cross-checks in `tests/grammar_coverage.rs` stop the argument from drifting again: an exhaustive wildcard-free match over SWC's own `Expr`, `Stmt` and `TsType` node kinds, which fails to compile if SWC gains a variant that nobody has classified; and a deterministic fuzzer that draws sources from the charged alphabet — biased towards small sub-alphabets, because uniform sequences hide an uncharged family behind its charged neighbours — and parses each inside a child process on the stack contract. The fuzzer was validated by deleting the label charge and confirming it fails, and it found the `for`-header separator defect on its first honest run.

The round-4 fixes changed the method, not only the code. The no-abort property had been verified against hand-written shape corpora three rounds running, and each round a family outside the corpus aborted the process. `src/adapter/nesting.rs` now carries the argument that the accepted grammar's recursive productions fall into exactly five families — prefix, infix, postfix, delimiter, statement form — with the reasoning for why nothing else recurses, and `tests/depth_guard.rs` turns that argument into the standing guard: 48 generated units, each repeated to 100 000 and parsed in its own process on the 2 MiB stack contract, covering every family and mixed combinations of them. The instance tests remain, but they are no longer what carries the guarantee. Writing the sweep found both round-3 P0s immediately.

The round-3 fixes were likewise driven red-first: the durability regression suspends and snapshots the way an RLM session does between turns, so it reproduces R2-4 at the boundary the verifier used rather than through `lashlang::execute`, which is exactly what the round-2 scoping tests missed.

The round-2 fixes were driven red-first against the verifier's own repros: the nine-source nesting sweep on a 2 MiB stack, the three transitive-capture programs, the two mutual-recursion and nested-declaration programs, and both generated-namespace collisions were each reproduced before any fix and are now permanent regressions. Two of the verifier's findings led further than reported: the round-1 nesting preflight also accumulated statement keywords across sibling statements, so 28 flat `if` statements falsely rejected (closed as P0-1b), and the mutual-recursion fix additionally required nested declarations to see enclosing-function bindings independent of source order.
