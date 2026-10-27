# Host upgrade checklist

Use this alongside [the workflow-host guide](workflow-hosts.md),
[package hosting](host-packages.md) and [durable hosting](operations/durable-hosting.md).
This checklist covers `7d4b2ee63c354fce7e73551c5c51a27a92897dbf` through
`459043d66791` on main, including the five repin-39 changes complete at
`519c8530f0`. It records host integration breaks and their current replacements.
The endpoint is a source snapshot, not a promise of stored-format compatibility.
Before 1.0, stored and wire shapes change in place. Rebuild consumers and
regenerate their declarations when moving the pin.

Each row names the old contract, the replacement, its introducing commit and
ticket, and the symptom if a host misses it. Compiler and decoder symptoms below
are examples, not stable error strings. Linked implementation files are the
current source authority. Mark the rows that apply to your host before upgrading.

## Registration, linking and source contracts

| Old contract | New contract / host action | Commit and ticket | Missed-change diagnostic or symptom |
| --- | --- | --- | --- |
| `ToolBinding`, `ToolDefinitionBindingExt`, `with_tool_binding`, separate name/module or aliases | Declare one canonical operation at construction, for example `ToolDefinition::raw(id, "x.read_document", description, input_schema, output_schema)?`. The operation owns its package path and effect name. See [tool construction](../crates/lash-vocabulary/src/tool_contract.rs). | [2e8253b876](https://github.com/Ascending-AI/lash/commit/2e8253b876), FIG-6913 | Unresolved imports or missing binding methods; a bare noncanonical operation is refused as `ToolCatalogBuildError::InvalidOperation`. |
| Hand-built model names, or one shared Standard/MCP/source spelling | Derive the Standard name with `ToolManifest::standard_name()`. It joins canonical segments with `__` and uses a digest suffix beyond 64 characters. Keep external MCP dispatch names separate. See [the three spellings](#one-operation-three-spellings). | [2e8253b876](https://github.com/Ascending-AI/lash/commit/2e8253b876), FIG-6913 | Catalogue dispatch misses, unknown model tools, or `TS_OPERATION_SPELLING` for an incorrectly spelled source member. |
| Installation sets, linked process effects, external manifest authority and name-keyed routing | Publish accepted package namespaces under a `HostArtifactPin`; supply `WorkflowEnvironmentRequest { snapshot, context, attachment }`, publish a `LinkedStart`, then `link_process_env` with `OperationUse`-keyed provider selections. Register model-facing providers separately. See [the compiled host examples](workflow-hosts.md#import-publish-link-and-start). | [d12659a1e7](https://github.com/Ascending-AI/lash/commit/d12659a1e7), FIG-7109 | Removed imports/fields, unresolved Imports or unavailable operations at admission. A root-only list can miss captured operations. |
| `publish_workflow(pin, document, entry_name, process_env, source) -> Result<WorkflowPublish>`; match `Published`/`Refused`/`Unsupported` | `publish_workflow(pin, document, selector, environment_request, source, limits) -> Result<LinkedStart>`. Use `StartSelector::Main` or `Export(name)`; handle publication errors through `Result`. `WorkflowPublish` is removed. [Facade implementation](../crates/lash/src/artifacts.rs). | [d12659a1e7](https://github.com/Ascending-AI/lash/commit/d12659a1e7), FIG-7109 | Unresolved `WorkflowPublish`, wrong argument count/types, or old result matching fails to compile. |
| `inspect_callable` returns only a definition; hosts infer operations from the root | It returns `(ProcessDefinition, LinkInspection)`. `inspect_target(&LinkTarget)` inspects either a linked start or a published definition. Review `inspection.operations` across captured defining Links before saving/linking. [Host contract](workflow-hosts.md#import-publish-link-and-start). | [99eab17855](https://github.com/Ascending-AI/lash/commit/99eab17855), FIG-7190 | Tuple/field compile errors; incomplete policy review or `EmbedError::ConflictingOperationRoutes` when uses select different routes for one operation. |
| Only product tools are considered when classifying whole-reach operations | Dialect services are operations too. `typescript.clock`, `typescript.random` and their Python counterparts are `ExecutionPolicy::Read`; `sleep` is repeatable and deferring. Attach the dialect package when source needs it and classify from the linked tool definition's execution policy. [Service definitions](../crates/lash-vm-runtime/src/dialect_services/mod.rs). | [d12659a1e7](https://github.com/Ascending-AI/lash/commit/d12659a1e7), FIG-7109; [a98c095705](https://github.com/Ascending-AI/lash/commit/a98c095705), FIG-7253 | Unavailable service at linking, or a host's hardcoded allowlist incorrectly labels `Date.now()` as a mutation. |
| Saved entries omit parameter/result annotations, or generated source erases them | Write a resolved type for every public parameter and settled result. Resolve local aliases/interfaces from retained declarations. Use representable data types; widen numeric literal types such as `sdk_version: 1` to `sdk_version: number` and retain exact-value checks in the product validator. [Type conversion](../crates/lash-dialect-typescript/src/adapter/types.rs), [entry laws](../crates/lash-dialect-typescript/src/tests/module.rs). | [4e7bb02165](https://github.com/Ascending-AI/lash/commit/4e7bb02165), FIG-7143 | `SIGNATURE_REQUIRED` for a missing annotation; `ENTRY_ANNOTATION_REFUSED` for a numeric literal type or an erased/unresolved alias such as `Row`. See [entry examples](#written-entry-types). |
| Treat omitted implicit-text policy as `Refuse` because of the old facade rustdoc | The actual default is `ImplicitTextConversionPolicy::Allow`. Set `Refuse` or `Strict` explicitly in `WorkflowSourceLimits` if required, and use the same limits on import and publication. [Policy implementation](../crates/lash-kernel-dialect/src/workflow.rs). | [fb62660729](https://github.com/Ascending-AI/lash/commit/fb62660729), FIG-7284; rustdoc corrected by FIG-7500 | Default admission allows conversions the host expected to refuse. Explicit refusal reports `TS_IMPLICIT_TEXT_CONVERSION`. |
| Match flattened boundary errors or pass no defining Link to validation | Consume `DecodeRefusal` and its `TypeMismatch`, `EntryTypeMismatch` at entry boundaries and typed `SignatureRequired` refusals. `validate_start_input` / `validate_start_result` take the defining `LinkId`. Preserve RFC 6901 value paths and the typed cause rather than matching prose. [Boundary vocabulary](../crates/lash-kernel-value/src/lib.rs). | [4e7bb02165](https://github.com/Ascending-AI/lash/commit/4e7bb02165), FIG-7143 | Removed `DecodeMismatch`, wrong validation arguments, or missing match arms; entry result failure is `entry_type_mismatch`. |

## Runtime, dependencies and storage

| Old contract | New contract / host action | Commit and ticket | Missed-change diagnostic or symptom |
| --- | --- | --- | --- |
| `SessionSpec::new(model, turn_budget, max_tool_calls)` followed by optional stall policy | `SessionSpec::new(model, turn_budget, max_tool_calls, no_progress_budget)`. Preserve the host's chosen budgets; use `NoProgressBudget::Unbounded` only for an explicit opt-out. [Signature](../crates/lash-execution-contracts/src/session_spec.rs). | [13fa014d44](https://github.com/Ascending-AI/lash/commit/13fa014d44), FIG-6872 | Rust reports a missing fourth argument; previously optional root fields are now required values. |
| Construct `ReadCall` with manifest, args, config, cancel and deadline only | Also supply `clock: &dyn Clock` and `history: Option<&ReadHistory>`. Workflow reads have `None` history; cell reads may have their immutable run history. The RLM history tool removal does not remove this Rust read context. [Fields](../crates/lash-core-execution/src/tool_reads.rs). | [d12659a1e7](https://github.com/Ascending-AI/lash/commit/d12659a1e7), FIG-7109; [55bc4554e1](https://github.com/Ascending-AI/lash/commit/55bc4554e1), FIG-7243 | Missing `clock` / `history` in a struct literal; code assuming history is always present fails. |
| Host pins `rmcp` 3.5.1 or directly depends on the removed `lash-rmcp` fork | Use crates.io `rmcp` with floor `^3.7.0`, update host lockfiles and regenerate its build graph. Lash's `mcp` feature keeps Lash-owned host interfaces. [Workspace dependency](../Cargo.toml). | [f625f3d962](https://github.com/Ascending-AI/lash/commit/f625f3d962), FIG-7371 | Dependency resolution conflict, missing `lash-rmcp`, or incompatible SDK types across host and adapter. |
| Reopen the old PostgreSQL catalog with the new binary or rely on current teardown statements to remove all old tables | For an incompatible generation with no declared migration, drain affected sessions and recreate the whole Lash trust domain. Drop the owned schema with `CASCADE` or recreate the database, then provision with this build's `lash migrate` or `PostgresStorage::schema_ddl()`. This is an operator procedure that discards old state, not an automatic upgrade. [ADR 0081](adr/0081-destructive-schema-changes-are-currently-reject-and-recreate.md), [refusal/remedy](../crates/lash-postgres-store/src/postgres/schema.rs). | [d12659a1e7](https://github.com/Ascending-AI/lash/commit/d12659a1e7), FIG-7109 package/store cutover; ADR 0081 governs the remedy | Startup refuses an unsupported schema or structural mismatch and reports the writing release when readable. Current `teardown_ddl()` may leave retired catalog tables behind. |
| Omitted ingress/retention policies, nullable fields, or a 1 MiB commit cap paired with default ingress | Defaults are 64 MiB ingress, 64 MiB / 512 rows per commit, one hour idle and 10,000 resident owners. `owner_retention` and `max_ingress_bytes` take plain values. A smaller commit budget needs a fitting ingress cap. Removed `MissingOwnerRetention`, `MissingIngressLimit` and `MissingCommitBudget` match arms go away. [Paired settings](#paired-postgresql-settings). | [4474938e2a](https://github.com/Ascending-AI/lash/commit/4474938e2a), FIG-7198 | `max_ingress_bytes (67108864) exceeds commit byte budget (1048576)`; `null` no longer decodes as an omitted policy; old `Some(...)` assignments fail to compile. |
| Override PostgreSQL durable settings with `DurableBackendBuilder::postgres(...).config(...)` | Put them in `PostgresHostConfig.node` before connecting. The backend copies `host.effective_config().node`. [Builder](../crates/lash/src/durable.rs), [host config](../crates/lash-postgres-store/src/host/config.rs). | [442f5dc940](https://github.com/Ascending-AI/lash/commit/442f5dc940), FIG-5240; [4474938e2a](https://github.com/Ascending-AI/lash/commit/4474938e2a), FIG-7198 defaults | `DurableBuildError::SettingsOwnedByHost` at backend build. |
| `SessionStore::input_status` / `RunStore::input_status` and `InputStatus`; separate run-binding and terminal reads | Call the inherited `RunStore::follow_facts(session, &FollowSubject::Input(input)) -> FollowFacts`. Read binding and terminal evidence together from `FollowFacts::Input` and its optional `run`. Re-read on each resolution; an input can rebind. [Snapshot contract](../crates/lash-core-store/src/store/run_follow.rs). | [3453a8da71](https://github.com/Ascending-AI/lash/commit/3453a8da71), FIG-7045 | Removed method/type at compile time; stale separate reads can associate terminal evidence with the wrong binding. Never await an active run while holding a product lock its tools need: release the lock before following, then reacquire and validate product state. |
| RLM `history[N]`, `rlm.history.length/at/slice`, `control.read_output` and host `read_output_tool_definition` | Remove retired prompt/catalogue guidance and calls. Cells see the current frame and its variables; retained values remain readable by hosts through their attachment store. Remove `SessionGlobal.cell` and `RetainedValueUnavailable` matches. [Current protocol](../crates/lash-protocol-rlm/src/lib.rs). | [656e9fdbec](https://github.com/Ascending-AI/lash/commit/656e9fdbec), FIG-7370 | Unbound history name or unavailable operation; removed facade symbols fail to compile. |
| XML/fenced source cells or a configurable RLM cell channel | Use one provider-native `execute_code` call with exactly `{"code":"<complete program>"}`. Invoke product tools and `control.finish` inside that program. Prose is commentary. FIG-7471's supplied examples print output and do not finish the task for the model. [Transport](../crates/lash-protocol-rlm/src/native/tool.rs), [prompt contract](../crates/lash-protocol-rlm/src/prompt_contract_tests.rs). | [b4882e1f03](https://github.com/Ascending-AI/lash/commit/b4882e1f03), FIG-7374; [6f7aea35df](https://github.com/Ascending-AI/lash/commit/6f7aea35df), FIG-7471 | Text source does not execute; malformed/multiple native calls get `No code executed` repair feedback. |
| Kernel `Int` tool results treated as TypeScript `number`, then logged with `JSON.stringify` | They are TypeScript `bigint`. Use `console.log(value)` for output, or an explicit JSON replacer that converts bigint to text. Use `Number(value)` only when the host accepts possible precision loss. [BigInt implementation and laws](../crates/lash-dialect-typescript/src/tests/module.rs). | [e60f5334dc](https://github.com/Ascending-AI/lash/commit/e60f5334dc), FIG-7286 | `TypeError: Cannot serialize a BigInt` from `JSON.stringify`; mixed Number/BigInt arithmetic also refuses. No new opt-in. |

## Repin-39 wire and Rust shape changes

All five are present at `519c8530f0` or later. Update readers and generated
schemas/declarations together. There is no version bump, migration or old-shape
reader. No new opt-in is required for these changes.

| Old contract | New contract / host action | Commit and ticket | Missed-change diagnostic or symptom |
| --- | --- | --- | --- |
| `BatchResultRow` / `ModelBatchResultRow`: `success` plus optional `result` / `error` | Typed rows require `outcome: ToolCallOutcome` with `status: success/failure/cancelled` and `payload`. Model rows require `outcome: ModelToolOutcome` with `status: completed/failure/cancelled` plus text `body`. Success payloads retain ToolValue's wire encoding, including its untrusted-JSON envelope. [Rows](../crates/lash-protocol-types/src/standard_batch.rs), [outcomes](../crates/lash-protocol-types/src/tool_output.rs). | [35ad2a50c3](https://github.com/Ascending-AI/lash/commit/35ad2a50c3), FIG-7488 | Missing `outcome`, unknown `success` field, or failure/cancellation lost by old boolean readers. |
| `TraceEvent::ExecCodeCompleted.error` and `.terminal_finish` | Require `result: CellOutcome`: `kind` is `completed`, `failed` or `controlled`; `value` carries failure or `{ tool_name, call_id, control }`. A finish with inline null remains controlled. Content hooks use `code_output.result.failure.*` / `code_output.result.finish`. [Event](../crates/lash-trace/src/event.rs), [outcome](../crates/lash-protocol-types/src/session.rs). | [13f9109456](https://github.com/Ascending-AI/lash/commit/13f9109456), FIG-7487 | Trace decoding reports missing `result`; old readers mistake null finish for ordinary completion. |
| Independent diagnostic `code`, `span`, `kind`, `unsupported`, optional `fact` / `operand_type`; `DialectRefusal`, `DiagnosticFact`, `WorkflowSourceDiagnosticFact` | `Diagnostic { reason, message, repairs }`; `DiagnosticReason` owns the evidence and `code()`, `kind()`, `span()` derive it. Use `lash::vm::Diagnostic`, `DiagnosticReason` and `WorkflowSourceDiagnosticReason`; worker refusal/error payloads share `Diagnostic`. Neither `Diagnostic` nor `DiagnosticKind` has `Default`. Generic codes use validated `DialectCode`. [Definition](../crates/lash-kernel-dialect/src/diagnostic.rs). | [a4aa10ca9d](https://github.com/Ascending-AI/lash/commit/a4aa10ca9d), FIG-7489 | Removed fields/types/defaults fail to compile; wire readers report missing `reason`. |
| `MaterializeRefusal::MissingHelper { name }`; migration `NotAdmitted { reason: String }`; admission `InvalidDocument { message }` | Missing helpers carry `function: FunctionId`; construction lookup uses `ConstructionHelperMissing { name }`. Preserve `LibraryRequirements` / `DocumentValidation` causes. Migration `NotAdmitted` carries `refusal: Invalid`, with separate `ExplicitSuccessorRequired` and `Succession`. Admission uses `DocumentValidation(Invalid)` or `DocumentCheck` for aggregate checker errors. `FunctionBody.functions` remains a `BTreeMap<FunctionId, FunctionName>`. [Materialization](../crates/lash-kernel-package/src/materialize.rs), [migration](../crates/lash-kernel-migrate/src/migration.rs). | [519c8530f0](https://github.com/Ascending-AI/lash/commit/519c8530f0), FIG-7490 | Removed fields/variants fail to compile; migration readers report missing `refusal` or encounter new refusal tags. |
| `SourceSite.ranges` + `.implicit`; `WorkflowNode.kind/members/within/lazy/header/arms/condition_ordinal` | `SourceSite.location` is `spelled { ranges }` with nonempty ranges or `implicit { offset }`. Omit unmapped sites. Node-specific data moves to `body`, tagged `action`, `control` or `code`. Shared Rust reads use `members()`; control header is `Option<Box<ControlHeader>>`, still an optional object on wire. Code members are nonempty; secondary action members may be empty. Source-map token bytes and identities change. [Locations](../crates/lash-kernel-doc/src/source_map.rs), [nodes](../crates/lash-kernel-view/src/coarse.rs). | [613172eefb](https://github.com/Ascending-AI/lash/commit/613172eefb), FIG-7491 | Missing `location` / `body` during decode or TypeScript property errors. Old source-map identities no longer identify rewritten maps. |

For example, a typed batch failure changes from:

```json
{"index":0,"tool":"read","success":false,"error":{"class":"invalid_request","code":"schema","message":"bad arguments","source":"runtime"}}
```

to:

```json
{"index":0,"tool":"read","outcome":{"status":"failure","payload":{"class":"invalid_request","code":"schema","message":"bad arguments","source":"runtime"}}}
```

A source location changes from `{"ranges":[],"implicit":5}` to
`{"location":{"kind":"implicit","offset":5}}`, with the common `site`
field unchanged. A node's action-specific fields move under
`{"body":{"kind":"action","members":[],"within":null,"lazy":[]}}`.

A trace's old `"error": null, "terminal_finish": null` could not distinguish
ordinary completion from a finish with null. The new controlled result preserves
that distinction and the controlling call:

```json
{"result":{"kind":"controlled","value":{"tool_name":"control.finish","call_id":"tc_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","control":{"type":"finish","value":{"inline":null}}}}}
```

For diagnostic consumers, the old top-level `code`, `span`, `unsupported` and
`operand_type` become one typed reason. An implicit conversion refusal now reads:

```json
{"reason":{"kind":"implicit_text_conversion","data":{"span":{"start":17,"end":22},"operand_type":"object"}},"message":"implicit text conversion refused by source admission policy","repairs":[]}
```

## One operation, three spellings

| Use | Spelling for the same operation |
| --- | --- |
| Canonical registration, effect name and stored records | `x.read_document` |
| Standard model tool name, from `ToolManifest::standard_name()` | `x__read_document` |
| TypeScript source, from `lash::dialect::spell_operation(Spelling::TypeScript, &operation)` | `x.readDocument` |
| Python source, from `spell_operation(Spelling::Python, &operation)` | `x.read_document` |

`Spelling` is also in `lash::dialect`. Do not manually replace dots or underscores:
Standard names can truncate, and source spelling handles path segments and
members differently. Match hooks and persisted records by canonical operation.
Generate source declarations and prompts with the dialect's spelling function.

## Written entry types

An unannotated saved entry such as `export function run(event) { return event; }`
is refused with `SIGNATURE_REQUIRED`. An annotation
`{ sdk_version: 1 }` is refused with `ENTRY_ANNOTATION_REFUSED` because numeric
literal types are not representable. Erasing `Row`'s declaration while keeping
`Row[]` produces an unresolved annotation refusal with that same code.

Retain declarations when lowering host source:

```typescript
type Row = { title: string };
interface Result { sdk_version: number; rows: Row[] }
export function run(rows: Row[]): Result {
  return { sdk_version: 1, rows };
}
```

Representable boundaries include `string`, `number`, `bigint`, `boolean`, null,
arrays, structural records, supported tuples, string literal enums and supported
unions/intersections. Local aliases and interfaces must resolve to those shapes.
Explicit `any` / `unknown` are unconstrained JSON boundaries, not arbitrary live
handles or functions. Async results unwrap `Promise<T>` to the settled `T`.
Callable, mapped, conditional and unresolved types refuse at the entry boundary;
numeric/template literal types also refuse. See the linked type converter and
entry laws for exact representability. Keep exact SDK-version validation in the
host's runtime validator even when the written type uses `number`.

## RLM output examples

The provider-native tool call below illustrates FIG-7471's print-only transport
example. It demonstrates execution without choosing the task's final result:

```text
execute_code({"code":"console.log(1 + 2);"})
```

For an integer-valued product tool result, print the value directly:

```typescript
const document = await x.readDocument({ id: "example" });
console.log(document);
```

Use the host's actual declared argument shape. Do not wrap an arbitrary result
in `JSON.stringify`: a nested kernel Int becomes BigInt and can make it throw.

## Paired PostgreSQL settings

These assignments preserve a host's existing 1 MiB commit limit and 60-second
idle policy. Apply them before opening `PostgresHost`; pass the commit budget to
`LashCore::builder(backend)` when assembling the core:

```rust
use std::num::NonZeroU64;
use std::time::Duration;
use lash::CommitBudget;
use lash::postgres::PostgresHostConfig;

let commit_budget = CommitBudget::bounded(1024 * 1024, 512);
let mut postgres = PostgresHostConfig::standard();
postgres.node.max_ingress_bytes = NonZeroU64::new(1024 * 1024).unwrap();
postgres.node.owner_retention.idle = Duration::from_secs(60);
// Connect PostgresHost with `postgres`, then build its backend without `.config(...)`.
// Apply `.commit_budget(commit_budget)` to the host's LashCore builder.
```

Ingress fitting within the commit byte limit is required, but does not promise
that every accepted input plus its resulting writes fits in one commit. Keep the
host's workload-specific row/byte budget checks. When serializing a `node`
section, supply the complete section; its leaf settings do not merge as a patch.

## Adding the next upgrade entry

Every change to a host-facing signature, wire shape, default or dependency floor
adds `docs/host-upgrades/unreleased/<ticket>.md` in the same landing. Use the
[entry format and release procedure](host-upgrades/unreleased/README.md).
