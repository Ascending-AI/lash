//! Compiled sources for the Rust snippets on `docs/plugins-tools.html`.

use std::sync::Arc;

use lash::plugins::PluginFactory;
use lash::tools::{ToolCall, ToolResult};

// docs:start:direct-completion-tool
use lash::direct::{DirectOutputSpec, DirectRequest};

async fn rank(call: ToolCall<'_>) -> ToolResult {
    let model = match call.context.sessions().model().await {
        Ok(model) => model,
        Err(err) => return ToolResult::err_fmt(format_args!("{err}")),
    };

    let request = DirectRequest {
        model: model.model,
        model_variant: model.model_variant,
        model_capability: model.model_capability,
        messages: vec![/* ... */],
        attachments: Vec::new(),
        output: DirectOutputSpec::Text,
        generation: Default::default(),
        stream_events: None,
        session_id: None, // filled by ToolContext
        caused_by: None,  // filled by ToolContext
        replay: None,
    };

    match call
        .context
        .direct_completions()
        .complete(request, "my_tool")
        .await
    {
        Ok(completion) => ToolResult::ok(serde_json::json!({ "text": completion.text })),
        Err(err) => ToolResult::err_fmt(format_args!("{err}")),
    }
}
// docs:end:direct-completion-tool

async fn await_external_completion(call: ToolCall<'_>) -> ToolResult {
    // docs:start:detached-tool
    use lash::tools::PendingCompletion;

    // Take the completion key BEFORE returning Pending, then hand it to whatever
    // will deliver the result out-of-band — a webhook, a job queue, a human.
    let key = match call.context.completion_key().await {
        Ok(key) => key,
        Err(err) => return ToolResult::err_fmt(format_args!("{err}")),
    };
    enqueue_external_work(key);

    // Returning Pending without first taking the key fails the call with
    // `pending_tool_missing_completion_key`.
    ToolResult::pending(PendingCompletion::new())
    // docs:end:detached-tool
}

fn enqueue_external_work(_key: lash::AwaitEventKey) {}

fn batched_tool(
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
) -> lash::tools::ToolDefinition {
    // docs:start:batched-tool
    use lash::tools::ToolDefinition;

    ToolDefinition::raw(
        "tool:write_file",
        "write_file",
        "Replace a file's contents.",
        input_schema,
        output_schema,
    )
    // docs:end:batched-tool
}

fn budget_stack() -> lash::PluginStack {
    // docs:start:budget-stack
    use lash::plugins::{
        ToolOutputBudgetConfig, ToolOutputBudgetPluginFactory, runtime_plugin_stack,
    };

    let config = ToolOutputBudgetConfig {
        limit: 32 * 1024, // default: 16 * 1024 bytes
        max_lines: 800,   // default: 400
        ..ToolOutputBudgetConfig::default()
    };

    let plugins = runtime_plugin_stack().configure(|plugins| {
        plugins.replace(Arc::new(ToolOutputBudgetPluginFactory::new(config)));
    });
    // docs:end:budget-stack
    plugins
}

#[cfg(test)]
mod asserted_examples {
    use lash::tools::{
        LashlangToolBinding, ToolActivation, ToolArgumentProjectionPolicy, ToolContract,
        ToolDefinition, ToolDefinitionLashlangExt, ToolManifest, ToolManifestLashlangExt,
        ToolOutputContract, ToolRetryPolicy,
    };
    use schemars::JsonSchema;

    #[derive(JsonSchema)]
    struct WriteArgs {
        path: String,
        contents: String,
    }

    #[derive(JsonSchema)]
    struct WriteReceipt {
        bytes_written: usize,
    }

    #[test]
    fn tool_authoring_projects_one_definition_into_catalog_docs_and_runtime_contracts() {
        let argument_projection =
            ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("contents");
        assert!(
            !ToolArgumentProjectionPolicy::is_materialize_projected_values(&argument_projection),
            "the runtime must preserve content references for this tool"
        );
        let ToolArgumentProjectionPolicy::PreserveProjectedRefsInField { field } =
            &argument_projection
        else {
            panic!("the configured projection policy must retain a target field");
        };
        assert_eq!(field, "contents");
        assert!(
            ToolArgumentProjectionPolicy::is_materialize_projected_values(
                &ToolArgumentProjectionPolicy::MaterializeProjectedValues
            )
        );

        let binding = LashlangToolBinding::new(["workspace", "files"], "write")
            .with_authority_type("WorkspaceAuthority")
            .with_aliases(["write_text"]);
        let definition: ToolDefinition = ToolDefinition::typed::<WriteArgs, WriteReceipt>(
            "tool:write_file",
            "write_file",
            "Replace a UTF-8 file and report the committed byte count.",
        )
        .with_examples(vec![
            r#"write_file({ path: "notes.md", contents: "ready" })"#.to_string(),
            r#"write_file({ path: "status.txt", contents: "green" })"#.to_string(),
            r#"write_file({ path: "extra.txt", contents: "trimmed" })"#.to_string(),
        ])
        .with_activation(ToolActivation::Internal)
        .with_argument_projection(argument_projection)
        .with_retry_policy(ToolRetryPolicy::safe(3, 25, 100))
        .with_output_contract(ToolOutputContract::Static)
        .with_input_schema_projection(
            "provider:test",
            serde_json::json!({ "type": "object", "required": ["path"] }),
        )
        .with_output_schema_projection(
            "provider:test",
            serde_json::json!({ "type": "object", "required": ["bytes_written"] }),
        )
        .with_lashlang_binding(binding);

        assert_eq!(ToolDefinition::id(&definition).as_str(), "tool:write_file");
        assert_eq!(ToolDefinition::name(&definition), "write_file");
        assert_eq!(
            ToolDefinition::description(&definition),
            "Replace a UTF-8 file and report the committed byte count."
        );
        assert!(ToolDefinition::input_signature(&definition).contains("contents: str"));
        assert_eq!(
            ToolDefinition::output_summary(&definition),
            "record{bytes_written: int}"
        );
        assert!(ToolDefinition::signature(&definition).ends_with("record{bytes_written: int}"));
        assert_eq!(ToolDefinition::parameter_metadata(&definition).len(), 2);
        assert_eq!(ToolDefinition::model_tool(&definition).name, "write_file");

        let compact = ToolDefinition::compact_contract(&definition);
        assert_eq!(
            compact.examples.len(),
            2,
            "catalogs cap examples by default"
        );
        let one_example = ToolDefinition::compact_contract_with_example_limit(&definition, 1);
        assert_eq!(one_example.examples.len(), 1);

        let manifest: ToolManifest = ToolDefinition::manifest(&definition);
        let contract: ToolContract = ToolDefinition::contract(&definition);
        assert_eq!(manifest.id.as_str(), "tool:write_file");
        assert_eq!(manifest.name, "write_file");
        assert_eq!(
            manifest.description,
            "Replace a UTF-8 file and report the committed byte count."
        );
        assert_eq!(manifest.activation, ToolActivation::Internal);
        assert!(manifest.compact_contract.is_some());
        assert_eq!(manifest.retry_policy, ToolRetryPolicy::safe(3, 25, 100));
        assert_eq!(
            manifest.argument_projection,
            definition.manifest.argument_projection
        );
        assert!(manifest.bindings.contains_key("lashlang.tool"));
        let decoded_binding = ToolManifestLashlangExt::lashlang_binding(&manifest)
            .expect("the binding must decode")
            .expect("the binding must be present");
        assert_eq!(decoded_binding.module_path, ["workspace", "files"]);
        assert_eq!(decoded_binding.operation.as_deref(), Some("write"));
        assert_eq!(
            decoded_binding.authority_type.as_deref(),
            Some("WorkspaceAuthority")
        );
        assert_eq!(decoded_binding.aliases, ["write_text"]);
        let resolved_binding =
            LashlangToolBinding::required_executable_for_remote(&decoded_binding, "write_file")
                .expect("a complete binding must resolve for remote execution");
        assert_eq!(resolved_binding.operation, "write");
        assert_eq!(
            LashlangToolBinding::required_for_remote(&manifest)
                .expect("the projected manifest must remain remotely executable"),
            resolved_binding
        );

        assert_eq!(contract.examples.len(), 3);
        assert!(contract.output_contract.is_static());
        assert!(
            contract
                .input_schema
                .canonical()
                .get("properties")
                .is_some()
        );
        assert!(
            contract
                .output_schema
                .canonical()
                .get("properties")
                .is_some()
        );
        assert_eq!(
            contract.input_schema.projection.overrides[0].dialect,
            "provider:test"
        );
        assert_eq!(
            contract.output_schema.projection.overrides[0].dialect,
            "provider:test"
        );
        assert_eq!(
            ToolContract::input_signature(&contract, &manifest),
            definition.input_signature()
        );
        assert!(
            ToolContract::input_signature_with_name(&contract, &manifest, "save")
                .starts_with("save(")
        );
        assert_eq!(
            ToolContract::output_summary(&contract),
            definition.output_summary()
        );
        assert_eq!(ToolContract::parameter_metadata(&contract).len(), 2);
        assert_eq!(
            ToolContract::model_tool(&contract, &manifest).name,
            "write_file"
        );
        assert_eq!(
            ToolContract::compact_contract(&contract, &manifest)
                .examples
                .len(),
            2
        );
        assert_eq!(
            ToolContract::compact_contract_with_example_limit(&contract, &manifest, 1)
                .examples
                .len(),
            1
        );
        assert_eq!(
            ToolContract::compact_contract_with_signature_name(&contract, &manifest, "save").name,
            "save"
        );
        assert_eq!(
            ToolContract::compact_contract_with_signature_name_and_example_limit(
                &contract, &manifest, "save", 1,
            )
            .examples
            .len(),
            1
        );

        let recomposed = ToolDefinition::from_parts(manifest.clone(), contract.clone());
        assert_eq!(recomposed.manifest.id, manifest.id);
        assert_eq!(recomposed.contract.examples, contract.examples);

        let docs = ToolDefinition::format_tool_docs(std::slice::from_ref(&definition));
        assert_eq!(
            docs,
            ToolDefinition::format_tool_docs_iter([&definition]),
            "slice and iterator catalog rendering must agree"
        );
        assert!(docs.contains("### write_file({ contents: str, path: str })"));
        assert!(docs.contains("-> record{bytes_written: int}"));
        assert!(docs.contains("Replace a UTF-8 file"));
        assert!(docs.contains("notes.md"));
        assert!(
            !docs.contains("extra.txt"),
            "catalog example limits must affect rendered docs"
        );

        let dynamic_contract = ToolOutputContract::from_input_schema(
            "schema",
            Some(serde_json::json!({ "type": "string" })),
        );
        let ToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } = &dynamic_contract
        else {
            panic!("the dynamic output constructor must retain its schema source");
        };
        assert_eq!(input_field, "schema");
        assert_eq!(
            default_schema
                .as_ref()
                .and_then(|value| value["type"].as_str()),
            Some("string")
        );
        assert!(!ToolOutputContract::is_static(&dynamic_contract));

        let dynamic = ToolDefinition::raw(
            "tool:decode",
            "decode",
            "Decode according to a caller-provided schema.",
            serde_json::json!({
                "type": "object",
                "properties": { "schema": { "type": "object" } },
                "required": ["schema"]
            }),
            ToolDefinition::default_input_schema(),
        )
        .with_output_from_input_schema("schema", Some(serde_json::json!({ "type": "string" })));
        assert!(dynamic.signature().starts_with("decode<T = str>"));
        assert_eq!(
            ToolContract::default_input_schema(),
            ToolDefinition::default_input_schema()
        );
        assert_eq!(dynamic.manifest.activation, ToolActivation::Always);
    }

    #[test]
    fn tool_results_expose_host_visible_completion_modes_and_failure_details() {
        use std::time::Duration;

        use lash::tools::{
            CancelHint, PendingCompletion, TimeoutBehavior, ToolCallOutput, ToolFailure,
            ToolFailureClass, ToolFailureSource, ToolResult, ToolValue,
        };

        let success: ToolResult = ToolResult::ok(serde_json::json!({ "saved": true }));
        assert!(ToolResult::is_success(&success));
        assert!(!ToolResult::is_pending(&success));
        assert_eq!(
            ToolResult::value_for_projection(&success),
            serde_json::json!({ "saved": true })
        );
        let output = ToolResult::as_done_output(&success).expect("success must be complete");
        assert_eq!(
            serde_json::to_value(ToolCallOutput::status(output))
                .expect("tool status must serialize"),
            serde_json::json!("success")
        );
        assert!(ToolCallOutput::is_success(output));
        assert_eq!(
            ToolCallOutput::value_for_projection(output),
            serde_json::json!({ "saved": true })
        );
        assert!(output.control.is_none());
        assert_eq!(
            serde_json::to_value(&output.outcome).expect("tool outcome must serialize")["status"],
            "success"
        );
        let ToolResult::Done(done) = success.clone() else {
            panic!("an inline success must use the completed result mode");
        };
        assert_eq!(
            serde_json::to_value(done.status()).expect("tool status must serialize"),
            serde_json::json!("success")
        );
        let consumed = ToolResult::into_done_output(success).expect("success must unwrap");
        assert_eq!(
            ToolCallOutput::into_value_for_projection(consumed),
            serde_json::json!({ "saved": true })
        );

        let direct_output = ToolCallOutput::success(serde_json::json!({ "generation": 7 }));
        let wrapped = ToolResult::from_output(direct_output);
        assert_eq!(
            ToolResult::as_output(&wrapped).value_for_projection(),
            serde_json::json!({ "generation": 7 })
        );
        let json_error = ToolResult::err(serde_json::json!({ "path": "missing.md" }));
        assert_eq!(
            ToolResult::value_for_projection(&json_error),
            serde_json::json!({ "path": "missing.md" })
        );
        let formatted_error = ToolResult::err_fmt("provider unavailable");
        assert_eq!(
            ToolResult::value_for_projection(&formatted_error),
            serde_json::json!("provider unavailable")
        );

        let mut tool_failure = ToolFailure::tool(
            ToolFailureClass::InvalidRequest,
            "invalid_path",
            "path leaves the workspace",
        );
        tool_failure.raw = Some(ToolValue::from(serde_json::json!({ "path": "../secret" })));
        assert_eq!(tool_failure.class, ToolFailureClass::InvalidRequest);
        assert_eq!(tool_failure.code, "invalid_path");
        assert_eq!(tool_failure.message, "path leaves the workspace");
        assert_eq!(
            tool_failure.raw.as_ref().map(ToolValue::to_json_value),
            Some(serde_json::json!({ "path": "../secret" }))
        );
        assert_eq!(ToolFailure::to_json_value(&tool_failure)["source"], "tool");
        let failed = ToolResult::failure(tool_failure.clone());
        assert_eq!(
            serde_json::to_value(ToolResult::as_output(&failed).status())
                .expect("tool status must serialize"),
            serde_json::json!("failure")
        );
        let direct_failure = ToolCallOutput::failure(tool_failure);
        assert_eq!(
            serde_json::to_value(direct_failure.status()).expect("tool status must serialize"),
            serde_json::json!("failure")
        );

        let runtime_failure = ToolFailure::runtime(
            ToolFailureClass::Internal,
            "runtime_fault",
            "runtime could not dispatch the tool",
        );
        assert_eq!(
            ToolFailure::to_json_value(&runtime_failure)["source"],
            "runtime"
        );
        let retryable = ToolResult::retryable_failure(
            ToolFailureClass::Unavailable,
            "service_busy",
            "try again",
            Some(250),
        );
        assert_eq!(
            ToolResult::as_output(&retryable).value_for_projection()["retry"]["after_ms"],
            250
        );

        let cancelled = ToolResult::cancelled("operator stopped the tool");
        assert_eq!(
            serde_json::to_value(ToolResult::as_output(&cancelled).status())
                .expect("tool status must serialize"),
            serde_json::json!("cancelled")
        );
        let cancelled_with_raw = ToolResult::cancelled_with_raw(
            "operator stopped the tool",
            serde_json::json!({ "checkpoint": 4 }),
        );
        assert_eq!(
            ToolResult::value_for_projection(&cancelled_with_raw),
            serde_json::json!({ "checkpoint": 4 })
        );
        let default_pending = PendingCompletion::new();
        assert_eq!(default_pending.deadline, None);
        assert_eq!(default_pending.on_timeout, TimeoutBehavior::ErrorAsResult);
        assert_eq!(default_pending.on_cancel, CancelHint::CancelExternalWork);
        let pending_spec = PendingCompletion::fail_turn_on_timeout(
            PendingCompletion::with_deadline(default_pending, Duration::from_secs(30)),
        );
        assert_eq!(pending_spec.deadline, Some(Duration::from_secs(30)));
        assert_eq!(pending_spec.on_timeout, TimeoutBehavior::FailTurn);
        let pending = ToolResult::pending(pending_spec.clone());
        assert!(ToolResult::is_pending(&pending));
        let ToolResult::Pending(observed_pending) = pending.clone() else {
            panic!("a deferred completion must use the pending result mode");
        };
        assert_eq!(observed_pending, pending_spec);
        assert_eq!(
            ToolResult::into_done_output(pending).expect_err("pending must not unwrap"),
            pending_spec
        );

        let failure_classes = [
            ToolFailureClass::InvalidRequest,
            ToolFailureClass::Io,
            ToolFailureClass::Unavailable,
            ToolFailureClass::PermissionDenied,
            ToolFailureClass::Timeout,
            ToolFailureClass::Execution,
            ToolFailureClass::External,
            ToolFailureClass::ResourceLimit,
            ToolFailureClass::Internal,
        ];
        assert_eq!(
            serde_json::to_value(failure_classes).expect("failure classes must serialize"),
            serde_json::json!([
                "invalid_request",
                "io",
                "unavailable",
                "permission_denied",
                "timeout",
                "execution",
                "external",
                "resource_limit",
                "internal"
            ])
        );
        let host_failure_sources = [
            ToolFailureSource::Runtime,
            ToolFailureSource::Plugin,
            ToolFailureSource::Policy,
            ToolFailureSource::Cancellation,
        ];
        assert_eq!(
            serde_json::to_value(host_failure_sources).expect("failure sources must serialize"),
            serde_json::json!(["runtime", "plugin", "policy", "cancellation"])
        );
    }

    #[test]
    fn catalogued_tools_render_searchable_lashlang_module_previews() {
        use lash::tools::{
            CataloguePreviewEntry, CataloguePreviewOptions,
            DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT, DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT,
            catalogue_preview_contribution, catalogue_preview_contribution_for_entries,
            catalogue_preview_contribution_for_entries_with_options,
            catalogue_preview_contribution_for_manifests,
            catalogue_preview_contribution_with_options,
            catalogue_preview_entries_from_catalog_records,
            catalogue_preview_entries_from_manifests, catalogue_preview_entry_from_catalog_record,
            catalogue_preview_entry_from_manifest,
        };

        let search = ToolDefinition::raw(
            "tool:search_docs",
            "search_docs",
            "Search documentation.",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["knowledge", "docs"], "search"));
        let fetch = ToolDefinition::raw(
            "tool:fetch_url",
            "fetch_url",
            "Fetch a public URL.",
            ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["network", "http"], "fetch"));
        let manifests = [search.manifest(), fetch.manifest()];

        let direct = CataloguePreviewEntry::new(["workspace", "files"], "read");
        assert_eq!(direct.module_path, ["workspace", "files"]);
        assert_eq!(direct.call, "read");
        assert_eq!(direct.module_path_string(), "workspace.files");
        let executable = LashlangToolBinding::new(["workspace", "files"], "write")
            .required_executable_for_remote("write_file")
            .expect("complete bindings must expose an executable call path");
        let executable_entry = CataloguePreviewEntry::from_lashlang_executable(executable);
        assert_eq!(executable_entry.call, "workspace.files.write");

        let manifest_entry = catalogue_preview_entry_from_manifest(&manifests[0])
            .expect("lashlang-bound manifests must project into preview entries");
        assert_eq!(manifest_entry.module_path_string(), "knowledge.docs");
        assert_eq!(manifest_entry.call, "knowledge.docs.search");
        let manifest_entries = catalogue_preview_entries_from_manifests(&manifests);
        assert_eq!(manifest_entries.len(), 2);

        let records = manifests
            .iter()
            .map(|manifest| {
                serde_json::json!({
                    "name": manifest.name,
                    "bindings": manifest.bindings,
                })
            })
            .collect::<Vec<_>>();
        let record_entry = catalogue_preview_entry_from_catalog_record(&records[1])
            .expect("catalog records must preserve executable call paths");
        assert_eq!(record_entry.module_path_string(), "network.http");
        assert_eq!(record_entry.call, "network.http.fetch");
        let record_entries = catalogue_preview_entries_from_catalog_records(&records);
        assert_eq!(record_entries, manifest_entries);

        let defaults = CataloguePreviewOptions::default();
        assert_eq!(defaults.title, "Catalogued Capabilities");
        assert_eq!(defaults.search_tool_name, "search_tools");
        assert_eq!(defaults.search_call_path, "tools.search");
        assert_eq!(
            defaults.module_limit,
            DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT
        );
        assert_eq!(
            defaults.call_name_limit,
            DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT
        );

        let options = CataloguePreviewOptions {
            title: "Remote capabilities".to_string(),
            search_tool_name: "find_tools".to_string(),
            search_call_path: "catalog.find".to_string(),
            module_limit: 10,
            call_name_limit: 10,
        };
        let contribution = catalogue_preview_contribution_for_entries_with_options(
            [direct, manifest_entry, record_entry],
            options.clone(),
        )
        .expect("non-empty catalogues must render a preview");
        assert_eq!(contribution.title.as_deref(), Some("Remote capabilities"));
        assert_eq!(contribution.gate.tools, ["find_tools"]);
        assert!(contribution.content.contains("catalog.find"));
        assert!(contribution.content.contains("knowledge.docs.search"));
        assert!(contribution.content.contains("network.http.fetch"));
        assert!(contribution.content.contains("workspace.files: read"));

        let from_entries = catalogue_preview_contribution_for_entries(manifest_entries.clone())
            .expect("entries must render");
        let from_manifests = catalogue_preview_contribution_for_manifests(&manifests)
            .expect("manifests must render");
        let from_records = catalogue_preview_contribution(&records).expect("records must render");
        assert_eq!(from_entries.content, from_manifests.content);
        assert_eq!(from_manifests.content, from_records.content);
        let customized = catalogue_preview_contribution_with_options(&records, options)
            .expect("customized records must render");
        assert_eq!(customized.title.as_deref(), Some("Remote capabilities"));
    }

    #[test]
    fn typed_io_and_invalid_request_failures_are_stable_host_contract() {
        use lash::tools::{ToolFailure, ToolFailureClass};

        let invalid = ToolFailure::invalid_request("invalid_glob", "bad pattern");
        assert_eq!(invalid.class, ToolFailureClass::InvalidRequest);
        assert_eq!(invalid.code, "invalid_glob");
        assert_eq!(invalid.retry, lash::tools::ToolRetryDisposition::Never);

        let io = ToolFailure::io("read_failed", "could not read config.toml");
        assert_eq!(io.class, ToolFailureClass::Io);
        assert_eq!(io.code, "read_failed");
        assert_eq!(io.retry, lash::tools::ToolRetryDisposition::Never);
        assert_eq!(serde_json::to_value(ToolFailureClass::Io).unwrap(), "io");
    }
}
