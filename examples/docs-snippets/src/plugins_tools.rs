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
}
