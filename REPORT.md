# FIG-1300 implementation report

## Result

FIG-1300 is implemented as a pure Lashlang-behavior-preserving refactor. `lash-protocol-rlm` now owns a dialect registry, pins one dialect when an RLM plugin session is constructed, and routes each `ExecRequest.language` through that registry before execution. The only registered dialect remains `lashlang`; an unregistered language produces a typed `DialectRegistryError::Unregistered` which is returned as a protocol `SessionError`, and requesting a registered but non-active dialect produces `DialectRegistryError::Inactive`.

No TypeScript implementation was added.

## Dialect seam

The seam lives in `crates/lash-protocol-rlm/src/dialect.rs`, with the Lashlang adapter in `dialect/lashlang.rs`. Its central signatures are:

```rust
trait RlmDialect: Send + Sync {
    fn language_id(&self) -> &'static str;
    fn snapshot_engine_id(&self) -> &'static str;
    fn cell_tags(&self) -> CellTags;
    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError>;
    fn render_execution_section(
        &self,
        features: RlmPromptFeatures,
        tool_catalog: &ToolCatalog,
    ) -> Result<String, SessionError>;
    fn render_history_cell(&self, prose: &str, code: &str) -> String;
    fn finalization_copy(&self, termination: &RlmTermination) -> &'static str;
    fn cell_error_message(&self, error: CellExtractionError) -> String;
    fn turn_limit_final_copy(&self, max_turns: usize) -> String;
    fn finish_required_copy(&self, requires_schema: bool) -> String;
    fn finish_schema_mismatch_copy(&self) -> String;
    fn invalid_cell_retry_copy(&self, error_text: &str) -> String;
    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String;
    fn code_stream_kind(&self) -> &'static str;
    fn execution_diagnostic_name(&self) -> &'static str;
    fn stream_cell_start_event_name(&self) -> &'static str;
    fn stream_cell_end_event_name(&self) -> &'static str;
}

trait RlmDialectSession: Send {
    async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError>;
    // Snapshot capture/restore, dirty-state lifecycle, global patch/prune,
    // and bound-variable rendering are also owned by the dialect session.
}

impl RlmDialectRegistry {
    fn new(dialects: impl IntoIterator<Item = Arc<dyn RlmDialect>>) -> Self;
    fn resolve(&self, language: &str)
        -> Result<Arc<dyn RlmDialect>, DialectRegistryError>;
    fn resolve_active(&self, language: &str, active: &str)
        -> Result<Arc<dyn RlmDialect>, DialectRegistryError>;
}
```

The dialect covers the required cell scan/extract/mask tags, prompt section and pedagogy, model-visible execution diagnostics, history-cell rendering, execution entry, snapshot engine discriminator, finish/retry copy, and stream diagnostic vocabulary. `LashlangDialectSession` owns the current Lashlang executor state and is the only adapter that converts protocol-owned execution bounds, abilities, and language features into Lashlang engine types.

## Leak fixes and wire shapes

### Protocol configuration

Before, `RlmProtocolPluginConfig` publicly named these engine-owned Rust types:

- `lashlang::ExecutionBound<NonZeroU64>`
- `lashlang::ExecutionBound<Duration>`
- `lashlang::LashlangAbilities`
- `lashlang::LashlangLanguageFeatures`

After, it names protocol-owned `ExecutionBound<T>`, `ExecutionBounds`, `RlmAbilities`, and `RlmLanguageFeatures`. Conversion to the engine types happens in the Lashlang dialect adapter. The serialized field names and values are unchanged, including:

```json
{
  "instruction_budget": { "bounded": 1000000 },
  "deadline": { "bounded": 30000 },
  "lashlang_abilities": {
    "processes": true,
    "sleep": true,
    "process_signals": true,
    "triggers": true
  },
  "lashlang_language_features": { "label_annotations": true }
}
```

`"unbounded"` retains the same enum representation. A direct equivalence test serializes each protocol-owned type beside its former Lashlang-owned counterpart and asserts equality. The existing `lashlang_abilities` and `lashlang_language_features` field/builder identifiers remain intentionally because changing them would alter public host config/API beyond replacing the leaked types.

### Shared trace vocabulary

Before (trace schema version 3):

```json
{
  "type": "lashlang_execution",
  "event": { "kind": "node_started" }
}
```

After (trace schema version 4):

```json
{
  "type": "language_execution",
  "language": "lashlang",
  "event": { "kind": "node_started" }
}
```

`TraceEvent::LashlangExecution` and the shared `TraceLashlangExecution*` DTO family became `TraceEvent::LanguageExecution` and `TraceLanguageExecution*`. All producers, schema tests, the OpenTelemetry sink, the trace viewer, simulations, runtime tests, and example consumers were updated. The Lashlang graph projection remains Lashlang-specific as allowed and ignores language-execution events whose `language` is not `lashlang`.

## Snapshot compatibility

The active dialect supplies the snapshot envelope's `engine` value and restore validates it through the generalized existing `EngineMismatch` path. The engine remains exactly `"lashlang"`.

The ticket text calls the canonical snapshot version 6, but `origin/main` at `b6b24f9b8` already had `RLM_SNAPSHOT_VERSION = 7`. This change preserves version 7, the existing field order, and all canonical bytes rather than reverting or bumping the live format. The version-7 golden byte test passed without updating any golden data, and an active-engine mismatch regression test was added.

## Files touched

- Dialect and protocol: `crates/lash-protocol-rlm/src/dialect.rs`, `dialect/lashlang.rs`, `cell_scan.rs`, `stream_mask.rs`, `driver.rs`, `driver/history.rs`, `driver/history/tests.rs`, `protocol.rs`, `protocol/cell.rs`, `protocol/driver.rs`, `protocol/finish.rs`, and `protocol/tests.rs`.
- Execution, session, and config: `crates/lash-protocol-rlm/src/executor.rs`, `executor/host_bridge.rs`, `executor/snapshot.rs`, `executor/state.rs`, `executor/state/tests.rs`, `plugin.rs`, `plugin/config.rs`, `plugin/config_types.rs`, `plugin/factory.rs`, `plugin/prose_projector.rs`, `plugin/protocol_driver.rs`, `plugin/protocol_session.rs`, `plugin/registration.rs`, `plugin/runtime_state.rs`, and `lib.rs`.
- Lashlang execution adapter consumers: `crates/lash-lashlang-runtime/src/lib.rs` and `process.rs`.
- Trace schema and consumers: `crates/lash-trace/src/lib.rs`, `lashlang_graph.rs`, `otel.rs`, `tests/schema.rs`; `crates/lash-trace-viewer/src/main.rs`, `render.rs`; and `crates/lash-sim/src/runner/agent_contracts.rs`.
- Runtime and examples: `crates/lash/src/lib.rs`, `tests/agent_scenarios/cases.rs`, `tests/agent_scenarios/contracts.rs`, `tests/turn_streaming.rs`; `examples/agent-workbench/src/execution_graphs.rs` and `main_sections/tests.rs`.
- Documentation: `docs/architecture/deps.html`, `docs/reporting.html`, and `docs/tracing.html`.

No file in `/workspace/code/lash` or another worktree was touched.

## Verification

- PASS — `cargo check --workspace --all-targets`
- PASS — `cargo test --workspace`
- PASS — `cargo clippy --workspace --all-targets -- -D warnings`
- PASS — `grep -rn "lashlang" crates/lash-protocol-rlm/src/plugin/config.rs`: output contains only the deliberately retained serialized field and builder identifiers; there are no Lashlang type names or `lashlang::` references in the public config surface.
- PASS — `grep -rn "LashlangExecution" crates/lash-trace/src/`: no output.
- PASS — `cargo test -p lash-protocol-rlm version_7_root_encodes_to_golden_bytes`: 1 passed; no golden bytes changed.
- PASS — `git diff --check`.

## Deliberate exclusions

- No TypeScript dialect, tags, runtime, or tests were added; that is later work.
- No VM or `lashlang` crate semantics changed.
- No Lashlang prompt, finalization, retry, tag, binding, artifact-namespace, or snapshot-engine wording/value changed.
- No SQL/schema storage changes were made.
- `lash-core` required no language-specific plumbing. Runtime and public re-export sites were updated only as consumers of the allowed generic trace/config vocabulary changes.
- No compatibility shim or alternate dispatch path was retained: the registry and pinned dialect session are the sole RLM execution path.
