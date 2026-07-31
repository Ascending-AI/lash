use lash_core::plugin::{PluginDirective, PluginError, ToolCallHookContext};

pub(super) fn normalize_projected_tool_args(
    ctx: ToolCallHookContext,
) -> Result<Vec<PluginDirective>, PluginError> {
    let original = ctx.args;
    let normalized = crate::projection::normalize_tool_args_for_projection(
        original.clone(),
        &ctx.argument_projection,
    );
    if normalized == original {
        Ok(Vec::new())
    } else {
        Ok(vec![PluginDirective::ReplaceToolArgs { args: normalized }])
    }
}

#[cfg(test)]
mod tests {
    use crate::projection::RlmSeed;

    fn projected(value: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "__projected__": value })
    }

    fn received_tool_args(
        policy: lash_core::ToolArgumentProjectionPolicy,
        args: serde_json::Value,
    ) -> serde_json::Value {
        crate::projection::normalize_tool_args_for_projection(args, &policy)
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
    fn projected_tool_arg_normalization_preserves_seed_roots_for_projection_aware_tools() {
        let args = serde_json::json!({
            "task": projected(serde_json::json!("inspect the file")),
            "capability": "explore",
            "seed": {
                "projected_root": projected(serde_json::json!("carry-over")),
                "computed_record": {
                    "field": projected(serde_json::json!("materialize me"))
                }
            }
        });

        let normalized = seed_preserving_args(args);

        assert_eq!(
            normalized,
            serde_json::json!({
                "task": "inspect the file",
                "capability": "explore",
                "seed": {
                    "projected_root": { "__projected__": "carry-over" },
                    "computed_record": {
                        "field": "materialize me"
                    }
                }
            })
        );
    }

    #[test]
    fn projected_tool_arg_normalization_preserves_continue_as_seed_roots() {
        let args = serde_json::json!({
            "task": projected(serde_json::json!("continue")),
            "seed": {
                "problem": projected(serde_json::json!({ "prompt": "large prompt" }))
            }
        });

        let normalized = seed_preserving_args(args);
        let seed = classify_received_seed(&normalized);

        assert_eq!(
            normalized.get("task").and_then(serde_json::Value::as_str),
            Some("continue")
        );
        assert_eq!(
            seed.projected.entries.as_slice(),
            &[(
                "problem".to_string(),
                serde_json::json!({ "prompt": "large prompt" })
            )]
        );
        assert!(seed.globals.is_empty());
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
    fn ordinary_tool_receives_projected_input_materialized_as_plain_json() {
        let received = materializing_args(serde_json::json!({
            "query": projected(serde_json::json!("lazy query")),
            "options": {
                "limit": projected(serde_json::json!(3)),
                "filters": [
                    projected(serde_json::json!("rust")),
                    "tests"
                ]
            }
        }));

        assert_eq!(
            received,
            serde_json::json!({
                "query": "lazy query",
                "options": {
                    "limit": 3,
                    "filters": ["rust", "tests"]
                }
            })
        );
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
                serde_json::json!("large parent context")
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
            let context = lash_core::plugin::ToolCallHookContext::new(
                "session".to_string(),
                tool_name.to_string(),
                args.clone(),
                policy.clone(),
                lash_core::TurnContext::default(),
                std::sync::Arc::new(lash_core::testing::MockSessionManager::default()),
            );
            let directives = super::normalize_projected_tool_args(context)
                .expect("projection normalization succeeds");
            let [lash_core::plugin::PluginDirective::ReplaceToolArgs { args: normalized }] =
                directives.as_slice()
            else {
                panic!("projection-aware args must be rewritten for {tool_name}")
            };
            assert_eq!(
                normalized,
                &serde_json::json!({
                    "seed": {
                        "projected": {"__projected__": "preserve me"},
                        "computed": {"value": 7}
                    }
                }),
                "tool identity must not participate in projection policy"
            );
        }
    }

    #[test]
    // Architecture lint: lexical cutover guard, not behavior proof. The table
    // above proves the name-independent runtime behavior.
    fn lint_projection_policy_cutover_has_no_name_based_projection_checks() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let registration_src =
            std::fs::read_to_string(manifest_dir.join("src/plugin/registration.rs"))
                .expect("read registration source");
        let tool_args_src = std::fs::read_to_string(manifest_dir.join("src/plugin/tool_args.rs"))
            .expect("read tool args source");
        let executor_src = std::fs::read_to_string(manifest_dir.join("src/executor.rs"))
            .expect("read executor source");
        let host_bridge_src =
            std::fs::read_to_string(manifest_dir.join("src/executor/host_bridge.rs"))
                .expect("read host bridge source");
        let projected_bindings_src =
            std::fs::read_to_string(manifest_dir.join("src/projection/bindings.rs"))
                .expect("read projected bindings source");

        let old_hook_call = ["normalize_tool_args_for_projection", "(&ctx.tool_name"].concat();
        let old_name_match = [
            "matches!",
            "(tool_name, ",
            "\"continue_as\" | \"spawn_agent\")",
        ]
        .concat();
        let old_invalid_ref = [
            "ProjectedBindingError::duplicate(",
            "\"invalid_projection_ref\")",
        ]
        .concat();

        assert!(!registration_src.contains(&old_hook_call));
        assert!(!tool_args_src.contains(&old_hook_call));
        assert!(!executor_src.contains(&old_name_match));
        assert!(!host_bridge_src.contains(&old_name_match));
        assert!(!projected_bindings_src.contains(&old_invalid_ref));
    }
}
