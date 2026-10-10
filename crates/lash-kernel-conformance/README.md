The corpus pins written kernel rules, independently of a dialect. Each file
`corpus/<family>/<K-RULE-ID>.json` owns one rule and contains `{rule, cases}`.
Each owning crate embeds its own shard tree with `index.py`; no lane edits
another lane's index. `load_corpus` decodes host-supplied `(path, bytes)` pairs
in sorted order, so the kernel library does no filesystem I/O;
`check_coverage` compares its rule ids with definitions in
`docs/kernel/semantics.md`, in both directions. Empty or duplicate shards and
duplicate case names fail. Add an executable case whose expectation comes
from the rule, never an outcome observed from the current implementation.

`pending.json`, beside the corpus, names the lane that owns each rule still
missing a kernel-text case. `check_coverage(semantics, shards, pending)` is
the coverage ratchet: every defined rule has a nonempty shard or one named
pending owner, never both. Unknown rules, duplicate pending rows and blank
owners fail. The list is committed and is not inferred at test time, so a
new rule without a case or an ownership row fails immediately. An owning
lane removes its row when adding its shard to the composed selection.
Native supplements do not satisfy this requirement.

`supplements/json/K-LJSON-005.json` pins the library's stringify cycle rule
at an immutable tuple root, using the real machine and registered native.
Library-specific rule documents remain separate from the core coverage list.

KLAND requires this list to be empty. Its complete-corpus gate asserts
`pending.is_empty()` and calls `check_coverage(semantics, shards, &[])`.
Passing the ratchet with pending rows does not claim full conformance.

A case contains `name`, `document` (kernel text), `environment` and `expected`.
Values in the envelope use kernel `Datum` serialization: `"null"`,
`{"int":"3"}`, `{"float":"1.0"}`, `{"text":"hello"}`,
`{"list":[...]}`, `{"record":[["field",value]]}`, etc. The document contains
literal library identities in its `use` lines; the test injects its registry.

`environment` defaults to no host reads or deliveries, a charge bound of
1,000,000, 16 MiB of memory, call depth 64, and 128 live tasks, requests per
park and join members. It may override those `bounds`, `slice`, `max_steps`,
`entry`, and `args`. `resume: true` exports state and rebuilds the machine at
every park. `deliveries` is a list of batches, one per park. Each batch is
delivered in its written order before the next run. A delivery is
`{"request":0,"outcome":{"completed":{"int":"2"}}}`, a failed outcome
`{"failed":{"kind":"io","message":"failure","data":"null"}}`, or
`"elapsed"` for a sleep. Request numbers index the complete request trace.
`dropped: true` expects a withdrawn wait's outcome to be dropped.

The adapter admits the document with `lash-kernel-check` before starting the
machine. Per-rule machine laws register the numeric, text/JSON and collection
libraries and the ECMAScript regex extension, so their kernel documents run
through the same adapter as the core forms. By default the scripted host provides the manifest's effect
signatures. `effects` supplies an explicit catalogue for admission cases;
an empty object provides no effects.

`host` is an ordered list of synchronous answers: `{"clock":"123"}`
(nanoseconds), `{"random":9223372036854775808}` (raw 64-bit bits),
`{"read":{"handle":{"kind":"table","id":"x"},"request":"null",
"answer":{"Ok":{"text":"answer"}}}}`, or `{"cancel":true}`.
Unscripted clock/random/read calls, mismatched requests, unconsumed answers,
and missing delivery batches are harness errors. Cancellation defaults false.

`expected` pins `prints`, `trace` and `end`. `charged` and `parks` are optional
where a rule does not pin them. Endings are `{"finished":datum}`,
`{"failed":datum}`, `{"uncaught":{"int":"7"}}`,
`{"tasks_outstanding":{"unfinished":[task identities],"unobserved":[]}}`,
`{"deadlock":{"waiting":[task identities]}}`,
`{"bound":{"name":"live_tasks","limit":1}}`, `"cancelled"`, or
`"refused"`. An uncaught throw retains its complete Datum, including non-error values.
Refusal is parsing or admission, never a machine/script failure.

An effect trace item is `{"effect":{"effect":"echo","args":[datum],
"result":"int","identity":effect identity}}`. A sleep trace item is
`{"sleep":{"nanoseconds":0,"identity":effect identity}}`. Actual output
always includes identities; expectations may omit them when the rule concerns
something else. Identity-sensitive cases must pin them. No extra prints,
requests or unused script rows are tolerated.

`check_case` accepts any `DocumentRunner`; `MachineRunner<M: Machine>` is the
standard adapter. A second reader implements the same observation seam.
`check_native<M>` uses the document corpus with a registry factory: every
registered native requires a case referencing it and pinning its charge;
every guard also requires a failing case. It runs each case cold twice, with
fresh native instances, then runs the whole selection twice on a shared warm
registry, comparing full observations. The factory must create fresh caches.

`NativeShard` files under `native/<family>/<rule>.json` supplement the document
corpus with direct native calls. They do not count as kernel-text coverage.
`NativeCase` holds `name`, `function` (definition identity), `args: Datum[]`,
and `expected: {outcome, charged, work}`. Outcomes are `{"returned":datum}`,
`{"raised":ErrorDatum}`, or `{"guard":{"limit":N}}`. `work` is the real
`WorkCounter::spent()`. `check_native_calls(factory, probe, cases)` repeats
every registered native cold twice and warm twice. The probe invokes the
registered implementation using its library's real heap and formula evaluator.
Keeping those in the owning library avoids a second implementation of kernel
decoding, copying, equality or charging here.

`examples/kernel-embedder` is compiled as a development dependency. It loads
the fan-out document, admits table effects at a park, discards the machine,
imports the saved state, and delivers answers in reverse order. The per-rule laws and fan-out law run against `KernelMachine`;
a scheduler double is not proof.

Witness scripts in `witness/` run trusted source under the pinned Node or
system CPython, with a scripted tool table. They emit values, errors, printed
mutations and effect issue/delivery traces. Captures are checked in beside the
source; Rust conformance tests need no runtime download or network.

Embed this lane's own shard tree with `python3 crates/lash-kernel-conformance/index.py crates/lash-kernel-conformance/corpus crates/lash-kernel-conformance/src/corpus_files.rs --tests-output crates/lash-kernel-conformance/src/machine_cases.rs`. `--check` verifies membership after formatting. The test selection also mounts numeric library K-KEY-002, K-KEY-004 and K-VAL-026 from their owning crate. Its other five document cases run as supplements to already-owned rules. Add further owning library and parked-state shards to the composed selection and remove their pending rows; native-call supplements never count as kernel-text coverage.

`corpus/dialect/K-DIALECT-001.json` and `K-DIALECT-002.json` are owned by
`lash-dialect-typescript`'s
`tests::printer_corpus::every_admitted_conformance_document_preserves_observations_when_printed_and_lowered`.
That law loads every core shard plus the numeric and JSON document supplements,
checks the original rule observations, prints and lowers in the original library
and effect environment, then checks a clone with only its document replaced.
It compares complete observations, including effect identities, charge and parks.
Refusal fixtures remain under the core parsing/admission oracle.
The printer law reuses the core embedding index, so adding a shard to the core
selection also adds it to the printer selection.

The core rule laws also run test-only contract probes from
`src/contract_cases.rs` alongside their owning shards. These inspect properties
that output traces cannot express: stored JSON, domain-separated identity test
vectors, annotation independence, definition validation, transitive manifests,
derived sites, and rejected deliveries leaving exported state unchanged.
Definition probes use kernel text and the public checker and machine seams.
The machine corpus registry includes the kernel numeric library for document
cases that call its native definitions. The portable case envelope is unchanged.
