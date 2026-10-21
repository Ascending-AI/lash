---
name: making-packages
description: Use when adding a host library package that guest TypeScript/Python code calls, implementing its operation runners, attaching it to workflows, or releasing a changed package.
---

# Making packages

Build a package whose code and declared operations are separate from the host
runner. Work from the [host-package guide](../../docs/host-packages.md) and the
tested [host-owned dataframe example](../../examples/dataframe/README.md) below.
Paths in this skill are relative to this file.

1. **Design the surface and contracts.**
   Choose a canonical package path and snake_case export names. Separate exported
   functions, sealed record types, statics, receiver methods, getter properties,
   errors and host operations. Decide which operations guests may call directly
   and which are internal to package bodies. Give each operation a contract id
   for its meaning and the exact intents its runner can request. Change the id
   when meaning changes, even with the same signature. Internal visibility hides
   an operation from guests; it does not remove the runner requirement.

   Read [Declare the package](../../docs/host-packages.md#declare-the-package)
   and [Contract meaning and visibility](../../docs/host-packages.md#contract-meaning-and-visibility).
   Inspect [dataframe.surface](../../examples/dataframe/dataframe.surface)
   and the export and contract laws in
   [laws/build.rs](../../crates/lash-kernel-package/src/laws/build.rs).

2. **Write the surface and kernel sources; build the artifact.**
   Author the declarations in a `.surface` file, definitions in `.kernel`, and
   display facts in one `.labels.json`: `members` maps surface paths to title
   strings or objects with `title`, optional `comparison` and `reply`;
   `definitions` is only for private definitions under the package roots.
   Duplicate keys, unknown fields or member paths, and empty titles are refused
   with a JSON pointer. Match each member's signature to its named
   definition. A method's receiver is first; a property has only its receiver.
   Mark a member `async` if it can wait, including through another body. Keep
   definitions under the package's path and off lash's reserved roots.

   Use `PackageSources` and `Package::build` with references to the exact
   dependency artifacts. The release label is for people; the digest identifies
   content. Stamp constructed package values as the example does, including
   `brand.built()`; callers see only declared members, and member entry checks
   the declared representations recursively.

   Read [Declare the package](../../docs/host-packages.md#declare-the-package).
   Follow [src/lib.rs](../../examples/dataframe/src/lib.rs),
   [dataframe.kernel](../../examples/dataframe/dataframe.kernel)
   and [dataframe.labels.json](../../examples/dataframe/dataframe.labels.json).
   Use the [surface reference](references/surface.md) for indentation and slots.

3. **Keep effects at the declared operation boundary.**
   Pure bodies compute values and plans. A body-only definition may `perform`
   only operations its package declares, recording each signature and contract
   in its effect declarations. The runner supplies the external action; package
   code never chooses it. A native is pure: a body beside a native declares no
   effects and cannot wait. Native functions cannot take function arguments.
   Keep charge formulas and native bounds sufficient for the real work.
   Preserve user callbacks as values; a closure made by a library body cannot
   be saved or cross an operation boundary.

   Read [Declare the package](../../docs/host-packages.md#declare-the-package)
   and [Library functions, especially K-LIB-004](../../docs/kernel/semantics.md#library-functions-k-lib).
   Compare the pure constructors, collecting and explaining bodies in
   [dataframe.kernel](../../examples/dataframe/dataframe.kernel).
   The operation-boundary law is in
   [laws/build.rs](../../crates/lash-kernel-package/src/laws/build.rs).

4. **Implement and register the runner.**
   Implement `ToolProvider`: advertise manifests, resolve contracts, prepare
   calls and execute them. On a package-owned operation path, the tool must
   declare the package's contract id and exactly its intents, and serve its
   signature. Use `ToolDraft::declared_contract`. Callback slots need the
   function marker described in the guide; arbitrary package values and `Any`
   are not JSON-schema substitutes.

   Choose an execution policy and explicit duration/retry bounds. `Read` can
   recompute after a crash. `Repeatable` can re-enter an interrupted attempt:
   make repetition safe, using the stable `ToolCallId` for external
   deduplication when needed. The
   dataframe runner safely recomputes immutable input without external effects.
   `Once` can leave an uncertain interrupted result. A committed outcome is consumed without
   another execution. Also bound input sizes and external work in the runner.

   Read [Write and replace a runner](../../docs/host-packages.md#write-and-replace-a-runner),
   [plugin authoring](../../docs/plugins.md) and
   [What survives a crash](../../README.md#what-survives-a-crash).
   Follow provider registration in
   [plugin_authoring.rs](../../crates/lash/examples/plugin_authoring.rs).
   Follow the real [runner](../../examples/dataframe/src/runner.rs) and
   [evaluator](../../examples/dataframe/src/evaluator.rs): callbacks stay in the
   kernel, and the runner receives data-only expression batches. The transport
   fake in [TypeScript package_members.rs](../../crates/lash-dialect-typescript/src/tests/package_members.rs)
   separately pins callback-bearing operation transport and package errors.

5. **Install one complete package set for the host.**
   Assemble the built-ins from `lash::package::lash_packages()` with your
   package and its exact dependencies using `HeldPackages::install`. Share the
   same holdings with `WorkerService::with_packages` and
   `RlmProtocolPluginFactory::with_packages`, or
   `KernelProcessPluginFactory::with_packages` for a standalone process host.
   Select holdings before starting or cloning workers. Register the runner via
   the host's session plugins or process components as appropriate.

   The core builder installs the set and definitions before the node serves
   and advertises its capabilities. `NodeInstall` is internal; hosts use the
   factory and builder path. Installing a package does not drive a turn; submit
   session work through `send()`.

   Read [Install once, choose a complete set](../../docs/host-packages.md#install-once-choose-a-complete-set).
   Follow the executable host assembly in
   [src/host.rs](../../examples/dataframe/src/host.rs).
   The builder and installer are in [core.rs](../../crates/lash/src/core.rs)
   and [packages.rs](../../crates/lash/src/packages.rs).

6. **Check both dialect spellings and lexical bindings.**
   TypeScript uses camelCase functions and methods; Python uses snake_case.
   Both use PascalCase types and errors. The canonical `data_quanta.read_text`
   member is called as `dataframe.DataQuanta.readText` in TypeScript and
   `dataframe.DataQuanta.read_text` in Python. Guests use installed paths without
   imports. Code members can be bound to short names; namespaces, types and
   operations cannot be taken as ordinary function values.

   Lexical bindings shadow package roots. A dotted call through a binding that
   would shadow an offered operation is refused, naming the binding and
   operation; rename the binding. Internal operations cannot be called directly,
   and computed strings cannot dispatch package operations.

   Read [A dataframe pipeline in two dialects](../../docs/host-packages.md#a-dataframe-pipeline-in-two-dialects).
   Use the spelling and value laws in
   [TypeScript package_members.rs](../../crates/lash-dialect-typescript/src/tests/package_members.rs)
   and [Python package_members.rs](../../crates/lash-dialect-python/src/tests/package_members.rs),
   plus the shadowing laws in
   [TypeScript naming.rs](../../crates/lash-dialect-typescript/src/tests/naming.rs)
   and [Python naming.rs](../../crates/lash-dialect-python/src/tests/naming.rs).

7. **Have the workflow handler derive its declaration.**
   In the linked host example, the model supplies code to `workflows.create`.
   The host handler inspects it against the offered surface and calls
   `lash::workflow::graph::requirements`.
   Project reached function identities through `SnapshotPackage::ids()`;
   collect canonical operations from `requirements.effects` and
   `requirements.performs.keys()`. Include reached language/kernel packages and
   internal operations too. Pass that package/operation map to
   `CellPackages::declare` and publish with the resulting `WorkflowPackages`.

   Keep the acquired surface handle through import and publication. Core takes
   the declaration as written; it does not choose or minimize it. Do not scrape
   names, hard-code the example's packages or ask the model to select them.
   A package key with no operations attaches code only; `declare_all()` is an
   explicit host choice for the whole offered surface.

   Read [Attach packages to a workflow](../../docs/host-packages.md#attach-packages-to-a-workflow)
   and [Publishing](../../docs/workflow-hosts.md#publishing).
   Follow `WorkflowCreator::create` in
   [src/host.rs](../../examples/dataframe/src/host.rs); the certification
   [workflows_create.rs](../../crates/lash/src/testing/plugin_harness/certification/workflows_create.rs)
   shows the code-only tool schema. See also the declaration laws in
   [workflow_laws/packages.rs](../../crates/lash/tests/workflow_laws/packages.rs).

8. **Plan rollout, rollback and already parked work.**
   Rebuild against changed dependency artifacts, even if your source is
   unchanged. Stored definitions supply body-backed code; execution still needs
   its kernel versions and any reached native-only callees. A serving node is
   live and decodes the current or a carried session format. With serving nodes,
   the gate selects the newest of their own stored sets by first install
   sequence whose native and kernel requirements the writer and every serving
   node meet. Reinstalling a set does not make it newer. Providers and codecs
   enter work's claim requirements through operation links; the gate does not
   check them. With an empty serving fleet, acquisition durably installs and
   answers the caller's complete own set;
   it does not grant execution or synthesize a serving node.

   Published definitions and parked runs keep pinned identities and operation
   requirements. Installing a release does not adopt them. A rollback lacking
   required natives, kernels, providers or codecs can leave work parked; restore
   a capable node. For advertised links, a changed live operation contract raises
   `operation_contract_changed` before its runner executes; resolved and dispatch
   links use their retained definitions. An unregistered provider parks work;
   a registered provider missing its advertised tool yields `linked_tool_missing`.
   Keep recorded provider/tool keys resolvable and retained codec payloads readable.

   Explicit adoption refuses a reached body's requirement that differs from the
   same operation's pinned requirement, and needs a valid correspondence for
   parked pages. Paged carry can move frames and waits; absent or incompatible
   pinned code still refuses rather than selecting
   another release. Rebuild carried package values when their representation
   becomes stale. Saved session functions relink at a new cell, unlike parked
   continuations.

   Read [Install once, choose a complete set](../../docs/host-packages.md#install-once-choose-a-complete-set),
   [Parked runs, publication and adoption](../../docs/host-packages.md#parked-runs-publication-and-adoption),
   [Sessions use the cell's current set](../../docs/host-packages.md#sessions-use-the-cells-current-set)
   and [When a linked tool changes](../../docs/workflow-hosts.md#when-a-linked-tool-changes).
   See selection in [packages.rs](../../crates/lash/src/packages.rs), the
   migration laws in [page_carry_tests.rs](../../crates/lash-vm-runtime/src/process/page_carry_tests.rs)
   and contract adoption laws in [process/tests.rs](../../crates/lash-vm-runtime/src/process/tests.rs).

9. **Prove the package's behavior.**
   Write a model law for a named package rule: construct values, run the real
   bodies against a deterministic fake runner, and compare values, operation
   requests and errors with the expected model. Follow the
   [package laws](../../crates/lash-kernel-package/src/laws/mod.rs),
   [build laws](../../crates/lash-kernel-package/src/laws/build.rs) and
   [shape laws](../../crates/lash-kernel-package/src/laws/shape.rs).

   Add an acceptance workflow in each supported dialect. Follow
   [wordcount.ts](../../examples/dataframe/wordcount.ts),
   [wordcount.py](../../examples/dataframe/wordcount.py) and `tests::df_split` in
   [src/tests.rs](../../examples/dataframe/src/tests.rs) for real collection,
   callback placement, data-only requests and interrupted/reopened work.
   For dialect transport and contract laws, follow
   `the_acceptance_program_lowers_to_its_golden_and_runs` in
   [TypeScript](../../crates/lash-dialect-typescript/src/tests/package_members.rs)
   and [Python](../../crates/lash-dialect-python/src/tests/package_members.rs):
   check lowered identities and effects, the golden, and the result against
   the fake runner. The reference goldens are
   [TypeScript wordcount.kernel](../../crates/lash-dialect-typescript/src/tests/wordcount.kernel)
   and [Python wordcount.kernel](../../crates/lash-dialect-python/src/tests/wordcount.kernel).
   Also exercise workflow attachment and the real host provider's retry and
   reopen contract where applicable; the fake does not establish those.

   Read [A dataframe pipeline in two dialects](../../docs/host-packages.md#a-dataframe-pipeline-in-two-dialects)
   and [One-call certification](../../docs/plugins.md#one-call-certification).
   Follow workflow publication in
   [src/tests.rs](../../examples/dataframe/src/tests.rs).

## Common refusals

| Refusal | Cause | Fix |
| --- | --- | --- |
| [`WorkflowAdmissionRefusal::UndeclaredMembers`](../../crates/lash-kernel-check/src/workflow.rs) | Code reaches functions or operations outside the workflow declaration. | Derive and declare the complete typed requirements, including internal performs and dependencies. |
| [`RuntimeErrorCause::NoCompatibleSnapshot`](../../crates/lash-core-store/src/runtime_error/cause.rs) | A nonempty serving fleet has no stored candidate whose native and kernel requirements the writer and every serving node meet. | Deploy the missing capabilities or finish the incompatible rollout; inspect the named candidates. |
| [`DiagnosticCode::OperationShadowed`](../../crates/lash-dialect-typescript/src/diagnostics.rs), [`Code::OperationShadowed`](../../crates/lash-dialect-python/src/diagnostics.rs) | A lexical binding shadows an offered operation's root. | Rename that binding and use the dialect's operation spelling. |
| [`InvalidReason::EffectNotDeclared` / `InvalidReason::NativeBodyMayWait`](../../crates/lash-kernel-doc/src/validate.rs) (K-LIB-004) | A body performs an undeclared effect, or a body beside a native can wait. | Declare the package operation and use a body-only async member for effects; keep the native path pure. |
| [`PackageRefusal::SyncMemberWaits`](../../crates/lash-kernel-package/src/refusal.rs) | A synchronous member reaches waiting code. | Declare the member async and await it in guest code. |
| [`PackageRefusal::NotClosed`](../../crates/lash-kernel-package/src/refusal.rs) | The set lacks an exact dependency artifact. | Rebuild dependents against the chosen artifacts and install the complete set. |
| [`StaleValue`](../../crates/lash-kernel-package/src/stale.rs) (`package.stale_value`) | A same-brand value has an incompatible representation. | Reconstruct the value under the selected package set; inspect the reported path and constructor. |
| [`ParkedRefusal::FunctionNotCarried` / `ParkedRefusal::StateNotCarried`](../../crates/lash-kernel-migrate/src/migration.rs) | The rewrite omits a pinned function, or the successor cannot express parked state. | Preserve the run and restore its capable build, or supply a valid carry; never substitute a different snapshot. |
