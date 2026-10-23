# TypeScript source admission

`typescript_admission` feeds bounded raw UTF-8 (lossily decoded), generated
TypeScript and byte/token mutations through cell and workflow lowering. The
shared harness lives in
`crates/lash-dialect-typescript/shared/source_admission.rs`. It caps input at
1,024 bytes, source at 4,096 bytes and nesting at 16; each input checks six
source/mode combinations. Successful lowering must produce valid source maps
and structurally valid documents that survive kernel text serialization and
parsing, then reach typed kernel admission. Source refusals cross the dialect
Diagnostic boundary; admission refusals retain their RefusalReason values.
libFuzzer's panic hook aborts before unwinding, including on parser threads, so
production panic containment cannot hide a fuzz failure.

The committed corpus covers annotations, generic calls/comparison ambiguity,
enums and const enums, satisfies/as/non-null wrappers, optional chaining,
malformed syntax, Unicode and workflows. The generator also authors generic
arrows, interfaces and type assertions. No arbitrary source is executed.

From an isolated fork after sourcing `env.sh`, copy the committed seeds to a
writable evidence directory and run a bounded campaign through Kiln:

```sh
mkdir -p .kiln/FIG-7237/corpus .kiln/FIG-7237/artifacts
cp fuzz/corpus/typescript_admission/*.ts .kiln/FIG-7237/corpus/
(ulimit -v 8388608
 /usr/bin/time -v kiln gate lash <fork> -- cargo +nightly fuzz run \
   --dev --sanitizer none typescript_admission .kiln/FIG-7237/corpus -- \
   -runs=512 -seed=7237 -max_len=1024 -timeout=10 -rss_limit_mb=4096 \
   -artifact_prefix=.kiln/FIG-7237/artifacts/)
```

This uses coverage instrumentation without a memory sanitizer. The existing
manifest-driven fuzz smoke also discovers the target and its seed corpus.
Minimize a failure into a law naming the broken rule before retaining it.

`//crates/lash-dialect-typescript:source_admission__test` contains four laws:

- `malformed_and_ambiguous_source_has_typed_admission_outcomes`: 256 deterministic
  inputs replay the same harness, covering every structured production.
- `deterministic_typescript_erasure_preserves_observations`: 128 pairs compare
  explicitly authored TypeScript/erased JavaScript against literal observations.
  `a < b > (c)` erases to `a(c)`; `(a < b) > c` remains a comparison. Only this
  deterministic admitted subset is executed.
- `enum_initializer_order_and_const_enum_inlining_are_observable`: literal
  observations pin initializer order, single evaluation and const-enum reads.
- `enum_reverse_lookup_uses_record_semantics`: a numeric reverse read must be
  admitted and return the member name.

Enums use explicit language expectations, without a Node enum oracle. These
laws run against the real in-process kernel with test-owned page storage and
require no SQL service. Select the full law names with `--test_arg`; add
`--test_arg=--nocapture` to expose input and pair counts.
