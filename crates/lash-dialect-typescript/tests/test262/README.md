# Test262 conformance data

This directory vendors every [tc39/test262](https://github.com/tc39/test262)
test the census accepts, at commit `3655e7464de3d52643ecddd4b5f9f4f3e7f62398`.
Every file under `test/` and `harness/` is a byte-for-byte copy of the upstream
file; `LICENSE` is the upstream BSD license. Normal tests and CI never access
the network.

## Figures

Each selected test has exactly one recorded outcome in
`outcomes/<directory>.tsv` — one shard per test directory, cut as deep as
the tree is hot (`built-ins/Array/prototype` sits beside
`language/identifiers`), so two lanes that change different directories
never share a file (FIG-3727). The figures — the selection size, the
per-class counts, the pass rate and the per-code/owner tallies — are derived
from that record, not pinned, so they cannot conflict in the merge queue. To
print them, run:

```sh
python3 scripts/check_test262_ratchet.py --base origin/main
```

## Selection

The selection is derived, never hand-picked. `sync.mjs` selects an upstream
test exactly when every census row it touches is `accepted`. The rows it
checks, in order (the first one that is not accepted names the exclusion in
the `skip-register/<kind>/<name>.tsv` shard of that census row):

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
     cell is one strict script, and Test262 forbids running either
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

The `outcomes/**/*.tsv` shards are main's record: one outcome per selected
test, sorted by path inside each file. There is no bare `fail` and no
wildcard.

- **`pass`:** the test runs and meets the specification. For a negative test of
  phase `parse`, this means the front end reports an early error.
- **`refused <code>`:** the dialect refuses the test with a real diagnostic,
  the code of a `rejected` census row.
- **`fail <owner>`:** the test runs and diverges from the specification; the
  owner is the ticket or registered deviation that owns the divergence.
- **`harness <capability>`:** the runner cannot give the test what it needs
  in-dialect; `harness-shim/unshimmable.tsv` names the capability.

The kernel runner (`//crates/lash-dialect-typescript:test262_kernel__test`,
`tests/test262_kernel.rs`) lowers each recorded test with this crate and runs
it on a kernel machine under the same deterministic bounds for every case; a
bound trip is a failure. The record is partitioned into the laws
`selection::shard_00` to `selection::shard_39`; run exact selectors in small
groups to stay inside each action's deadline. `TEST262_KERNEL_FILTER` takes
comma-separated path prefixes for a focused local rerun. For each case the
runner prints `outcome\t<path>\t<class>\t<qualifier>`.

The ratchet holds the kernel to the record. Collect the last three fields of
every `outcome` line into a TSV and run:

```sh
python3 scripts/check_test262_ratchet.py --base origin/main \
  --kernel-outcomes <observations.tsv>
```

It requires every recorded case exactly once. Every case the record marks
`pass` must pass, or be excluded by an exact row of the deviation register
(`crates/lash-dialect-typescript/deviations.md`); a diagnostic-wide refusal or
a routed feature gap never exempts a regression. Every demotion is printed.
The nightly `Test262 nightly` workflow runs the whole partition and this
check. Without `--kernel-outcomes`, the script holds the record itself to its
base: a test that passed keeps passing, and a failure is new only where the
base could not run the test.

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
  caught error's `name`. The dialect has no constructor values. A kernel error
  propagates as itself, so the record shows the fault or the
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
node crates/lash-dialect-typescript/tests/test262/sync.mjs inventory /path/to/test262
```

Then, in order:

1. Review `inventory/` — one `inventory/<kind>/<name>.tsv` file per row — and
   classify every changed row under `census/`: each ruling lives in the
   `census/<kind>/<name>.tsv` file it names, so two lanes' rulings never
   share a file.
2. Regenerate the selection:

   ```sh
   node crates/lash-dialect-typescript/tests/test262/sync.mjs sync /path/to/test262
   node crates/lash-dialect-typescript/tests/test262/sync.mjs check /path/to/test262
   ```

3. Record the outcome of every newly selected test in `outcomes/`, and run
   the ratchet above.

The script refuses a checkout whose commit differs from the pin. `check` is
non-mutating. It regenerates every derived file in memory and fails on any
difference, byte for byte: the `inventory/` and `skip-register/` shards, the
sample and count files, the vendored tests, the harness files and the
license. The Rust checks also re-check the selection rule over the vendored
files themselves, so no selected test can touch a census row that is not
accepted.
