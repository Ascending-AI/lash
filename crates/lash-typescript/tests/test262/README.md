# Test262 conformance data

This directory vendors a deliberately small executable subset of
[tc39/test262](https://github.com/tc39/test262) at commit
`3655e7464de3d52643ecddd4b5f9f4f3e7f62398`. The selected test files and the
four upstream harness files are byte-for-byte copies; `LICENSE` is the upstream
BSD license. Normal tests and CI never access the network.

`inventory.tsv` is generated before selection. It lists every official feature
tag from the pinned `features.txt`, every top-level test directory, and the
TypeScript-only syntax decisions required by the dialect contract. Every row
must have exactly one explicit ruling in `census.tsv`: `accepted`, `rejected`
with a real `TS_*` diagnostic, or `skip` with a ticket/deviation reason. There
is no fallback or wildcard row. The Rust harness fails if the two sets differ.

`manifest.tsv` selects 42 executable probes and assigns each an area plus a
`pass` or ratcheted `skip` disposition. `skip-register.tsv` names every other
upstream test path and its reason, so the 53,578-test source tree has no silent
omissions. `expected-counts.tsv` pins 34 passes and 8 executable skips by area.
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
