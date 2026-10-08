use super::*;
use lash_core::llm::types::LlmOutputSpec;
use lash_sansio::{ProviderFailureKind, SchemaContract, SchemaDialect};

fn provider() -> GoogleOAuthProvider {
    GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access"),
    ))
}
fn projected_contract() -> SchemaContract {
    SchemaContract::admit(json!({"type":"object","properties":{"canonical":{"type":"string"}}}))
        .expect("valid declared schema")
        .with_override(
            SchemaDialect::GoogleSchema,
            lash_sansio::JsonSchema::admit(
                json!({"type":"object","properties":{"projected":{"type":"string"}}}),
            )
            .expect("valid declared projection schema"),
        )
}
fn tool_request(model: &str) -> LlmRequest {
    let mut req = request(None);
    req.model.metadata_mut().wire_model = model.into();
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "lookup".into(),
        description: "lookup".into(),
        input_schema: projected_contract(),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    req
}
fn request_body(req: &LlmRequest) -> Value {
    GoogleOAuthProvider::build_request(&provider(), req, vec![], None).expect("schema projection")
}

#[test]
fn protocol53_lookalike_name_does_not_supply_dialect() {
    let mut req = request(None);
    req.model.metadata_mut().wire_model = "unrelated-gemini-3-lookalike".into();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call".into(),
            tool_name: "lookup".into(),
            input_json: "{}".into(),
            replay: None,
        }],
    )];
    let contents = provider()
        .build_contents_with_attachment_parts(&req)
        .expect("retention policy");
    assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
}

#[test]
fn protocol53_schema_contract_modes_apply_to_every_dialect_and_site() {
    use lash_core::{GoogleDialect, ProjectionMode};
    for dialect in [
        GoogleDialect::Legacy,
        GoogleDialect::Gemini3,
        GoogleDialect::ClaudeOnVertex,
    ] {
        for output in [false, true] {
            for mode in [
                ProjectionMode::ExplicitOnly,
                ProjectionMode::Exact,
                ProjectionMode::Auto,
            ] {
                let mut req = request(None);
                req.model.metadata_mut().wire_model = "host-catalog-alias".into();
                req.model.metadata_mut().capability.google_dialect = dialect;
                let mut contract = SchemaContract::admit(
                    json!({"type":"object","properties":{"value":{"type":"string"}}}),
                )
                .expect("valid declared schema");
                contract.projection.mode = mode;
                if output {
                    req.output_spec = Some(LlmOutputSpec::JsonSchema(
                        lash_core::llm::types::LlmJsonSchema {
                            name: "answer".into(),
                            schema: contract,
                            strict: false,
                        },
                    ));
                } else {
                    req.tools = Arc::new(vec![LlmToolSpec {
                        name: "lookup".into(),
                        description: "lookup".into(),
                        input_schema: contract,
                        output_schema: lash_sansio::SchemaContract::admit(json!({}))
                            .expect("valid declared schema"),
                    }]);
                }
                let result = GoogleOAuthProvider::build_request(&provider(), &req, vec![], None);
                if mode == ProjectionMode::Auto {
                    let body = result.expect("automatic projection remains permissive");
                    let schema = if output {
                        &body["request"]["generationConfig"]["responseSchema"]
                    } else {
                        &body["request"]["tools"][0]["functionDeclarations"][0][if dialect
                            == GoogleDialect::ClaudeOnVertex
                        {
                            "parameters"
                        } else {
                            "parametersJsonSchema"
                        }]
                    };
                    assert!(schema["properties"].get("value").is_some());
                } else {
                    let error = result.expect_err("strict contract needs an available projection");
                    assert_eq!(error.kind, ProviderFailureKind::Validation);
                    assert!(error.message.contains("projection"));
                }
            }
            // Both strict modes accept the host's explicit Google projection.
            for mode in [ProjectionMode::ExplicitOnly, ProjectionMode::Exact] {
                let mut req = tool_request("host-catalog-alias");
                req.model.metadata_mut().capability.google_dialect = dialect;
                let mut contract = projected_contract();
                contract.projection.mode = mode;
                if output {
                    req.tools = Arc::new(vec![]);
                    req.output_spec = Some(LlmOutputSpec::JsonSchema(
                        lash_core::llm::types::LlmJsonSchema {
                            name: "answer".into(),
                            schema: contract,
                            strict: false,
                        },
                    ));
                } else {
                    Arc::make_mut(&mut req.tools)[0].input_schema = contract;
                }
                let body = request_body(&req);
                let schema = if output {
                    &body["request"]["generationConfig"]["responseSchema"]
                } else {
                    &body["request"]["tools"][0]["functionDeclarations"][0][if dialect
                        == GoogleDialect::ClaudeOnVertex
                    {
                        "parameters"
                    } else {
                        "parametersJsonSchema"
                    }]
                };
                assert!(schema["properties"].get("projected").is_some());
                assert!(schema["properties"].get("canonical").is_none());
            }
        }
    }
}

#[test]
fn protocol53_only_explicit_gemini_dialect_supplies_missing_signature() {
    use lash_core::GoogleDialect;
    for dialect in [
        GoogleDialect::Legacy,
        GoogleDialect::Gemini3,
        GoogleDialect::ClaudeOnVertex,
    ] {
        let mut req = request(None);
        req.model.metadata_mut().wire_model = "unrelated-host-alias".into();
        req.model.metadata_mut().capability.google_dialect = dialect;
        req.messages = vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::ToolCall {
                call_id: "call".into(),
                tool_name: "lookup".into(),
                input_json: "{}".into(),
                replay: None,
            }],
        )];
        let contents = provider()
            .build_contents_with_attachment_parts(&req)
            .expect("retention policy");
        assert_eq!(
            contents[0]["parts"][0].get("thoughtSignature").cloned(),
            if dialect == GoogleDialect::Gemini3 {
                Some(json!("skip_thought_signature_validator"))
            } else {
                None
            }
        );
    }
}
