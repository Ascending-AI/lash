use super::*;

#[tokio::test]
async fn direct_exec_preserves_legacy_argument_normalization() {
    let shell = shell_provider(StandardShell::new().with_cwd("/"));
    let legacy = lash_core::testing::run_tool(
        &shell,
        "exec_command",
        &json!({
            "cmd": "printf legacy-normalized",
            "workdir": null,
            "shell": false,
            "login": null,
            "max_output_tokens": "NoNe",
            "legacy_unknown": "ignored",
        }),
    )
    .await;
    let canonical = lash_core::testing::run_tool(
        &shell,
        "exec_command",
        &json!({"cmd": "printf legacy-normalized"}),
    )
    .await;

    assert!(legacy.is_success(), "{}", legacy.value_for_projection());
    assert!(
        canonical.is_success(),
        "{}",
        canonical.value_for_projection()
    );
    for result in [&legacy, &canonical] {
        assert_eq!(result.value_for_projection()["output"], "legacy-normalized");
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert_eq!(result.value_for_projection()["status"], "completed");
    }
}

#[test]
fn typed_parsers_preserve_legacy_direct_defaults_and_lane_authority() {
    let shell = StandardShell::new().with_cwd("/");
    let public_start = shell
        .parse_public_start_command_params(&json!({
            "cmd": "cat",
            "workdir": null,
            "shell": 7,
            "login": null,
            "max_output_tokens": null,
            "detach": null,
            "legacy_unknown": true,
        }))
        .expect("legacy direct public start args");
    assert_eq!(public_start.workdir, std::path::PathBuf::from("/"));
    assert_eq!(public_start.shell_path, shell.runtime.shell_path);
    assert!(!public_start.login);
    assert_eq!(public_start.max_output_tokens, None);
    assert!(!public_start.detach);
    assert_eq!(public_start.detached_process_id, None);

    let internal_start = shell
        .parse_internal_start_command_params(&json!({
            "cmd": "cat",
            "detached_process_id": 7,
            "legacy_unknown": true,
        }))
        .expect("legacy direct internal start args");
    assert_eq!(internal_start.detached_process_id, None);
    assert!(
        shell
            .parse_public_start_command_params(&json!({
                "cmd": "cat",
                "detached_process_id": "caller-chosen",
            }))
            .is_err(),
        "the public lane must still reject caller-chosen process identity"
    );
}

#[test]
fn schemas_do_not_advertise_legacy_direct_spellings() {
    let definitions = StandardShell::default().tool_definitions();
    let rejects = [
        ("exec_command", json!({"cmd": "echo", "workdir": null})),
        ("exec_command", json!({"cmd": "echo", "login": null})),
        (
            "exec_command",
            json!({"cmd": "echo", "max_output_tokens": "none"}),
        ),
        (
            "exec_command",
            json!({"cmd": "echo", "legacy_unknown": true}),
        ),
        ("start_command", json!({"cmd": "cat", "detach": null})),
        (
            "run_start_command",
            json!({"cmd": "cat", "detached_process_id": null}),
        ),
    ];

    for (name, args) in rejects {
        let definition = definitions
            .iter()
            .find(|definition| definition.name() == name)
            .expect("shell definition");
        assert!(
            lash_sansio::validate_tool_input(&definition.contract, &args).is_err(),
            "{name} schema unexpectedly admitted legacy direct input {args}"
        );
    }
}
