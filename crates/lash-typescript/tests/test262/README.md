# Test262 conformance data

This directory vendors every [tc39/test262](https://github.com/tc39/test262)
test the census accepts, at commit `3655e7464de3d52643ecddd4b5f9f4f3e7f62398`.
Every file under `test/` and `harness/` is a byte-for-byte copy of the upstream
file; `LICENSE` is the upstream BSD license. Normal tests and CI never access
the network (ADR 0062).

## Figures

Each selected test has exactly one recorded outcome in `outcomes.tsv`. The
figures — the selection size, the per-class counts, the pass rate and the
per-code/owner tallies — are derived from that record, not pinned, so they
cannot conflict in the merge queue. To print them, run:

```sh
python3 scripts/check_test262_ratchet.py --base origin/main
```

The `test262` and `test262_full` test binaries print the same tallies in their
output.

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
   inherits its feature's ruling, since those tests would otherwise be
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

`sample.tsv` is the PR lane's stratified sample of the selection. Each
second-level directory contributes `round(count × 500 / selected)` tests, at
least one, chosen in SHA-256 order of the path. The sample is stable across
runs and changes only when the selection does.

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
  A fourth qualifier, `instruction-cost`, is not an unshimmable capability:
  it names a selected test whose full run exceeds the CI lane's cost bound,
  registered in `harness-cost.tsv` (see below).

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

  A recorded failure matches any divergence; its evidence is not pinned. The
  record's tallies are derived — both binaries print them rather than pinning
  a second copy.
- **Across commits.** `scripts/check_test262_ratchet.py --base <base>` (CI)
  holds the record to its base: a test that passed keeps passing, and a
  failure is new only where the base could not run the test (refused or
  harness).

To re-record after a deliberate change, run:

```sh
TEST262_BLESS=1 TEST262_EVIDENCE=/tmp/test262-evidence.tsv \
  kiln run //crates/lash-typescript:test262_full__test
```

This rewrites `outcomes.tsv` from a full run and prints its tallies, and
writes each divergence's and refusal's evidence to the evidence file:

- a divergence keeps its recorded owner;
- a new one is recorded as `UNTRIAGED`, which the record checks refuse until a
  ticket owns it.

## The cost register and the wall-clock backstop

`harness-cost.tsv` registers the few selected tests whose full run exceeds
the CI lane's cost bound. The runner records each as
`harness instruction-cost` **without executing it**. The register's rules:

- a test enters only with a ticket that owns making it affordable;
- a test enters only when it exceeds the CI cost bound — never to hide a
  wrong answer;
- the register only shrinks: a registered path that is no longer selected, or
  whose recorded outcome is no longer `harness instruction-cost`, fails the
  record checks, so a test leaves the register by running inside the bound
  again as the VM gets faster (FIG-3730), not by being edited out.

Separately, a wall-clock backstop bounds any single test at 300 s. Worker
threads cannot be killed once a test starts, so the orchestrating thread
records each test's start time and, once the backstop passes, reports the
unfinished tests by name and exits non-zero: the CI job fails fast with names
instead of hanging.

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
