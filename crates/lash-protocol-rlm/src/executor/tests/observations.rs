use super::*;

pub(super) async fn execute_with_host_environment(
    code: &str,
    resources: lash_vm::LashVmHostCatalog,
) -> ExecResponse {
    execute_with_host_environment_and_archives(code, resources)
        .await
        .0
}

/// Run a cell and return its response and attachment store.
pub(super) async fn execute_with_host_environment_and_archives(
    code: &str,
    resources: lash_vm::LashVmHostCatalog,
) -> (
    ExecResponse,
    Arc<lash_core::facade_support::RuntimeAttachmentStore>,
) {
    let mut state = RlmExecutionState::new();
    let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let artifact_store = handler.artifacts();
    let ctx = lash_core::testing::code_execution_context(handler.ports());
    let surface = LashVmSurface::new(lash_vm::LashVmLanguageFeatures::default(), resources);
    let attachments = ctx.attachment_store();
    let response = execute_code_with_test_render(
        &mut state,
        ctx,
        ExecRequest {
            code: code.to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        None,
        lash_vm::ExecutionBounds::new(
            lash_vm::ExecutionBound::instructions(1_000_000),
            lash_vm::ExecutionBound::Unbounded,
        ),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    (response, attachments)
}

#[test]
pub(super) fn print_observation_preserves_typed_value_and_records_cut_metadata() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let record = format!(
            "{{ output: {}, status: \"failed\", error: \"boom\", exit_code: 2, stderr: \"short\" }}",
            serde_json::to_string(&large).expect("string literal")
        );
        let (response, attachments) = execute_with_host_environment_and_archives(
            &format!("print({record});"),
            lash_vm::LashVmHostCatalog::new(),
        )
        .await;

        assert!(response.error().is_none(), "{:?}", response.error());
        assert!(response.prints.is_empty());
        let archive = response
            .prints_retained
            .as_ref()
            .expect("aggregate is archived");
        let bytes = attachments
            .read(&archive.reference)
            .await
            .expect("exact archive");
        let observations: Vec<lash_core::CellPrint> =
            serde_json::from_slice(&bytes).expect("observations");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].value["output"], large);
        assert!(observations[0].text.starts_with("[cut: "));
        assert!(observations[0].text.contains("narrow with history["));
        let metadata = &observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert!(metadata.original_chars > 60_000);
        assert!(metadata.projected_chars < metadata.original_chars);
        assert_eq!(metadata.limit_chars, 8_000);
    });
}

/// Console output is also rendered under the fixed character cap.
#[test]
pub(super) fn console_log_of_a_large_record_stops_at_the_char_cap() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let code = format!(
            "console.log({{ output: {}, status: \"failed\" }});",
            serde_json::to_string(&large).expect("string literal")
        );
        let (response, attachments) =
            execute_with_host_environment_and_archives(&code, lash_vm::LashVmHostCatalog::new())
                .await;
        assert!(response.error().is_none(), "{:?}", response.error());
        let archive = response.prints_retained.as_ref().expect("archive");
        let bytes = attachments
            .read(&archive.reference)
            .await
            .expect("archive bytes");
        let observations: Vec<lash_core::CellPrint> =
            serde_json::from_slice(&bytes).expect("observations");
        let metadata = &observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert!(
            metadata.projected_chars < metadata.original_chars,
            "{metadata:?}"
        );
    });
}

#[test]
pub(super) fn subcap_prints_stay_fully_inline_including_empty_and_null_values() {
    block_on(async {
        let response = execute_with_host_environment(
            "print(\"\"); print(null); print({text: \"é🙂\", nested: [1, false]});",
            lash_vm::LashVmHostCatalog::new(),
        )
        .await;
        assert!(response.error().is_none(), "{:?}", response.error());
        assert!(response.prints_retained.is_none());
        assert_eq!(
            response
                .prints
                .iter()
                .map(|print| print.value.clone())
                .collect::<Vec<_>>(),
            vec![
                serde_json::json!(""),
                serde_json::Value::Null,
                serde_json::json!({"text": "é🙂", "nested": [1, false]})
            ]
        );
    });
}
