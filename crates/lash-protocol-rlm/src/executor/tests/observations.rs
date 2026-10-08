use super::*;

pub(super) async fn execute_with_host_environment(
    code: &str,
    abilities: lashlang::LashlangAbilities,
    resources: lashlang::LashlangHostCatalog,
) -> ExecResponse {
    execute_with_host_environment_and_archives(code, abilities, resources)
        .await
        .0
}

/// Run a cell and return its response and attachment store.
pub(super) async fn execute_with_host_environment_and_archives(
    code: &str,
    abilities: lashlang::LashlangAbilities,
    resources: lashlang::LashlangHostCatalog,
) -> (
    ExecResponse,
    Arc<lash_core::facade_support::RuntimeAttachmentStore>,
) {
    let mut state = RlmExecutionState::new();
    let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let artifact_store = handler.artifacts();
    let ctx = lash_core::testing::code_execution_context(handler.ports());
    let surface = LashlangSurface::new(
        abilities,
        lashlang::LashlangLanguageFeatures::default(),
        resources,
    );
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
        lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::instructions(1_000_000),
            lashlang::ExecutionBound::Unbounded,
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
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.observations.is_empty());
        let archive = response
            .output_archive
            .as_ref()
            .expect("aggregate is archived");
        let bytes = attachments
            .get(&archive.reference.id)
            .await
            .expect("exact archive")
            .bytes;
        let observations: Vec<lash_core::Observation> =
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
        let (response, attachments) = execute_with_host_environment_and_archives(
            &code,
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let archive = response.output_archive.as_ref().expect("archive");
        let bytes = attachments
            .get(&archive.reference.id)
            .await
            .expect("archive bytes")
            .bytes;
        let observations: Vec<lash_core::Observation> =
            serde_json::from_slice(&bytes).expect("observations");
        let metadata = &observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert!(
            metadata.projected_chars < metadata.original_chars,
            "{metadata:?}"
        );
    });
}

/// A host can withhold the sleep ability.
#[test]
pub(super) fn executor_reports_a_disabled_lashlang_ability_at_link_time() {
    block_on(async {
        let code = "await sleep(1000);";
        lash_typescript::parse(code).expect("the fixture parses");
        let response = execute_with_host_environment(
            code,
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;
        let error = response
            .error
            .as_ref()
            .expect("a withheld ability fails at link time");

        assert!(
            error
                .message
                .contains("lashlang feature `sleep` is disabled by this host"),
            "error was {}",
            error.message,
        );
        assert!(response.calls.is_empty(), "no runtime tools are called");
        assert!(
            response.observations.is_empty(),
            "no observations are emitted"
        );
        assert!(response.printed_images.is_empty(), "no images are emitted");
        assert!(
            response.terminal_finish.is_none(),
            "the program does not finish terminally"
        );
    });
}

#[test]
pub(super) fn subcap_prints_stay_fully_inline_including_empty_and_null_values() {
    block_on(async {
        let response = execute_with_host_environment(
            "print(\"\"); print(null); print({text: \"é🙂\", nested: [1, false]});",
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.output_archive.is_none());
        assert_eq!(
            response
                .observations
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
