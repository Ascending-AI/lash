# Test262 conformance data

This directory vendors a deliberately small executable subset of
[tc39/test262](https://github.com/tc39/test262) at commit
`3655e7464de3d52643ecddd4b5f9f4f3e7f62398`. The selected test files and the
four upstream harness files are byte-for-byte copies; `LICENSE` is the upstream
BSD license. Normal tests and CI never access the network.

`inventory.tsv` is generated before selection. It lists every official feature
tag from the pinned `features.txt`, every top-level test directory, and the
`typescript`-kind rows: dialect decisions that have no upstream feature tag to
hang on, most of them TypeScript-only syntax and one — `async-array-callbacks` —
a registered semantic deviation. Every row
must have exactly one explicit ruling in `census.tsv`: `accepted`, `rejected`
with a real `TS_*` diagnostic, or `skip` with a ticket/deviation reason. There
is no fallback or wildcard row. The Rust harness fails if the two sets differ.

A deviation reason is spelled `registered-deviation:<NAME>`, naming an entry in
the deviation register in the crate README. It is the only way a row can record
"the dialect accepts this construct and diverges in a named, documented way":
`accepted` must carry reason `-`, and `rejected` must name a real `TS_*`
diagnostic that some path actually produces. Such a row **indexes** a deviation
so no ruling is unrecorded; it is not executable evidence of the behaviour it
names, and the tests that pin that behaviour live elsewhere in the crate.

A `rejected` row also carries a fifth column, the **probe**: a source that must
reject with exactly the diagnostic the row names. Naming a diagnostic is a claim
about the code, and before the probes existed nothing connected the two — rows
named codes that no path produced, and rows named rejections for constructs that
compiled and ran. A feature with no derivable one-line probe writes
`probe-exempt:` and the reason, which a reader can check; a bare omission is not
available. Non-rejected rows carry `-`.

`manifest.tsv` selects 118 executable probes and assigns each an area plus a
`pass` or ratcheted `skip` disposition. `skip-register.tsv` names every other
upstream test path and its reason, so the 53,578-test source tree has no silent
omissions. `expected-counts.tsv` pins 59 passes and 59 executable skips by area.

The `block-scope`, `per-iteration-bindings`, `let-const`, `tdz` and
`global-code` areas (FIG-3599) probe the scoping classes the multi-cell
defects touched. Every positive case that stays in the dialect passes; each
case the dialect refuses is a ratcheted skip naming its refusal. A read in the
temporal dead zone is refused statically (`TS_TEMPORAL_DEAD_ZONE`), so every
`tdz` case is such a skip. The `global-code` cases compare global lexical and
global object bindings across Scripts through `$262.evalScript`, top-level
`this` and classes, none of which one script in the dialect can express; they
are skips naming the refusal, and the Node session oracle
(`tests/differential/sessions/`) carries the cross-Script rules instead.
A skipped probe is still compiled: if its named rejection changes or it starts
compiling, the suite fails and requires an explicit promotion/count update.
For selected tests that Test262 would also synthesize in strict mode, a
`path#strict` row records `strict-mode-variant:n.a.`: the dialect has one script
mode and does not silently claim the second variant.

## Harness shims

The upstream `assert.js`, `sta.js`, `compareArray.js`, and `propertyHelper.js`
are retained under `harness/` for provenance. They use `var`, constructors,
prototype mutation, descriptors, and other out-of-dialect constructs. The
runner therefore prepends the small implementations under `harness-shim/`:

- `sta.js` supplies message-valued `Test262Error` and `$DONOTEVALUATE`.
- `assert.js` supplies SameValue, not-SameValue, and array assertions.
- `compareArray.js` compares dense arrays through the accepted loop surface.
- `assert.throws` compares the caught error's `name` with the expected class,
  which the runner passes by name (`assert.throws(ReferenceError, f)` becomes
  `assert["throws"]("ReferenceError", f)`): the dialect has no constructor
  values.
- `propertyHelper.js` is an explicit failing stub because descriptors are not
  accepted; no passing selected test may use it.

The dialect reserves dotted method-call syntax for its method allowlist. At
ingestion the runner bridges `assert.sameValue`, `assert.notSameValue`, and
`assert.compareArray` to equivalent computed-property function calls, supplies
an omitted diagnostic-message argument, and normalizes direct
`new Test262Error(...)` construction to the callable shim. Vendored source
remains unchanged and the assertion semantics run inside the real VM.

## Deliberate sync

Clone Test262 separately, check out the pinned commit, and run inventory first:

```sh
node crates/lash-typescript/tests/test262/sync.mjs inventory /path/to/test262
```

Review `inventory.tsv`, classify every changed row in `census.tsv`, update the
manifest/count pin, and only then regenerate the vendored data:

```sh
node crates/lash-typescript/tests/test262/sync.mjs sync /path/to/test262
node crates/lash-typescript/tests/test262/sync.mjs check /path/to/test262
```

The script refuses a checkout whose commit differs from the pin. `check` is
non-mutating and verifies inventory, skip register, count, selected test bytes,
and upstream harness bytes.
