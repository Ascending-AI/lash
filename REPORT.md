# FIG-1304 dual-review fix report

Branch: `samuel-fig-1304`

Implementation baseline: `e3b073423` (reviewed FIG-1303 head)

Fix-round baseline: `bf704079b`

Outcome: all 22 merged findings from the Opus and sol-sub adversarial reviews are closed. Accepted TypeScript operations now follow the checked Node v25.2.1 oracle, unsupported operations reject with stable `TS_*` diagnostics, the durable dialect marker fails closed without becoming VM identity, and the full repository gate battery passes.

This layer is still a **pure calculator**. A TypeScript cell has no tool calls, no `await`, and no deferred tool resolution. Rendered tool signatures are synchronous descriptions for FIG-1305; they do not make tools executable here.

## Finding ledger

Each row identifies the committed failing regression and the commit that made it green. Shared commits contain separate focused tests for every item in their row.

| # | Finding and final behavior | Red commit | Fix commit |
| ---: | --- | --- | --- |
| 1 | Resume derives reference semantics from the compiled dialect; a non-aliased first suspension does not prevent later TypeScript aliasing. | `eb47763e2` | `42ec07fd7` |
| 2 | Lashlang refuses an authored TypeScript marker and any shared graph, regardless of the wire marker; failures are typed. | `eb47763e2` | `42ec07fd7` |
| 3 | `State::reference_semantics` is derived per program and no longer sticks historically. | `eb47763e2` | `42ec07fd7` |
| 4 | Missing, out-of-range, and negative reads produce `undefined`; non-negative writes extend arrays with holes; negative/non-index writes reject without element corruption. | `0749b7c2f` | `6b2dfae1a` |
| 5 | `typeof` distinguishes heap objects, arrays, and functions, while an unresolved operand returns `"undefined"`. | `0749b7c2f` | `6b2dfae1a` |
| 6 | Loose equality follows the recursive boolean-to-number rule, including `null == false` being false. | `0749b7c2f` | `6b2dfae1a` |
| 7 | TypeScript Number-to-String uses shortest round trips and ECMA exponent thresholds/formatting without changing Lashlang formatting. | `0749b7c2f` | `6b2dfae1a` |
| 8 | String-to-Number rejects Rust-only spellings and handles signed prefixes and arbitrarily large hexadecimal input per ECMA. | `0749b7c2f` | `6b2dfae1a` |
| 9 | TypeScript `split` and `join` use dialect-specific ECMA conversion, including empty separators and recursive array conversion. | `0749b7c2f` | `6b2dfae1a` |
| 10 | String/array `.length`, `NaN`, and `Infinity` are supported with UTF-16 string length. | `0749b7c2f` | `6b2dfae1a` |
| 11 | String relational comparison orders UTF-16 code units. | `0749b7c2f` | `6b2dfae1a` |
| 12 | Lone-surrogate literals reject as `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`; no lossy U+FFFD conversion occurs. | `0749b7c2f` | `6b2dfae1a` |
| 13 | A TypeScript `this` parameter is erased and does not affect runtime arity or binding. | `0749b7c2f` | `6b2dfae1a` |
| 14 | Both reads and writes of captured mutable bindings reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED`. | `1eb18993e` | `e42be1caa` |
| 15 | Catch bodies have their own mangled lexical scope and cannot overwrite enclosing slots. | `1eb18993e` | `e42be1caa` |
| 16 | Unresolved identifiers and `arguments` reject statically as `TS_UNKNOWN_BINDING`; only `typeof unresolved` is legal. | `1eb18993e` | `e42be1caa` |
| 17 | Assignment to an undeclared name rejects statically and cannot create an implicit durable global. | `1eb18993e` | `e42be1caa` |
| 18 | Hoisted function declarations see top-level lexical bindings regardless of source order, including the README captured-object example and later-`const` captures. | `1eb18993e`, `6eed7d3fc` | `e42be1caa`, `4f9c240ce` |
| 19 | Lexical bindings named `print`, `finish`, or `console` win over host interception. | `1eb18993e` | `e42be1caa` |
| 20 | Source nesting is guarded before SWC/adapter recursion and reports `TS_SOURCE_NESTING_LIMIT` under the 2 MiB stack contract. | `77e266297` | `01d331c26` |
| 21 | A checked-in Node v25.2.1 differential table permanently covers both review corpora and the fix findings. | `98cb57028` | `614c5ce85` |
| 22 | The README/deviation register, structural diagnostics, `console.log` arity, and signature rendering now describe and enforce the real surface. | `2ee5e66ed` | `6a0b14ec9` |

Auxiliary verification repairs are `26fb6e42b` (synchronous signature docs assertion), `864cf5a73` and `ab5dba80e` (strict-lint style), and `dcde899c6` (remove one redundant Lashlang test so its established package count remains 461). None changes the accepted language contract.

## Review conflict resolution

Item 14 was resolved by executing the competing claim, not by compromise. The exact Opus assign-only capture repro was committed in `1eb18993e` and failed before the fix: a write to an outer `let` silently targeted a function-local slot. Therefore the Opus finding was correct and the sol-sub conclusion that no hole existed was incorrect for that shape. Commit `e42be1caa` applies the same ownership check to reads and writes and returns `TS_MUTABLE_CAPTURE_UNSUPPORTED` in both paths.

## Design decisions

### Array writes

- `a[3] = 9` on `[1]` follows ECMAScript: the array extends and positions 1 and 2 are `undefined` holes.
- Negative and other non-index writes would create named array-object properties, which the v1 heap-list representation cannot encode. They reject at runtime as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`.
- Such a rejection never wraps the index or mutates an existing element.

### `console.log`

Free `console.log` accepts zero or more arguments. Each argument receives ECMA `ToString`, the results are joined with one ASCII space, and one host print effect is emitted. Zero arguments print the empty string. A lexical `console` binding is ordinary user data and takes precedence.

### Source nesting

The effective v1 TypeScript source-nesting budget is **28 levels**. Level 28 compiles and executes on a 2 MiB child-thread stack; level 29 rejects as `TS_SOURCE_NESTING_LIMIT`. The adapter performs an iterative delimiter preflight before SWC and maintains explicit conversion counters. A 10,000-parenthesis child-process regression proves named-diagnostic-not-abort behavior. Block lowering was flattened enough to make 28 the pinned source-level budget rather than exposing the shared AST's internal per-node cost.

### Tool signatures and structural diagnostics

Rendered signatures are synchronous. Unsafe or reserved tool identifiers use collision-proof hexadecimal `__lash_tool_...` names, unsafe properties are quoted, and user names cannot collide with the generated namespace. Default parameters, rest parameters, and ambient declarations reject precisely as `TS_PARAMETER_DEFAULT_UNSUPPORTED`, `TS_PARAMETER_REST_UNSUPPORTED`, and `TS_DECLARE_UNSUPPORTED`.

## Durable dialect outcome

The continuation marker is a persistence requirement and lower bound, not VM identity. The compiled program dialect selects the VM behavior on fresh install and resume. TypeScript may persist a reachable acyclic shared graph; Lashlang continues to require its exclusive-ownership forest. Program/marker mismatch, shared Lashlang state, dangling references, cycles, unreachable objects, and invalid accounting fail closed with typed errors. Flag derivation occurs for each program, so prior TypeScript execution cannot contaminate later Lashlang execution.

## Differential oracle

`crates/lash-typescript/tests/differential/expectations.tsv` contains 304 committed rows plus its header:

| Provenance | Rows |
| --- | ---: |
| Opus review corpus | 163 |
| sol-sub review corpus | 124 |
| Combined fix findings | 17 |

Duplicates are retained to keep provenance counts executable. `generate.mjs` stamps and requires Node `v25.2.1`; regeneration is a deliberate reviewed action, never part of an ordinary test run. Accepted rows compare observable output, static rejections compare their named diagnostic, and the unrepresentable array-property write compares its named runtime rejection.

## Honest deviation register

These are the only intentional deviations for the accepted v1 surface. They are runtime-system or representation constraints, not alternate silent semantics:

1. Existing instruction, wall-clock, logical-memory, and call-frame limits may terminate execution with typed VM bound errors.
2. TypeScript source nesting is limited to 28 and rejects as `TS_SOURCE_NESTING_LIMIT`.
3. Cyclic heap objects reject at durable capture. Shared acyclic identity is preserved; cycle-capable durable graph encoding is deferred.
4. Mutable lexical captures reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED` on both reads and writes until durable lexical cells exist. Immutable captures and mutation through captured object references work.
5. The host boundary is JSON-shaped: object properties holding `undefined` are omitted, array elements become `null`, and incoming JSON cannot manufacture `undefined`.
6. Lone UTF-16 surrogates are not representable by the v1 UTF-8 value model and reject as `TS_LONE_SURROGATE_LITERAL_UNSUPPORTED`.
7. Negative and other non-index array writes reject as `TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED`; non-negative out-of-range writes retain ECMAScript hole-extension behavior.

No reviewed semantic divergence was moved into this register.

## Compatibility and identity evidence

The Opus seven-program corpus was compiled independently from base `e3b073423` and fix-round head `dcde899c6`. For every program, module refs, host-requirements refs, raw `ModuleArtifact::to_store_bytes()` output, and normalized first-effect continuation bytes matched. The concatenated identity records on both sides have SHA-256:

`b76f7578d37928ac4c8b044f62b7bbedd40e0079c114627b428063efa8dc603d`

The continuation comparison removes only `active_execution_elapsed` (pre-existing nondeterminism), `format_version`, and the new `reference_semantics` persistence marker, exactly as the reviewer probe does. `CompiledProgram` debug text intentionally gained `dialect: Lashlang`; this is diagnostic metadata and is not part of the artifact or continuation byte comparison. The dedicated package run remains exactly **461 Lashlang unit tests**, all passing with the pre-existing semantic expectations.

## Gate results

Commands ran unpiped from `/workspace/code/lash-fig-1304` with `CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1304` where applicable.

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
| `git diff --check bf704079b..HEAD` | PASS | No whitespace errors before the final report commit. |
| `cargo test -p lash-typescript --locked` | PASS | Unit, depth, dialect, differential, ECMA, rejection, scoping, structural, and curated test262 suites passed. |
| Committed Node differential table | PASS | All 304 rows match the checked Node v25.2.1 expectations or named rejection. |
| Base-vs-head Lashlang byte identity | PASS | Seven of seven raw artifact and normalized continuation records match; combined SHA-256 shown above. |
| `cargo test -p lashlang --locked` | PASS | 461 unit tests passed, unchanged; all package integration/property/stack tests also passed. |

The first verification pass exposed a stale docs-snippet assertion that still expected `Promise` signatures and two strict-lint style findings. Commits `26fb6e42b`, `864cf5a73`, and `ab5dba80e` corrected them; every affected gate and the full battery were rerun successfully on the corrected tree.

## Explicitly deferred

- FIG-1305 owns TypeScript tool execution and `await`; this layer deliberately disables both and also disables deferred tool resolution.
- Continuation `active_execution_elapsed` nondeterminism predates this layer and remains deferred; the byte-identity method normalizes that field explicitly.
- Cycle-capable durable graphs and durable mutable lexical cells remain deferred and fail closed as documented above.

No compatibility shim, fallback parser, dual execution path, migration adapter, silent semantic divergence, or `docs/adr/` change was added.
