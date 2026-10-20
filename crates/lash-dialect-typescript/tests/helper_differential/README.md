# Helper differential corpus (FIG-6372)

`cases.json` holds small real-language programs for both dialects. Each calls
`await host.observe(value)` for its answer. `typescript.expected.json` and
`python.expected.json` contain only observations from the pinned Node v25.2.1
and CPython 3.12.3 engines. Re-record both with one command:

```sh
python3 scripts/record-helper-differential.py
```

The recorder refuses another interpreter version and writes only those two
expected files. It does not read Lash's output. Host reads are controlled:
Node's random adapter consumes the injected 64-bit source by K-HOST-003, and
its Date adapter uses the injected nanosecond instant, truncated to milliseconds.
The clock laws separately assert epoch, pre-epoch and leap-day UTC fields.
Python currently lowers no random, time or datetime module helpers.

Observations are tagged JSON: numbers carry their binary64 bits, Python integers
carry decimal strings, strings carry UTF-16 code units (including lone
surrogates), containers retain member order, and exceptions carry their class.
Observe values separately when undefined must survive: the existing TypeScript
tool boundary converts absent array slots to null. Programs comparing RegExp,
URL or iterator results project their language-visible fields before crossing
that boundary, rather than comparing kernel branding data with language objects.

The Rust integration target `helper_differential__test` runs the programs through
both production dialect front ends and the kernel machine. It uses no external
interpreter. Its census reads the generated `TypeScriptHelper`/`PythonHelper` closed
sets, checks their names against the current release writes, and sums
actual per-body/native calls from
`count_work()`. A manifest reference or an unexecuted branch is not coverage.
Every helper must run in a corpus program or have a deliberate semantic deviation
with its specific helper name, case, rule and reason. `deviations.json` pins those
intentional differences separately from the real-language expected results.
Stale rows fail when the dialect matches its oracle again.

The comparison policy is defined once in `shared/helper_differential/comparison.rs`.
All values compare exactly except a run that actually calls an
[implementation-approximated Math function](https://tc39.es/ecma262/multipage/numbers-and-dates.html#sec-math-object)
(ECMA-262 §21.3.2): its finite nonzero float results allow one ULP, under the
FIG-6372 ruling. NaN, infinities and both zero signs remain exact. `sqrt`, rounding,
formatting, parsing and arithmetic alone receive no tolerance. The matching
Python math function class is reserved for when those helpers are lowered.

The live fuzzer is outside cacheable test actions. Its typed expression trees
combine edge pools with number, string and collection operations. It uses the
same Rust comparator as the laws, shrinks a mismatch through smaller expressions
of the same type, writes a ready-to-import case with its real oracle observation,
and exits unsuccessfully. Run under the memory cap and record peak RSS:

```sh
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/fuzz-helper-differential.py \
  --cases 10000 --seed 6372 --output .kiln/FIG-6372/fuzz)
```

The nightly workflow changes the seed each run and retains minimized cases and
counts as artifacts. Import a found case into `cases.json`, re-record, fix the
cause or add a specific reviewed deviation, and run the owning corpus law.
