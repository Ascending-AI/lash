# Test262 conformance data

This directory vendors every [tc39/test262](https://github.com/tc39/test262)
test the census accepts, at commit `3655e7464de3d52643ecddd4b5f9f4f3e7f62398`.
Every file under `test/` and `harness/` is a byte-for-byte copy of the upstream
file; `LICENSE` is the upstream BSD license. Normal tests and CI never access
the network (ADR 0062).

## Figures

At the pinned commit, 18,970 of the 53,578 upstream tests are selected. Each has
exactly one recorded outcome:

| outcome | tests |
|---|---:|
| `pass` | 4,062 |
| `refused <TS_* code>` | 14,124 |
| `fail <ticket>` | 763 |
| `harness <capability>` | 21 |

The pass rate is **21.4% of the selection** and **84.2% of the executable
tests** (the 4,825 that run: pass plus fail). `expected-counts.tsv` pins the
count for each class, each refusal code and each owning ticket. The largest
entries are:

- **Refusals:** `TS_PROTOTYPE_MUTATION_UNSUPPORTED` 3,672, `TS_METHOD_UNSUPPORTED` 2,877,
  `TS_ASSIGN_CONST` 1,864, `TS_CLASS_UNSUPPORTED` 1,174, `TS_NEW_UNSUPPORTED` 1,120,
  `TS_UNKNOWN_BINDING` 721 and `TS_FOR_UNSUPPORTED` 651.
- **Failures:** FIG-3653 (runtime faults instead of `TypeError`) 153, FIG-3650 (early
  errors accepted) 141, FIG-3651 (valid programs rejected with early-error
  diagnostics) 139, FIG-3652 (ToPrimitive ignores guest `valueOf`/`toString`)
  105 and FIG-3656 (built-in values answer `typeof`/`in` wrongly) 77.

## Selection

The selection is derived, never hand-picked. `sync.mjs` selects an upstream
test exactly when every census row it touches is `accepted`. The rows it
checks, in order (the first one that is not accepted names the exclusion in
`skip-register.tsv`):

1. **The test's top-level directory.** `annexB`, `intl402`, `staging` and
   `harness` are skipped.
2. **For `test/built-ins/<X>/…`, the feature row named `<X>`, when the census
   has one.** Upstream feature tags are incomplete: untagged tests under
   `built-ins/Promise`, `DataView`, `WeakMap`, `WeakSet`, `ArrayBuffer` and
   `Proxy` exercise those features all the same. So a built-in directory
   inherits its feature's ruling, as 778 such tests would otherwise be
   selected against a skipped feature.
3. **Each of its flags.** A `flag` census row rules on every flag
   INTERPRETING.md defines:
   - `noStrict` and `raw` are skipped because the dialect is strict-only. Every
     cell is one strict script (ADR 0062), and Test262 forbids running either
     kind in strict mode.
   - `module` is skipped because a cell is a Script, never module code.
   - `CanBlockIsTrue` is skipped because the host never blocks.
   - `non-deterministic` is skipped because such an outcome cannot be pinned.
   - The rest are accepted. Because the dialect has one mode, a test without
     a strictness flag runs once, as its strict variant.
4. **Each of its feature tags.** An untagged test is eligible, which covers
   most of ES5.

`sample.tsv` is the PR lane's stratified sample of 515 tests. Each second-level
directory contributes `round(count × 500 / selected)` tests, at least one,
chosen in SHA-256 order of the path. The sample is stable across runs and
changes only when the selection does.

## Outcomes and the ratchet

`outcomes.tsv` records one outcome per selected test. There is no bare `fail`
and no wildcard.

- **`pass`:** the test runs and meets the specification. For a negative test of
  phase `parse`, this means the front end reports an early error
  (`TS_SYNTAX_ERROR`, `TS_REGEX_INVALID`, `TS_DUPLICATE_BINDING`, …).
- **`refused <code>`:** the dialect refuses the test with a real diagnostic.
  The refusal is either static, or a shape-dependent refusal at run time that
  the crate README's deviation register names. Every code in the record must be
  the diagnostic of a `rejected` census row, and that row carries a probe that
  fires it. ES5 constructs carry no feature tag, so their rulings are
  `typescript`-kind rows (for example `accessors`, `binding-reassignment`,
  `this`, `closed-shape-field-guard`).
- **`fail <owner>`:** the test runs and diverges from the specification. The
  owner is the ticket (`FIG-…`) or `registered-deviation:<name>` that owns the
  divergence. Two cases count as divergences rather than refusals:
  - an early-error diagnostic on a valid, non-negative test, because the front
    end rejects a program ECMAScript accepts;
  - an identifier the linker resolves as a module.
- **`harness <capability>`:** the runner cannot give the test what it needs
  in-dialect. `harness-shim/unshimmable.tsv` names the capability for each
  case: an include that cannot be rendered, `program-size` (the test fits the
  64 KiB cell alone but not with the harness prepended) or `host-effects`.

The runner admits each test the way a cell is admitted: lowered, linked
against a host environment, compiled from the linked artifact, then run under
a deterministic instruction budget. The ratchet has three layers:

- **Each outcome.** `test262` (the sample, in `//:dev_tests`) and
  `test262_full` (the whole selection, in the `//:workspace_tests` tail and the
  nightly `Test262 nightly` workflow) fail when a test's outcome changes from
  its record in any of three ways:
  - a new failure;
  - a pass that is not promoted;
  - a refusal whose code changed.

  A recorded failure matches any divergence; its evidence is not pinned.
- **The counts.** `expected-counts.tsv` must equal the tallies of
  `outcomes.tsv`, so every change shows in the reviewed totals as well.
- **Across commits.** `scripts/check_test262_ratchet.py --base <base>` (CI)
  holds the record to its base: a test that passed keeps passing, and a
  failure is new only where the base could not run the test (refused or
  harness).

To re-record after a deliberate change, run:

```sh
TEST262_BLESS=1 TEST262_EVIDENCE=/tmp/test262-evidence.tsv \
  kiln run //crates/lash-typescript:test262_full__test
```

This rewrites both files from a full run, and writes each divergence's and
refusal's evidence to the evidence file:

- a divergence keeps its recorded owner;
- a new one is recorded as `UNTRIAGED`, which the record checks refuse until a
  ticket owns it.

## Harness

The runner prepends each test's harness to it in one cell:

- `sta.js` and `assert.js`;
- `doneprintHandle.js` for an `async` test;
- then each include.

The upstream files under `harness/` do not run in the dialect: they use
constructors, prototypes and function properties. So `harness-shim/` holds
renderings that keep upstream's pass/fail semantics and message text:

- `Test262Error` is a factory for an error record named `Test262Error`, and
  `__test262ErrorThrower` is `Test262Error.thrower`.
- `assert.throws` receives the expected class by name and compares it with the
  caught error's `name`. The dialect has no constructor values. A VM fault
  (`RuntimeError`) propagates as itself, so the record shows the fault or the
  refusal it carries rather than a mismatched class.
- Failure messages name an object by its kind. A plain object has no string
  conversion in the dialect, and a failing assertion must report its failure,
  never a refusal raised while it builds its message.
- `propertyHelper.js` checks each attribute a test names through the behaviour
  that defines it: a write takes effect, `for...in` visits the property, and
  `delete` removes it. The dialect has no descriptor reflection, so the
  comparison with a reported descriptor is the part it cannot perform.
- `doneprintHandle.js` settles through the dialect's `print`, which the runner
  observes.
- `asyncHelpers.js` awaits the test function where upstream chains `.then`.

Other renderings note their own differences in spelling.
`every_harness_rendering_compiles_and_runs` and
`assertion_harness_keeps_upstream_semantics` in `test262_sample.rs` run every
rendering on its own and pin the assertion semantics.

The dialect reserves dotted method-call syntax for its method allowlist. At
ingestion, the runner rewrites a small set of spellings in code only; strings,
template text, comments and regular-expression literals are left alone:

- `assert.x(...)` becomes `assert["x"](...)`;
- `assert(...)` becomes `__test262Assert(...)`;
- `new Test262Error(...)` becomes a factory call;
- `assert.throws(C, ...)` passes the name `"C"`.

The vendored bytes are unchanged.

## Deliberate sync

Clone Test262 separately, check out the pinned commit, and refresh the
inventory first:

```sh
node crates/lash-typescript/tests/test262/sync.mjs inventory /path/to/test262
```

Then, in order:

1. Review `inventory.tsv` and classify every changed row in `census.tsv`.
2. Regenerate the selection:

   ```sh
   node crates/lash-typescript/tests/test262/sync.mjs sync /path/to/test262
   node crates/lash-typescript/tests/test262/sync.mjs check /path/to/test262
   ```

3. Re-record the outcomes as above.

The script refuses a checkout whose commit differs from the pin. `check` is
non-mutating. It regenerates every derived file in memory and fails on any
difference, byte for byte: the inventory, skip register, sample and count, the
vendored tests, the harness files and the license. The Rust checks also
restate the selection rule over the vendored files themselves, so no
selected test can touch a census row that is not accepted.
