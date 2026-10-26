# Standalone kernel engine

`lash-kernel-engine` compiles TypeScript or Python source and runs the Lash
kernel in the calling process. Enable `dialect-typescript` or `dialect-python`
explicitly. Disabled dialects refuse compilation. There are no worker, broker,
store, protocol or durable dependencies. See `examples/kernel-engine-hello`.

`Engine::new(Limits)` installs the build's exact helper release and native
implementations. Workers use `embedding::install` for the same installation
against their parent's immutable image. Workers still own process isolation,
wire pages, cancellation and resets. `Engine::prepare` admits linked kernel
content for advanced callers and uses the same driver as source compilation.

`compile` accepts a module exporting a typed, startable `main` function.
TypeScript writes `export async function main(name: string): Promise<string>`;
Python writes `async def main(name: str) -> str`. `run` supplies positional
arguments. Module initialization does not execute. `compile_snippet` accepts a
fresh cell whose last expression supplies its result, through `Mode::Cell`.
It does not retain bindings for a subsequent cell.

Externals declare qualified source names, signatures and host contract IDs.
They are waited operations; this interface resolves no ambient filesystem,
network, clock or dynamic imports. Compile pins the operation fills. Call IDs
include run, effect and wait identity. Names are routing/display aids.

`run` and `resume` return ordered print deltas and either a result or a paused
snapshot. `Paused.calls` contains newly issued calls; `Snapshot::pending()`
contains all unanswered calls. Answers can be partial and arrive in any order.
Unknown IDs, already answered IDs and duplicate IDs in a batch refuse before
any delivery. Withdrawals remove pending calls; late answers for withdrawn
calls are discarded. A consumed snapshot cannot be resumed twice. On refusal
the supplied snapshot is consumed, so validate host inputs before resuming.

The host must persist the call ledger and matching checkpoint together
**before dispatch** if it needs durable admission. Commit outcomes before resume
when crash recovery matters. This lane's simple snapshots are in memory and
cannot provide crash recovery. Durable hosts use `embedding::install` with the
kernel `Machine`/`PageSource` API and their own checkpoint publication transaction.
The facade supplies no durable transaction or exactly-once side effects. Persist
prints with the host's park transaction if presentation must survive crashes.
Dropping a snapshot drops its retained pages.

Snapshots resume in the same process and exact build. There is no `to_bytes`,
archive, restore or cross-version compatibility promise before 1.0. Each
snapshot retains its effective limits and cumulative printed-output budget;
an engine with smaller ceilings refuses it. Charge and heap meters remain in
the kernel checkpoint and are not reset by resume.

`Value::new(Datum, max_bytes)` preserves absent/null, arbitrary integers, IEEE
floats, bytes, timestamps, tuples and ordered collections. It refuses callable,
undecoded number-token and live host-handle transport. Nodes and payload bytes
count against the cap; depth is at most 128. Each argument batch and external
call's arguments share the value cap. Printed values share a cumulative output
cap. Page retention has a separate storage cap and keeps immutable pages until
the run completes or the snapshot is dropped. It does not reclaim obsolete
pages during a run. Storage exhaustion is an infrastructure failure.

Defaults are finite: 1 million charge units, 64 MiB guest resident capacity,
1024 call depth, 256 live tasks/calls per park/join members, 64 KiB source,
28 source nesting levels, 4 MiB values, 4 MiB cumulative output and 128 MiB
retained page storage. The call-depth default matches the worker at this lane's
base; FIG-7430 was not yet present. Resident accounting is not total process RSS.
Hard wall-clock deadlines and process-memory isolation belong to the host.
