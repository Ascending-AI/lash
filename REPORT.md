# FIG-1304 review fix report

Branch: `samuel-fig-1304`

Implementation baseline: `e3b073423` (reviewed FIG-1303 head)

Round-1 fix baseline: `bf704079b` — a pre-rebase SHA, retained for provenance only. The branch was rebased onto `93874e275`; the reachable branch history is `218a580d7..HEAD`, and every commit cited in the ledgers below is an ancestor of the head.

Round-2 fix baseline: `13e31367e` (first decisive fresh-eyes verification, verdict BLOCK)

Round-3 fix baseline: `4dc2bd5cc` (round-2 closure verification, verdict BLOCK)

Outcome: all 22 merged findings from the Opus and sol-sub adversarial reviews are closed, all ten findings from the first verification round are closed, and all five findings from the round-2 closure verification are closed. Accepted TypeScript operations now follow the checked Node v25.2.1 oracle, unsupported operations reject with stable `TS_*` diagnostics, the durable dialect marker fails closed without becoming VM identity, and the full repository gate battery passes.

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
2. TypeScript source nesting is limited to 28 and rejects as `TS_SOURCE_NESTING_LIMIT`.
3. Cyclic heap objects reject at durable capture. Shared acyclic identity is preserved; cycle-capable durable graph encoding is deferred.
4. Mutable lexical captures reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED` on both reads and writes until durable lexical cells exist. Immutable captures and mutation through captured object references work.
5. The host boundary is JSON-shaped: object properties holding `undefined` are omitted, array elements become `null`, and incoming JSON cannot manufacture `undefined`.
6. Lone UTF-16 surrogates are not representable by the v1 UTF-8 value model. Literals reject statically as `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`; two further shapes produce one only at runtime and reject there as `TS_LONE_SURROGATE_UNSUPPORTED` — splitting a string containing an astral character into units, and indexing into one. Both are catchable TypeScript exceptions.
7. Negative and other non-index array writes reject as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`; non-negative out-of-range writes retain ECMAScript hole-extension behavior.
8. Identifiers starting with `__typescript_` are reserved for the lowerer's generated bindings and reject as `TS_RESERVED_IDENTIFIER`.
10. Mutually recursive function declarations reject as `TS_MUTUAL_RECURSION_UNSUPPORTED` with the cycle named. This is item 3's deferral seen from the front end: the only v1 lowering for the shape builds a durable-rooted heap cycle, so the shape fails closed at compile time instead of at the durability boundary.
9. `console.log` is host-defined rather than ECMA-262 and prints ECMA `ToString` of each argument, so `console.log({a: 1})` prints `[object Object]` where Node's inspector prints `{ a: 1 }`.

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
| `cargo test -p lash-typescript --locked` | PASS | 115 tests across unit, depth, dialect, differential, ECMA, rejection, scoping, structural, and curated test262 suites (94 before round 2, 109 before round 3). |
| Committed Node differential table | PASS | All 310 rows match the checked Node v25.2.1 expectations or named rejection; regeneration under Node v25.2.1 reproduces the committed file byte for byte. |
| `node crates/lash-typescript/tests/differential/generate.mjs` | PASS | Deliberate regeneration, Node version stamped and enforced by the generator. |
| Base-vs-head Lashlang byte identity | PASS | Seven of seven raw artifact and normalized continuation records match; combined SHA-256 shown above. |
| `cargo test -p lashlang --locked` | PASS | 461 unit tests passed, unchanged; all package integration/property/stack tests also passed. |
| Verifier nesting sweep (2 MiB stack, 26 shapes) | PASS | Every delimiter, operator, statement, template and newline-separated shape returns `TS_SOURCE_NESTING_LIMIT`; zero aborts. |
| Shared-AST leak sweep (8 shapes x 80 counts) | PASS | No accepted-grammar source reaches `TS_INVALID_SHARED_AST`. |
| Durability corpus (suspend + snapshot) | PASS | Every accepted binding shape suspends, encodes its continuation, and snapshots; no generated name reaches the global surface. |

The first verification pass exposed a stale docs-snippet assertion that still expected `Promise` signatures and two strict-lint style findings. Commits `074b24b3e`, `db4201eb7`, and `ea8ca2aeb` corrected them; every affected gate and the full battery were rerun successfully on the corrected tree. In round 2 the workspace suite exited 0 on the first run; the `lash-sqlite-store` `sqlite_real_turn_crash_matrix` seeded flake recorded by the verifier (P3-9) did not reproduce, and it remains a pre-existing concurrency-load flake unrelated to this layer — no TypeScript or Lashlang code path is involved.

Round 3 reran the whole battery on the final tree; the workspace suite again exited 0 on the first run and the `sqlite_real_turn_crash_matrix` seeded flake did not recur.

The `ToNumber(String)` whitespace correction lives in `crates/lashlang/src/runtime/javascript.rs`, which is reached only through the `JavaScriptUnary`/`JavaScriptBinary` opcodes the TypeScript lowering emits. No Lashlang program semantics, and no semantic hash, change with it.

## Explicitly deferred

- FIG-1305 owns TypeScript tool execution and `await`; this layer deliberately disables both and also disables deferred tool resolution.
- Continuation `active_execution_elapsed` nondeterminism predates this layer and remains deferred; the byte-identity method normalizes that field explicitly.
- Cycle-capable durable graphs and durable mutable lexical cells remain deferred and fail closed as documented above.

No compatibility shim, fallback parser, dual execution path, migration adapter, silent semantic divergence, or `docs/adr/` change was added.

## Round-2 verification note

The round-3 fixes were likewise driven red-first: the durability regression suspends and snapshots the way an RLM session does between turns, so it reproduces R2-4 at the boundary the verifier used rather than through `lashlang::execute`, which is exactly what the round-2 scoping tests missed.

The round-2 fixes were driven red-first against the verifier's own repros: the nine-source nesting sweep on a 2 MiB stack, the three transitive-capture programs, the two mutual-recursion and nested-declaration programs, and both generated-namespace collisions were each reproduced before any fix and are now permanent regressions. Two of the verifier's findings led further than reported: the round-1 nesting preflight also accumulated statement keywords across sibling statements, so 28 flat `if` statements falsely rejected (closed as P0-1b), and the mutual-recursion fix additionally required nested declarations to see enclosing-function bindings independent of source order.
