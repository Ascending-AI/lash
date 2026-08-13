# FIG-1304 implementation report

Branch: `samuel-fig-1304`

Baseline: `55a1001c4`

Scope: TypeScript parse, normalization, lowering, dialect enforcement, RLM registration, and conformance. The agent-facing standard library and prompt/parity work remain in FIG-1305 and FIG-1306.

## Delivered commits

| Commit | Delivery |
| --- | --- |
| `4a1c852e6` | Added the Lash-owned shared AST/heap-VM ground, JavaScript value operations, `undefined`, dialect-aware compilation, and the SWC-backed `lash-typescript` frontend. |
| `08144e71b` | Registered the `typescript` RLM dialect and wired end-to-end dispatch, session pinning, state projection, and JSON-boundary behavior. |
| `393c5bc11` | Added the TypeScript behavior, rejection, reference-semantics, durability, signature-rendering, and licensed test262-derived conformance suites. |
| `ee94723cf` | Bounded dialect executor test futures so failures cannot hang the suite. |
| `80156753c` | Scoped shared durable heap identity to TypeScript while retaining Lashlang's exclusive-ownership persistence contract. |
| `f7b159516` | Refreshed the two mechanical RLM snapshot-size goldens for the required 21-byte heap semantics marker. |

The implementation changes 77 files relative to the baseline: 4,907 insertions and 333 deletions. `crates/lash-typescript` is a workspace member but is intentionally absent from `default-members`.

## Delivered design

### Frontend and dependency boundary

- The public crate surface is deliberately small: `parse`, `validate`, `compile`, `link`, `compile_linked`, stable diagnostics, source spans, and `render_tool_signature`.
- SWC is confined to `crates/lash-typescript/src/adapter/`; no `swc_*` type crosses the adapter or appears in a public signature.
- Workspace dependencies are exact pins: `swc_common = 25.0.0`, `swc_ecma_ast = 28.0.0`, and `swc_ecma_parser = 44.0.0` with only its TypeScript parser feature. No transform or codegen dependency is used.
- The adapter produces a Lash-owned normalized tree. Lowering then targets `lashlang::Program`, so TypeScript and Lashlang share linking, bytecode, metering, profiling, heap, continuation, and execution machinery.

### Dialect semantics

- Compilation carries an explicit `CompilationDialect`. Lashlang retains value-isolating `DeepCopy` emission; TypeScript omits it and uses heap-backed reference operations.
- The shared AST/VM now represents `undefined`, JavaScript unary and binary operations, operand-returning logical operators, heap list/record construction, reference-preserving assignment, templates, and return completion through `finally`.
- JavaScript coercion, truthiness, strict/loose equality, numeric edge behavior, string concatenation, `typeof`, and supported String methods are implemented in one Lash-owned VM module rather than in the SWC adapter.
- Type annotations, aliases, and interfaces are parsed and erased or used for type/signature work. Runtime behavior is not altered by annotations.
- `let`/`const` scope checks include duplicate bindings, statically decidable TDZ reads, and assignment-to-const diagnostics. Mutable lexical captures are rejected until durable lexical cells exist.
- Every new opcode participates in the VM heap-plan table, formatting/profiling, and instruction metering. Every new value path participates in canonical encoding, JSON conversion, projection, GC, and durability validation.

### Durable reference identity without Lashlang drift

TypeScript aliasing requires a shared acyclic heap graph, while existing Lashlang snapshots and continuations require an exclusive-ownership forest. The implementation preserves both contracts:

- A TypeScript-compiled program opts its state/VM into reference semantics.
- Writers first validate the ordinary Lashlang forest. If that succeeds, the durable marker stays `false`, preserving byte-identical continuation shapes for cross-dialect programs without aliases.
- If forest validation fails only because a TypeScript program has shared ownership, writers validate the acyclic/reachable graph and persist the required `reference_semantics: true` marker.
- Readers require the marker and select the corresponding validator; there is no decoder default. Dangling references, cycles, unreachable objects, and invalid accounting fail closed in both modes.
- Ordinary Lashlang execution still rejects shared durable ownership exactly as before. Its semantic test expectations were not changed.

### RLM integration

- `plugin/factory.rs` registers `lashlang` and `typescript`; `lashlang` remains the default.
- TypeScript uses `language_id() == snapshot_engine_id() == "typescript"` and `<typescript>...</typescript>` cells.
- Existing active-session resolution continues to pin a session to one dialect. Registered-but-inactive requests are rejected before execution.
- A real `ExecRequest.language == "typescript"` path parses, links, executes, snapshots, restores, and projects through the normal RLM executor.
- JSON projection follows JavaScript container rules for `undefined`: omit object properties and substitute `null` in arrays.

## Dialect contract and conformance

The accepted v1 surface is documented in `crates/lash-typescript/README.md`: lexical declarations, functions/arrows with immutable captures, blocks and bounded control flow, exceptions/finally/return, arrays and records, access and assignment, calls, primitive operators, conditionals, templates, and explicitly mapped String methods.

Unsupported grammar and runtime capabilities are rejected before execution with stable `TS_*` diagnostics. The executable inventory contains 50 one-construct-per-test cases, including classes, generators, async/await, `var`, destructuring, all `for` forms, imports/exports, JSX, enums, namespaces, decorators, dynamic import, `eval`/`Function`, prototype access, accessors/methods, regular expressions, BigInt, spread, optional chaining, `this`, `super`, `with`, labels, and unsupported operators.

Conformance evidence includes:

- 14 integration tests for coercion/equality/templates, operand-returning logical operators, string methods, TDZ/const rules, `return`/`finally`, `undefined` JSON behavior, aliases, captured-object mutation, argument aliasing, cross-dialect VM equivalence, identical representable continuation bytes, and cross-process/GC-stress durability.
- 50 stable named rejection tests.
- Seven executable fixtures adapted from test262 commit `3655e7464de3d52643ecddd4b5f9f4f3e7f62398`, with upstream BSD license, source paths, adaptation rule, and selection policy retained under `crates/lash-typescript/tests/test262/`.
- Tool JSON schemas rendered as TypeScript signatures through the shared Lash type engine, covered both in crate tests and published API snippets.
- End-to-end RLM registration/dispatch tests, including session engine pinning and a TypeScript request through the real executor.
- The complete pre-existing Lashlang library battery: 461 passed with semantic expectations unchanged. Existing golden edits are limited to required format-version bytes/hashes and the two measured full-snapshot sizes; incremental commit-size bounds did not move.

## Deviation register draft

These are runtime-system constraints, not alternate semantics for an accepted operation:

1. Existing instruction, wall-clock, logical-memory, and call-frame limits may terminate execution with typed VM bound errors.
2. Cyclic heap objects reject at durable capture. Shared acyclic identity is preserved; cycle-capable durable encoding is deferred.
3. Mutable lexical captures reject as `TS_MUTABLE_CAPTURE_UNSUPPORTED`; immutable captures and mutation through captured object references work.
4. The host boundary is JSON-shaped: object `undefined` properties disappear, array `undefined` elements become `null`, and incoming JSON cannot construct `undefined`.

No other semantic deviation is intentionally accepted. Unsupported cases belong in the named rejection inventory rather than silently lowering with different behavior. This draft is intentionally outside `docs/adr/`; FIG-1307 owns the ADR.

## Format-version changes

| Contract | Before | After | Reason and failure behavior |
| --- | ---: | ---: | --- |
| Lashlang bytecode | 6 | 7 | New JavaScript/reference instructions and `undefined`; incompatible bytecode is rejected. |
| Lashlang VM ABI | `lashlang-vm-abi-v3` | `lashlang-vm-abi-v4` | VM value/instruction and reference semantics changed; artifact/cache compatibility is explicit. |
| VM continuation | 4 | 5 | Adds required `reference_semantics`; missing/old markers fail closed. |
| Lashlang snapshot | 3 | 4 | Adds `undefined`/heap reference-semantics encoding; old versions fail closed. |
| Semantic hash grammar | `lashlang-semantic-v2` | unchanged | New AST variants have explicit encodings; existing Lashlang inputs retain their prior semantics. |
| RLM execution root | 10 | unchanged | Only embedded Lashlang snapshot leaf bytes/hashes change mechanically. |

## Gate results

All commands were run from `/workspace/code/lash-fig-1304` with the worktree-specific Cargo target directory configured by the environment.

| Gate | Result | Reproducible evidence |
| --- | --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS | Entire workspace and all targets checked. |
| `cargo test --workspace` | PASS | Full unit, integration, property, UI/trybuild, simulation, conformance, and doctest suite completed with zero failures. |
| Lashlang compatibility battery | PASS | `cargo test -p lashlang --lib --locked`: 461 passed, 0 failed. The same 461 passed in the full workspace run. |
| TypeScript focused suite | PASS | 1 unit, 14 dialect, 50 rejection, and 1 curated-slice runner test passed; the runner executes all seven fixtures. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS | Zero warnings. |
| `cargo fmt --all --check` | PASS | No formatting drift. |
| `python3 scripts/check_included_file_formatting.py` | PASS | 37 include-assembled Rust files checked. |
| `python3 scripts/lint_docs.py` | PASS | 46 HTML pages and 42 registry pages checked. |
| `bash scripts/check-rustdoc.sh` | PASS | 599 `lash-core` public members documented, 0 missing; workspace docs built. |
| `python3 scripts/check_test_quarantines.py` | PASS | Quarantine metadata valid. |
| `python3 scripts/check_api_example_coverage.py` | PASS | 8,065 API coverage entries satisfied, including `lash-typescript`. |
| `just perf-guard` | PASS | 297 Lashlang perf results and 1 profile result; runtime and stack budgets passed. Reports are under `.benchmarks/perf-guard/`. |
| `bash scripts/check-production-file-size.sh` | PASS | All production and test/support files are within repository budgets. |
| Schema congruence | N/A | No SQL file or table changed. The workspace's schema-congruence tests nevertheless passed. |
| `git diff --check 55a1001c4..HEAD` | PASS | No whitespace errors before this report commit. |

The first post-format full test run identified only the two 21-byte snapshot-size golden deltas introduced by the required marker. Commit `f7b159516` updated those mechanical expectations, the two focused measurements passed, and the full workspace test command was rerun to completion successfully.

## Deferred work

- FIG-1305: agent-facing TypeScript standard library and complete session surface.
- FIG-1306: production prompt, parity battery, and broader conformance expansion.
- FIG-1307: absorb the deviation register and final architecture decisions into ADRs.
- A bounded oxc re-evaluation; SWC remains deliberately isolated so this does not affect public contracts.
- Cycle-capable durable object graphs and durable mutable lexical cells.
- Expansion of the curated test262 slice as more constructs can be accepted exactly without weakening the dialect.

No compatibility shim, fallback parser, dual execution path, migration adapter, or `docs/adr/` change was added.
