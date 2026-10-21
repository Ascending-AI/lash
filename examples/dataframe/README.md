# Host-owned dataframe

This example computes the Rigel wordcount using a host package and a small Rust
evaluator. It adds no dataframe subsystem to lash. Copy the package and provider
into your host; `src/evaluator.rs` is where you plug in DataFusion or a database.

`wordcount.ts` and `wordcount.py` use the installed `dataframe` namespace, without
imports. `viaJs` / `via_js` applies every callback in the kernel and makes no
native call. `viaExprs` / `via_exprs` tokenizes in the kernel, then sends filter,
count, sort and limit as one recorded expression run. Python uses whitespace
`.split()`, which differs from TypeScript's `\W+`: punctuation remains part of a
Python word. The shared proof inputs deliberately contain only words and spaces.
`read_text` starts with one text row; the callback chooses tokenization.

The `.kernel` collector cuts maximal consecutive expression stages. A callback
flushes the pending native run, applies its function to the returned rows, then
continues collecting. Thus native → callback → native makes two recorded runs.
Both `collect_rows` and `explain` are async. Explanation reports kernel stages
and host expression runs without executing user callbacks or sending them out.
The transport-only fixture in `crates/lash-kernel-package` remains separate.

The wire contract is `{rows, schema, plan}` → `{rows, schema}`. Scalar input has
one column, `value`; native aggregation returns positional rows with key columns
then aggregate columns. `count` and signed-integer `sum` return decimal **text**,
with `:int64_text` in the schema. Sorting those columns uses integer comparison.
This preserves counts above JavaScript's safe integer range. The lambda sample
also renders its small counts as text so its result has the same representation;
its arithmetic follows its dialect's ordinary numeric rules. Sort is stable;
text ordering is Rust's UTF-8 lexical order and equal aggregate keys retain first
appearance. Unsupported expressions, invalid rows, integer overflow and
cancellation produce typed failures. This is a small inline-row specimen,
bounded by the normal worker effect-size and heap limits. Large inputs need a
host-owned staging/paging protocol.

Install `packages()` before starting workers and give the same holdings to
`WorkerService::with_packages` and the protocol or kernel process factory's
`with_packages`. Register `runner::Runner` with a `StaticPluginFactory` and
`PluginSpec::with_tool_provider`. For a workflow process, register that provider
as a process component and link `runner::definitions()` in its `ProcessEnv`.
`host::build(backend, owner)` wires that component, environment, workers and
kernel process engine; the caller supplies its node lease identity.
`host::WorkflowCreator::create` imports code against that environment, derives
package and operation membership from typed graph requirements, and publishes
under the caller's artifact pin. A host supplies the entry name and retains its
pin for as long as the publication is needed.

Both operations declare data-only `Json` slots, recorded `Repeatable` execution
and a 30-second execution bound. Evaluation changes no external state, so an
uncommitted attempt can safely rerun with the same call id and immutable input.
Cancellation is checked between stages, rows, aggregate members and sort
key evaluation and before/after the bounded stable sort. Committed outcomes are consumed by lash's engine on recovery.

Charge formulas cover builder input/output sizes. The collector and callback
loops have size-based completion charges and explicit per-iteration metering;
user callback bodies and nested library calls retain their own charges. There
is no constant-cost arbitrary planner loop. Distinct-key accumulators and
flat-map expansion remain subject to the normal kernel memory bound.

From the repository root, after `. ./env.sh`:

```sh
kiln test //examples/dataframe:dataframe__unit_test --test_arg=tests::df_split --test_arg=--exact
```

`DF-SPLIT` runs both source samples and parked callback-bearing pipelines over
two inputs in both dialects on SQLite memory. It checks exact counts, data-only
wire payloads, maximal runs, no native call for `viaJs`, exact large integer
sums, typed failures, and a crash inside native → callback → native. Only the
uncommitted Repeatable call re-enters after reopening, with the same call id.
