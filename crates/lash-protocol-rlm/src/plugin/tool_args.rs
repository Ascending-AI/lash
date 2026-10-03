use lash_core::plugin::{PluginError, ToolArgsTransformInput};

/// The argument transform both RLM paths register: projected values in a
/// call's arguments become what the tool's projection policy asks for. It
/// returns the arguments even when nothing changed.
pub(crate) fn normalize_projected_tool_args(
    input: ToolArgsTransformInput,
) -> Result<serde_json::Value, PluginError> {
    crate::projection::normalize_tool_args_for_projection(
        input.current,
        &input.context.argument_projection,
    )
    .map_err(|error| PluginError::Session(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod tests {
    use crate::projection::RlmSeed;

    fn projected(value: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "__projected__": lash_rlm_types::RlmProjectedSeedEntry::Materialized(value),
        })
    }

    fn received_tool_args(
        policy: lash_core::ToolArgumentProjectionPolicy,
        args: serde_json::Value,
    ) -> serde_json::Value {
        crate::projection::normalize_tool_args_for_projection(args, &policy)
            .expect("projection transport should be canonical")
    }

    fn materializing_args(args: serde_json::Value) -> serde_json::Value {
        received_tool_args(
            lash_core::ToolArgumentProjectionPolicy::MaterializeProjectedValues,
            args,
        )
    }

    fn seed_preserving_args(args: serde_json::Value) -> serde_json::Value {
        received_tool_args(
            lash_core::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
            args,
        )
    }

    fn classify_received_seed(received: &serde_json::Value) -> RlmSeed {
        RlmSeed::from_tool_args(received).expect("seed should classify")
    }

    #[test]
    fn projected_tool_arg_normalization_materializes_ordinary_tools_recursively() {
        let args = serde_json::json!({
            "path": projected(serde_json::json!("/tmp/projected.txt")),
            "nested": {
                "items": [
                    projected(serde_json::json!("a")),
                    { "plain": projected(serde_json::json!(true)) }
                ]
            }
        });

        let normalized = materializing_args(args);

        assert_eq!(
            normalized,
            serde_json::json!({
                "path": "/tmp/projected.txt",
                "nested": {
                    "items": [
                        "a",
                        { "plain": true }
                    ]
                }
            })
        );
    }

    #[test]
    fn ordinary_tool_receives_non_projected_input_without_materialization() {
        let args = serde_json::json!({
            "query": "plain",
            "options": { "limit": 3, "exact": true }
        });

        let received = materializing_args(args.clone());

        assert_eq!(received, args);
    }

    #[test]
    fn projection_aware_tool_receives_non_projected_seed_as_plain_input() {
        let received = seed_preserving_args(serde_json::json!({
            "task": "continue from facts",
            "capability": "explore",
            "seed": {
                "facts": { "count": 2 },
                "label": "plain"
            }
        }));

        let seed = classify_received_seed(&received);

        assert!(seed.projected.is_empty());
        assert_eq!(
            seed.globals,
            serde_json::Map::from_iter([
                ("facts".to_string(), serde_json::json!({ "count": 2 })),
                ("label".to_string(), serde_json::json!("plain")),
            ])
        );
    }

    #[test]
    fn projection_aware_tool_receives_projected_seed_roots_without_materializing_them() {
        let received = seed_preserving_args(serde_json::json!({
            "task": projected(serde_json::json!("continue from projected context")),
            "capability": "explore",
            "seed": {
                "problem": projected(serde_json::json!("large parent context")),
                "computed": {
                    "summary": projected(serde_json::json!("short summary"))
                }
            }
        }));

        let seed = classify_received_seed(&received);

        assert_eq!(
            received.get("task").and_then(serde_json::Value::as_str),
            Some("continue from projected context")
        );
        assert_eq!(
            seed.projected.entries.as_slice(),
            &[(
                "problem".to_string(),
                lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!(
                    "large parent context"
                ))
            )]
        );
        assert_eq!(
            seed.globals,
            serde_json::Map::from_iter([(
                "computed".to_string(),
                serde_json::json!({ "summary": "short summary" })
            )])
        );
    }

    #[test]
    fn renaming_a_tool_does_not_change_projection_policy() {
        let args = serde_json::json!({
            "seed": {
                "projected": projected(serde_json::json!("preserve me")),
                "computed": {"value": projected(serde_json::json!(7))}
            }
        });
        let policy =
            lash_core::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed");

        for tool_name in ["continue_as", "renamed_control_tool", "arbitrary_tool_name"] {
            let input = lash_core::plugin::ToolArgsTransformInput {
                context: lash_core::testing::tool_hook_context(tool_name, policy.clone()),
                original: std::sync::Arc::new(args.clone()),
                current: args.clone(),
            };
            let normalized = &super::normalize_projected_tool_args(input)
                .expect("projection normalization succeeds");
            assert_eq!(
                normalized,
                &serde_json::json!({
                    "seed": {
                        "projected": {
                            "__projected__": {
                                "kind": "materialized",
                                "value": "preserve me"
                            }
                        },
                        "computed": {"value": 7}
                    }
                }),
                "tool identity must not participate in projection policy"
            );
        }
    }
}
